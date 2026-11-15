//! Win32 process helpers for the watchdog: find, inspect, kill, launch, elevate.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_ELEVATION_REQUIRED: i32 = 740;

/// (pid, creation time) identifies a process; `exe` may be empty if it couldn't be queried.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub created: f64, // epoch seconds
    pub exe: String,
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Closes a HANDLE when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: we own this handle and close it exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn find_pids(exe_name: &str) -> Result<Vec<u32>, String> {
    // SAFETY: snapshot handle is wrapped in Owned and closed on every path.
    let snap = Owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(|e| e.to_string())?);
    let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut pids = Vec::new();
    // SAFETY: `e` is a correctly sized PROCESSENTRY32W, `snap` is a valid snapshot.
    let mut ok = unsafe { Process32FirstW(snap.0, &mut e) }.is_ok();
    while ok {
        let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(exe_name) {
            pids.push(e.th32ProcessID);
        }
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap.0, &mut e) }.is_ok();
    }
    Ok(pids)
}

fn created_of(h: HANDLE) -> Option<f64> {
    let (mut c, mut x, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: `h` is an open process handle with query rights; out-params are stack structs.
    unsafe { GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) }.ok()?;
    let ticks = ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64;
    Some(ticks as f64 / 1e7 - 11_644_473_600.0) // FILETIME (1601) -> epoch
}

/// Proc for pid, or None if it can't be opened (gone, or protected).
pub fn proc_info(pid: u32) -> Option<Proc> {
    // SAFETY: OpenProcess with limited query rights; the handle is closed by Owned.
    let h = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let created = created_of(h.0)?;
    let mut buf = vec![0u16; 32768];
    let mut n = buf.len() as u32;
    // SAFETY: `buf` has `n` u16 slots; the API writes at most that many and updates `n`.
    let exe = match unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n) } {
        Ok(()) => String::from_utf16_lossy(&buf[..n as usize]),
        Err(_) => String::new(),
    };
    Some(Proc { pid, created, exe })
}

pub fn alive(p: &Proc) -> bool {
    proc_info(p.pid).is_some_and(|q| (q.created - p.created).abs() < 1.0)
}

/// Kill `p` (checking the PID wasn't recycled). True once it is gone.
pub fn terminate(p: &Proc, timeout: f64) -> bool {
    let rights = PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: handle closed by Owned; rights limited to what we use.
    match unsafe { OpenProcess(rights, false, p.pid) } {
        Ok(h) => {
            let h = Owned(h);
            match created_of(h.0) {
                Some(c) if (c - p.created).abs() < 1.0 => {}
                _ => return true, // the PID now belongs to something else: ours is already gone
            }
            // SAFETY: valid handle with PROCESS_TERMINATE / SYNCHRONIZE.
            match unsafe { TerminateProcess(h.0, 1) } {
                Err(e) => warn!("TerminateProcess(PID {}) failed: {e}", p.pid),
                Ok(()) => {
                    // SAFETY: valid handle with SYNCHRONIZE.
                    if unsafe { WaitForSingleObject(h.0, (timeout * 1000.0) as u32) } == WAIT_OBJECT_0 {
                        return true;
                    }
                }
            }
        }
        Err(e) => warn!("OpenProcess(PID {}) failed ({e}) - trying taskkill", p.pid),
    }
    if alive(p) {
        let out = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &p.pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        if let Ok(o) = out {
            let text =
                String::from_utf8_lossy(if o.stdout.is_empty() { &o.stderr } else { &o.stdout }).trim().to_string();
            info!("taskkill: {text}");
        }
        let end = Instant::now() + Duration::from_secs_f64(timeout);
        while alive(p) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    !alive(p)
}

pub fn is_admin() -> bool {
    // SAFETY: no arguments, returns a BOOL.
    unsafe { IsUserAnAdmin() }.as_bool()
}

fn quote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '\t', '"']) {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// ShellExecute "runas": one UAC prompt, then `exe args` runs elevated in `dir`.
pub fn shell_runas(exe: &Path, args: &[String], dir: &Path, show: i32) -> Result<(), String> {
    let (verb, file, params, cwd) =
        (wide("runas"), wide(&exe.to_string_lossy()), wide(&quote_args(args)), wide(&dir.to_string_lossy()));
    // SAFETY: all strings are NUL-terminated buffers that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR(cwd.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD(show),
        )
    };
    let code = r.0 as isize;
    if code <= 32 {
        return Err(format!("elevation was cancelled or failed (ShellExecute {code})"));
    }
    Ok(())
}

/// Start HWiNFO detached from us - and from the scheduled task's job, so stopping the task doesn't kill it.
pub fn launch(exe: &Path, args: &[String]) -> Result<(), String> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let r = Command::new(exe)
            .args(args)
            .current_dir(dir)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match r {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED) => {
                warn!(
                    "HWiNFO needs admin and the watchdog isn't elevated - a UAC prompt will appear. \
                     Run `hwviewer watchdog install` for silent restarts."
                );
                return shell_runas(exe, args, dir, SW_SHOWMINNOACTIVE.0);
            }
            Err(e) if flags & CREATE_BREAKAWAY_FROM_JOB != 0 && e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                continue; // our job object forbids breakaway: start inside it
            }
            Err(e) => return Err(format!("could not start {}: {e}", exe.display())),
        }
    }
    Err(format!("could not start {}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_inspects_ourselves() {
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().to_string();
        let pids = find_pids(&name).unwrap();
        assert!(pids.contains(&std::process::id()));
        let p = proc_info(std::process::id()).unwrap();
        assert!(p.exe.eq_ignore_ascii_case(&me.to_string_lossy()));
        assert!(crate::shm::now_epoch() - p.created < 600.0);
        assert!(alive(&p));
        assert!(!alive(&Proc { created: p.created - 100.0, ..p }));
    }

    #[test]
    fn quoting() {
        assert_eq!(
            quote_args(&["-a".into(), "C:\\Program Files\\x".into(), "".into()]),
            "-a \"C:\\Program Files\\x\" \"\""
        );
    }
}
