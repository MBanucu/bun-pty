use super::super::{
    control::*,
    io_helpers::{NonBlockingReader, NonBlockingWriter, PtyIoError},
};
use super::helpers::{HandleReader, HandleWriter};
use crate::pty::{Msg, PtyTrait, Reader};
use crossbeam::channel::unbounded;
use portable_pty::win::ConPtyMaster;
use portable_pty::{native_pty_system, ChildKiller, MasterPty, PtySize};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc, Mutex,
    },
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    Security::SECURITY_ATTRIBUTES,
    System::Pipes::CreatePipe,
    System::Pipes::{SetNamedPipeHandleState, PIPE_NOWAIT},
};

pub struct PtyImpl {
    pub(crate) reader: crate::pty::Reader,
    pub(crate) master: Arc<Mutex<ConPtyMaster>>,
    pub(crate) killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    pub(crate) exited: AtomicBool,
    pub(crate) exit_code: AtomicI32,
    pub(crate) pid: i32,
    pub(crate) control_pipe_read: OwnedHandle,
    pub(crate) control_pipe_write: OwnedHandle,
    pub(crate) pty_handle: HANDLE,
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

        let (tx_r, rx_r) = unbounded::<Msg>();

        // Cast to concrete ConPtyMaster (safe because we know the underlying type on Windows)
        let master_con: ConPtyMaster =
            unsafe { *Box::from_raw(Box::into_raw(pair.master) as *mut ConPtyMaster) };
        let master = Arc::new(Mutex::new(master_con));

        // Get PTY handle once for efficiency
        let pty_handle = master.lock().unwrap().as_raw_handle() as HANDLE;

        // Create control pipe
        let mut read_handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut write_handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: false.into(),
        };

        unsafe {
            if CreatePipe(&mut read_handle, &mut write_handle, Some(&sa), 0) == 0 {
                return Err("Failed to create control pipe".into());
            }
            // Set non-blocking
            let mode = PIPE_NOWAIT;
            SetNamedPipeHandleState(
                read_handle,
                &mode,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            SetNamedPipeHandleState(
                write_handle,
                &mode,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }

        let control_pipe_read = unsafe { OwnedHandle::from_raw_handle(read_handle as _) };
        let control_pipe_write = unsafe { OwnedHandle::from_raw_handle(write_handle as _) };

        let pty = Arc::new(Self {
            reader: Reader::new(rx_r),
            master: master.clone(),
            killer,
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(-1),
            pid,
            control_pipe_read,
            control_pipe_write,
            pty_handle,
        });

        // Spawn threads
        Self::spawn_wait_thread(pty.clone(), child);
        Self::spawn_read_thread(pty.clone(), master.clone(), killer_clone, tx_r.clone());

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
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut buf = vec![MSG_RESIZE];
        buf.extend_from_slice(&size.rows.to_le_bytes());
        buf.extend_from_slice(&size.cols.to_le_bytes());
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
        writer.write_all_nonblocking(&buf).map_err(Into::into)
    }

    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut writer = HandleWriter(self.control_pipe_write.as_raw_handle() as HANDLE);
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
        // Cleanup handles if needed
    }
}
