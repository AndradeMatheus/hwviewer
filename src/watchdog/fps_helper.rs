//! PresentMon FPS helper, hosted by the (elevated) watchdog: ETW capture needs admin, the viewer doesn't have it.
//!
//! The app sends `{"want": pid}` to 127.0.0.1:<port> about once a second. The helper keeps one PresentMon
//! child for that pid, parses `MsBetweenPresents` from its CSV, and replies `{pid, fps, frametime_ms}`.
//! PresentMon is stopped 5 s after the last request, so nothing runs while nobody shows an FPS counter.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::process::CREATE_NO_WINDOW;
use crate::server::lock;

const IDLE_STOP: Duration = Duration::from_secs(5);
const WINDOW: Duration = Duration::from_secs(1);

/// Rolling 1 s window of frame intervals.
#[derive(Default)]
pub struct Frames {
    q: VecDeque<(Instant, f64)>,
}

impl Frames {
    pub fn push(&mut self, at: Instant, ms: f64) {
        if ms.is_finite() && ms > 0.0 && ms < 5000.0 {
            self.q.push_back((at, ms));
        }
        while self.q.front().is_some_and(|(t, _)| at.duration_since(*t) > WINDOW) {
            self.q.pop_front();
        }
    }

    /// (fps, mean frame time in ms)
    pub fn stats(&self) -> Option<(f64, f64)> {
        if self.q.is_empty() {
            return None;
        }
        let mean = self.q.iter().map(|(_, ms)| ms).sum::<f64>() / self.q.len() as f64;
        Some((1000.0 / mean, mean))
    }
}

/// Column index of the frame interval in PresentMon's CSV header (1.x and 2.x names).
pub fn interval_column(header: &str) -> Option<usize> {
    header.split(',').position(|c| matches!(c.trim(), "MsBetweenPresents" | "msBetweenPresents" | "MsBetweenAppStart"))
}

struct State {
    want: Option<u32>,
    last_req: Instant,
    sample: Option<(u32, f64, f64, Instant)>, // pid, fps, frametime, when
}

fn start_presentmon(exe: &Path, pid: u32, state: Arc<Mutex<State>>) -> Option<Child> {
    let mut child = Command::new(exe)
        .args(["--process_id", &pid.to_string(), "--output_stdout", "--no_console_stats", "--v1_metrics"])
        .args(["--no_track_input", "--no_track_gpu", "--session_name", "HWiNFOViewer", "--stop_existing_session"])
        .arg("--terminate_on_proc_exit")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| warn!("FPS helper: could not start PresentMon: {e}"))
        .ok()?;
    let out = child.stdout.take()?;
    std::thread::spawn(move || {
        let mut col = None;
        let mut frames = Frames::default();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            match col {
                None => col = interval_column(&line),
                Some(i) => {
                    if let Some(ms) = line.split(',').nth(i).and_then(|v| v.trim().parse::<f64>().ok()) {
                        let now = Instant::now();
                        frames.push(now, ms);
                        if let Some((fps, ft)) = frames.stats() {
                            lock(&state).sample = Some((pid, fps, ft, now));
                        }
                    }
                }
            }
        }
    });
    info!("FPS helper: PresentMon capturing PID {pid}");
    Some(child)
}

/// Start the helper if PresentMon is available. Returns the UDP port to publish.
pub fn spawn(presentmon: PathBuf) -> Option<u16> {
    if !presentmon.is_file() {
        info!("FPS helper disabled: {} not found", presentmon.display());
        return None;
    }
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    let port = sock.local_addr().ok()?.port();
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    let state = Arc::new(Mutex::new(State { want: None, last_req: Instant::now(), sample: None }));
    info!("FPS helper listening on 127.0.0.1:{port} ({})", presentmon.display());
    std::thread::spawn(move || {
        let mut child: Option<(u32, Child)> = None;
        let mut buf = [0u8; 256];
        loop {
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                let want =
                    serde_json::from_slice::<Value>(&buf[..n]).ok().and_then(|v| v["want"].as_u64()).map(|p| p as u32);
                if let Some(pid) = want.filter(|&p| p != 0) {
                    let reply = {
                        let mut st = lock(&state);
                        st.want = Some(pid);
                        st.last_req = Instant::now();
                        st.sample.filter(|s| s.0 == pid && s.3.elapsed() < Duration::from_secs(2))
                    };
                    if let Some((p, fps, ft, _)) = reply {
                        let _ = sock
                            .send_to(json!({"pid": p, "fps": fps, "frametime_ms": ft}).to_string().as_bytes(), from);
                    }
                }
            }
            // keep exactly one PresentMon for the requested pid, and none when idle
            let (want, idle) = {
                let st = lock(&state);
                (st.want, st.last_req.elapsed() > IDLE_STOP)
            };
            let exited = child.as_mut().is_some_and(|(_, c)| !matches!(c.try_wait(), Ok(None)));
            let stale = child.as_ref().is_some_and(|(p, _)| Some(*p) != want);
            if child.is_some() && (idle || stale || exited) {
                if let Some((_, mut c)) = child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
            if child.is_none() && !idle {
                if let Some(pid) = want {
                    child = start_presentmon(&presentmon, pid, state.clone()).map(|c| (pid, c));
                    if child.is_none() {
                        std::thread::sleep(Duration::from_secs(5)); // don't spin on a broken PresentMon
                    }
                }
            }
        }
    });
    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_columns() {
        assert_eq!(interval_column("Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,AllowsTearing,PresentMode,TimeInSeconds,MsBetweenPresents,MsBetweenDisplayChange"), Some(9));
        assert_eq!(interval_column("Application,ProcessID"), None);
    }

    #[test]
    fn rolling_window_stats() {
        let mut f = Frames::default();
        let t0 = Instant::now();
        for i in 0..10 {
            f.push(t0 + Duration::from_millis(i * 10), 10.0);
        }
        let (fps, ft) = f.stats().unwrap();
        assert!((fps - 100.0).abs() < 1e-9 && (ft - 10.0).abs() < 1e-9);
        f.push(t0 + Duration::from_millis(2500), 20.0); // everything older than 1 s drops out
        assert_eq!(f.stats(), Some((50.0, 20.0)));
        f.push(t0 + Duration::from_millis(2600), f64::NAN); // garbage is ignored
        assert_eq!(f.stats(), Some((50.0, 20.0)));
    }
}
