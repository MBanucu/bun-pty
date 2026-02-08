// rust-pty/src/platform/linux.rs

use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::{unbounded, Sender};
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    io::{self, ErrorKind, Read, Write},
    os::unix::io::RawFd,
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
};
use libc::{self, pollfd, POLLIN, POLLHUP, POLLERR, POLLNVAL};

const MSG_WRITE: u8 = 1;
const MSG_RESIZE: u8 = 2;
const MSG_KILL: u8 = 3;

fn debug(msg: &str) {
    if std::env::var("BUN_PTY_DEBUG").unwrap_or_default() == "1" {
        eprintln!("[rust-pty] {msg}");
    }
}

/// Helper function to read all available data non-blockingly
fn read_all_nonblocking(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<usize> {
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
fn write_all_nonblocking(fd: RawFd, data: &[u8]) -> io::Result<()> {
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
fn process_control_messages(
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

pub struct PtyImpl {
    reader: crate::pty::Reader,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    exited: AtomicBool,
    exit_code: AtomicI32,
    pid: i32,
    control_pipe: [RawFd; 2], // [read_fd, write_fd]
}

impl PtyImpl {
    pub fn new(cmd: &crate::pty::Command, size: PtySize) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let sys = native_pty_system();
        let pair = sys.openpty(size)?;
        let mut child = pair.slave.spawn_command(cmd.to_builder())?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let killer_clone = killer.clone();
        let pid = child.process_id().map(|p| p as i32).unwrap_or(-1);

        // Channels for reader
        let (tx_r, rx_r) = unbounded::<Msg>();

        let master = Arc::new(Mutex::new(pair.master));

        // Create control pipe
        let mut control_pipe = [-1i32, -1i32];
        if unsafe { libc::pipe(control_pipe.as_mut_ptr()) } != 0 {
            return Err("Failed to create control pipe".into());
        }

        // Set non-blocking on both ends
        unsafe {
            let flags = libc::fcntl(control_pipe[0], libc::F_GETFL);
            libc::fcntl(control_pipe[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
            let flags = libc::fcntl(control_pipe[1], libc::F_GETFL);
            libc::fcntl(control_pipe[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
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

    fn spawn_wait_thread(pty: Arc<Self>, mut child: Box<dyn portable_pty::Child + Send + Sync>) {
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

    fn spawn_read_thread(
        pty: Arc<Self>,
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
        killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
        tx: Sender<Msg>,
    ) {
        let mut rdr = master.lock().unwrap().try_clone_reader().unwrap();
        let control_read_fd = pty.control_pipe[0];
        let master_clone = master.clone();

        thread::spawn(move || {
            debug("read-thread started");
            let mut buf = vec![0; 65536];
            let mut control_buf: Vec<u8> = Vec::with_capacity(8192);

            // Get PTY FD and set non-blocking
            let pty_fd = master_clone.lock().unwrap().as_raw_fd().expect("Failed to get PTY FD");
            unsafe {
                let flags = libc::fcntl(pty_fd, libc::F_GETFL);
                libc::fcntl(pty_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
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

            debug(&format!("read-thread: got PTY fd {}, control fd {}", pty_fd, control_read_fd));

            // Poll structures
            let mut pollfds = [
                pollfd { fd: pty_fd, events: POLLIN | POLLHUP | POLLERR, revents: 0 },
                pollfd { fd: control_read_fd, events: POLLIN, revents: 0 },
            ];

            loop {
                debug("read-thread: polling...");
                let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
                debug(&format!("read-thread: poll returned {}", ret));

                if ret < 0 {
                    debug(&format!("poll error: {}", io::Error::last_os_error()));
                    break;
                }

                // Handle control events first
                if pollfds[1].revents & POLLIN != 0 {
                    debug("read-thread: control pipe has data");
                    if let Err(e) = read_all_nonblocking(control_read_fd, &mut control_buf) {
                        debug(&format!("Control read error: {}", e));
                    }
                    if process_control_messages(&mut control_buf, &mut writer, &master_clone, &killer, &tx) {
                        break; // Kill processed
                    }
                }

                // Handle PTY data or hangup
                if pollfds[0].revents & (POLLIN | POLLHUP) != 0 {
                    debug("read-thread: PTY has data or event");
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

                // Handle errors
                if pollfds[0].revents & (POLLERR | POLLNVAL | POLLHUP) != 0 {
                    debug("read-thread: PTY error or hangup");
                    let _ = tx.send(Msg::End);
                    break;
                }
                if pollfds[1].revents & (POLLERR | POLLNVAL) != 0 {
                    debug("read-thread: control pipe error");
                    break;
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
        write_all_nonblocking(self.control_pipe[1], &buf).map_err(Into::into)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = vec![MSG_RESIZE];
        buf.extend_from_slice(&size.rows.to_le_bytes());
        buf.extend_from_slice(&size.cols.to_le_bytes());
        write_all_nonblocking(self.control_pipe[1], &buf).map_err(Into::into)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        write_all_nonblocking(self.control_pipe[1], &[MSG_KILL]).map_err(Into::into)
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
            if self.control_pipe[0] >= 0 {
                libc::close(self.control_pipe[0]);
            }
            if self.control_pipe[1] >= 0 {
                libc::close(self.control_pipe[1]);
            }
        }
    }
}