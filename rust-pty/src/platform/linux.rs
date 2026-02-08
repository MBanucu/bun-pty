use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::{unbounded};
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
    os::unix::io::RawFd,
    io::{Read, Write, ErrorKind},
};
use libc;

fn debug(msg: &str) {
    if std::env::var("BUN_PTY_DEBUG").unwrap_or_default() == "1" {
        eprintln!("[rust-pty] {msg}");
    }
}

pub struct PtyImpl {
    reader: crate::pty::Reader,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    exited: AtomicBool,
    exit_code: AtomicI32,
    pid: i32,
    // Control pipe for waking read-thread on control events
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

        /* channels */
        let (tx_r, rx_r) = unbounded::<Msg>();

        let master = Arc::new(Mutex::new(pair.master));

        // Create control pipe for waking read-thread on control events
        let mut control_pipe = [-1, -1];
        if unsafe { libc::pipe(control_pipe.as_mut_ptr()) } != 0 {
            return Err("Failed to create control pipe".into());
        }

        // Make both ends non-blocking
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
        {
            let pty_clone = pty.clone();
            thread::spawn(move || {
                debug("wait-thread: waiting for child...");
                let status = child.wait();
                debug("wait-thread: child.wait() returned");
                if let Ok(exit_status) = status {
                    let code = exit_status.exit_code() as i32;
                    debug(&format!("exit_status.exit_code(): {}", code));
                    pty_clone.exit_code.store(code, Ordering::Relaxed);
                }
                pty_clone.exited.store(true, Ordering::Relaxed);
                debug("wait-thread: exited and code stored");
            });
        }

        /* read-thread */
        {
            let mut rdr = master.lock().unwrap().try_clone_reader()?;
            let tx = tx_r.clone();
            let control_read_fd = pty.control_pipe[0];
            let master_clone = master.clone();
            let killer_clone = killer_clone.clone();
            thread::spawn(move || {
                debug("read-thread started");
                let mut buf = vec![0; 65536];

                // Get PTY file descriptor from the master
                let pty_fd = master_clone.lock().unwrap().as_raw_fd().expect("Failed to get PTY FD");

                // Make PTY FD non-blocking
                unsafe {
                    let flags = libc::fcntl(pty_fd, libc::F_GETFL);
                    libc::fcntl(pty_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }

                // Take writer ONCE before loop
                let mut writer = match master_clone.lock().unwrap().take_writer() {
                    Ok(w) => w,
                    Err(e) => {
                        debug(&format!("read-thread: failed to take writer: {}", e));
                        let _ = tx.send(Msg::End); // Early exit on failure
                        return;
                    }
                };

                debug(&format!("read-thread: got PTY fd {}, control fd {}", pty_fd, control_read_fd));

                // Set up pollfd structures
                let mut pollfds = [
                    libc::pollfd {
                        fd: pty_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: control_read_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];

                let mut control_buf: Vec<u8> = Vec::with_capacity(8192); // Pre-alloc for typical writes

                loop {
                    debug("read-thread: polling...");
                    let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
                    debug(&format!("read-thread: poll returned {}", ret));

                    if ret < 0 {
                        debug(&format!("read-thread: poll error: {}", std::io::Error::last_os_error()));
                        break;
                    }

                    // Handle control first to avoid input delays
                    if pollfds[1].revents & libc::POLLIN != 0 {
                        debug("read-thread: control pipe has data");

                        // Read ALL available data non-blocking (no spin)
                        let mut temp_buf = [0u8; 8192];
                        loop {
                            let n = unsafe { libc::read(control_read_fd, temp_buf.as_mut_ptr() as *mut libc::c_void, temp_buf.len()) };
                            if n < 0 {
                                let err = std::io::Error::last_os_error();
                                if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::Interrupted {
                                    break; // No more data, stop reading
                                }
                                debug(&format!("read-thread: control read error: {}", err));
                                break;
                            } else if n == 0 {
                                break; // EOF
                            }
                            control_buf.extend_from_slice(&temp_buf[0..n as usize]);
                        }

                        // Now parse COMPLETE messages from control_buf
                        let mut pos = 0;
                        while pos < control_buf.len() {
                            if control_buf.len() - pos < 1 {
                                break; // Partial type, wait for next poll
                            }
                            let msg_type = control_buf[pos];
                            pos += 1;

                            match msg_type {
                                1 => { // Write
                                    if control_buf.len() - pos < 4 {
                                        pos -= 1; // Rewind type, partial
                                        break;
                                    }
                                    let data_len = u32::from_le_bytes([control_buf[pos], control_buf[pos+1], control_buf[pos+2], control_buf[pos+3]]) as usize;
                                    pos += 4;

                                    if control_buf.len() - pos < data_len {
                                        pos -= 5; // Rewind type+len, partial
                                        break;
                                    }
                                    let data = &control_buf[pos..pos + data_len];
                                    // Write to writer (as before)
                                    if let Err(e) = writer.write_all(data) {
                                        debug(&format!("read-thread: write error: {}", e));
                                    } else if let Err(e) = writer.flush() {
                                        debug(&format!("read-thread: flush error: {}", e));
                                    }
                                    pos += data_len;
                                }
                                2 => { // Resize
                                    if control_buf.len() - pos < 4 {
                                        pos -= 1; // Rewind type, partial
                                        break;
                                    }
                                    let rows = u16::from_le_bytes([control_buf[pos], control_buf[pos+1]]);
                                    let cols = u16::from_le_bytes([control_buf[pos+2], control_buf[pos+3]]);
                                    pos += 4;
                                    // Resize (as before)
                                    if let Err(e) = master_clone.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                                        debug(&format!("read-thread: resize error: {}", e));
                                    }
                                }
                                3 => { // Kill
                                    // No payload
                                    // Kill (as before)
                                    if let Ok(mut k) = killer_clone.lock() {
                                        let _ = k.kill();
                                    }
                                    let _ = tx.send(Msg::End);
                                    // Drain remaining control_buf if needed, but break
                                    break;
                                }
                                _ => {
                                    debug(&format!("read-thread: unknown message type: {}", msg_type));
                                    // Skip or error?
                                }
                            }
                        }

                        // Remove processed bytes from control_buf
                        if pos > 0 {
                            control_buf.drain(0..pos);
                        }
                    }

                    // Handle PTY data second
                    if pollfds[0].revents & libc::POLLIN != 0 {
                        debug("read-thread: PTY has data");
                        loop {
                            match rdr.read(&mut buf) {
                                Ok(0) => {
                                    debug("read-thread: got Ok(0) - EOF");
                                    break;
                                }
                                Ok(n) => {
                                    debug(&format!("read-thread: got Ok({}) bytes", n));
                                    let _ = tx.send(Msg::Data(buf[..n].to_vec()));
                                }
                                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                                Err(e) => {
                                    debug(&format!("read-thread: read error: {}", e));
                                    break;
                                }
                            }
                        }
                    }

                    // Check for other events (errors, etc.)
                    if pollfds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        debug("read-thread: PTY error");
                        break;
                    }
                    if pollfds[1].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        debug("read-thread: control pipe error");
                        break;
                    }
                }

                debug("read-thread: loop exited, sending Msg::End");
                let _ = tx.send(Msg::End);
                debug("read-thread: ended");
            });
        }

        Ok(pty)
    }
}

