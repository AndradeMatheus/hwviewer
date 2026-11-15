//! The viewer: [`App`] (data + config state) and a small tiny_http server for the web UI and its JSON API.

use std::collections::HashSet;
use std::io::Read;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use regex::RegexBuilder;
use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::paths::Web;
use crate::shm::{mono, now_epoch, DataSource, Reading, SmError};
use crate::storage::{read_json, safe_name, save_image, Profiles, Settings, MAX_IMAGE};
use crate::views::{build_payload, RxCache};

pub const CACHE_FOR: f64 = 0.4; // seconds a snapshot is reused (several windows poll at 1 Hz)
pub const GRACE: f64 = 30.0; // keep showing the last good data ("reconnecting") this long after HWiNFO drops
pub const RESTART_GRACE: f64 = 120.0; // ... and this long while the watchdog reports it is restarting HWiNFO
pub const WATCHDOG_FRESH: f64 = 150.0; // a status file older than this means the watchdog isn't running
const MAX_BODY: usize = 1 << 20;

/// Lock that survives a panicked holder (we'd rather serve slightly odd state than die).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads data/watchdog_status.json (written by the watchdog process) when it changes.
pub struct WatchdogStatus {
    path: PathBuf,
    mtime: Option<SystemTime>,
    data: Option<Value>,
}

impl WatchdogStatus {
    pub fn new(path: PathBuf) -> Self {
        WatchdogStatus { path, mtime: None, data: None }
    }

    pub fn get(&mut self) -> Value {
        let Ok(m) = std::fs::metadata(&self.path).and_then(|m| m.modified()) else { return Value::Null };
        if Some(m) != self.mtime {
            self.mtime = Some(m);
            self.data = read_json(&self.path);
        }
        let Some(d) = self.data.as_ref().filter(|d| d.is_object()) else { return Value::Null };
        if now_epoch() - d.get("updated").and_then(Value::as_f64).unwrap_or(0.0) > WATCHDOG_FRESH {
            return Value::Null;
        }
        let mut out = Map::new();
        for k in ["state", "message", "restart_at", "last_restart", "restarts", "hwinfo_started"] {
            out.insert(k.into(), d.get(k).cloned().unwrap_or(Value::Null));
        }
        Value::Object(out)
    }
}

enum Fail {
    Sm(SmError),
    Other(String),
}

impl From<SmError> for Fail {
    fn from(e: SmError) -> Self {
        Fail::Sm(e)
    }
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::Other(e)
    }
}

struct Inner {
    source: Box<dyn DataSource>,
    cfg_path: PathBuf,
    cfg: Option<Value>,
    cfg_mtime: Option<SystemTime>,
    layout_cache: Option<(String, SystemTime, Value)>,
    cache: Option<Value>,
    cache_t: f64,
    good: Option<Value>,
    good_t: f64,
    since: Option<f64>,
    last_readings: Vec<Reading>,
    rx: RxCache,
    watchdog: WatchdogStatus,
}

/// Lock order, always: `inner` before `settings`.
pub struct App {
    inner: Mutex<Inner>,
    pub settings: Mutex<Settings>,
    pub profiles: Profiles,
    pub layouts: Profiles,
    pub assets: PathBuf,
}

impl App {
    pub fn new(source: Box<dyn DataSource>, cfg_path: PathBuf, data: &Path) -> Self {
        App {
            inner: Mutex::new(Inner {
                source,
                cfg_path,
                cfg: None,
                cfg_mtime: None,
                layout_cache: None,
                cache: None,
                cache_t: f64::NEG_INFINITY,
                good: None,
                good_t: f64::NEG_INFINITY,
                since: None,
                last_readings: Vec::new(),
                rx: RxCache::default(),
                watchdog: WatchdogStatus::new(data.join("watchdog_status.json")),
            }),
            settings: Mutex::new(Settings::load(data.join("settings.json"))),
            profiles: Profiles::new(data.join("profiles")),
            layouts: Profiles::new(data.join("layouts")),
            assets: data.join("assets"),
        }
    }

    pub fn invalidate(&self) {
        lock(&self.inner).cache = None;
    }

