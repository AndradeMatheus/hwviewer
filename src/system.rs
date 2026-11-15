//! `GET /api/system`: everything the setup wizard shows - HWiNFO, shared memory, watchdog task, autostart, URLs.

use serde_json::{json, Value};

use crate::shm;
use crate::watchdog::{self, process};

/// This machine's LAN address (no packet is sent: connecting a UDP socket just picks the outbound interface).
pub fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never routed anywhere
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

pub fn info(mode: &str, port: u16, lan: bool) -> Value {
    let running = process::find_pids("HWiNFO64.exe").map(|p| !p.is_empty()).unwrap_or(false);
    let (shm_state, shm_msg) = match shm::read_raw() {
        Ok((_, r)) => ("ok".to_string(), format!("{} readings", r.len())),
        Err(e) => (e.kind.as_str().to_string(), e.msg),
    };
    let task = watchdog::task_info();
    let legacy = task.as_ref().is_some_and(|(_, c)| c.to_lowercase().contains(".py"));
    let autostart = crate::autostart::get();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "mode": mode,
        "url": format!("http://127.0.0.1:{port}/"),
        "lan": lan,
        "lan_url": if lan { lan_ip().map(|ip| format!("http://{ip}:{port}/")) } else { None },
        "data_dir": crate::paths::data_dir().display().to_string(),
        "elevated": process::is_admin(),
        "hwinfo": {"running": running, "shm": shm_state, "shm_msg": shm_msg},
        "watchdog": {
            "installed": task.is_some(),
            "state": task.as_ref().map(|t| t.0.clone()),
            "command": task.as_ref().map(|t| t.1.clone()),
            "legacy": legacy,
        },
        "autostart": {
            "on": autostart.is_some(),
            "mode": autostart.map(|c| if c.trim_end().ends_with(" tray") { "tray" } else { "app" }),
        },
    })
}
