//! Desktop shell (`app` and `tray` modes): tao event loop, wry windows (main + detached views/sections),
//! tray icon. The web UI talks to its window through wry IPC (`window.ipc.postMessage(json)`); everything
//! else goes through the same local HTTP API the browser uses.
//!
//! Windows are created with WS_EX_NOREDIRECTIONBITMAP: that's what makes the WebView's transparent pixels
//! really transparent (DECISIONS #10), so background/box/text opacity are just CSS alphas.

pub mod icon;
pub mod win32;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Window, WindowBuilder, WindowId};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::server::{lock, App};

pub const TITLE: &str = "HWiNFO viewer";
const MAIN_SIZE: (f64, f64) = (660.0, 980.0);
const DETACHED_SIZE: (f64, f64) = (520.0, 640.0);
const OPTS: [&str; 7] = ["frameless", "on_top", "pinned", "opacity", "click_through", "close_to_tray", "start_hidden"];
const FLAGS: [(&str, &str); 5] = [
    ("on_top", "Always on top"),
    ("pinned", "Pin to desktop"),
    ("click_through", "Click-through (lock)"),
    ("close_to_tray", "Close button minimizes to tray"),
    ("start_hidden", "Start hidden"),
];
const OPACITIES: [u32; 7] = [100, 90, 80, 70, 60, 50, 30];
const GEO_DEBOUNCE: Duration = Duration::from_millis(600);
const PIN_EVERY: Duration = Duration::from_secs(1);

pub enum Ev {
    Ipc(String, String), // window key, JSON message
    Menu(MenuId),
    TrayLeftClick,
    Focus, // a second launch asked us to come forward
}

struct Win {
    window: Window,
    _webview: WebView,
}

struct Tray {
    _icon: TrayIcon,
    show: MenuItem,
    open: MenuItem,
    copy: MenuItem,
    quit: MenuItem,
    flags: Vec<(&'static str, CheckMenuItem)>,
    opacity: Vec<(u32, CheckMenuItem)>,
}

pub struct Desktop {
    app: Arc<App>,
    url: String,
    proxy: EventLoopProxy<Ev>,
    web_ctx: WebContext,
    wins: HashMap<String, Win>,
    ids: HashMap<WindowId, String>,
    tray: Option<Tray>,
    tray_mode: bool,
    visible: bool,
    geo_pending: Map<String, Value>,
    geo_dirty_at: Option<Instant>,
    next_pin: Instant,
}

/// Like Python's quote(key, safe="/").
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn on_screen(target: &EventLoopWindowTarget<Ev>, x: i64, y: i64) -> bool {
    let mut any = false;
    for m in target.available_monitors() {
        any = true;
        let (p, s) = (m.position(), m.size());
        let (mx, my) = (p.x as i64, p.y as i64);
        if x >= mx - 50 && x < mx + s.width as i64 - 50 && y >= my - 50 && y < my + s.height as i64 - 50 {
            return true;
        }
    }
    !any
}

impl Desktop {
    pub fn new(app: Arc<App>, url: String, proxy: EventLoopProxy<Ev>, tray_mode: bool) -> Self {
        Desktop {
            app,
            url,
            proxy,
            // WebView2 profile under the data dir (Program Files isn't writable)
            web_ctx: WebContext::new(Some(crate::paths::data_dir().join("webview2"))),
            wins: HashMap::new(),
            ids: HashMap::new(),
            tray: None,
            tray_mode,
            visible: false,
            geo_pending: Map::new(),
            geo_dirty_at: None,
            next_pin: Instant::now() + PIN_EVERY,
        }
    }

    fn opts(&self) -> Value {
        lock(&self.app.settings).data["window"].clone()
    }

    fn flag(o: &Value, k: &str) -> bool {
        o[k].as_bool().unwrap_or(false)
    }

