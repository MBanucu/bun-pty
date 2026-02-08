use super::super::io_helpers::{NonBlockingReader, NonBlockingWriter, PtyIoError};
use std::io::Error;
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{ReadFile, WriteFile},
};

// Windows-specific error codes
const ERROR_NO_DATA: i32 = 232;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_IO_PENDING: i32 = 997;

/// Wrapper for Windows handles to implement NonBlockingReader
pub struct HandleReader(pub HANDLE);

impl NonBlockingReader for HandleReader {
    fn read_all_nonblocking(&mut self, buf: &mut Vec<u8>) -> Result<usize, PtyIoError> {
        let mut temp = [0u8; 131072]; // Larger buffer for Windows
        let mut total = 0;
        loop {
            let mut bytes_read = 0u32;
            let res = unsafe {
                ReadFile(
                    self.0,
                    temp.as_mut_ptr() as *mut std::ffi::c_void,
                    temp.len() as u32,
                    &mut bytes_read,
                    std::ptr::null_mut(),
                )
            };
            if res == 0 {
                let err = Error::last_os_error();
                let raw_err = err.raw_os_error().unwrap_or(0);
                if raw_err == ERROR_NO_DATA {
                    break;
                } else if raw_err == ERROR_IO_PENDING {
                    continue;
                }
                return Err(PtyIoError::from(err));
            }
            if bytes_read == 0 {
                break;
            }
            buf.extend_from_slice(&temp[0..bytes_read as usize]);
            total += bytes_read as usize;
        }
        Ok(total)
    }
}

/// Wrapper for Windows handles to implement NonBlockingWriter
pub struct HandleWriter(pub HANDLE);

impl NonBlockingWriter for HandleWriter {
    fn write_all_nonblocking(&mut self, data: &[u8]) -> Result<(), PtyIoError> {
        let mut pos = 0;
        while pos < data.len() {
            let mut bytes_written = 0u32;
            let res = unsafe {
                WriteFile(
                    self.0,
                    data[pos..].as_ptr() as *const std::ffi::c_void,
                    (data.len() - pos) as u32,
                    &mut bytes_written,
                    std::ptr::null_mut(),
                )
            };
            if res == 0 {
                let err = Error::last_os_error();
                let raw_err = err.raw_os_error().unwrap_or(0);
                if raw_err == ERROR_IO_PENDING {
                    continue;
                }
                return Err(PtyIoError::from(err));
            }
            if bytes_written == 0 {
                return Err(PtyIoError::BrokenPipe);
            }
            pos += bytes_written as usize;
        }
        Ok(())
    }
}
