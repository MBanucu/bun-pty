use super::super::io_helpers::{NonBlockingReader, NonBlockingWriter, PtyIoError};
use libc;
use std::io::{self, ErrorKind};
use std::os::unix::io::RawFd;

/// Wrapper for Unix file descriptors to implement NonBlockingReader
pub struct FdReader(pub RawFd);

impl NonBlockingReader for FdReader {
    fn read_all_nonblocking(&mut self, buf: &mut Vec<u8>) -> Result<usize, PtyIoError> {
        let mut temp = [0u8; 8192];
        let mut total = 0;
        loop {
            let n =
                unsafe { libc::read(self.0, temp.as_mut_ptr() as *mut libc::c_void, temp.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    ErrorKind::WouldBlock => break,
                    ErrorKind::Interrupted => continue,
                    _ => return Err(PtyIoError::from(err)),
                }
            } else if n == 0 {
                break;
            }
            buf.extend_from_slice(&temp[0..n as usize]);
            total += n as usize;
        }
        Ok(total)
    }
}

/// Wrapper for Unix file descriptors to implement NonBlockingWriter
pub struct FdWriter(pub RawFd);

impl NonBlockingWriter for FdWriter {
    fn write_all_nonblocking(&mut self, data: &[u8]) -> Result<(), PtyIoError> {
        let mut pos = 0;
        while pos < data.len() {
            let n = unsafe {
                libc::write(
                    self.0,
                    data[pos..].as_ptr() as *const libc::c_void,
                    data.len() - pos,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    ErrorKind::WouldBlock | ErrorKind::Interrupted => continue,
                    _ => return Err(PtyIoError::from(err)),
                }
            } else if n == 0 {
                return Err(PtyIoError::BrokenPipe);
            }
            pos += n as usize;
        }
        Ok(())
    }
}
