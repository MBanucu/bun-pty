use std::io::{self, ErrorKind};
use libc;
use std::os::unix::io::RawFd;

/// Helper function to read all available data non-blockingly
pub(crate) fn read_all_nonblocking(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut temp = [0u8; 8192];
    let mut total = 0;
    loop {
        let n = unsafe { libc::read(fd, temp.as_mut_ptr() as *mut libc::c_void, temp.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                break;
            }
            return Err(err);
        } else if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[0..n as usize]);
        total += n as usize;
    }
    Ok(total)
}

/// Helper function to write all data with retries for interruptions
pub(crate) fn write_all_nonblocking(fd: RawFd, data: &[u8]) -> io::Result<()> {
    let mut pos = 0;
    while pos < data.len() {
        let n = unsafe { libc::write(fd, data[pos..].as_ptr() as *const libc::c_void, data.len() - pos) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        } else if n == 0 {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "Pipe closed"));
        }
        pos += n as usize;
    }
    Ok(())
}