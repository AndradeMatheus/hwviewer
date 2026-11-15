//! HWiNFO viewer: reads HWiNFO64's shared memory and shows it in a desktop window, the tray and a browser.
//!
//! ```text
//! hwviewer [app]        desktop window + tray icon + local URL (default)
//! hwviewer tray         tray icon + local URL only; the window opens on demand (no WebView until then)
//! hwviewer serve        browser only (no window, no tray)
//! hwviewer dump         every sensor/label HWiNFO exposes -> <data>/dump.txt
//! hwviewer watchdog …   restart HWiNFO before the free version's 12 h limit (run|install|uninstall|status|restart-now)
//! ```
// GUI subsystem: no console window flashes up; CLI use re-attaches to the parent terminal (see main()).
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[macro_use]
mod log;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod desktop;
mod paths;
#[cfg(windows)]
mod runtime;
mod server;
mod shm;
mod storage;
#[cfg(windows)]
mod system;
mod views;
#[cfg(windows)]
mod watchdog;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::{Args, Parser, Subcommand};

use crate::shm::{DataSource, MockSource, SharedMemorySource};

static HAS_CONSOLE: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(name = "hwviewer", version, about = "HWiNFO sensor viewer: desktop window, tray, browser UI, watchdog")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Desktop window + tray icon + local URL (default)
    #[cfg(windows)]
    App(DeskArgs),
    /// Tray icon + local URL only; the window opens on demand
    #[cfg(windows)]
    Tray(DeskArgs),
    /// Serve the web UI and JSON API (browser only, no window)
    Serve(ServeArgs),
    /// Write every HWiNFO reading to <data>/dump.txt (to find patterns for views.json)
    Dump(SourceArgs),
    /// Restart HWiNFO silently before the free version's 12 h shared-memory limit
    #[cfg(windows)]
    Watchdog(watchdog::WdArgs),
}

#[derive(Args, Clone, Default)]
struct SourceArgs {
    /// Fake data instead of HWiNFO
    #[arg(long)]
    mock: bool,
    /// With --mock: simulate a 4 s HWiNFO restart every SECONDS
    #[arg(long, value_name = "SECONDS", default_value_t = 0.0)]
    mock_outage: f64,
    /// views.json to use (default: <data>/views.json)
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Args, Clone)]
struct DeskArgs {
    #[command(flatten)]
    src: SourceArgs,
    /// Preferred port (the next free one up to +10 is used if it's taken)
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Serve the UI from this folder instead of the built-in copy
    #[arg(long)]
    web_dir: Option<PathBuf>,
}

impl Default for DeskArgs {
    fn default() -> Self {
        DeskArgs { src: SourceArgs::default(), port: 8765, web_dir: None }
    }
}

#[derive(Args, Clone)]
struct ServeArgs {
    #[command(flatten)]
    desk: DeskArgs,
    /// 0.0.0.0 lets other devices on your LAN view it (read-only from them)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Open the browser
    #[arg(long)]
    open: bool,
}

/// Report a fatal error: stderr when there is a console, otherwise a message box. Exits.
pub fn fatal(msg: &str) -> ! {
    eprintln!("error: {msg}");
    #[cfg(windows)]
    if !HAS_CONSOLE.load(Ordering::SeqCst) {
        desktop::win32::message_box("HWiNFO viewer", msg);
    }
    std::process::exit(1)
}

fn source(a: &SourceArgs) -> Box<dyn DataSource> {
    if a.mock {
        Box::new(MockSource::new(a.mock_outage))
    } else {
        Box::new(SharedMemorySource::live())
    }
}

fn config_path(a: &SourceArgs) -> Result<PathBuf, String> {
    match &a.config {
        Some(p) => Ok(p.clone()),
        None => paths::ensure_views().map_err(|e| format!("could not create views.json: {e}")),
    }
}

