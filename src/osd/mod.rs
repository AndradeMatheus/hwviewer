//! In-game OSD: sensor values (+ FPS) drawn over games. Two engines:
//! * **RTSS** (`rtss.rs`): our text goes into a RivaTuner Statistics Server OSD slot. RTSS renders it inside
//!   the game, which works in exclusive fullscreen. Requires RTSS to be running.
//! * **Native** (`overlay.rs`): our own click-through layered window (Direct2D). Windowed/borderless games only,
//!   no third-party software. We never inject anything into a game ourselves.
//!
//! `auto` uses RTSS when it is running, else the native overlay. The controller thread re-reads `settings.osd`
//! every 100 ms (so the UI, tray and hotkey apply at once) and redraws every `interval_ms`.
//! `status()` feeds `GET /api/osd/status`, including the rendered lines, which the UI uses as its live preview.

pub mod fps;
pub mod model;
pub mod overlay;
pub mod rtss;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::server::{lock, App};
use model::{build_lines, rtss_text, Engine, OsdCfg};

static QUIT: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static STATUS: OnceLock<Arc<Mutex<Value>>> = OnceLock::new();

pub fn status_handle() -> Arc<Mutex<Value>> {
    STATUS.get_or_init(|| Arc::new(Mutex::new(json!({"available": false})))).clone()
}

fn engine_name(e: Engine) -> &'static str {
    match e {
        Engine::Auto => "auto",
        Engine::Rtss => "rtss",
        Engine::Native => "native",
    }
}

struct Ctl {
    app: Arc<App>,
    overlay: Sender<overlay::Cmd>,
    fps: fps::FpsSource,
    rtss_claimed: bool,
}

impl Ctl {
    fn release_rtss(&mut self) {
        if self.rtss_claimed {
            let _ = rtss::with_mapping(true, |b| rtss::write_osd(b, None));
            self.rtss_claimed = false;
        }
    }

    fn tick(&mut self, cfg: &OsdCfg) {
        let want_fps = cfg.enabled && (cfg.show_fps || cfg.show_frametime);
        let (sample, source) = if want_fps { self.fps.sample() } else { (None, "off") };
        let payload = self.app.snapshot();
        let lines = build_lines(&payload, cfg, sample);
        let mut rtss_err = None;
        let active = if !cfg.enabled {
            let _ = self.overlay.send(overlay::Cmd::Hide);
            self.release_rtss();
            "off"
        } else {
            let wrote = if cfg.engine == Engine::Native {
                Err("native engine selected".to_string())
            } else {
                rtss::with_mapping(true, |b| rtss::write_osd(b, Some(&rtss_text(&lines, cfg)))).and_then(|r| r)
            };
            match wrote {
                Ok(()) => {
                    self.rtss_claimed = true;
                    let _ = self.overlay.send(overlay::Cmd::Hide);
                    "rtss"
                }
                Err(e) => {
                    rtss_err = Some(e);
                    self.release_rtss();
                    if cfg.engine == Engine::Rtss {
                        let _ = self.overlay.send(overlay::Cmd::Hide);
                        "none"
                    } else {
                        let _ = self.overlay.send(overlay::Cmd::Show(lines.clone(), Box::new(cfg.clone())));
                        "native"
                    }
                }
            }
        };
        let rtss_running = rtss::with_mapping(false, |_| ()).is_ok();
        *lock(&status_handle()) = json!({
            "available": true,
            "enabled": cfg.enabled,
            "engine": engine_name(cfg.engine),
            "active": active,
            "rtss_running": rtss_running,
            "rtss_error": rtss_err.filter(|_| cfg.enabled && cfg.engine != Engine::Native),
            "fps_source": source,
            "fps": sample.map(|s| s.fps),
            "frametime_ms": sample.map(|s| s.frametime_ms),
            "hotkey": cfg.hotkey,
            "lines": lines.iter().map(|l| json!({"label": l.label, "value": l.value, "color": l.color})).collect::<Vec<_>>(),
        });
    }
}

/// Start the OSD controller (desktop modes). Idempotent.
pub fn start(app: Arc<App>) {
    let mut slot = lock(&THREAD);
    if slot.is_some() {
        return;
    }
    QUIT.store(false, Ordering::SeqCst);
    let (ov_tx, ov_rx) = channel();
    let (hk_tx, hk_rx) = channel();
    std::thread::spawn(move || overlay::run(ov_rx, hk_tx));
    *slot = Some(std::thread::spawn(move || {
        let mut ctl = Ctl { app, overlay: ov_tx, fps: fps::FpsSource::new(), rtss_claimed: false };
        let mut last: Option<OsdCfg> = None;
        let mut next = Instant::now();
        while !QUIT.load(Ordering::SeqCst) {
            // the hotkey toggles the OSD through settings, so the UI and tray follow
            if hk_rx.try_recv().is_ok() {
                while hk_rx.try_recv().is_ok() {}
                let on = lock(&ctl.app.settings).data["osd"]["enabled"].as_bool().unwrap_or(false);
                let _ = lock(&ctl.app.settings).patch(&json!({"osd": {"enabled": !on}}));
            }
            let cfg = OsdCfg::from_settings(&lock(&ctl.app.settings).data["osd"]);
            if last.as_ref() != Some(&cfg) {
                let _ = ctl.overlay.send(overlay::Cmd::Hotkey(cfg.hotkey.clone()));
                next = Instant::now(); // apply changes at once
            }
            if Instant::now() >= next {
                // a panic in one frame must not end the OSD (or leave RTSS's busy bit set: write_osd unlocks first)
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctl.tick(&cfg)));
                next = Instant::now() + Duration::from_millis(cfg.interval_ms);
            }
            last = Some(cfg);
            std::thread::sleep(Duration::from_millis(100));
        }
        ctl.release_rtss(); // otherwise RTSS keeps showing our last frame after we exit
        let _ = ctl.overlay.send(overlay::Cmd::Quit);
    }));
}

/// Stop the controller, release the RTSS slot and close the overlay (called on quit).
pub fn shutdown() {
    QUIT.store(true, Ordering::SeqCst);
    if let Some(h) = lock(&THREAD).take() {
        let _ = h.join();
    }
}
