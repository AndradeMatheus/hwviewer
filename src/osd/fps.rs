//! Frame rate of the foreground app. RTSS first (cheap, unelevated, exact). Otherwise ask the watchdog's PresentMon
//! helper (ETW needs admin, and the watchdog is the elevated process) over localhost UDP. Its port is in
//! `<data>/watchdog_status.json` (`fps_port`).

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::model::Fps;
use super::rtss;
use crate::storage::read_json;

/// Process ID owning the foreground window (0 if none).
pub fn foreground_pid() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut pid = 0u32;
    // SAFETY: plain query; `pid` is a stack out-param.
    unsafe {
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
    }
    pid
}

pub struct FpsSource {
    sock: Option<UdpSocket>,
    port: Option<u16>,
    port_checked: Option<Instant>,
    latest: Option<(Instant, u32, Fps)>,
}

impl FpsSource {
    pub fn new() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").ok().filter(|s| s.set_nonblocking(true).is_ok());
        FpsSource { sock, port: None, port_checked: None, latest: None }
    }

    /// The helper's port, re-read every 5 s (the watchdog may start, stop or restart).
    fn helper_port(&mut self) -> Option<u16> {
        if self.port_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.port_checked = Some(Instant::now());
            let st = read_json(&crate::paths::data_dir().join("watchdog_status.json")).unwrap_or(Value::Null);
            let fresh = crate::shm::now_epoch() - st["updated"].as_f64().unwrap_or(0.0) < 150.0;
            self.port = st["fps_port"].as_u64().filter(|_| fresh).map(|p| p as u16);
        }
        self.port
    }

    /// (sample, source): source is "RTSS", "PresentMon" or "none".
    pub fn sample(&mut self) -> (Option<Fps>, &'static str) {
        let pid = foreground_pid();
        if pid == 0 || pid == std::process::id() {
            return (None, "none"); // our own window is in front: there is no game to measure
        }
        if let Ok(Some(f)) = rtss::with_mapping(false, |b| rtss::read_fps(b, pid)) {
            return (Some(f), "RTSS");
        }
        let (Some(port), Some(sock)) = (self.helper_port(), self.sock.as_ref()) else { return (None, "none") };
        // replies arrive asynchronously: take what came in since the last call, then ask for this pid again
        let mut buf = [0u8; 512];
        while let Ok(n) = sock.recv(&mut buf) {
            if let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) {
                if let (Some(p), Some(fps), Some(ft)) =
                    (v["pid"].as_u64(), v["fps"].as_f64(), v["frametime_ms"].as_f64())
                {
                    self.latest = Some((Instant::now(), p as u32, Fps { fps, frametime_ms: ft }));
                }
            }
        }
        let _ = sock.send_to(json!({"want": pid}).to_string().as_bytes(), ("127.0.0.1", port));
        match self.latest {
            Some((t, p, f)) if p == pid && t.elapsed() < Duration::from_secs(3) => (Some(f), "PresentMon"),
            _ => (None, "none"),
        }
    }
}
