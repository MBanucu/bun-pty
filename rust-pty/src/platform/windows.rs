// Windows-specific implementation
use super::{control::*, io_helpers::{NonBlockingReader, NonBlockingWriter, PtyIoError}};
use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::{unbounded, Sender};
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    io::{self, ErrorKind},
    os::windows::io::{AsRawHandle, OwnedHandle},
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_FAILED, INVALID_HANDLE_VALUE},
    System::Threading::{WaitForMultipleObjects, INFINITE},
    Storage::FileSystem::{PIPE_NOWAIT, SECURITY_ATTRIBUTES, SetNamedPipeHandleState, ReadFile, WriteFile},
    System::Pipes::{CreatePipe, PIPE_ACCESS_DUPLEX},
};

// Windows-specific error codes
const ERROR_NO_DATA: i32 = 232;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_IO_PENDING: i32 = 997;

/// Wrapper for Windows handles to implement NonBlockingReader
pub struct HandleReader(pub HANDLE);

impl NonBlockingReader for HandleReader {
    fn read_all_nonblocking(&mut self, buf: &mut Vec<u8>) -> Result<usize, PtyIoError> {
        use std::io::Error;
        let mut temp = [0u8; 131072]; // Larger buffer for Windows
        let mut total = 0;
        loop {
            let mut bytes_read = 0u32;
            let res = unsafe { ReadFile(self.0, temp.as_mut_ptr() as *mut std::ffi::c_void, temp.len() as u32, &mut bytes_read, std::ptr::null_mut()) };
            if res == 0 {
                let err = Error::last_os_error();
                let raw_err = err.raw_os_error().unwrap_or(0);
                if raw_err == ERROR_NO_DATA {
                    break;
                } else if raw_err == ERROR_IO_PENDING {
                    continue; // For async, but here non-blocking
                }
                return Err(PtyIoError::from(err));
            }
            if bytes_read == 0 {
                break;
            }
            buf.extend_from_slice(&temp[0..bytes_read as usize]);
            total += bytes_read as usize;
        }
        Ok(total)
    }
}

/// Wrapper for Windows handles to implement NonBlockingWriter
pub struct HandleWriter(pub HANDLE);

impl NonBlockingWriter for HandleWriter {
    fn write_all_nonblocking(&mut self, data: &[u8]) -> Result<(), PtyIoError> {
        use std::io::Error;
        let mut pos = 0;
        while pos < data.len() {
            let mut bytes_written = 0u32;
            let res = unsafe { WriteFile(self.0, data[pos..].as_ptr() as *const std::ffi::c_void, (data.len() - pos) as u32, &mut bytes_written, std::ptr::null_mut()) };
            if res == 0 {
                let err = Error::last_os_error();
                let raw_err = err.raw_os_error().unwrap_or(0);
                if raw_err == ERROR_IO_PENDING {
                    continue;
                }
                return Err(PtyIoError::from(err));
            }
            if bytes_written == 0 {
                return Err(PtyIoError::BrokenPipe);
            }
            pos += bytes_written as usize;
        }
        Ok(())
    }
}

pub struct PtyImpl {
    pub(crate) reader: crate::pty::Reader,
    pub(crate) master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub(crate) killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    pub(crate) exited: AtomicBool,
    pub(crate) exit_code: AtomicI32,
    pub(crate) pid: i32,
    pub(crate) control_pipe_read: OwnedHandle,
    pub(crate) control_pipe_write: OwnedHandle,
}

impl PtyImpl {
    pub fn new(cmd: &crate::pty::Command, size: PtySize) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let sys = native_pty_system();
        let pair = sys.openpty(size)?;
        let child = pair.slave.spawn_command(cmd.to_builder())?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let killer_clone = killer.clone();
        let pid = child.process_id().map(|p| p as i32).unwrap_or(-1);

        let (tx_r, rx_r) = unbounded::<Msg>();

        let master = Arc::new(Mutex::new(pair.master));

