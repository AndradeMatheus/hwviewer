//! OSD configuration (`settings.osd`) and the pure payload -> lines step both engines share.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Auto,   // RTSS when it's running (works in exclusive fullscreen), else the native overlay
    Rtss,   // RTSS only
    Native, // our own click-through overlay window (windowed / borderless games)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,            // a payload `sensors[].id`
    pub label: Option<String>, // overrides the sensor name
    pub color: Option<String>, // value colour (#rrggbb); warn/crit colours still win
    pub decimals: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OsdCfg {
    pub enabled: bool,
    pub engine: Engine,
    pub hotkey: String,
    pub interval_ms: u64,
    pub items: Vec<Item>,
    pub show_fps: bool,
    pub show_frametime: bool,
    pub horizontal: bool,
    pub anchor: Anchor,
    pub offset: (i32, i32),
    pub font: String,
    pub size: f32,
    pub bg_alpha: f32,  // 0..1 for the whole panel
    pub box_alpha: f32, // 0..1 behind each line
    pub text_alpha: f32,
    pub bg: String,
    pub label_color: String,
    pub value_color: String,
    pub warn: String,
    pub crit: String,
}

pub const DEFAULT_ITEMS: [&str; 3] = ["cpu_tctl", "gpu_temp", "gpu_hot"];

fn pct(v: &Value, k: &str, d: f64) -> f32 {
    (v[k].as_f64().unwrap_or(d).clamp(0.0, 100.0) / 100.0) as f32
}

fn text(v: &Value, k: &str, d: &str) -> String {
    v[k].as_str().filter(|s| !s.is_empty()).unwrap_or(d).to_string()
}

impl OsdCfg {
    /// Defaults overlaid with `settings.osd` (any missing or bad field falls back to its default).
    pub fn from_settings(o: &Value) -> Self {
        let items = match o["items"].as_array() {
            Some(a) => a
                .iter()
                .filter_map(|i| {
                    let id = i.as_str().or_else(|| i["id"].as_str())?.to_string();
                    let opt = |k: &str| i[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
                    Some(Item {
                        id,
                        label: opt("label"),
                        color: opt("color"),
                        decimals: i["decimals"].as_u64().map(|d| d.min(4) as u32),
                    })
                })
                .collect(),
            None => DEFAULT_ITEMS
                .iter()
                .map(|id| Item { id: id.to_string(), label: None, color: None, decimals: None })
                .collect(),
        };
        OsdCfg {
            enabled: o["enabled"].as_bool().unwrap_or(false),
            engine: match o["engine"].as_str() {
                Some("rtss") => Engine::Rtss,
                Some("native") => Engine::Native,
                _ => Engine::Auto,
            },
            hotkey: text(o, "hotkey", "Ctrl+Shift+O"),
            interval_ms: o["interval_ms"].as_u64().unwrap_or(1000).clamp(250, 10_000),
            items,
            show_fps: o["show_fps"].as_bool().unwrap_or(true),
            show_frametime: o["show_frametime"].as_bool().unwrap_or(false),
            horizontal: o["layout"].as_str() == Some("horizontal"),
            anchor: match o["anchor"].as_str() {
                Some("top-right") => Anchor::TopRight,
                Some("bottom-left") => Anchor::BottomLeft,
                Some("bottom-right") => Anchor::BottomRight,
                _ => Anchor::TopLeft,
            },
            offset: (o["x"].as_i64().unwrap_or(16) as i32, o["y"].as_i64().unwrap_or(16) as i32),
            font: text(o, "font", "Segoe UI"),
            size: o["size"].as_f64().unwrap_or(15.0).clamp(8.0, 64.0) as f32,
            bg_alpha: pct(o, "bgAlpha", 45.0),
            box_alpha: pct(o, "boxAlpha", 0.0),
            text_alpha: pct(o, "textAlpha", 100.0),
            bg: text(o, "bg", "#000000"),
            label_color: text(o, "label_color", "#b0b6c0"),
            value_color: text(o, "value_color", "#ffffff"),
            warn: text(o, "warn", "#f5b335"),
            crit: text(o, "crit", "#ff5252"),
        }
    }
}

/// Frame-rate sample from RTSS or PresentMon.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fps {
    pub fps: f64,
    pub frametime_ms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub label: String,
    pub value: String,
    pub color: String, // resolved value colour
}

pub fn decimals_for(unit: &str) -> u32 {
    match unit {
        "V" => 3,
        "W" | "A" => 1,
        _ => 0, // °C, RPM, MHz, %: whole numbers read best in a corner of a game
    }
}

pub fn format_value(v: f64, unit: &str, decimals: Option<u32>) -> String {
    let d = decimals.unwrap_or_else(|| decimals_for(unit)) as usize;
    if unit.is_empty() {
        format!("{v:.d$}")
    } else {
        format!("{v:.d$} {unit}")
    }
}

