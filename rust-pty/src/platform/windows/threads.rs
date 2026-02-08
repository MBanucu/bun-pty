use super::super::control::*;
use crate::debug;
use crate::pty::Msg;
use crossbeam::channel::Sender;
use portable_pty::{ChildKiller, MasterPty};
use std::io::{self, ErrorKind, Write};
use std::mem;
use std::ptr;
use std::sync::{atomic::Ordering, Arc, Mutex};
use std::thread;
use windows_sys::Win32::Foundation::{FALSE, HANDLE, TRUE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

pub fn spawn_wait_thread(
    pty: Arc<super::PtyImpl>,
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

pub fn spawn_read_thread(
    pty: Arc<super::PtyImpl>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    tx: Sender<Msg>,
) {
    let rdr = master.lock().unwrap().try_clone_reader().unwrap();
    let master_clone = master.clone();
    thread::spawn(move || {
        debug("read-thread started");

        // Get PTY handle from reader (unsafe access assuming Reader { handle: HANDLE })
        let pty_handle = unsafe {
            use std::ffi::c_void;
            use std::mem::transmute;
            let raw = Box::into_raw(rdr);
            let parts: (*mut c_void, *const ()) = transmute(raw);
            let handle_ptr = parts.0 as *const HANDLE;
            let pty_handle = *handle_ptr;
            let _rdr = Box::from_raw(raw);
            pty_handle
        };

        let control_handle = pty.control_pipe[0];

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
            pty_handle, control_handle
        ));

        // Create manual-reset events (TRUE for manual, initial FALSE)
        let pty_event = unsafe { CreateEventW(ptr::null_mut(), TRUE, FALSE, ptr::null_mut()) };
        if pty_event == 0 {
            debug("Failed to create PTY event");
            let _ = tx.send(Msg::End);
            return;
        }

        let control_event = unsafe { CreateEventW(ptr::null_mut(), TRUE, FALSE, ptr::null_mut()) };
        if control_event == 0 {
            debug("Failed to create control event");
            unsafe { windows_sys::Win32::Foundation::CloseHandle(pty_event) };
            let _ = tx.send(Msg::End);
            return;
        }

        // Buffers
        let mut pty_buf = vec![0u8; 131072];
        let mut control_temp = vec![0u8; 8192];
        let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

        // Overlapped structures
        let mut pty_overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        let mut control_overlapped: OVERLAPPED = unsafe { mem::zeroed() };

        // Pending flags
        let mut pty_pending = false;
        let mut control_pending = false;

        // Function to initiate overlapped read
        fn initiate_read(
            handle: HANDLE,
            buf: &mut [u8],
            overlapped: &mut OVERLAPPED,
            event: HANDLE,
        ) -> Result<Option<u32>, io::Error> {
            overlapped.hEvent = event;
            let mut bytes_read = 0u32;
            let res = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut bytes_read,
                    overlapped,
                )
            };
            if res != FALSE {
                Ok(Some(bytes_read))
            } else {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(997i32) {
                    // ERROR_IO_PENDING
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }

        // Process PTY data
        let process_pty_data = |data: &[u8], writer: &mut dyn Write, tx: &Sender<Msg>| -> bool {
            if data.is_empty() {
                let _ = tx.send(Msg::End);
                return true; // Exit loop
            }
            if let Some(response) = handle_vt_query(data) {
                let _ = writer.write_all(&response);
                let _ = writer.flush();
            }
            let _ = tx.send(Msg::Data(data.to_vec()));
            false
        };

        // Process control data
        let process_control_data = |temp_data: &[u8],
                                    control_buf: &mut Vec<u8>,
                                    writer: &mut dyn Write,
                                    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
                                    killer: &Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
                                    tx: &Sender<Msg>|
         -> bool {
            control_buf.extend_from_slice(temp_data);
            process_control_messages(control_buf, writer, master, killer, tx)
        };

        // Process control data
        let process_control_data = |temp_data: &[u8],
                                    control_buf: &mut Vec<u8>,
                                    writer: &mut dyn Write,
                                    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
                                    killer: &Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
                                    tx: &Sender<Msg>|
         -> bool {
            control_buf.extend_from_slice(temp_data);
            process_control_messages(control_buf, writer, master, killer, tx)
        };

        loop {
            // Initiate PTY read if not pending
            if !pty_pending {
                pty_buf.fill(0);
                match initiate_read(pty_handle, &mut pty_buf, &mut pty_overlapped, pty_event) {
                    Ok(Some(bytes)) => {
                        let data = &pty_buf[0..bytes as usize];
                        if process_pty_data(data, &mut *writer, &tx) {
                            break;
                        }
                    }
                    Ok(None) => pty_pending = true,
                    Err(e) => {
                        debug(&format!("PTY initiate read error: {}", e));
                        if e.kind() == ErrorKind::BrokenPipe {
                            let _ = tx.send(Msg::End);
                            break;
                        }
                    }
                }
            }

            // Initiate control read if not pending
            if !control_pending {
                control_temp.fill(0);
                match initiate_read(
                    control_handle,
                    &mut control_temp,
                    &mut control_overlapped,
                    control_event,
                ) {
                    Ok(Some(bytes)) => {
                        let temp_data = &control_temp[0..bytes as usize];
                        if process_control_data(
                            temp_data,
                            &mut control_buf,
                            &mut *writer,
                            &master_clone,
                            &killer,
                            &tx,
                        ) {
                            break;
                        }
                    }
                    Ok(None) => control_pending = true,
                    Err(e) => {
                        debug(&format!("Control initiate read error: {}", e));
                    }
                }
            }

            // Prepare wait handles (only for pending reads)
            let mut wait_handles = Vec::new();
            if pty_pending {
                wait_handles.push(pty_event);
            }
            if control_pending {
                wait_handles.push(control_event);
            }

            if wait_handles.is_empty() {
                // Rare case: no pending reads; short sleep to avoid spin
                thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }

            // Wait for any event
            let ret = unsafe {
                WaitForMultipleObjects(
                    wait_handles.len() as u32,
                    wait_handles.as_ptr(),
                    FALSE,
                    INFINITE,
                )
            };
            if ret == WAIT_FAILED {
                debug(&format!(
                    "WaitForMultipleObjects error: {}",
                    io::Error::last_os_error()
                ));
                break;
            }

            let signaled_index = (ret - WAIT_OBJECT_0) as usize;
            let signaled_handle = wait_handles[signaled_index];

            if pty_pending && signaled_handle == pty_event {
                let mut bytes = 0u32;
                let res = unsafe {
                    GetOverlappedResult(pty_handle, &mut pty_overlapped, &mut bytes, FALSE)
                };
                unsafe { ResetEvent(pty_event) };
                if res == 0 {
                    let err = io::Error::last_os_error();
                    debug(&format!("PTY GetOverlappedResult error: {}", err));
                    if err.kind() == ErrorKind::BrokenPipe {
                        let _ = tx.send(Msg::End);
                        break;
                    }
                } else {
                    let data = &pty_buf[0..bytes as usize];
                    if process_pty_data(data, &mut *writer, &tx) {
                        break;
                    }
                }
                pty_pending = false;
            } else if control_pending && signaled_handle == control_event {
                let mut bytes = 0u32;
                let res = unsafe {
                    GetOverlappedResult(control_handle, &mut control_overlapped, &mut bytes, FALSE)
                };
                unsafe { ResetEvent(control_event) };
                if res == 0 {
                    debug(&format!(
                        "Control GetOverlappedResult error: {}",
                        io::Error::last_os_error()
                    ));
                } else {
                    let temp_data = &control_temp[0..bytes as usize];
                    if process_control_data(
                        temp_data,
                        &mut control_buf,
                        &mut *writer,
                        &master_clone,
                        &killer,
                        &tx,
                    ) {
                        break;
                    }
                }
                control_pending = false;
            }
        }

        // Cleanup events
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(pty_event);
            windows_sys::Win32::Foundation::CloseHandle(control_event);
        }

        let _ = tx.send(Msg::End);
        debug("read-thread: ended");
    });
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
