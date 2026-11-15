//! Every file location the app uses, in one place.
//!
//! Data dir: `$HWV_DATA_DIR` > `<exe dir>/data` when `portable.txt` sits next to the exe >
//! `<repo>/data` for debug builds (so `cargo run` shares data with the dev checkout) >
//! `%LOCALAPPDATA%\HWiNFO Viewer` for installed release builds.
//! Web UI: `--web-dir` > `<repo>/web` on disk for debug builds (edit + refresh, no rebuild) > embedded copy.

use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::storage::atomic_write;

pub const DEFAULT_VIEWS: &str = include_str!("../defaults/views.json");
const EMBEDDED_INDEX: &[u8] = include_bytes!("../web/index.html");
const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(PathBuf::from)
}

pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(d) = std::env::var_os("HWV_DATA_DIR") {
            return PathBuf::from(d);
        }
        if let Some(e) = exe_dir().filter(|e| e.join("portable.txt").exists()) {
            return e.join("data");
        }
        if cfg!(debug_assertions) {
            return PathBuf::from(MANIFEST_DIR).join("data");
        }
        std::env::var_os("LOCALAPPDATA")
            .map(|l| PathBuf::from(l).join("HWiNFO Viewer"))
            .unwrap_or_else(|| exe_dir().unwrap_or_default().join("data"))
    })
    .clone()
}

pub fn views_file() -> PathBuf {
    data_dir().join("views.json")
}
pub fn dump_file() -> PathBuf {
    data_dir().join("dump.txt")
}

/// The user-editable views.json, created from the factory copy on first run.
pub fn ensure_views() -> io::Result<PathBuf> {
    let p = views_file();
    if !p.exists() {
        // the pre-Rust layout kept it in the project root: carry a customised copy over
        let legacy = PathBuf::from(MANIFEST_DIR).join("views.json");
        let data = if cfg!(debug_assertions) && legacy.exists() {
            std::fs::read(&legacy)?
        } else {
            DEFAULT_VIEWS.as_bytes().to_vec()
        };
        atomic_write(&p, &data)?;
    }
    Ok(p)
}

#[derive(Clone, Debug)]
pub enum Web {
    Disk(PathBuf),
    Embedded,
}

impl Web {
    pub fn resolve(override_dir: Option<PathBuf>) -> Web {
        match override_dir {
            Some(d) => Web::Disk(d),
            None if cfg!(debug_assertions) => Web::Disk(PathBuf::from(MANIFEST_DIR).join("web")),
            None => Web::Embedded,
        }
    }

    pub fn index(&self) -> io::Result<Vec<u8>> {
        match self {
            Web::Disk(d) => std::fs::read(d.join("index.html")),
            Web::Embedded => Ok(EMBEDDED_INDEX.to_vec()),
        }
    }
}