        // Create control pipe
        let mut read_handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut write_handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: false.into(),
        };

        unsafe {
            if CreatePipe(&mut read_handle, &mut write_handle, Some(&sa), 0) == 0 {
                return Err("Failed to create control pipe".into());
            }
            // Set non-blocking
            let mode = PIPE_NOWAIT;
            SetNamedPipeHandleState(read_handle, Some(&mode), std::ptr::null_mut(), std::ptr::null_mut());
            SetNamedPipeHandleState(write_handle, Some(&mode), std::ptr::null_mut(), std::ptr::null_mut());
        }

        let control_pipe_read = unsafe { OwnedHandle::from_raw_handle(read_handle as _) };
        let control_pipe_write = unsafe { OwnedHandle::from_raw_handle(write_handle as _) };

        let pty = Arc::new(Self {
            reader: Reader::new(rx_r),
            master: master.clone(),
            killer,
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(-1),
            pid,
            control_pipe_read,
            control_pipe_write,
        });

        // Spawn threads
        Self::spawn_wait_thread(pty.clone(), child);
        Self::spawn_read_thread(pty.clone(), master.clone(), killer_clone, tx_r.clone());

        Ok(pty)
    }

    pub(super) fn spawn_wait_thread(pty: Arc<Self>, mut child: Box<dyn portable_pty::Child + Send + Sync>) {
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
        let mut rdr = master.lock().unwrap().try_clone_reader().unwrap();
        let control_read_handle = pty.control_pipe_read.as_raw_handle() as HANDLE;
        let master_clone = master.clone();

        thread::spawn(move || {
            debug("read-thread started");
            let mut buf = vec![0; 65536];
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

            // Get PTY handle
            let pty_handle = master_clone.lock().unwrap().as_raw_handle() as HANDLE;

            // Take writer once
            let mut writer = match master_clone.lock().unwrap().take_writer() {
                Ok(w) => w,
                Err(e) => {
                    debug(&format!("Failed to take writer: {}", e));
                    let _ = tx.send(Msg::End);
                    return;
                }
            };

            debug(&format!("read-thread: got PTY handle {:?}, control handle {:?}", pty_handle, control_read_handle));

            loop {
                debug("read-thread: waiting for events...");
                let handles = [pty_handle, control_read_handle];
                let wait_result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), false.into(), INFINITE) };

                if wait_result == WAIT_FAILED {
                    debug(&format!("Wait failed: {}", io::Error::last_os_error()));
                    break;
                }

                match wait_result {
                    WAIT_OBJECT_0 => {
                        // PTY event
                        debug("read-thread: PTY has data");
                        let mut temp_buf = vec![0; 65536];
                        let mut reader = HandleReader(pty_handle);
                        match reader.read_all_nonblocking(&mut temp_buf) {
                            Ok(0) => {
                                debug("read-thread: got 0 bytes - EOF");
                                let _ = tx.send(Msg::End);
                                return;
                            }
                            Ok(n) => {
                                debug(&format!("read-thread: read {} bytes", n));
                                let _ = tx.send(Msg::Data(temp_buf[..n].to_vec()));
                            }
                            Err(e) => {
                                debug(&format!("read-thread: read error: {}", e));
                                let _ = tx.send(Msg::End);
                                return;
                            }
                        }
                    }
                    WAIT_OBJECT_0 + 1 => {
                        // Control event
                        debug("read-thread: control pipe has data");
                        let mut temp_buf = vec![0; 8192];
                        let mut reader = HandleReader(control_read_handle);
                        if reader.read_all_nonblocking(&mut control_buf).is_err() {
                            debug("Control read error");
                        }
                        if process_control_messages(&mut control_buf, &mut writer, &master_clone, &killer, &tx) {
                            break; // Kill processed
                        }
                    }
                    _ => {}
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
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = vec![MSG_RESIZE];
        buf.extend_from_slice(&size.rows.to_le_bytes());
        buf.extend_from_slice(&size.cols.to_le_bytes());
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
        writer.write_all_nonblocking(&[MSG_KILL]).map_err(Into::into)
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
        // Cleanup handles if needed
    }
}