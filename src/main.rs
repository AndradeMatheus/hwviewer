//! HWiNFO viewer: reads HWiNFO64's shared memory and serves a local web UI.
//!
//! ```text
//! hwviewer serve [--open] [--mock] [--mock-outage 20] [--port 8765] [--host 0.0.0.0]
//! hwviewer dump                       # every sensor/label HWiNFO exposes -> data/dump.txt
//! ```

mod paths;
mod server;
mod shm;
mod storage;
mod views;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};

use crate::shm::{DataSource, MockSource, SharedMemorySource};

#[derive(Parser)]
#[command(name = "hwviewer", version, about = "HWiNFO sensor viewer (web UI, desktop widget, watchdog, OSD)")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the web UI and JSON API (browser only, no window)
    Serve(ServeArgs),
    /// Write every HWiNFO reading to data/dump.txt (to find patterns for views.json)
    Dump(SourceArgs),
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
struct ServeArgs {
    #[command(flatten)]
    src: SourceArgs,
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// 0.0.0.0 lets other devices on your LAN view it (read-only from them)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Open the browser
    #[arg(long)]
    open: bool,
    /// Serve the UI from this folder instead of the built-in copy
    #[arg(long)]
    web_dir: Option<PathBuf>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        ServeArgs { src: SourceArgs::default(), port: 8765, host: "127.0.0.1".into(), open: false, web_dir: None }
    }
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
    let app = Arc::new(server::App::new(source(&a.src), config_path(&a.src)?, &data));
    let (srv, port) = server::bind(&a.host, a.port, 10)?;
    let lan = a.host == "0.0.0.0";
    let url = format!("http://{}:{port}/", if lan { "127.0.0.1" } else { &a.host });
    println!("HWiNFO viewer running at {url} (Ctrl+C to stop){}", if a.src.mock { "  [MOCK DATA]" } else { "" });
    println!("data: {}", data.display());
    if a.open {
        open_browser(&url);
    }
    let ctx = Arc::new(server::Ctx { port, lan, web: paths::Web::resolve(a.web_dir) });
    server::run(Arc::new(srv), app, ctx, 4);
    Ok(())
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
    let cli = Cli::parse();
    let r = match cli.cmd {
        None => serve(ServeArgs::default()),
        Some(Cmd::Serve(a)) => serve(a),
        Some(Cmd::Dump(a)) => dump(a),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