    fn config(g: &mut Inner) -> Result<Value, String> {
        let m = std::fs::metadata(&g.cfg_path).and_then(|m| m.modified());
        match m {
            Ok(m) if Some(m) == g.cfg_mtime => {}
            Ok(m) => {
                let parsed = std::fs::read(&g.cfg_path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
                    .and_then(|v| {
                        if v.get("catalog").is_some_and(Value::is_object) && v.get("views").is_some_and(Value::is_array)
                        {
                            Ok(v)
                        } else {
                            Err("views.json needs a 'catalog' object and a 'views' list".to_string())
                        }
                    });
                match parsed {
                    Ok(v) => {
                        g.cfg = Some(v);
                        g.cache = None;
                    }
                    Err(e) if g.cfg.is_some() => {
                        eprintln!("views.json has an error, keeping the previous version: {e}")
                    }
                    Err(e) => return Err(format!("views.json: {e}")),
                }
                g.cfg_mtime = Some(m);
            }
            Err(e) if g.cfg.is_none() => return Err(format!("views.json: {e}")),
            Err(_) => {}
        }
        g.cfg.clone().ok_or_else(|| "views.json not loaded".to_string())
    }

    fn active_layout(&self, g: &mut Inner) -> (Option<String>, Option<Value>) {
        let Some(name) = lock(&self.settings).str_of("active_layout") else { return (None, None) };
        let Some(f) = self.layouts.path(&name).filter(|f| f.exists()) else { return (None, None) };
        let Ok(m) = std::fs::metadata(&f).and_then(|m| m.modified()) else { return (None, None) };
        let fresh = matches!(&g.layout_cache, Some((n, t, _)) if *n == name && *t == m);
        if !fresh {
            match std::fs::read(&f).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                Some(v) => g.layout_cache = Some((name.clone(), m, v)),
                None => return (None, None),
            }
        }
        (Some(name), g.layout_cache.as_ref().map(|c| c.2.clone()))
    }

