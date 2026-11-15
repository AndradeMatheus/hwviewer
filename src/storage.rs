//! Persistent state: settings.json, profile folders, uploaded images, and a crash-safe file writer.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

pub const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];
pub const MAX_IMAGE: usize = 25 * 1024 * 1024;

/// A short unique-enough token for temp/asset file names (no RNG dependency).
pub fn token_hex(n: usize) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = t ^ (std::process::id() as u64).rotate_left(32) ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    let s = format!("{x:016x}");
    s[..n.min(16)].to_string()
}

/// Write via temp file + fsync + rename, so readers never see a half-written file.
/// Windows refuses the rename while another process (editor, antivirus) has the target open: retry briefly.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!("{name}.{}.tmp", token_hex(8)));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        let mut attempt = 0;
        loop {
            match fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && attempt < 5 => {
                    std::thread::sleep(Duration::from_millis(50 << attempt));
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    })();
    let _ = fs::remove_file(&tmp); // no-op once renamed
    result
}

pub fn write_json(path: &Path, v: &Value) -> io::Result<()> {
    let s = serde_json::to_string_pretty(v).map_err(io::Error::other)?;
    atomic_write(path, s.as_bytes())
}

/// Load JSON; a corrupt file is kept as `<name>.bad` instead of being silently overwritten later.
pub fn read_json(path: &Path) -> Option<Value> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            eprintln!("{name} is unreadable ({e}); keeping a copy as {name}.bad");
            let _ = fs::rename(path, path.with_file_name(format!("{name}.bad")));
            None
        }
    }
}

pub fn deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(dv) if dv.is_object() && v.is_object() => deep_merge(dv, v),
                    _ => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (d, s) => *d = s.clone(),
    }
}

/// settings.json: UI prefs per view, widget mode, window geometry, active profiles.
pub struct Settings {
    path: PathBuf,
    pub data: Value,
    pub rev: u64,
}

impl Settings {
    pub fn defaults() -> Value {
        json!({"active_profile": null, "active_layout": null, "show_unmatched": false, "ui": {}, "theme": {},
               "window": {"frameless": false, "on_top": false, "pinned": false, "opacity": 1.0,
                          "click_through": false, "close_to_tray": true, "start_hidden": false},
               "geometry": {}, "detached": []})
    }

    pub fn load(path: PathBuf) -> Self {
        let mut data = Self::defaults();
        if let Some(stored @ Value::Object(_)) = read_json(&path) {
            deep_merge(&mut data, &stored);
        }
        Settings { path, data, rev: 0 }
    }

    pub fn get(&self) -> Value {
        let mut d = self.data.clone();
        d["rev"] = json!(self.rev);
        d
    }

    fn commit(&mut self) -> io::Result<Value> {
        self.rev += 1;
        write_json(&self.path, &self.data)?;
        Ok(self.get())
    }

    pub fn patch(&mut self, patch: &Value) -> Result<Value, String> {
        let mut p = patch.clone();
        let Some(obj) = p.as_object_mut() else { return Err("settings patch must be an object".into()) };
        obj.remove("rev");
        deep_merge(&mut self.data, &p);
        self.commit().map_err(|e| e.to_string())
    }

    /// Set a key wholesale (deep_merge can't delete keys or shrink lists).
    pub fn replace_key(&mut self, key: &str, value: Value) -> Result<Value, String> {
        self.data[key] = value;
        self.commit().map_err(|e| e.to_string())
    }

    pub fn str_of(&self, key: &str) -> Option<String> {
        self.data.get(key).and_then(Value::as_str).map(str::to_string)
    }
}

/// ASCII word characters, '-' and ' ' only; trimmed; at most 40 characters.
pub fn safe_name(name: &str) -> Option<String> {
    let kept: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ')).collect();
    let n: String = kept.trim().chars().take(40).collect();
    (!n.is_empty()).then_some(n)
}

/// A folder of `<name>.json` documents with sanitised names (no path traversal).
pub struct Profiles {
    pub dir: PathBuf,
}

impl Profiles {
    pub fn new(dir: PathBuf) -> Self {
        Profiles { dir }
    }

    pub fn path(&self, name: &str) -> Option<PathBuf> {
        safe_name(name).map(|n| self.dir.join(format!("{n}.json")))
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .then(|| p.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .collect();
        v.sort();
        v
    }

    pub fn load(&self, name: &str) -> Option<Value> {
        let p = self.path(name)?;
        serde_json::from_slice(&fs::read(p).ok()?).ok()
    }

    pub fn save(&self, name: &str, data: &Value) -> Result<String, String> {
        let p = self.path(name).filter(|_| data.is_object()).ok_or("bad profile")?;
        write_json(&p, data).map_err(|e| e.to_string())?;
        Ok(p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string())
    }

    pub fn delete(&self, name: &str) {
        if let Some(p) = self.path(name) {
            let _ = fs::remove_file(p);
        }
    }
}

pub fn save_image(assets: &Path, name: &str, blob: &[u8]) -> Result<String, String> {
    let p = Path::new(name);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !IMAGE_EXT.contains(&ext.as_str()) {
        return Err("unsupported image type (use png, jpg, webp or gif)".into());
    }
    let stem: String = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(30)
        .collect();
    let stem = if stem.is_empty() { "image".to_string() } else { stem };
    let fname = format!("{stem}-{}.{ext}", token_hex(6));
    atomic_write(&assets.join(&fname), blob).map_err(|e| e.to_string())?;
    Ok(fname)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hwv-test-{tag}-{}", token_hex(8)));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_leaves_no_temp() {
        let d = temp_dir("aw");
        atomic_write(&d.join("s.json"), b"{}").unwrap();
        atomic_write(&d.join("s.json"), b"{\"a\": 1}").unwrap();
        let names: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec!["s.json"]);
    }

    #[test]
    fn corrupt_settings_kept_as_bad() {
        let d = temp_dir("bad");
        fs::write(d.join("settings.json"), "{nope").unwrap();
        let s = Settings::load(d.join("settings.json"));
        assert_eq!(s.get()["window"]["opacity"], json!(1.0));
        assert!(d.join("settings.json.bad").exists());
    }

    #[test]
    fn patch_merges_and_bumps_rev() {
        let d = temp_dir("patch");
        let mut s = Settings::load(d.join("settings.json"));
        let out = s.patch(&json!({"window": {"opacity": 0.5}, "rev": 99})).unwrap();
        assert_eq!(out["window"]["opacity"], json!(0.5));
        assert_eq!(out["window"]["close_to_tray"], json!(true));
        assert_eq!(out["rev"], json!(1));
        assert!(s.patch(&json!([1])).is_err());
    }

    #[test]
    fn safe_name_blocks_traversal() {
        assert_eq!(safe_name("../../evil").as_deref(), Some("evil"));
        assert_eq!(safe_name("../"), None);
        assert_eq!(safe_name("  My Profile-1 ").as_deref(), Some("My Profile-1"));
    }

    #[test]
    fn image_names_sanitised() {
        let d = temp_dir("img");
        let f = save_image(&d, "my pic!.PNG", b"x").unwrap();
        assert!(f.starts_with("my_pic_-") && f.ends_with(".png"));
        assert!(save_image(&d, "x.exe", b"x").is_err());
    }
}
