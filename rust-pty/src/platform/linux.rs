use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::{unbounded};
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
    os::unix::io::RawFd,
    io::Read,
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
                let mut buf = vec![0; 8192];

                // Get PTY file descriptor from the master
                let pty_fd = master_clone.lock().unwrap().as_raw_fd().expect("Failed to get PTY FD");

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

                loop {
                    debug("read-thread: polling...");
                    let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
                    debug(&format!("read-thread: poll returned {}", ret));

                    if ret < 0 {
                        debug(&format!("read-thread: poll error: {}", std::io::Error::last_os_error()));
                        break;
                    }

                    // Check for PTY data
                    if pollfds[0].revents & libc::POLLIN != 0 {
                        debug("read-thread: PTY has data");
                        match rdr.read(&mut buf) {
                            Ok(0) => {
                                debug("read-thread: got Ok(0) - EOF");
                                break;
                            }
                            Ok(n) => {
                                debug(&format!("read-thread: got Ok({}) bytes", n));
                                let _ = tx.send(Msg::Data(buf[..n].to_vec()));
                            }
                            Err(e) => {
                                debug(&format!("read-thread: got Err: {}", e));
                                break;
                            }
                        }
                    }

                    // Check for control pipe data (control events)
                    if pollfds[1].revents & libc::POLLIN != 0 {
                        debug("read-thread: control pipe has data");
                        
                        // Read with retry for full buffer
                        let read_full = |fd: RawFd, buf: &mut [u8]| -> Result<usize, std::io::Error> {
                            let mut pos = 0;
                            while pos < buf.len() {
                                let n = unsafe { libc::read(fd, buf[pos..].as_mut_ptr() as *mut libc::c_void, buf.len() - pos) };
                                if n < 0 {
                                    let err = std::io::Error::last_os_error();
                                    if err.kind() != std::io::ErrorKind::WouldBlock && err.kind() != std::io::ErrorKind::Interrupted {
                                        return Err(err);
                                    }
                                    continue;
                                } else if n == 0 {
                                    return Ok(pos); // EOF
                                }
                                pos += n as usize;
                            }
                            Ok(pos)
                        };

                        let mut msg_type_buf = [0u8; 1];
                        if read_full(control_read_fd, &mut msg_type_buf).unwrap_or(0) != 1 { continue; }
                        let msg_type = msg_type_buf[0];
                        debug(&format!("read-thread: received control message type: {}", msg_type));
                        
                        match msg_type {
                            1 => { // Write
                                // Read data length
                                let mut len_buf = [0u8; 4];
                                if read_full(control_read_fd, &mut len_buf).unwrap_or(0) != 4 { continue; }
                                let data_len = u32::from_le_bytes(len_buf) as usize;
                                
                                // Read data
                                let mut data_buf = vec![0u8; data_len];
                                if read_full(control_read_fd, &mut data_buf).unwrap_or(0) != data_len { continue; }
                                
                                debug(&format!("read-thread: writing {} bytes", data_len));
                                // Perform the write operation
                                if let Err(e) = master_clone.lock().unwrap().take_writer().and_then(|mut wtr| {
                                    wtr.write_all(&data_buf)?;
                                    wtr.flush()?;
                                    Ok(())
                                }) {
                                    debug(&format!("read-thread: write error: {}", e));
                                }
                            }
                            2 => { // Resize
                                // Read rows and cols (5 bytes total: 1 msg_type + 2 rows + 2 cols)
                                let mut size_buf = [0u8; 4];
                                if read_full(control_read_fd, &mut size_buf).unwrap_or(0) != 4 { continue; }
                                let rows = u16::from_le_bytes([size_buf[0], size_buf[1]]);
                                let cols = u16::from_le_bytes([size_buf[2], size_buf[3]]);
                                
                                debug(&format!("read-thread: resizing to {}x{}", rows, cols));
                                // Perform the resize operation
                                if let Err(e) = master_clone.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                                    debug(&format!("read-thread: resize error: {}", e));
                                }
                            }
                            3 => { // Kill
                                debug("read-thread: killing process");
                                // Perform the kill operation
                                if let Ok(mut k) = killer_clone.lock() {
                                    let _ = k.kill();
                                }
                            }
                            _ => {
                                debug(&format!("read-thread: unknown message type: {}", msg_type));
                            }
                        }
                    }

                    // Check for other events (errors, etc.)
                    if pollfds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        debug("read-thread: PTY error/hangup");
                        break;
                    }
                    if pollfds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        debug("read-thread: control pipe error/hangup");
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