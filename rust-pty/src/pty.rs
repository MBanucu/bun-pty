use crossbeam::channel::Receiver;
use portable_pty::PtySize;
use serde::{Deserialize, Serialize};
use shell_words::split;
use std::{
    collections::HashMap,
    ffi::CStr,
    os::raw::c_char,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    cmd: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: String,
}

impl Command {
    pub fn from_cmdline(cmdline: &str, cwd: &str, env_ptr: *const c_char) -> Self {
        let tokens = split(cmdline).unwrap_or_default();
        if tokens.is_empty() {
            return Self {
                cmd: String::new(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: cwd.to_owned(),
            };
        }

        let cmd = tokens[0].clone();
        let args = tokens[1..].to_vec();

        let env = parse_env_string(env_ptr);

        Self {
            cmd,
            args,
            env,
            cwd: cwd.to_owned(),
        }
    }

    pub fn to_builder(&self) -> portable_pty::CommandBuilder {
        let mut b = portable_pty::CommandBuilder::new(&self.cmd);
        b.cwd(&self.cwd);
        for a in &self.args {
            b.arg(a);
        }
        for (k, v) in &self.env {
            b.env(k, v);
        }
        b
    }
}

fn parse_env_string(env_ptr: *const c_char) -> HashMap<String, String> {
    if env_ptr.is_null() {
        return HashMap::new();
    }

    let mut env_map = HashMap::new();
    let mut current_ptr = env_ptr;

    unsafe {
        while *current_ptr != 0 {
            let cstr = CStr::from_ptr(current_ptr);

            if let Ok(env_str) = cstr.to_str() {
                if let Some((key, value)) = env_str.split_once('=') {
                    if !key.is_empty() {
                        env_map.insert(key.to_string(), value.to_string());
                    }
                }
            }

            current_ptr = current_ptr.add(cstr.to_bytes_with_nul().len());
        }
    }

    env_map
}

#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Data(Vec<u8>),
    End,
    #[allow(dead_code)]
    Write(Vec<u8>),
    #[allow(dead_code)]
    Resize(PtySize),
    #[allow(dead_code)]
    Kill,
}

pub struct Reader {
    rx: Receiver<Msg>,
    done: std::sync::atomic::AtomicBool,
}

impl Reader {
    pub fn new(rx: Receiver<Msg>) -> Self {
        Self {
            rx,
            done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn read(&self, blocking: bool) -> Result<Msg, Box<dyn std::error::Error + Send + Sync>> {
        use std::sync::atomic::Ordering;
        if self.done.load(Ordering::Relaxed) {
            return Ok(Msg::End);
        }
        if blocking {
            // Blocking: wait for next message with timeout for responsiveness
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Msg::End) => {
                    self.done.store(true, Ordering::Relaxed);
                    Ok(Msg::End)
                }
                Ok(msg) => Ok(msg),
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => Ok(Msg::Data(Vec::new())),
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => Ok(Msg::End), // channel closed
            }
        } else {
            // Non-blocking: collect all available
            let msgs: Vec<_> = self.rx.try_iter().collect();
            let has_end = msgs.iter().any(|m| matches!(m, Msg::End));
            if has_end {
                self.done.store(true, Ordering::Relaxed);
            }
            let data_msgs: Vec<_> = msgs
                .into_iter()
                .filter(|m| matches!(m, Msg::Data(_)))
                .collect();
            if data_msgs.is_empty() {
                if has_end {
                    Ok(Msg::End)
                } else {
                    Ok(Msg::Data(Vec::new()))
                }
            } else {
                let mut out = Vec::new();
                for m in data_msgs {
                    if let Msg::Data(d) = m {
                        out.extend(d);
                    }
                }
                Ok(Msg::Data(out))
            }
        }
    }
}

// Common trait for PTY operations
pub trait PtyTrait: Send + Sync {
    fn read(&self, blocking: bool) -> Result<Msg, Box<dyn std::error::Error + Send + Sync>>;
    fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn get_pid(&self) -> i32;
    fn get_exit_code(&self) -> i32;
    fn is_exited(&self) -> bool;
}

// Pty struct (now holds an Arc<dyn PtyTrait> for the platform-specific impl)
pub struct Pty {
    inner: Arc<dyn PtyTrait>,
    pub pending: Mutex<Vec<u8>>, // Keep for handling partial reads
}

impl Pty {
    pub fn new(
        cmd: Command,
        size: PtySize,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let inner = crate::platform::create_pty_impl(&cmd, size)?;
        Ok(Arc::new(Self {
            inner,
            pending: Mutex::new(Vec::new()),
        }))
    }

    pub fn read(&self, blocking: bool) -> Result<Msg, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.read(blocking)
    }

    pub fn write(&self, data: *const u8, len: usize) -> std::os::raw::c_int {
        use std::os::raw::c_int;
        const SUCCESS: c_int = 0;
        const ERROR: c_int = -1;
        const CHILD_EXITED: c_int = -2;
        if self.is_exited() {
            return CHILD_EXITED;
        }
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        self.inner.write(slice).map(|_| SUCCESS).unwrap_or(ERROR)
    }

    pub fn resize(&self, size: PtySize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.resize(size)
    }

    pub fn kill(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.inner.kill()
    }

    pub fn get_pid(&self) -> i32 {
        self.inner.get_pid()
    }

    pub fn get_exit_code(&self) -> i32 {
        self.inner.get_exit_code()
    }

    pub fn is_exited(&self) -> bool {
        self.inner.is_exited()
    }
}
