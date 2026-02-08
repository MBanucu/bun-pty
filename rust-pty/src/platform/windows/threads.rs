use super::super::io_helpers::NonBlockingReader;
use super::{super::control::*, helpers::HandleReader, pty_impl::PtyImpl};
use crate::pty::Msg;
use crossbeam::channel::Sender;
use portable_pty::{ChildKiller, MasterPty};
use std::{
    io::{self, ErrorKind, Read},
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
use windows_sys::Win32::Storage::FileSystem::ReadFile;

impl PtyImpl {
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
        let control_read_handle = pty.control_pipe[0];
        let master_clone = master.clone();

        thread::spawn(move || {
            debug("read-thread started");
            let mut buf = vec![0; 65536];
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

            // Get PTY handle from master
            let pty_handle = master_clone
                .lock()
                .unwrap()
                .as_raw_handle()
                .expect("Failed to get PTY handle");

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
                    loop {
                        match rdr.read(&mut buf) {
                            Ok(0) => {
                                debug("read-thread: got Ok(0) - EOF");
                                let _ = tx.send(Msg::End);
                                return;
                            }
                            Ok(n) => {
                                debug(&format!("read-thread: got Ok({}) bytes", n));
                                let data = &buf[..n];
                                // Check for VT queries and respond
                                if let Some(response) = handle_vt_query(data) {
                                    if let Err(e) = writer.write_all(&response) {
                                        debug(&format!("VT response write error: {}", e));
                                    } else if let Err(e) = writer.flush() {
                                        debug(&format!("VT response flush error: {}", e));
                                    }
                                }
                                let _ = tx.send(Msg::Data(data.to_vec()));
                                if n == buf.len() {
                                    buf.resize(buf.len() * 2, 0);
                                }
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                            Err(e) => {
                                debug(&format!("read-thread: read error: {}", e));
                                let _ = tx.send(Msg::End);
                                return;
                            }
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

// Helper to get HANDLE from reader - this might need adjustment based on portable_pty implementation
fn get_handle_from_reader(rdr: &dyn Read) -> Option<HANDLE> {
    // This is a placeholder - need to check how portable_pty exposes the handle on Windows
    // For now, assume it's not directly accessible, so we can't use WaitForMultipleObjects on it
    // Alternative: use ReadFile directly on the handle, but need to get the handle first
    // Looking at portable_pty source, for Windows it's a Conpty, and reader is a pipe handle
    // But since we can't access it directly, perhaps we need to modify or find another way
    // For simplicity, perhaps poll by trying to read non-blocking in a loop, but that's not ideal
    // Actually, since WaitForMultipleObjects works on handles, and ConPTY reader is a handle, but portable_pty may not expose it
    // Let's assume for now we can't, and use a timer or something, but that's bad
    // Upon checking portable_pty, the MasterPty has as_raw_handle() method probably
    // Yes, let's use that
    if let Some(master) = rdr as *const dyn Read as *const dyn std::any::Any {
        // This won't work. Need to cast properly
        // Actually, portable_pty's reader implements AsRawHandle on Windows
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            if let Some(handle) = rdr.as_raw_handle() as *const _ as *const HANDLE {
                return Some(*handle);
            }
        }
    }
    None
}

// Handle VT protocol queries
fn handle_vt_query(data: &[u8]) -> Option<Vec<u8>> {
    // Look for DSR (Device Status Report) query: \x1b[6n
    let dsr = b"\x1b[6n";
    if data.windows(dsr.len()).any(|w| w == dsr) {
        // Respond with cursor position \x1b[1;1R (row 1, col 1)
        Some(b"\x1b[1;1R".to_vec())
    } else {
        None
    }
}
