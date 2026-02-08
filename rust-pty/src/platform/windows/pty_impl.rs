use super::super::control::*;
use super::super::io_helpers::NonBlockingWriter;
use super::helpers::HandleWriter;
use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::unbounded;
use portable_pty::{native_pty_system, ChildKiller, MasterPty, PtySize};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::{
    CreatePipe, SetNamedPipeHandleState, PIPE_NOWAIT, PIPE_READMODE_BYTE,
};

use super::super::io_helpers::NonBlockingReader;
use super::helpers::HandleReader;
use crate::debug;
use crate::platform::control::process_control_messages;
use crossbeam::channel::Sender;
use std::{ffi::c_void, io, mem::transmute, thread};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;

pub struct PtyImpl {
    pub(crate) reader: crate::pty::Reader,
    #[allow(dead_code)]
    pub(crate) master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    #[allow(dead_code)]
    pub(crate) killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    pub(crate) exited: AtomicBool,
    pub(crate) exit_code: AtomicI32,
    pub(crate) pid: i32,
    pub(crate) control_pipe: [HANDLE; 2], // [read_handle, write_handle]
}

impl PtyImpl {
    pub fn new(
        cmd: &crate::pty::Command,
        size: PtySize,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let sys = native_pty_system();
        let pair = sys.openpty(size)?;
        let child = pair.slave.spawn_command(cmd.to_builder())?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let killer_clone = killer.clone();
        let pid = child.process_id().map(|p| p as i32).unwrap_or(-1);

        // Channels for reader
        let (tx_r, rx_r) = unbounded::<Msg>();

        let master = Arc::new(Mutex::new(pair.master));

        // Create control pipe
        let mut control_pipe: [HANDLE; 2] = [0; 2];
        if unsafe {
            CreatePipe(
                &mut control_pipe[0],
                &mut control_pipe[1],
                std::ptr::null(),
                0,
            )
        } == 0
        {
            return Err("Failed to create control pipe".into());
        }

        // Set control pipe read handle to non-blocking mode
        unsafe {
            let mut mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
            SetNamedPipeHandleState(
                control_pipe[0],
                &mut mode,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }

        let pty = Arc::new(Self {
            reader: Reader::new(rx_r),
            master: master.clone(),
            killer,
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(-1),
            pid,
            control_pipe,
        });

        // Wait thread for child exit
        Self::spawn_wait_thread(pty.clone(), child);

        // Read thread for PTY I/O and control handling
        Self::spawn_read_thread(pty.clone(), master.clone(), killer_clone, tx_r.clone());

        Ok(pty)
    }

    pub(super) fn spawn_wait_thread(
        pty: Arc<Self>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        thread::spawn(move || {
            debug("wait-thread: waiting for child...");
            let status = child.wait();
            debug("wait-thread: child.wait() returned");
            if let Ok(exit_status) = status {
                let code = exit_status.exit_code() as i32;
                debug(&format!("exit_status.exit_code(): {}", code));
                pty.exit_code.store(code, Ordering::Release);
            }
            pty.exited.store(true, Ordering::Release);
            debug("wait-thread: exited and code stored");
        });
    }

    pub(super) fn spawn_read_thread(
        pty: Arc<Self>,
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
        killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
        tx: Sender<Msg>,
    ) {
        let rdr = master.lock().unwrap().try_clone_reader().unwrap();
        let control_read_handle = pty.control_pipe[0];
        let master_clone = master.clone();

        thread::spawn(move || {
            debug("read-thread started");
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);
            let control_read_handle = control_read_handle;
            // Get PTY handle from reader (unsafe access assuming Reader { handle: HANDLE })
            let pty_handle = unsafe {
                let raw = Box::into_raw(rdr);
                let parts: (*mut c_void, *const ()) = transmute(raw);
                let handle_ptr = parts.0 as *const HANDLE;
                let pty_handle = *handle_ptr;
                let _rdr = Box::from_raw(raw);
                pty_handle
            };
            // Set PTY reader pipe to non-blocking mode
            unsafe {
                let mut mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
                SetNamedPipeHandleState(
                    pty_handle,
                    &mut mode,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            // Take writer once
            let mut writer = match master_clone.lock().unwrap().take_writer() {
                Ok(w) => w,
                Err(e) => {
                    debug(&format!("Failed to take writer: {}", e));
                    let _ = tx.send(Msg::End);
                    return;
                }
            };
            debug(&format!(
                "read-thread: got PTY handle {:?}, control handle {:?}",
                pty_handle, control_read_handle
            ));
            // Wait handles: [PTY, Control]
            let handles = [pty_handle, control_read_handle];
            loop {
                debug("read-thread: waiting for objects...");
                let ret = unsafe {
                    WaitForMultipleObjects(
                        handles.len() as u32,
                        handles.as_ptr(),
                        0,          // Wait for any
                        0xFFFFFFFF, // INFINITE
                    )
                };
                debug(&format!(
                    "read-thread: WaitForMultipleObjects returned {}",
                    ret
                ));
                if ret == 0xFFFFFFFF {
                    debug(&format!(
                        "WaitForMultipleObjects error: {}",
                        io::Error::last_os_error()
                    ));
                    break;
                }
                let signaled_index = (ret - WAIT_OBJECT_0) as usize;
                // Handle control events first
                if signaled_index == 1 {
                    debug("read-thread: control pipe has data");
                    let mut control_reader = HandleReader(control_read_handle);
                    if control_reader
                        .read_all_nonblocking(&mut control_buf)
                        .is_err()
                    {
                        debug(&format!("Control read error"));
                    }
                    if process_control_messages(
                        &mut control_buf,
                        &mut writer,
                        &master_clone,
                        &killer,
                        &tx,
                    ) {
                        break; // Kill processed
                    }
                }
                // Handle PTY data
                if signaled_index == 0 {
                    debug("read-thread: PTY has data");
                    let mut temp_buf = Vec::new();
                    match HandleReader(pty_handle).read_all_nonblocking(&mut temp_buf) {
                        Ok(0) => {} // no data available
                        Ok(_) => {
                            let data = &temp_buf;
                            // Check for VT queries and respond
                            if let Some(response) = handle_vt_query(data) {
                                if let Err(e) = writer.write_all(&response) {
                                    debug(&format!("VT response write error: {}", e));
                                } else if let Err(e) = writer.flush() {
                                    debug(&format!("VT response flush error: {}", e));
                                }
                            }
                            let _ = tx.send(Msg::Data(data.clone()));
                        }
                        Err(e) => {
                            debug(&format!("PTY read error: {}", e));
                            let _ = tx.send(Msg::End);
                            return;
                        }
                    }
                }
            }
            debug("read-thread: loop exited, sending Msg::End");
            let _ = tx.send(Msg::End);
            debug("read-thread: ended");
        });
    }
}

impl PtyTrait for PtyImpl {
    fn read(&self, blocking: bool) -> Result<Msg, Box<dyn std::error::Error + Send + Sync>> {
        self.reader.read(blocking)
    }

    fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug(&format!("PtyImpl::write: writing {} bytes", data.len()));
        let mut buf = vec![MSG_WRITE];
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(data);
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = vec![MSG_RESIZE];
        buf.extend_from_slice(&size.rows.to_le_bytes());
        buf.extend_from_slice(&size.cols.to_le_bytes());
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer
            .write_all_nonblocking(&[MSG_KILL])
            .map_err(Into::into)
    }

    fn get_pid(&self) -> i32 {
        self.pid
    }

    fn get_exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }

    fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for PtyImpl {
    fn drop(&mut self) {
        unsafe {
            if self.control_pipe[0] != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.control_pipe[0]);
            }
            if self.control_pipe[1] != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.control_pipe[1]);
            }
        }
    }
}

fn handle_vt_query(data: &[u8]) -> Option<Vec<u8>> {
    // Look for DSR (Device Status Report) query: \x1b[6n
    let dsr = b"\x1b[6n";
    if data.windows(dsr.len()).any(|w| w == dsr) {
        // Respond with cursor position \x1b[1;1R (row 1, col 1)
        Some(b"\x1b[1;1R".to_vec())
    } else {
        // Check for other VT sequences that may need responses
        let focus9001 = b"\x1b[?9001h";
        let focus1004 = b"\x1b[?1004h";
        if data.windows(focus9001.len()).any(|w| w == focus9001) {
            Some(b"\x1b[?9001h".to_vec()) // Acknowledge
        } else if data.windows(focus1004.len()).any(|w| w == focus1004) {
            Some(b"\x1b[?1004h".to_vec()) // Acknowledge
        } else {
            None
        }
    }
}
