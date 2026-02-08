use super::super::{
    control::*,
    io_helpers::{NonBlockingReader, PtyIoError},
};
use super::helpers::HandleReader;
use super::pty_impl::PtyImpl;
use crate::pty::Msg;
use crossbeam::channel::Sender;
use portable_pty::win::ConPtyMaster;
use portable_pty::{ChildKiller, MasterPty};
use std::{
    io::{self, ErrorKind},
    os::windows::io::AsRawHandle,
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0},
    System::Threading::{WaitForMultipleObjects, INFINITE},
};

const CONTROL_EVENT_INDEX: u32 = WAIT_OBJECT_0 + 1;

impl PtyImpl {
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
        master: Arc<Mutex<ConPtyMaster>>,
        killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
        tx: Sender<Msg>,
    ) {
        let control_read_handle = pty.control_pipe_read.as_raw_handle() as HANDLE;
        let master_clone = master.clone();

        thread::spawn(move || {
            debug("read-thread started");
            let mut buf = vec![0; 65536];
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

            // Use pre-extracted pty_handle (no lock needed)
            let pty_handle = pty.pty_handle;

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

            loop {
                debug("read-thread: waiting for events...");
                let handles = [pty_handle, control_read_handle];
                let wait_result = unsafe {
                    WaitForMultipleObjects(
                        handles.len() as u32,
                        handles.as_ptr(),
                        false.into(),
                        INFINITE,
                    )
                };

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
                    CONTROL_EVENT_INDEX => {
                        // Control event
                        debug("read-thread: control pipe has data");
                        let mut temp_buf = vec![0; 8192];
                        let mut reader = HandleReader(control_read_handle);
                        if reader.read_all_nonblocking(&mut control_buf).is_err() {
                            debug("Control read error");
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
                    _ => {}
                }
            }

            debug("read-thread: loop exited, sending Msg::End");
            let _ = tx.send(Msg::End);
            debug("read-thread: ended");
        });
    }
}
