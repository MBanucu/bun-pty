use std::io;
use std::fmt;

/// Unified error enum for PTY I/O operations across platforms
#[derive(Debug)]
pub enum PtyIoError {
    WouldBlock,
    Interrupted,
    Eof,
    BrokenPipe,
    OsSpecific(i32),
    Other(io::Error),
}

impl fmt::Display for PtyIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyIoError::WouldBlock => write!(f, "Operation would block"),
            PtyIoError::Interrupted => write!(f, "Operation interrupted"),
            PtyIoError::Eof => write!(f, "End of file"),
            PtyIoError::BrokenPipe => write!(f, "Broken pipe"),
            PtyIoError::OsSpecific(code) => write!(f, "OS error: {}", code),
            PtyIoError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for PtyIoError {}

impl From<io::Error> for PtyIoError {
    fn from(err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::WouldBlock => PtyIoError::WouldBlock,
            io::ErrorKind::Interrupted => PtyIoError::Interrupted,
            io::ErrorKind::BrokenPipe => PtyIoError::BrokenPipe,
            io::ErrorKind::UnexpectedEof => PtyIoError::Eof,
            _ => PtyIoError::OsSpecific(err.raw_os_error().unwrap_or(0)),
        }
    }
}

impl From<PtyIoError> for io::Error {
    fn from(err: PtyIoError) -> Self {
        match err {
            PtyIoError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "Would block"),
            PtyIoError::Interrupted => io::Error::new(io::ErrorKind::Interrupted, "Interrupted"),
            PtyIoError::Eof => io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"),
            PtyIoError::BrokenPipe => io::Error::new(io::ErrorKind::BrokenPipe, "Broken pipe"),
            PtyIoError::OsSpecific(code) => io::Error::from_raw_os_error(code),
            PtyIoError::Other(e) => e,
        }
    }
}

/// Trait for non-blocking readers across platforms
pub trait NonBlockingReader {
    fn read_all_nonblocking(&mut self, buf: &mut Vec<u8>) -> Result<usize, PtyIoError>;
}

/// Trait for non-blocking writers across platforms
pub trait NonBlockingWriter {
    fn write_all_nonblocking(&mut self, data: &[u8]) -> Result<(), PtyIoError>;
}