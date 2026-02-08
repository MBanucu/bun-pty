mod pty;
mod platform;

/// lib.rs  —  bun-pty backend (refactored)

use std::{
    collections::HashMap,
    ffi::CStr,
    os::raw::{c_char, c_int},
};

/* ---------- constants ---------- */

const SUCCESS: c_int      = 0;
const ERROR: c_int        = -1;
const CHILD_EXITED: c_int = -2;

/* ---------- helpers ---------- */

fn debug(msg: &str) {
    if std::env::var("BUN_PTY_DEBUG").unwrap_or_default() == "1" {
        eprintln!("[rust-pty] {msg}");
    }
}

/* ---------- registry ---------- */

use std::sync::atomic::AtomicU32;
lazy_static::lazy_static! {
    static ref REG: std::sync::Mutex<HashMap<u32, std::sync::Arc<pty::Pty>>> = std::sync::Mutex::new(HashMap::new());
}
static NEXT: AtomicU32 = AtomicU32::new(1);

fn store(pty: std::sync::Arc<pty::Pty>) -> u32 {
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    REG.lock().unwrap().insert(id, pty);
    id
}
fn with<F: FnOnce(&std::sync::Arc<pty::Pty>) -> c_int>(id: u32, f: F) -> c_int {
    REG.lock().unwrap().get(&id).map(f).unwrap_or(ERROR)
}

/* ---------- FFI ---------- */

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bun_pty_spawn(
    cmd:  *const c_char,
    cwd:  *const c_char,
    env:  *const c_char,
    cols: c_int,
    rows: c_int,
) -> c_int {
    if cmd.is_null() || cwd.is_null() || cols <= 0 || rows <= 0 { return ERROR; }

    let cmdline = unsafe { CStr::from_ptr(cmd) }.to_string_lossy();
    let cwd     = unsafe { CStr::from_ptr(cwd) }.to_string_lossy();

    let size = portable_pty::PtySize { cols: cols as u16, rows: rows as u16, pixel_width: 0, pixel_height: 0 };
    let cmd = pty::Command::from_cmdline(&cmdline, &cwd, env);
    match pty::Pty::new(cmd, size) {
        Ok(p)  => store(p) as c_int,
        Err(e) => { debug(&format!("spawn error: {e}")); ERROR },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bun_pty_write(
    handle: c_int,
    data:   *const u8,
    len:    c_int,
) -> c_int {
    if handle <= 0 || data.is_null() || len < 0 { return ERROR; }
    with(handle as u32, |p| p.write(data, len as usize))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bun_pty_read(
    handle: c_int,
    buf:    *mut u8,
    len:    c_int,
    blocking: c_int,
) -> c_int {
    if handle <= 0 || buf.is_null() || len <= 0 { return ERROR; }
    with(handle as u32, |pty| {
        debug("bun_pty_read: starting");
        let max = len as usize;

        // 1) serve pending data first
        let mut pend = pty.pending.lock().unwrap();
        if !pend.is_empty() {
            let n = pend.len().min(max);
            unsafe { std::ptr::copy_nonoverlapping(pend.as_ptr(), buf, n); }
            // drop the bytes we returned
            pend.drain(..n);
            return n as c_int;
        }
        drop(pend); // release lock before potentially blocking ops

        // 2) pull fresh data
        match pty.read(blocking != 0) {
            Ok(pty::Msg::Data(d)) if !d.is_empty() => {
                let n = d.len().min(max);
                unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), buf, n); }
                if d.len() > n {
                    // stash remainder for next call
                    let mut pend = pty.pending.lock().unwrap();
                    pend.extend_from_slice(&d[n..]);
                }
                debug(&format!("bun_pty_read: returning {} bytes data", n));
                n as c_int
            }
            Ok(pty::Msg::End) => {
                debug("bun_pty_read: returning CHILD_EXITED");
                CHILD_EXITED
            }
            _ => {
                debug("bun_pty_read: returning 0 (no data)");
                0
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn bun_pty_resize(handle: c_int, cols: c_int, rows: c_int) -> c_int {
    if handle <= 0 || cols <= 0 || rows <= 0 { return ERROR; }
    with(handle as u32, |p| {
        p.resize(portable_pty::PtySize { cols: cols as u16, rows: rows as u16, pixel_width: 0, pixel_height: 0 }).map(|_| SUCCESS).unwrap_or(ERROR)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn bun_pty_kill(handle: c_int) -> c_int {
    if handle <= 0 { return ERROR; }
    with(handle as u32, |p| p.kill().map(|_| SUCCESS).unwrap_or(ERROR))
}

#[unsafe(no_mangle)]
pub extern "C" fn bun_pty_get_pid(handle: c_int) -> c_int {
    if handle <= 0 { return ERROR; }
    with(handle as u32, |p| p.get_pid())
}

#[unsafe(no_mangle)]
pub extern "C" fn bun_pty_get_exit_code(handle: c_int) -> c_int {
    if handle <= 0 { return ERROR; }
    with(handle as u32, |p| p.get_exit_code())
}

#[unsafe(no_mangle)]
pub extern "C" fn bun_pty_close(handle: c_int) {
    if handle <= 0 { return; }
    REG.lock().unwrap().remove(&(handle as u32));
}