    // ------------------------------------------------------------------ windows
    fn create(&mut self, target: &EventLoopWindowTarget<Ev>, key: &str, hidden: bool) -> Result<(), String> {
        let o = self.opts();
        let title = if key == "main" {
            TITLE.to_string()
        } else {
            format!("{TITLE} - {}", key.split_once('/').map_or(key, |(_, s)| s))
        };
        let geo = lock(&self.app.settings).data["geometry"][key].clone();
        let (dw, dh) = if key == "main" { MAIN_SIZE } else { DETACHED_SIZE };
        let icon = tao::window::Icon::from_rgba(icon::rgba(32), 32, 32).ok();
        // see-through backgrounds only for frameless windows: a title-bar window with no redirection bitmap
        // renders unreliably, and a transparent client under an opaque frame looks broken anyway
        let see_through = Self::flag(&o, "frameless");
        let mut b = WindowBuilder::new()
            .with_title(&title)
            .with_decorations(!see_through)
            .with_transparent(see_through)
            .with_no_redirection_bitmap(see_through)
            .with_always_on_top(Self::flag(&o, "on_top") && !Self::flag(&o, "pinned"))
            .with_visible(!hidden)
            .with_min_inner_size(LogicalSize::new(240.0, 160.0))
            .with_window_icon(icon);
        b = match (geo["w"].as_u64(), geo["h"].as_u64()) {
            (Some(w), Some(h)) if w > 0 && h > 0 => b.with_inner_size(PhysicalSize::new(w as u32, h as u32)),
            _ => b.with_inner_size(LogicalSize::new(dw, dh)),
        };
        if let (Some(x), Some(y)) = (geo["x"].as_i64(), geo["y"].as_i64()) {
            if on_screen(target, x, y) {
                b = b.with_position(PhysicalPosition::new(x as i32, y as i32));
            }
        }
        let window = b.build(target).map_err(|e| format!("could not create window: {e}"))?;
        let solo = if key == "main" { String::new() } else { format!("&solo={}", enc(key)) };
        let url = format!("{}?shell=desktop&win={}{solo}", self.url, enc(key));
        let init =
            format!("window.__HWV = {};", json!({"win": key, "url": self.url, "version": env!("CARGO_PKG_VERSION")}));
        let (proxy, k) = (self.proxy.clone(), key.to_string());
        let webview = WebViewBuilder::new_with_web_context(&mut self.web_ctx)
            .with_url(url)
            .with_transparent(see_through)
            .with_initialization_script(init)
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(Ev::Ipc(k.clone(), req.body().clone()));
            })
            .build(&window)
            .map_err(|e| format!("could not create WebView2 (is the WebView2 runtime installed?): {e}"))?;
        let hwnd = win32::hwnd_of(&window);
        win32::set_click_through(hwnd, Self::flag(&o, "click_through"));
        win32::stack(hwnd, Self::flag(&o, "on_top"), Self::flag(&o, "pinned"));
        self.ids.insert(window.id(), key.to_string());
        self.wins.insert(key.to_string(), Win { window, _webview: webview });
        if key == "main" {
            self.visible = !hidden;
        }
        Ok(())
    }

    fn destroy(&mut self, key: &str) {
        if let Some(w) = self.wins.remove(key) {
            self.ids.remove(&w.window.id());
        }
    }

    fn detach(&mut self, target: &EventLoopWindowTarget<Ev>, view: &str, section: &str) {
        let key = if section.is_empty() { view.to_string() } else { format!("{view}/{section}") };
        if let Some(w) = self.wins.get(&key) {
            w.window.set_visible(true);
            w.window.set_focus();
            return;
        }
        if let Err(e) = self.create(target, &key, false) {
            error!("{e}");
        }
        self.persist_detached();
    }

    fn persist_detached(&self) {
        let list: Vec<Value> = self
            .wins
            .keys()
            .filter(|k| *k != "main")
            .map(|k| {
                let (v, s) = k.split_once('/').unwrap_or((k, ""));
                json!({"view": v, "section": s})
            })
            .collect();
        if let Err(e) = lock(&self.app.settings).replace_key("detached", Value::Array(list)) {
            warn!("could not save detached windows: {e}");
        }
    }

    fn show_main(&mut self, target: &EventLoopWindowTarget<Ev>) {
        if !self.wins.contains_key("main") {
            if let Err(e) = self.create(target, "main", false) {
                error!("{e}");
                return;
            }
        }
        if let Some(w) = self.wins.get("main") {
            w.window.set_visible(true);
            w.window.set_minimized(false);
            w.window.set_focus();
        }
        self.visible = true;
    }

    fn hide_main(&mut self) {
        if let Some(w) = self.wins.get("main") {
            w.window.set_visible(false);
        }
        self.visible = false;
    }

    fn toggle_main(&mut self, target: &EventLoopWindowTarget<Ev>) {
        if self.visible && self.wins.contains_key("main") {
            self.hide_main()
        } else {
            self.show_main(target)
        }
    }

    fn apply_all(&self) {
        let o = self.opts();
        for w in self.wins.values() {
            let hwnd = win32::hwnd_of(&w.window);
            win32::set_click_through(hwnd, Self::flag(&o, "click_through"));
            win32::stack(hwnd, Self::flag(&o, "on_top"), Self::flag(&o, "pinned"));
        }
    }

    /// Change window options (from the UI or the tray). A title-bar change recreates the windows in place.
    fn set_opts(&mut self, target: &EventLoopWindowTarget<Ev>, patch: &Value) {
        let mut p = Map::new();
        for (k, v) in patch.as_object().into_iter().flatten() {
            if OPTS.contains(&k.as_str()) {
                let v = if k == "opacity" { json!(v.as_f64().unwrap_or(1.0).clamp(0.2, 1.0)) } else { v.clone() };
                p.insert(k.clone(), v);
            }
        }
        let before = self.opts();
        if let Err(e) = lock(&self.app.settings).patch(&json!({"window": p})) {
            warn!("could not save window options: {e}");
        }
        let after = self.opts();
        if before["frameless"] != after["frameless"] {
            self.flush_geo();
            let keys: Vec<String> = self.wins.keys().cloned().collect();
            let main_hidden = !self.visible;
            for k in &keys {
                self.destroy(k);
            }
            for k in &keys {
                if let Err(e) = self.create(target, k, k == "main" && main_hidden) {
                    error!("{e}");
                }
            }
        } else {
            self.apply_all();
        }
        self.sync_tray();
    }

    fn on_ipc(&mut self, target: &EventLoopWindowTarget<Ev>, key: &str, msg: &str, cf: &mut ControlFlow) {
        let m: Value = serde_json::from_str(msg).unwrap_or(Value::Null);
        let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
        match m["cmd"].as_str().unwrap_or("") {
            "drag" => {
                if let Some(w) = self.wins.get(key) {
                    let _ = w.window.drag_window();
                }
            }
            "set_window" => self.set_opts(target, &m["patch"]),
            "resize" => {
                if let Some(w) = self.wins.get(key) {
                    let (wd, ht) =
                        (m["w"].as_f64().unwrap_or(0.0).max(240.0), m["h"].as_f64().unwrap_or(0.0).max(160.0));
                    w.window.set_inner_size(LogicalSize::new(wd, ht));
                }
            }
            "detach" => self.detach(target, &s("view"), &s("section")),
            "close" if key != "main" => {
                self.destroy(key);
                self.persist_detached();
            }
            "hide" | "close" => self.hide_main(),
            "quit" => self.quit(cf),
            "log" => warn!("ui [{key}]: {}", s("msg")),
            // setup wizard: the task needs admin, so this shows one UAC prompt; the wizard polls /api/system
            "watchdog_install" | "watchdog_uninstall" => {
                let sub = if m["cmd"] == "watchdog_install" { "install" } else { "uninstall" };
                let r = std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
                    let dir = exe.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
                    crate::watchdog::process::shell_runas(&exe, &["watchdog".into(), sub.into()], &dir, 0)
                });
                if let Err(e) = r {
                    warn!("watchdog {sub}: {e}");
                }
            }
            "autostart" => {
                let mode = if s("mode") == "tray" { "tray" } else { "app" };
                if let Err(e) = crate::autostart::set(m["on"].as_bool().unwrap_or(false), mode) {
                    warn!("autostart: {e}");
                }
            }
            "open_url" => win32::open_url(&self.url),
            "copy_url" => {
                let _ = win32::copy_text(&self.url);
            }
            other => warn!("unknown window command {other:?} from {key}"),
        }
    }

    // ------------------------------------------------------------------ tray
    fn make_tray(&self) -> Result<Tray, String> {
        let o = self.opts();
        let show = MenuItem::new(if self.tray_mode { "Open window" } else { "Show / hide" }, true, None);
        let open = MenuItem::new(format!("Open in browser   {}", self.url), true, None);
        let copy = MenuItem::new("Copy URL", true, None);
        let flags: Vec<(&'static str, CheckMenuItem)> =
            FLAGS.iter().map(|(k, label)| (*k, CheckMenuItem::new(*label, true, Self::flag(&o, k), None))).collect();
        let cur = (o["opacity"].as_f64().unwrap_or(1.0) * 100.0).round() as u32;
        let opacity: Vec<(u32, CheckMenuItem)> =
            OPACITIES.iter().map(|p| (*p, CheckMenuItem::new(format!("{p}%"), true, *p == cur, None))).collect();
        let op_items: Vec<&dyn IsMenuItem> = opacity.iter().map(|(_, i)| i as &dyn IsMenuItem).collect();
        let sub = Submenu::with_items("Opacity", true, &op_items).map_err(|e| e.to_string())?;
        let quit = MenuItem::new("Quit", true, None);
        let (s1, s2, s3) =
            (PredefinedMenuItem::separator(), PredefinedMenuItem::separator(), PredefinedMenuItem::separator());
        let mut items: Vec<&dyn IsMenuItem> = vec![&show, &s1, &open, &copy, &s2];
        items.extend(flags.iter().map(|(_, i)| i as &dyn IsMenuItem));
        items.extend([&sub as &dyn IsMenuItem, &s3, &quit]);
        let menu = Menu::with_items(&items).map_err(|e| e.to_string())?;
        let ico = tray_icon::Icon::from_rgba(icon::rgba(32), 32, 32).map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(format!("{TITLE} - {}", self.url))
            .with_icon(ico)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Tray { _icon: icon, show, open, copy, quit, flags, opacity })
    }

    fn sync_tray(&self) {
        let Some(t) = &self.tray else { return };
        let o = self.opts();
        for (k, item) in &t.flags {
            item.set_checked(Self::flag(&o, k));
        }
        let cur = (o["opacity"].as_f64().unwrap_or(1.0) * 100.0).round() as u32;
        for (p, item) in &t.opacity {
            item.set_checked(*p == cur);
        }
    }

    fn on_menu(&mut self, target: &EventLoopWindowTarget<Ev>, id: &MenuId, cf: &mut ControlFlow) {
        let Some(t) = &self.tray else { return };
        if id == t.show.id() {
            self.toggle_main(target);
        } else if id == t.open.id() {
            win32::open_url(&self.url);
        } else if id == t.copy.id() {
            let _ = win32::copy_text(&self.url);
        } else if id == t.quit.id() {
            self.quit(cf);
        } else if let Some((k, _)) = t.flags.iter().find(|(_, i)| i.id() == id) {
            let k = *k;
            let cur = Self::flag(&self.opts(), k);
            self.set_opts(target, &json!({ k: !cur }));
        } else if let Some((p, _)) = t.opacity.iter().find(|(_, i)| i.id() == id) {
            let p = *p;
            self.set_opts(target, &json!({"opacity": p as f64 / 100.0}));
        }
    }

    // ------------------------------------------------------------------ geometry, pinning, lifecycle
    fn save_geo(&mut self, id: WindowId, kv: &[(&str, i64)]) {
        let Some(key) = self.ids.get(&id).cloned() else { return };
        let entry = self.geo_pending.entry(key).or_insert_with(|| json!({}));
        for (k, v) in kv {
            entry[*k] = json!(v);
        }
        self.geo_dirty_at = Some(Instant::now());
    }

    fn flush_geo(&mut self) {
        if self.geo_pending.is_empty() {
            return;
        }
        let p = std::mem::take(&mut self.geo_pending);
        if let Err(e) = lock(&self.app.settings).patch(&json!({"geometry": p})) {
            warn!("could not save window geometry: {e}");
        }
        self.geo_dirty_at = None;
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if self.geo_dirty_at.is_some_and(|t| now >= t + GEO_DEBOUNCE) {
            self.flush_geo();
        }
        if now >= self.next_pin {
            self.next_pin = now + PIN_EVERY;
            let o = self.opts();
            let fg = win32::foreground();
            for w in self.wins.values() {
                let h = win32::hwnd_of(&w.window);
                if Self::flag(&o, "pinned") {
                    // clicking a pinned window raises it; push it back once focus moves away
                    if h != fg {
                        win32::stack(h, false, true);
                    }
                } else if Self::flag(&o, "on_top") && !win32::is_topmost(h) {
                    // WebView2 start-up / activation can drop TOPMOST: re-assert it
                    win32::stack(h, true, false);
                }
            }
        }
    }

    fn next_wake(&self) -> Instant {
        let pin = self.next_pin;
        match self.geo_dirty_at {
            Some(t) => pin.min(t + GEO_DEBOUNCE),
            None => pin,
        }
    }

    fn quit(&mut self, cf: &mut ControlFlow) {
        self.flush_geo();
        self.tray = None; // removes the tray icon now rather than at process exit
        *cf = ControlFlow::Exit;
    }

    fn start(&mut self, target: &EventLoopWindowTarget<Ev>) {
        match self.make_tray() {
            Ok(t) => self.tray = Some(t),
            Err(e) => warn!("tray icon unavailable: {e}"),
        }
        if !self.tray_mode || self.tray.is_none() {
            let hidden = Self::flag(&self.opts(), "start_hidden") && self.tray.is_some();
            if let Err(e) = self.create(target, "main", hidden) {
                crate::fatal(&e);
            }
        }
        let detached = lock(&self.app.settings).data["detached"].clone();
        for d in detached.as_array().into_iter().flatten() {
            let (v, s) =
                (d["view"].as_str().unwrap_or("full").to_string(), d["section"].as_str().unwrap_or("").to_string());
            self.detach(target, &v, &s);
        }
    }

    pub fn run(mut self, event_loop: EventLoop<Ev>) -> ! {
        // tray-icon delivers its events through global handlers: forward them into the loop
        let p = Mutex::new(self.proxy.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = lock(&p).send_event(Ev::Menu(e.id));
        }));
        let p = Mutex::new(self.proxy.clone());
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                let _ = lock(&p).send_event(Ev::TrayLeftClick);
            }
        }));
        event_loop.run(move |event, target, cf| {
            if *cf != ControlFlow::Exit {
                *cf = ControlFlow::WaitUntil(self.next_wake());
            }
            match event {
                Event::NewEvents(StartCause::Init) => self.start(target),
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => self.timers(),
                Event::WindowEvent { window_id, event, .. } => match event {
                    WindowEvent::CloseRequested => {
                        let Some(key) = self.ids.get(&window_id).cloned() else { return };
                        if key == "main" {
                            if Self::flag(&self.opts(), "close_to_tray") && self.tray.is_some() {
                                self.hide_main(); // keep running in the tray
                            } else {
                                self.quit(cf);
                            }
                        } else {
                            self.destroy(&key); // a detached window the user closed: forget it
                            self.persist_detached();
                        }
                    }
                    WindowEvent::Moved(pos) if pos.x > -30000 => {
                        self.save_geo(window_id, &[("x", pos.x as i64), ("y", pos.y as i64)])
                    }
                    WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                        self.save_geo(window_id, &[("w", size.width as i64), ("h", size.height as i64)])
                    }
                    _ => {}
                },
                Event::UserEvent(ev) => match ev {
                    Ev::Ipc(key, msg) => self.on_ipc(target, &key, &msg, cf),
                    Ev::Menu(id) => self.on_menu(target, &id, cf),
                    Ev::TrayLeftClick => self.toggle_main(target),
                    Ev::Focus => self.show_main(target),
                },
                Event::LoopDestroyed => self.flush_geo(),
                _ => {}
            }
        })
    }
}
