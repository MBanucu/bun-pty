// Windows-specific implementation (shared with macOS, as portable-pty handles cross-platform)
use super::control::*;
use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::unbounded;
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
    os::windows::io::OwnedHandle,
    io::ErrorKind,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_FAILED, INVALID_HANDLE_VALUE, GetLastError},
    System::Threading::{WaitForMultipleObjects, INFINITE},
    Storage::FileSystem::{CreatePipe, PIPE_NOWAIT, SetNamedPipeHandleState, ReadFile, WriteFile, SECURITY_ATTRIBUTES},
    System::Pipes::PIPE_ACCESS_DUPLEX,
};

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
        let mut child = pair.slave.spawn_command(cmd.to_builder())?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
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
            use windows_sys::Win32::Storage::FileSystem::SetNamedPipeHandleState;
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
        Self::spawn_read_thread(pty.clone(), master, tx_r);

        Ok(pty)
    }

    fn spawn_wait_thread(pty: Arc<Self>, mut child: Box<dyn portable_pty::Child + Send + Sync>) {
        thread::spawn(move || {
            debug("wait-thread: waiting for child...");
            let status = child.wait();
            debug("wait-thread: child.wait() returned");
            if let Ok(exit_status) = status {
                let code = exit_status.exit_code() as i32;
                debug(&format!("exit_status.exit_code(): {}", code));
                pty.exit_code.store(code, Ordering::Relaxed);
            }
            pty.exited.store(true, Ordering::Relaxed);
            debug("wait-thread: exited and code stored");
        });
    }

    fn spawn_read_thread(
        pty: Arc<Self>,
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
        tx: Sender<Msg>,
    ) {
        let mut rdr = master.lock().unwrap().try_clone_reader().unwrap();
        let control_handle = pty.control_pipe_read.as_raw_handle();
        let master_clone = master.clone();
        let killer = pty.killer.clone();

        // Set PTY reader to non-blocking
        let mode: u32 = PIPE_NOWAIT;
        unsafe {
            if SetNamedPipeHandleState(rdr.as_raw_handle() as HANDLE, Some(&mode), std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                debug("Failed to set PTY pipe to non-blocking");
                let _ = tx.send(Msg::End);
                return;
            }
        }

        thread::spawn(move || {
            debug("read-thread started");
            let mut buf = vec![0; 131072]; // Increased for better draining
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

            // Get PTY handle
            let pty_handle = master_clone.lock().unwrap().as_raw_handle();

            // Take writer
            let mut writer = match master_clone.lock().unwrap().take_writer() {
                Ok(w) => w,
                Err(e) => {
                    debug(&format!("Failed to take writer: {}", e));
                    let _ = tx.send(Msg::End);
                    return;
                }
            };

            loop {
                let handles: [HANDLE; 2] = [pty_handle as HANDLE, control_handle as HANDLE];
                let res = unsafe { WaitForMultipleObjects(handles.as_ptr(), handles.len() as u32, INFINITE, 0) };

                if res == 0xFFFFFFFF { // WAIT_FAILED
                    let err = unsafe { GetLastError() };
                    debug(&format!("WaitForMultipleObjects failed with error: {}", err));
                    break;
                }

                if res == WAIT_OBJECT_0 {
                    // PTY ready: Drain all data non-blockingly
                    loop {
                        match rdr.read(&mut buf) {
                            Ok(0) => {
                                debug("read-thread: got Ok(0) - EOF");
                                let _ = tx.send(Msg::End);
                                return;
                            }
                            Ok(n) => {
                                debug(&format!("read-thread: got Ok({}) bytes", n));
                                let _ = tx.send(Msg::Data(buf[..n].to_vec()));
                            }
                            Err(e) if e.raw_os_error() == Some(232) || e.kind() == ErrorKind::WouldBlock => {
                                break;
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(e) => {
                                debug(&format!("read-thread: read error: {}", e));
                                let _ = tx.send(Msg::End);
                                return;
                            }
                        }
                    }
                } else if res == WAIT_OBJECT_0 + 1 {
                    // Control ready: Read messages
                    if Self::read_all_nonblocking_handle(control_handle, &mut control_buf).is_err() {
                        debug(&format!("Control read error"));
                    }
                    if process_control_messages(&mut control_buf, &mut writer, &master_clone, &killer, &tx) {
                        break; // Kill processed
                    }
                }
            }
            debug("read-thread: loop exited, sending Msg::End");
            let _ = tx.send(Msg::End);
            debug("read-thread: ended");
        });
    }

    fn read_all_nonblocking_handle(handle: *mut std::ffi::c_void, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use std::io::Error;
        let mut temp = [0u8; 8192];
        let mut total = 0;
        loop {
            let mut bytes_read = 0u32;
            let res = unsafe { ReadFile(handle as HANDLE, temp.as_mut_ptr() as *mut std::ffi::c_void, temp.len() as u32, &mut bytes_read, std::ptr::null_mut()) };
            if res == 0 {
                let err = Error::last_os_error();
                if err.raw_os_error() == Some(232) { // ERROR_NO_DATA for non-blocking pipes
                    break;
                }
                return Err(err);
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

impl crate::pty::PtyTrait for PtyImpl {
    fn read(&self, blocking: bool) -> Result<crate::pty::Msg, Box<dyn std::error::Error + Send + Sync>> {
        self.reader.read(blocking)
    }

    fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Send write message via control pipe
        let mut msg = vec![MSG_WRITE];
        msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
        msg.extend_from_slice(data);
        Self::write_all_nonblocking_handle(self.control_pipe_write.as_raw_handle(), &msg)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut msg = vec![MSG_RESIZE];
        msg.extend_from_slice(&size.rows.to_le_bytes());
        msg.extend_from_slice(&size.cols.to_le_bytes());
        Self::write_all_nonblocking_handle(self.control_pipe_write.as_raw_handle(), &msg)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let msg = vec![MSG_KILL];
        Self::write_all_nonblocking_handle(self.control_pipe_write.as_raw_handle(), &msg)?;
        Ok(())
    }

    fn get_pid(&self) -> i32 {
        self.pid
    }

    fn get_exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Relaxed)
    }

    fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }
}

impl PtyImpl {
    fn write_all_nonblocking_handle(handle: *mut std::ffi::c_void, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use std::io::Error;
        let mut pos = 0;
        while pos < data.len() {
            let mut bytes_written = 0u32;
            let res = unsafe { WriteFile(handle as HANDLE, data[pos..].as_ptr() as *const std::ffi::c_void, (data.len() - pos) as u32, &mut bytes_written, std::ptr::null_mut()) };
            if res == 0 {
                let err = Error::last_os_error();
                if err.raw_os_error() == Some(997) { // ERROR_IO_PENDING
                    continue;
                }
                return Err(err.into());
            }
            if bytes_written == 0 {
                return Err("Pipe closed".into());
            }
            pos += bytes_written as usize;
        }
        Ok(())
    }
}