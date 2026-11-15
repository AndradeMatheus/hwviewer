//! Tiny rotating file logger (no dependency): `<data>/logs/<component>.log`, 512 KB x 3 files.
//! Also echoes to stderr when there is a console.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 512 * 1024;
const KEEP: usize = 2;

static TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Route log lines to `<data>/logs/<component>.log` (call once at start-up).
pub fn init(component: &str) {
    let dir = crate::paths::data_dir().join("logs");
    let _ = fs::create_dir_all(&dir);
    *TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.join(format!("{component}.log")));
}

fn rotate(p: &PathBuf) {
    if fs::metadata(p).map(|m| m.len()).unwrap_or(0) < MAX_BYTES {
        return;
    }
    for i in (1..KEEP).rev() {
        let _ = fs::rename(p.with_extension(format!("log.{i}")), p.with_extension(format!("log.{}", i + 1)));
    }
    let _ = fs::rename(p, p.with_extension("log.1"));
}

pub fn write(level: &str, msg: &str) {
    let line = format!("{} {level} {msg}", local_time(crate::shm::now_epoch()));
    eprintln!("{line}");
    let guard = TARGET.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = guard.as_ref() {
        rotate(p);
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write("INFO", &format!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write("WARNING", &format!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write("ERROR", &format!($($t)*)) } }

/// "YYYY-MM-DD HH:MM:SS" in local time for an epoch timestamp.
#[cfg(windows)]
pub fn local_time(epoch: f64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let t = ((epoch + 11_644_473_600.0) * 1e7) as u64;
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    let (mut utc, mut loc) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: plain value conversions into stack structs we own.
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc).is_ok() && SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).is_ok()
    };
    if !ok {
        return format!("{epoch:.0}");
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", loc.wYear, loc.wMonth, loc.wDay, loc.wHour, loc.wMinute, loc.wSecond)
}

#[cfg(not(windows))]
pub fn local_time(epoch: f64) -> String {
    format!("{epoch:.0}")
}
