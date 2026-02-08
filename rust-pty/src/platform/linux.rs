use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::{unbounded, Sender};
use portable_pty::{native_pty_system, PtySize, ChildKiller, MasterPty};
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicI32, Ordering}},
    thread,
    os::unix::io::RawFd,
    any::Any,
    fs::File,
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
    tx_w: Sender<(Vec<u8>, usize)>,
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
        let pid = child.process_id().map(|p| p as i32).unwrap_or(-1);

        /* channels */
        let (tx_r, rx_r) = unbounded::<Msg>();
        let (tx_w, rx_w) = unbounded::<(Vec<u8>, usize)>();

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
            tx_w,
            master: master.clone(),
            killer,
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(-1),
            pid,
            control_pipe,
        });

        /* wait-thread */
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
            thread::spawn(move || {
                debug("read-thread started");
                let mut buf = vec![0; 8192];

                // Get PTY file descriptor by downcasting the reader
                let pty_fd = if let Some(file) = (&*rdr as &dyn Any).downcast_ref::<File>() {
                    file.as_raw_fd()
                } else {
                    debug("read-thread: failed to downcast reader to File, falling back to blocking read");
                    // Fallback to old behavior if downcast fails
                    loop {
                        debug("read-thread: attempting read...");
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
                    let _ = tx.send(Msg::End);
                    debug("read-thread: ended");
                    return;
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
                        let mut control_buf = [0u8; 1];
                        match unsafe { libc::read(control_read_fd, control_buf.as_mut_ptr() as *mut libc::c_void, 1) } {
                            -1 => {
                                debug(&format!("read-thread: control pipe read error: {}", std::io::Error::last_os_error()));
                            }
                            0 => {
                                debug("read-thread: control pipe EOF");
                            }
                            1 => {
                                debug(&format!("read-thread: received control signal: {}", control_buf[0]));
                                // Control signals are handled by the main event loop in pty-worker.ts
                                // We just wake up to check for pending operations
                            }
                            _ => {
                                debug("read-thread: unexpected control pipe read result");
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

        /* write-thread */
        {
            let mut wtr = master.lock().unwrap().take_writer()?;
            thread::spawn(move || {
                while let Ok((data, len)) = rx_w.recv() {
                    if wtr.write_all(&data[..len]).is_err() { break; }
                    let _ = wtr.flush();
                }
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
        self.tx_w.send((data.to_vec(), data.len())).map_err(|e| e.into())
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.master.lock().unwrap().resize(size).map_err(|e| e.into())
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut k = self.killer.lock().unwrap();
        k.kill().map_err(|e| e.into())
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