    /// Factory views.json, overlaid with the active sensor profile (if any).
    fn effective(&self, g: &mut Inner) -> Result<(Value, Option<String>), String> {
        let base = Self::config(g)?;
        let (name, lay) = self.active_layout(g);
        let Some(lay) = lay else { return Ok((base, None)) };
        let views: Vec<Value> = base["views"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                match lay.get("views").and_then(|vs| vs.get(id)).and_then(|ov| ov.get("sections")) {
                    Some(secs) if !secs.is_null() => {
                        let mut v = v.clone();
                        v["sections"] = secs.clone();
                        v
                    }
                    _ => v.clone(),
                }
            })
            .collect();
        let mut catalog = base["catalog"].clone();
        if let (Some(c), Some(extra)) = (catalog.as_object_mut(), lay.get("catalog_extra").and_then(Value::as_object)) {
            for (k, v) in extra {
                c.insert(k.clone(), v.clone());
            }
        }
        Ok((json!({"catalog": catalog, "views": views}), name))
    }

    pub fn active_layout_name(&self) -> Option<String> {
        let mut g = lock(&self.inner);
        self.active_layout(&mut g).0
    }

    pub fn active_layout_value(&self) -> Option<Value> {
        let mut g = lock(&self.inner);
        self.active_layout(&mut g).1
    }

    /// Apply a view edit (order/add/remove) to the active profile, forking "Custom" from Default if needed.
    pub fn edit_layout(&self, body: &Value) -> Result<String, String> {
        let mut g = lock(&self.inner);
        let base = Self::config(&mut g)?;
        let view = body.get("view").and_then(Value::as_str).unwrap_or("");
        let empty = Map::new();
        let add = match body.get("add") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(o)) => o,
            Some(_) => return Err("bad edit".into()),
        };
        let sections = body.get("sections").and_then(Value::as_array).ok_or("bad edit")?;
        if !base["views"].as_array().into_iter().flatten().any(|v| v.get("id").and_then(Value::as_str) == Some(view)) {
            return Err("bad edit".into());
        }
        for it in add.values() {
            for fld in ["sensor", "label", "unit"] {
                if let Some(p) = it.get(fld).and_then(Value::as_str).filter(|p| !p.is_empty()) {
                    RegexBuilder::new(p).case_insensitive(true).build().map_err(|e| e.to_string())?;
                    // reject early
                }
            }
        }
        let (name, lay) = self.active_layout(&mut g);
        let (name, mut lay) = match (name, lay) {
            (Some(n), Some(l)) => (n, l),
            _ => {
                let (mut n, mut i) = ("Custom".to_string(), 1);
                while self.layouts.path(&n).is_some_and(|p| p.exists()) {
                    i += 1;
                    n = format!("Custom {i}");
                }
                (n, json!({"views": {}, "catalog_extra": {}}))
            }
        };
        if !lay.get("views").is_some_and(Value::is_object) {
            lay["views"] = json!({});
        }
        if !lay.get("catalog_extra").is_some_and(Value::is_object) {
            lay["catalog_extra"] = json!({});
        }
        if let Some(extra) = lay["catalog_extra"].as_object_mut() {
            for (k, v) in add {
                extra.insert(k.clone(), v.clone());
            }
        }
        let known: HashSet<String> = base["catalog"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .chain(lay["catalog_extra"].as_object().into_iter().flatten().map(|(k, _)| k.clone()))
            .collect();
        let mut new_secs = Vec::new();
        for s in sections {
            let s = s.as_object().ok_or("bad edit")?;
            let title: String = match s.get("title") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Null) | None => "?".into(),
                Some(other) => other.to_string(),
            };
            let items: Vec<Value> = s
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|k| k.as_str().is_some_and(|k| known.contains(k)))
                .cloned()
                .collect();
            new_secs.push(json!({"title": title.chars().take(40).collect::<String>(), "items": items}));
        }
        lay["views"][view] = json!({"sections": new_secs});
        let items_of = |v: &Value| -> Vec<String> {
            v.get("sections")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|s| s.get("items").and_then(Value::as_array).into_iter().flatten())
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect()
        };
        let overridden: HashSet<String> =
            lay["views"].as_object().into_iter().flatten().map(|(k, _)| k.clone()).collect();
        let mut used: HashSet<String> =
            lay["views"].as_object().into_iter().flatten().flat_map(|(_, v)| items_of(v)).collect();
        for v in base["views"].as_array().into_iter().flatten() {
            if !overridden.contains(v.get("id").and_then(Value::as_str).unwrap_or("")) {
                used.extend(items_of(v));
            }
        }
        if let Some(extra) = lay["catalog_extra"].as_object_mut() {
            extra.retain(|k, _| used.contains(k));
        }
        self.layouts.save(&name, &lay)?;
        lock(&self.settings).patch(&json!({"active_layout": name}))?;
        g.cache = None;
        Ok(name)
    }

    fn build(&self, g: &mut Inner, now: f64) -> Result<Value, Fail> {
        let (cfg, layout_name) = self.effective(g)?;
        let (_poll, readings) = g.source.read()?;
        let mut payload = build_payload(&cfg, &readings, &mut g.rx);
        g.last_readings = readings;
        payload["layout"] = json!(layout_name);
        let since = *g.since.get_or_insert(now);
        payload["uptime"] = json!(now - since);
        g.good = Some(payload.clone());
        g.good_t = now;
        Ok(payload)
    }

    pub fn snapshot(&self) -> Value {
        let mut g = lock(&self.inner);
        let now = mono();
        if let Some(c) = g.cache.as_ref().filter(|_| now - g.cache_t < CACHE_FOR) {
            return c.clone();
        }
        let wd = g.watchdog.get();
        let built = catch_unwind(AssertUnwindSafe(|| self.build(&mut g, now)));
        let mut payload = match built {
            Ok(Ok(p)) => p,
            Ok(Err(Fail::Sm(e))) => {
                g.since = None; // the next successful read starts a new run (e.g. a restarted HWiNFO)
                let grace =
                    if wd.get("state").and_then(Value::as_str) == Some("restarting") { RESTART_GRACE } else { GRACE };
                match g.good.as_ref().filter(|_| now - g.good_t < grace) {
                    Some(good) => {
                        let mut p = good.clone();
                        p["reconnecting"] = json!(true);
                        p["error"] = json!(e.msg);
                        p["stale_for"] = json!(now - g.good_t);
                        p
                    }
                    None => json!({"ok": false, "error": e.msg, "kind": e.kind.as_str(), "ts": now_epoch()}),
                }
            }
            Ok(Err(Fail::Other(msg))) => {
                json!({"ok": false, "error": format!("viewer error: {msg}"), "kind": "internal", "ts": now_epoch()})
            }
            Err(_) => {
                json!({"ok": false, "error": "viewer error: internal panic", "kind": "internal", "ts": now_epoch()})
            }
        };
        payload["rev"] = json!(lock(&self.settings).rev);
        payload["watchdog"] = wd;
        g.cache = Some(payload.clone());
        g.cache_t = now;
        payload
    }

    /// Unique (sensor, label, unit) readings from the last poll, for the "add sensor" picker.
    pub fn readings(&self) -> Value {
        self.snapshot();
        let g = lock(&self.inner);
        let mut seen = HashSet::new();
        Value::Array(
            g.last_readings
                .iter()
                .filter(|r| seen.insert((r.sensor.clone(), r.label.clone(), r.unit.clone())))
                .map(|r| json!({"sensor": r.sensor, "label": r.label, "unit": r.unit, "cur": r.cur}))
                .collect(),
        )
    }
}

