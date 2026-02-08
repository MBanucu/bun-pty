use std::io::{self, ErrorKind, Write};
use libc;
use portable_pty::{PtySize, MasterPty, ChildKiller};
use std::sync::{Arc, Mutex};
use crossbeam::channel::Sender;
use crate::pty::Msg;
use std::os::unix::io::RawFd;

pub(crate) const MSG_WRITE: u8 = 1;
pub(crate) const MSG_RESIZE: u8 = 2;
pub(crate) const MSG_KILL: u8 = 3;

pub(crate) fn debug(msg: &str) {
    if std::env::var("BUN_PTY_DEBUG").unwrap_or_default() == "1" {
        eprintln!("[rust-pty] {msg}");
    }
}

/// Helper function to read all available data non-blockingly
pub(crate) fn read_all_nonblocking(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut temp = [0u8; 8192];
    let mut total = 0;
    loop {
        let n = unsafe { libc::read(fd, temp.as_mut_ptr() as *mut libc::c_void, temp.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                break;
            }
            return Err(err);
        } else if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[0..n as usize]);
        total += n as usize;
    }
    Ok(total)
}

/// Helper function to write all data with retries for interruptions
pub(crate) fn write_all_nonblocking(fd: RawFd, data: &[u8]) -> io::Result<()> {
    let mut pos = 0;
    while pos < data.len() {
        let n = unsafe { libc::write(fd, data[pos..].as_ptr() as *const libc::c_void, data.len() - pos) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        } else if n == 0 {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "Pipe closed"));
        }
        pos += n as usize;
    }
    Ok(())
}

/// Processes complete control messages from the buffer
/// Returns true if a kill message was processed (to break the loop)
pub(crate) fn process_control_messages(
    control_buf: &mut Vec<u8>,
    writer: &mut dyn Write,
    master: &Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: &Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    tx: &Sender<Msg>,
) -> bool {
    let mut pos = 0;
    while pos < control_buf.len() {
        if control_buf.len() - pos < 1 {
            break; // Partial type
        }
        let msg_type = control_buf[pos];
        pos += 1;

        debug(&format!("Processing control message type: {}", msg_type));

        match msg_type {
            MSG_WRITE => {
                if control_buf.len() - pos < 4 {
                    pos -= 1; // Rewind type
                    break;
                }
                let data_len = u32::from_le_bytes([control_buf[pos], control_buf[pos+1], control_buf[pos+2], control_buf[pos+3]]) as usize;
                pos += 4;

                if control_buf.len() - pos < data_len {
                    pos -= 5; // Rewind type + len
                    break;
                }
                let data = &control_buf[pos..pos + data_len];
                if let Err(e) = writer.write_all(data) {
                    debug(&format!("Write error: {}", e));
                } else if let Err(e) = writer.flush() {
                    debug(&format!("Flush error: {}", e));
                }
                pos += data_len;
            }
            MSG_RESIZE => {
                if control_buf.len() - pos < 4 {
                    pos -= 1; // Rewind type
                    break;
                }
                let rows = u16::from_le_bytes([control_buf[pos], control_buf[pos+1]]);
                let cols = u16::from_le_bytes([control_buf[pos+2], control_buf[pos+3]]);
                pos += 4;
                if let Err(e) = master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                    debug(&format!("Resize error: {}", e));
                }
            }
            MSG_KILL => {
                // No payload
                if let Ok(mut k) = killer.lock() {
                    let _ = k.kill();
                }
                let _ = tx.send(Msg::End);
                // Drain remaining buffer if needed
                control_buf.drain(..);
                return true; // Signal to break the loop
            }
            _ => {
                debug(&format!("Unknown message type: {}", msg_type));
                // Skip invalid message
            }
        }
    }

    // Remove processed bytes
    if pos > 0 {
        control_buf.drain(0..pos);
    }

    false
}