/// The lines to draw: FPS first (when enabled), then each configured sensor that is present in the payload.
pub fn build_lines(payload: &Value, cfg: &OsdCfg, fps: Option<Fps>) -> Vec<Line> {
    let mut out = Vec::new();
    if cfg.show_fps || cfg.show_frametime {
        let mut parts = Vec::new();
        if cfg.show_fps {
            parts.push(fps.map_or("—".to_string(), |f| format!("{:.0}", f.fps)));
        }
        if cfg.show_frametime {
            parts.push(fps.map_or("— ms".to_string(), |f| format!("{:.1} ms", f.frametime_ms)));
        }
        let label = if cfg.show_fps { "FPS" } else { "Frame" };
        out.push(Line { label: label.into(), value: parts.join("  "), color: cfg.value_color.clone() });
    }
    let sensors = payload["sensors"].as_array();
    for it in &cfg.items {
        let Some(s) = sensors.and_then(|a| a.iter().find(|s| s["id"].as_str() == Some(it.id.as_str()))) else {
            continue;
        };
        let Some(cur) = s["cur"].as_f64() else { continue };
        let unit = s["unit"].as_str().unwrap_or("");
        let color = match s["state"].as_str() {
            Some("crit") => cfg.crit.clone(),
            Some("warn") => cfg.warn.clone(),
            _ => it.color.clone().unwrap_or_else(|| cfg.value_color.clone()),
        };
        let label = it.label.clone().unwrap_or_else(|| s["name"].as_str().unwrap_or(&it.id).to_string());
        out.push(Line { label, value: format_value(cur, unit, it.decimals), color });
    }
    out
}

/// "#rrggbb" -> (r, g, b) in 0..1; anything unparsable is white.
pub fn rgb(hex: &str) -> (f32, f32, f32) {
    let h = hex.trim_start_matches('#');
    match (h.len(), u32::from_str_radix(h, 16)) {
        (6, Ok(n)) => (((n >> 16) & 255) as f32 / 255.0, ((n >> 8) & 255) as f32 / 255.0, (n & 255) as f32 / 255.0),
        _ => (1.0, 1.0, 1.0),
    }
}

/// RTSS OSD text: colour tags around labels/values. Values outside cp1252 are dropped by the writer.
pub fn rtss_text(lines: &[Line], cfg: &OsdCfg) -> String {
    let tag = |hex: &str| format!("<C={}>", hex.trim_start_matches('#').to_uppercase());
    let parts: Vec<String> = lines
        .iter()
        .map(|l| format!("{}{}<C> {}{}<C>", tag(&cfg.label_color), l.label, tag(&l.color), l.value))
        .collect();
    parts.join(if cfg.horizontal { "   " } else { "\n" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({"sensors": [
            {"id": "cpu_tctl", "name": "CPU Tctl/Tdie", "unit": "°C", "cur": 71.6, "state": "warn"},
            {"id": "gpu_temp", "name": "GPU Temperature", "unit": "°C", "cur": 45.2, "state": ""},
            {"id": "volt", "name": "Vcore", "unit": "V", "cur": 1.2345, "state": ""},
            {"id": "gone", "name": "Missing", "unit": "°C", "cur": null, "state": ""}
        ]})
    }

    #[test]
    fn defaults_when_settings_missing() {
        let c = OsdCfg::from_settings(&Value::Null);
        assert!(!c.enabled && c.show_fps && c.engine == Engine::Auto && c.anchor == Anchor::TopLeft);
        assert_eq!(c.items.len(), DEFAULT_ITEMS.len());
        assert_eq!(c.interval_ms, 1000);
    }

    #[test]
    fn lines_follow_items_states_and_fps() {
        let cfg = OsdCfg::from_settings(
            &json!({"items": ["volt", {"id": "cpu_tctl", "label": "CPU"}, "gpu_temp", "gone", "nope"],
                                                "show_frametime": true}),
        );
        let l = build_lines(&payload(), &cfg, Some(Fps { fps: 143.6, frametime_ms: 6.96 }));
        assert_eq!(l[0], Line { label: "FPS".into(), value: "144  7.0 ms".into(), color: "#ffffff".into() });
        assert_eq!(l[1].value, "1.234 V"); // f64 1.2345 rounds half-even-ish in binary
        assert_eq!((l[2].label.as_str(), l[2].value.as_str(), l[2].color.as_str()), ("CPU", "72 °C", "#f5b335"));
        assert_eq!(l.len(), 4); // missing value and unknown id are skipped
    }

    #[test]
    fn fps_placeholder_and_rtss_tags() {
        let cfg = OsdCfg::from_settings(&json!({"items": ["gpu_temp"]}));
        let l = build_lines(&payload(), &cfg, None);
        assert_eq!(l[0].value, "—");
        assert_eq!(rtss_text(&l[1..], &cfg), "<C=B0B6C0>GPU Temperature<C> <C=FFFFFF>45 °C<C>");
    }

    #[test]
    fn colours_parse() {
        assert_eq!(rgb("#ff8000"), (1.0, 128.0 / 255.0, 0.0));
        assert_eq!(rgb("bad"), (1.0, 1.0, 1.0));
    }
}