// --------------------------------------------------------------------------- HTTP
pub type Hook = Arc<dyn Fn() + Send + Sync>;

pub struct Ctx {
    pub port: u16,
    pub lan: bool,
    pub web: Web,
    /// Desktop modes: bring the main window forward (POST /api/focus from a second launch).
    pub on_focus: Option<Hook>,
}

struct Reply {
    code: u16,
    body: Vec<u8>,
    ctype: &'static str,
}

fn reply_json(code: u16, v: &Value) -> Reply {
    Reply { code, body: serde_json::to_vec(v).unwrap_or_default(), ctype: "application/json; charset=utf-8" }
}

fn reply_text(code: u16, s: &str) -> Reply {
    Reply { code, body: s.as_bytes().to_vec(), ctype: "text/plain" }
}

fn err(code: u16, msg: &str) -> Reply {
    reply_json(code, &json!({"error": msg}))
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

/// `application/x-www-form-urlencoded` style decoding, like Python's parse_qs.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

fn body(req: &mut Request, limit: usize) -> Result<Vec<u8>, String> {
    if req.body_length().is_some_and(|n| n > limit) {
        return Err("bad body size".into());
    }
    let mut buf = Vec::new();
    req.as_reader().take(limit as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > limit {
        return Err("bad body size".into());
    }
    Ok(buf)
}

fn mime(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

fn guard(req: &Request, ctx: &Ctx, post: bool) -> Option<Reply> {
    // blocks DNS-rebinding (Host check) and cross-site POSTs (custom header forces a CORS preflight)
    let host = header(req, "Host").unwrap_or("");
    if !ctx.lan && host != format!("127.0.0.1:{}", ctx.port) && host != format!("localhost:{}", ctx.port) {
        return Some(err(403, "bad host"));
    }
    if post && header(req, "X-HWV") != Some("1") {
        return Some(err(403, "missing X-HWV header"));
    }
    if post && ctx.lan && !req.remote_addr().is_some_and(|a: &SocketAddr| a.ip().is_loopback()) {
        return Some(err(403, "read-only over the network - change settings on the PC itself"));
    }
    None
}

fn get(app: &App, ctx: &Ctx, path: &str, query: &str) -> Reply {
    match path {
        "/api/data" => reply_json(200, &app.snapshot()),
        "/api/settings" => reply_json(200, &lock(&app.settings).get()),
        "/api/profiles" => reply_json(200, &json!({"names": app.profiles.names()})),
        "/api/layouts" => reply_json(200, &json!({"names": app.layouts.names(), "active": app.active_layout_name()})),
        "/api/readings" => reply_json(200, &app.readings()),
        "/api/profile" => match app.profiles.load(&query_param(query, "name").unwrap_or_default()) {
            Some(v) => reply_json(200, &v),
            None => err(404, "not found"),
        },
        "/" | "/index.html" => match ctx.web.index() {
            Ok(b) => Reply { code: 200, body: b, ctype: "text/html; charset=utf-8" },
            Err(e) => reply_text(500, &format!("index.html: {e}")),
        },
        p if p.starts_with("/assets/") => {
            let name = p.rsplit('/').next().unwrap_or("");
            let ok_ext = name
                .rsplit('.')
                .next()
                .is_some_and(|e| crate::storage::IMAGE_EXT.contains(&e.to_ascii_lowercase().as_str()));
            let f = app.assets.join(name);
            if ok_ext && !name.contains(['\\', ':']) && !name.starts_with('.') && f.is_file() {
                match std::fs::read(&f) {
                    Ok(b) => Reply { code: 200, body: b, ctype: mime(name) },
                    Err(_) => reply_text(404, "not found"),
                }
            } else {
                reply_text(404, "not found")
            }
        }
        _ => reply_text(404, "not found"),
    }
}

fn post(app: &App, ctx: &Ctx, path: &str, query: &str, req: &mut Request) -> Result<Reply, String> {
    if path == "/api/focus" {
        return Ok(match &ctx.on_focus {
            Some(f) => {
                f();
                reply_json(200, &json!({"ok": true}))
            }
            None => err(404, "no window in this mode"),
        });
    }
    if path == "/api/image" {
        let name = query_param(query, "name").unwrap_or_default();
        let blob = body(req, MAX_IMAGE)?;
        return Ok(reply_json(200, &json!({"file": save_image(&app.assets, &name, &blob)?})));
    }
    let raw = body(req, MAX_BODY)?;
    let b: Value = if raw.is_empty() { json!({}) } else { serde_json::from_slice(&raw).map_err(|e| e.to_string())? };
    if !b.is_object() {
        return Err("expected a JSON object".into());
    }
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(match path {
        "/api/settings" => reply_json(200, &lock(&app.settings).patch(&b)?),
        "/api/profile" => {
            reply_json(200, &json!({"name": app.profiles.save(&s("name"), b.get("data").unwrap_or(&Value::Null))?}))
        }
        "/api/profile/delete" => {
            app.profiles.delete(&s("name"));
            reply_json(200, &json!({"ok": true}))
        }
        "/api/layout/select" => {
            // name=null -> back to the original views.json
            let n = if s("name").is_empty() { None } else { safe_name(&s("name")) };
            if let Some(n) = &n {
                if !app.layouts.names().contains(n) {
                    return Err("no such profile".into());
                }
            }
            lock(&app.settings).patch(&json!({"active_layout": n}))?;
            app.invalidate();
            reply_json(200, &json!({"active": n}))
        }
        "/api/layout/edit" => reply_json(200, &json!({"active": app.edit_layout(&b)?})),
        "/api/layout/save_as" => {
            // copy the active profile (or Default) under a new name
            let lay = app.active_layout_value().unwrap_or_else(|| json!({"views": {}, "catalog_extra": {}}));
            let n = app.layouts.save(&s("name"), &lay)?;
            lock(&app.settings).patch(&json!({"active_layout": n}))?;
            app.invalidate();
            reply_json(200, &json!({"active": n}))
        }
        "/api/layout/delete" => {
            if let Some(n) = safe_name(&s("name")) {
                app.layouts.delete(&n);
                let mut st = lock(&app.settings);
                if st.str_of("active_layout").as_deref() == Some(n.as_str()) {
                    st.patch(&json!({"active_layout": null}))?;
                }
            }
            app.invalidate();
            reply_json(200, &json!({"ok": true}))
        }
        "/api/settings/reset_key" => {
            let k = s("key");
            if !["theme", "ui", "window"].contains(&k.as_str()) {
                return Err("bad key".into());
            }
            reply_json(200, &lock(&app.settings).replace_key(&k, Settings::defaults()[&k].clone())?)
        }
        _ => err(404, "not found"),
    })
}

fn handle(app: &App, ctx: &Ctx, mut req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let is_post = *req.method() == Method::Post;
    if std::env::var_os("HWV_TRACE").is_some() {
        info!("{} {url} host={:?}", req.method(), header(&req, "Host"));
    }
    let reply = if let Some(r) = guard(&req, ctx, is_post) {
        r
    } else if is_post {
        post(app, ctx, path, query, &mut req).unwrap_or_else(|e| err(400, &e))
    } else if *req.method() == Method::Get {
        get(app, ctx, path, query)
    } else {
        reply_text(405, "method not allowed")
    };
    let mut resp = Response::from_data(reply.body).with_status_code(reply.code);
    for (k, v) in [("Content-Type", reply.ctype), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff")]
    {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    let _ = req.respond(resp);
}

/// Bind `host:port`, falling back to the next ports (up to +10) if it is taken. Returns the server and its port.
pub fn bind(host: &str, port: u16, tries: u16) -> Result<(Server, u16), String> {
    let mut last = String::new();
    for p in port..=port.saturating_add(tries) {
        match Server::http((host, p)) {
            Ok(s) => return Ok((s, p)),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("no free port in {port}..={}: {last}", port.saturating_add(tries)))
}

/// Serve requests on `workers` threads; returns when the server is unblocked/dropped.
pub fn run(server: Arc<Server>, app: Arc<App>, ctx: Arc<Ctx>, workers: usize) {
    let handles: Vec<_> = (0..workers.max(1))
        .map(|_| {
            let (server, app, ctx) = (server.clone(), app.clone(), ctx.clone());
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    // a panicking handler drops that one connection, never the worker
                    let _ = catch_unwind(AssertUnwindSafe(|| handle(&app, &ctx, req)));
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DEFAULT_VIEWS;
    use crate::shm::{Kind, Snapshot};
    use crate::storage::tests::temp_dir;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Flaky(Arc<AtomicBool>);

    impl DataSource for Flaky {
        fn read(&mut self) -> Result<Snapshot, SmError> {
            if self.0.load(Ordering::SeqCst) {
                return Err(SmError::new("gone", Kind::Missing));
            }
            Ok((
                1,
                vec![Reading {
                    sensor: "AMD Ryzen 7 5800X: Enhanced".into(),
                    label: "CPU (Tctl/Tdie)".into(),
                    label_orig: "CPU (Tctl/Tdie)".into(),
                    unit: "°C".into(),
                    typ: 1,
                    cur: 50.0,
                    min: 40.0,
                    max: 60.0,
                    avg: 45.0,
                }],
            ))
        }
    }

    fn app(fail: Arc<AtomicBool>) -> (App, PathBuf) {
        let d = temp_dir("app");
        std::fs::write(d.join("views.json"), DEFAULT_VIEWS).unwrap();
        (App::new(Box::new(Flaky(fail)), d.join("views.json"), &d), d)
    }

    #[test]
    fn last_values_held_then_error() {
        let fail = Arc::new(AtomicBool::new(false));
        let (app, _d) = app(fail.clone());
        let p = app.snapshot();
        assert_eq!(p["ok"], true);
        assert_eq!(p["views"][0]["sections"][0]["rows"][0]["cur"], json!(50.0));
        fail.store(true, Ordering::SeqCst);
        app.invalidate();
        let p = app.snapshot();
        assert!(p["ok"] == true && p["reconnecting"] == true); // inside the grace window: last values
        {
            let mut g = lock(&app.inner);
            g.good_t -= GRACE + 1.0;
            g.cache = None;
        }
        let p = app.snapshot();
        assert_eq!(p["ok"], false); // grace over: a real error
        assert_eq!(p["kind"], "missing");
        fail.store(false, Ordering::SeqCst);
        app.invalidate();
        let p = app.snapshot();
        assert!(p["ok"] == true && p.get("reconnecting").is_none());
    }

    #[test]
    fn edit_layout_forks_custom_and_filters_unknown() {
        let (app, _d) = app(Arc::new(AtomicBool::new(false)));
        let n = app
            .edit_layout(
                &json!({"view": "compact", "sections": [{"title": "Mine", "items": ["cpu_tctl", "nope", "x1"]}],
                                 "add": {"x1": {"name": "Extra", "label": "^Foo$"}}}),
            )
            .unwrap();
        assert_eq!(n, "Custom");
        let lay = app.layouts.load("Custom").unwrap();
        assert_eq!(lay["views"]["compact"]["sections"][0]["items"], json!(["cpu_tctl", "x1"]));
        assert!(lay["catalog_extra"].get("x1").is_some());
        assert!(app.edit_layout(&json!({"view": "compact", "sections": [], "add": {"b": {"label": "("}}})).is_err());
        assert!(app.edit_layout(&json!({"view": "nope", "sections": []})).is_err());
        let p = app.snapshot();
        assert_eq!(p["layout"], "Custom");
    }

    #[test]
    fn query_decoding() {
        assert_eq!(query_param("a=1&name=My%20Pic+2.png", "name").as_deref(), Some("My Pic 2.png"));
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%e2%80%ba"), "›");
    }
}