impl PtyTrait for PtyImpl {
    fn read(&self, blocking: bool) -> Result<Msg, Box<dyn std::error::Error + Send + Sync>> {
        self.reader.read(blocking)
    }

    fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug(&format!("PtyImpl::write: writing {} bytes", data.len()));
        // Send write command through control pipe
        let msg_type = 1u8; // 1 = write
        let len_bytes = (data.len() as u32).to_le_bytes();
        
        // Write with retry for partial writes
        let write_full = |fd: RawFd, buf: &[u8]| -> Result<(), std::io::Error> {
            let mut pos = 0;
            while pos < buf.len() {
                let n = unsafe { libc::write(fd, buf[pos..].as_ptr() as *const libc::c_void, buf.len() - pos) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::WouldBlock && err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                    // Retry on EAGAIN/EINTR
                    continue;
                } else if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Pipe closed"));
                }
                pos += n as usize;
            }
            Ok(())
        };
        
        write_full(self.control_pipe[1], &[msg_type])?;
        write_full(self.control_pipe[1], &len_bytes)?;
        write_full(self.control_pipe[1], data)?;
        debug("PtyImpl::write: write to control pipe succeeded");
        
        Ok(())
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Send resize command through control pipe
        let msg_type = 2u8; // 2 = resize
        let rows_bytes = size.rows.to_le_bytes();
        let cols_bytes = size.cols.to_le_bytes();
        let mut size_buf = [0u8; 5];
        size_buf[0] = msg_type;
        size_buf[1..3].copy_from_slice(&rows_bytes);
        size_buf[3..5].copy_from_slice(&cols_bytes);
        
        // Write with retry for partial writes
        let write_full = |fd: RawFd, buf: &[u8]| -> Result<(), std::io::Error> {
            let mut pos = 0;
            while pos < buf.len() {
                let n = unsafe { libc::write(fd, buf[pos..].as_ptr() as *const libc::c_void, buf.len() - pos) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::WouldBlock && err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                    // Retry on EAGAIN/EINTR
                    continue;
                } else if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Pipe closed"));
                }
                pos += n as usize;
            }
            Ok(())
        };
        
        // Use same write_full helper
        write_full(self.control_pipe[1], &size_buf)?;
        Ok(())
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Send kill command through control pipe
        let msg_type = 3u8; // 3 = kill
        
        // Write with retry for partial writes
        let write_full = |fd: RawFd, buf: &[u8]| -> Result<(), std::io::Error> {
            let mut pos = 0;
            while pos < buf.len() {
                let n = unsafe { libc::write(fd, buf[pos..].as_ptr() as *const libc::c_void, buf.len() - pos) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::WouldBlock && err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                    // Retry on EAGAIN/EINTR
                    continue;
                } else if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Pipe closed"));
                }
                pos += n as usize;
            }
            Ok(())
        };
        
        // Write message type
        write_full(self.control_pipe[1], &[msg_type])?;
        
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