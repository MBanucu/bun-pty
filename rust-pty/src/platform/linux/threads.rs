use super::super::io_helpers::NonBlockingReader;
use super::{super::control::*, helpers::FdReader, pty_impl::PtyImpl};
use crate::pty::Msg;
use crossbeam::channel::Sender;
use libc::{pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL};
use portable_pty::{ChildKiller, MasterPty};
use std::{
    io::{self, ErrorKind, Read},
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
};

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
            let pty_fd = master_clone
                .lock()
                .unwrap()
                .as_raw_fd()
                .expect("Failed to get PTY FD");
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

            debug(&format!(
                "read-thread: got PTY fd {}, control fd {}",
                pty_fd, control_read_fd
            ));

            // Poll structures
            let mut pollfds = [
                pollfd {
                    fd: pty_fd,
                    events: POLLIN | POLLHUP | POLLERR,
                    revents: 0,
                },
                pollfd {
                    fd: control_read_fd,
                    events: POLLIN,
                    revents: 0,
                },
            ];

            loop {
                debug("read-thread: polling...");
                let ret =
                    unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
                debug(&format!("read-thread: poll returned {}", ret));

                if ret < 0 {
                    debug(&format!("poll error: {}", io::Error::last_os_error()));
                    break;
                }

                // Handle control events first
                if pollfds[1].revents & POLLIN != 0 {
                    debug("read-thread: control pipe has data");
                    let mut control_reader = FdReader(control_read_fd);
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
