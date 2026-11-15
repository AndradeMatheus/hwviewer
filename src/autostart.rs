//! "Start with Windows": a value under HKCU\Software\Microsoft\Windows\CurrentVersion\Run (no admin needed).

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::watchdog::process::wide;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const VALUE: &str = "HWiNFO Viewer"; // the installer writes the same value name

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: closing a key we opened.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open() -> Result<Key, String> {
    let path = wide(RUN_KEY);
    let mut h = HKEY::default();
    // SAFETY: NUL-terminated path; `h` receives the opened key, closed by Key's Drop.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut h,
            None,
        )
    }
    .ok()
    .map_err(|e| e.to_string())?;
    Ok(Key(h))
}

/// The registered command line, if any.
pub fn get() -> Option<String> {
    let k = open().ok()?;
    let name = wide(VALUE);
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` has `size` bytes; the API writes at most that and updates `size`.
    unsafe {
        RegQueryValueExW(k.0, PCWSTR(name.as_ptr()), None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut size))
    }
    .ok()
    .ok()?;
    let n = (size as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]).trim_end_matches('\0').to_string())
}

/// Register (`mode` = "app" or "tray") or remove the autostart entry for this exe.
pub fn set(on: bool, mode: &str) -> Result<(), String> {
    let k = open()?;
    let name = wide(VALUE);
    if !on {
        // SAFETY: deleting a value under our open key; "not found" is fine.
        let _ = unsafe { RegDeleteValueW(k.0, PCWSTR(name.as_ptr())) };
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cmd = if mode == "tray" { format!("\"{}\" tray", exe.display()) } else { format!("\"{}\"", exe.display()) };
    let data = wide(&cmd);
    // SAFETY: REG_SZ data is the NUL-terminated UTF-16 buffer, passed as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
    // SAFETY: valid open key and NUL-terminated value name.
    unsafe { RegSetValueExW(k.0, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes)) }.ok().map_err(|e| e.to_string())
}
