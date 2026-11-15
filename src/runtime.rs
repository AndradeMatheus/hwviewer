//! One running app per user session: a named mutex, plus `<data>/runtime.json` ({pid, port, url, mode}) so a
//! second launch (or the CLI) can find the running instance and bring it forward.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::json;

use crate::storage::{read_json, write_json};

const MUTEX_NAME: &str = "Local\\HWiNFO-Viewer-App";

pub fn file() -> std::path::PathBuf {
    crate::paths::data_dir().join("runtime.json")
}

/// Ok(handle) when we are the only instance; Err(()) when another one holds the mutex.
#[cfg(windows)]
pub fn acquire() -> Result<windows::Win32::Foundation::HANDLE, ()> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = crate::watchdog::process::wide(MUTEX_NAME);
    // SAFETY: NUL-terminated name; the handle is held for the process lifetime on purpose.
    let h = unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) }.map_err(|_| ())?;
    // SAFETY: reads this thread's last-error value, set by CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(());
    }
    Ok(h)
}

pub fn publish(port: u16, url: &str, mode: &str) {
    let _ = write_json(&file(), &json!({"pid": std::process::id(), "port": port, "url": url, "mode": mode}));
}

/// Ask the running instance to show its window. True if it answered.
pub fn focus_existing() -> bool {
    let Some(port) = read_json(&file()).and_then(|v| v["port"].as_u64()) else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], port as u16).into(), Duration::from_secs(2)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "POST /api/focus HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-HWV: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    matches!(s.read(&mut buf), Ok(n) if n > 12 && buf.starts_with(b"HTTP/1.1 200"))
}