fn open_browser(url: &str) {
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

fn serve(a: ServeArgs) -> Result<(), String> {
    let data = paths::data_dir();
    let app = Arc::new(server::App::new(source(&a.desk.src), config_path(&a.desk.src)?, &data));
    let (srv, port) = server::bind(&a.host, a.desk.port, 10)?;
    let lan = a.host == "0.0.0.0";
    let url = format!("http://{}:{port}/", if lan { "127.0.0.1" } else { &a.host });
    println!("HWiNFO viewer running at {url} (Ctrl+C to stop){}", if a.desk.src.mock { "  [MOCK DATA]" } else { "" });
    println!("data: {}", data.display());
    if a.open {
        open_browser(&url);
    }
    let ctx =
        Arc::new(server::Ctx { port, lan, web: paths::Web::resolve(a.desk.web_dir), on_focus: None, mode: "serve" });
    server::run(Arc::new(srv), app, ctx, 4);
    Ok(())
}

#[cfg(windows)]
fn desktop_mode(a: DeskArgs, tray_mode: bool) -> Result<(), String> {
    use tao::event_loop::EventLoopBuilder;
    let _instance = match runtime::acquire() {
        Ok(h) => h,
        Err(()) if runtime::focus_existing() => return Ok(()), // the running one came forward
        Err(()) => return Err("HWiNFO viewer is already running (check the tray icon)".into()),
    };
    log::init("app");
    let data = paths::data_dir();
    let app = Arc::new(server::App::new(source(&a.src), config_path(&a.src)?, &data));
    // "lan" (setup wizard): other devices may view it, read-only; applied at start-up
    let lan = server::lock(&app.settings).data["lan"].as_bool().unwrap_or(false);
    let (srv, port) = server::bind(if lan { "0.0.0.0" } else { "127.0.0.1" }, a.port, 10)?;
    let url = format!("http://127.0.0.1:{port}/");
    let mode = if tray_mode { "tray" } else { "app" };
    runtime::publish(port, &url, mode);
    info!(
        "HWiNFO viewer {} at {url} ({} mode, data {})",
        env!("CARGO_PKG_VERSION"),
        if tray_mode { "tray" } else { "app" },
        data.display()
    );

    let event_loop = EventLoopBuilder::<desktop::Ev>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let focus = Mutex::new(proxy.clone());
    let ctx = Arc::new(server::Ctx {
        port,
        lan,
        web: paths::Web::resolve(a.web_dir),
        on_focus: Some(Arc::new(move || {
            let _ = server::lock(&focus).send_event(desktop::Ev::Focus);
        })),
        mode,
    });
    let (srv, app2) = (Arc::new(srv), app.clone());
    std::thread::spawn(move || server::run(srv, app2, ctx, 4));
    desktop::Desktop::new(app, url, proxy, tray_mode).run(event_loop)
}

fn dump(a: SourceArgs) -> Result<(), String> {
    let (_, readings) = source(&a).read().map_err(|e| e.to_string())?;
    let mut out = String::from("sensor | label | unit | type | current\n");
    for r in &readings {
        out.push_str(&format!("{} | {} | {} | {} | {}\n", r.sensor, r.label, r.unit, r.typ, r.cur));
    }
    let p = paths::dump_file();
    storage::atomic_write(&p, out.as_bytes()).map_err(|e| e.to_string())?;
    println!("wrote {} readings to {}", readings.len(), p.display());
    Ok(())
}

fn main() {
    #[cfg(windows)]
    HAS_CONSOLE.store(desktop::win32::attach_parent_console(), Ordering::SeqCst);
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) if !HAS_CONSOLE.load(Ordering::SeqCst) && e.use_stderr() => fatal(&e.to_string()),
        Err(e) => e.exit(),
    };
    let r = match cli.cmd {
        #[cfg(windows)]
        None => desktop_mode(DeskArgs::default(), false),
        #[cfg(not(windows))]
        None => serve(ServeArgs { desk: DeskArgs::default(), host: "127.0.0.1".into(), open: false }),
        #[cfg(windows)]
        Some(Cmd::App(a)) => desktop_mode(a, false),
        #[cfg(windows)]
        Some(Cmd::Tray(a)) => desktop_mode(a, true),
        Some(Cmd::Serve(a)) => serve(a),
        Some(Cmd::Dump(a)) => dump(a),
        #[cfg(windows)]
        Some(Cmd::Watchdog(a)) => watchdog::cli(a),
    };
    if let Err(e) = r {
        fatal(&e);
    }
}
