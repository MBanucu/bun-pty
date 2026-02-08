use super::super::control::*;
use super::super::io_helpers::NonBlockingWriter;
use super::helpers::HandleWriter;
use crate::debug;
use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::unbounded;
use portable_pty::{native_pty_system, ChildKiller, MasterPty, PtySize};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, Mutex,
};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::CreatePipe;

pub struct PtyImpl {
    pub(crate) reader: crate::pty::Reader,
    #[allow(dead_code)]
    pub(crate) master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    #[allow(dead_code)]
    pub(crate) killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    pub(crate) exited: AtomicBool,
    pub(crate) exit_code: AtomicI32,
    pub(crate) pid: i32,
    pub(crate) control_pipe: [HANDLE; 2], // [read_handle, write_handle]
}

impl PtyImpl {
    pub fn new(
        cmd: &crate::pty::Command,
        size: PtySize,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let sys = native_pty_system();
        let pair = sys.openpty(size)?;
        let child = pair.slave.spawn_command(cmd.to_builder())?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let killer_clone = killer.clone();
        let pid = child.process_id().map(|p| p as i32).unwrap_or(-1);

        // Channels for reader
        let (tx_r, rx_r) = unbounded::<Msg>();

        let master = Arc::new(Mutex::new(pair.master));

        // Create control pipe (synchronous anonymous pipe)
        let mut control_pipe: [HANDLE; 2] = [0; 2];
        if unsafe {
            CreatePipe(
                &mut control_pipe[0],
                &mut control_pipe[1],
                std::ptr::null(),
                0,
            )
        } == 0
        {
            return Err("Failed to create control pipe".into());
        }

        // No need for PIPE_NOWAIT; overlapped handles asynchronicity

        let pty = Arc::new(Self {
            reader: Reader::new(rx_r),
            master: master.clone(),
            killer,
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(-1),
            pid,
            control_pipe,
        });

        // Spawn threads
        super::threads::spawn_wait_thread(pty.clone(), child);
        super::threads::spawn_read_thread(pty.clone(), master.clone(), killer_clone, tx_r.clone());

        Ok(pty)
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
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = vec![MSG_RESIZE];
        buf.extend_from_slice(&size.rows.to_le_bytes());
        buf.extend_from_slice(&size.cols.to_le_bytes());
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut writer = HandleWriter(self.control_pipe[1]);
        writer
            .write_all_nonblocking(&[MSG_KILL])
            .map_err(Into::into)
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
            if self.control_pipe[0] != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.control_pipe[0]);
            }
            if self.control_pipe[1] != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.control_pipe[1]);
            }
        }
    }
}
