use super::super::control::*;
use crate::debug;
use crate::pty::Msg;
use crossbeam::channel::Sender;
use portable_pty::{ChildKiller, MasterPty, PtySize};
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::{atomic::Ordering, Arc, Mutex};
use std::thread;

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

pub fn spawn_pty_read_thread(
    pty: Arc<super::PtyImpl>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        debug("pty_read-thread started");
        let mut rdr = pty.master.lock().unwrap().try_clone_reader().unwrap();
        let mut buf = vec![0u8; 131072];
        loop {
            match rdr.read(&mut buf) {
                Ok(0) => {
                    debug("pty_read-thread: got EOF");
                    let _ = tx.send(Msg::End);
                    return;
                }
                Ok(n) => {
                    debug(&format!("pty_read-thread: got {} bytes", n));
                    let data = &buf[0..n];
                    if let Some(response) = handle_vt_query(data) {
                        writer.lock().unwrap().write_all(&response).ok();
                        writer.lock().unwrap().flush().ok();
                    }
                    let _ = tx.send(Msg::Data(data.to_vec()));
                }
                Err(e) => {
                    debug(&format!("pty_read-thread: read error: {}", e));
                    let _ = tx.send(Msg::End);
                    return;
                }
            }
        }
    });
}

pub fn spawn_control_thread(
    pty: Arc<super::PtyImpl>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        debug("control-thread started");
        let control_read_handle = pty.control_pipe[0];
        let mut control_reader = unsafe {
            std::fs::File::from_raw_handle(control_read_handle as std::os::windows::raw::HANDLE)
        };
        loop {
            let mut header = [0u8; 1];
            if control_reader.read_exact(&mut header).is_err() {
                debug("control-thread: read header error");
                break;
            }
            match header[0] {
                MSG_WRITE => {
                    let mut len_buf = [0u8; 4];
                    if control_reader.read_exact(&mut len_buf).is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;
                    let mut data = vec![0u8; len];
                    if control_reader.read_exact(&mut data).is_err() {
                        break;
                    }
                    writer.lock().unwrap().write_all(&data).ok();
                    writer.lock().unwrap().flush().ok();
                }
                MSG_RESIZE => {
                    let mut rows_buf = [0u8; 2];
                    if control_reader.read_exact(&mut rows_buf).is_err() {
                        break;
                    }
                    let rows = u16::from_le_bytes(rows_buf);
                    let mut cols_buf = [0u8; 2];
                    if control_reader.read_exact(&mut cols_buf).is_err() {
                        break;
                    }
                    let cols = u16::from_le_bytes(cols_buf);
                    let size = PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    master.lock().unwrap().resize(size).ok();
                }
                MSG_KILL => {
                    killer.lock().unwrap().kill().ok();
                    let _ = tx.send(Msg::End);
                    break;
                }
                _ => debug(&format!(
                    "control-thread: unknown message type: {}",
                    header[0]
                )),
            }
        }
        debug("control-thread: ended");
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
