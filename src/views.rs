//! Turn readings + the views.json catalog into the JSON payload the UI renders.
//!
//! Catalog item keys: name, sensor/label/unit (case-insensitive regexes), warn/crit (°C thresholds),
//! group (collapse all matches into one "hottest" row), expand (one row per matching drive),
//! nest/nest_regex/primary (how expanded rows are grouped).

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};
use serde_json::{json, Map, Value};

use crate::shm::{now_epoch, Reading};

/// Compiled-pattern cache (catalogs are matched every poll).
#[derive(Default)]
pub struct RxCache {
    map: HashMap<String, Result<Regex, String>>,
}

impl RxCache {
    /// None for an empty/absent pattern (matches everything); Err for an invalid one.
    pub fn get(&mut self, pat: Option<&str>) -> Result<Option<Regex>, String> {
        let Some(p) = pat.filter(|p| !p.is_empty()) else { return Ok(None) };
        self.map
            .entry(p.to_string())
            .or_insert_with(|| RegexBuilder::new(p).case_insensitive(true).build().map_err(|e| e.to_string()))
            .clone()
            .map(Some)
    }
}

fn text<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn truthy(v: Option<&Value>, default: bool) -> bool {
    match v {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, |v| json!(v)) // NaN/inf become null
}

pub fn find<'a>(item: &Value, readings: &'a [Reading], rx: &mut RxCache) -> Result<Vec<&'a Reading>, String> {
    let sre = rx.get(text(item, "sensor"))?;
    let lre = rx.get(text(item, "label"))?;
    let ure = rx.get(text(item, "unit"))?;
    Ok(readings
        .iter()
        .filter(|r| sre.as_ref().is_none_or(|re| re.is_match(&r.sensor)))
        .filter(|r| lre.as_ref().is_none_or(|re| re.is_match(&r.label) || re.is_match(&r.label_orig)))
        .filter(|r| ure.as_ref().is_none_or(|re| re.is_match(&r.unit)))
        .collect())
}

pub fn state_of(item: &Value, value: Option<f64>, unit: &str) -> &'static str {
    let Some(v) = value else { return "" };
    if !unit.ends_with('C') {
        return "";
    }
    let at = |k| item.get(k).and_then(Value::as_f64);
    if at("crit").is_some_and(|c| v >= c) {
        "crit"
    } else if at("warn").is_some_and(|w| v >= w) {
        "warn"
    } else {
        ""
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    item: &Value,
    rid: &str,
    name: &str,
    unit: &str,
    cur: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    avg: Option<f64>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(rid));
    m.insert("name".into(), json!(name));
    m.insert("unit".into(), json!(unit));
    m.insert("found".into(), json!(cur.is_some()));
    m.insert("cur".into(), num(cur));
    m.insert("min".into(), num(min));
    m.insert("max".into(), num(max));
    m.insert("avg".into(), num(avg));
    m.insert("state".into(), json!(state_of(item, cur, unit)));
    m
}

fn missing_row(item: &Value, rid: &str, name: &str) -> Map<String, Value> {
    row(item, rid, name, "", None, None, None, None)
}

fn child(item: &Value, rid: &str, name: &str, r: &Reading) -> Value {
    json!({"id": rid, "name": name, "unit": r.unit, "cur": num(Some(r.cur)), "min": num(Some(r.min)),
           "max": num(Some(r.max)), "avg": num(Some(r.avg)), "state": state_of(item, Some(r.cur), &r.unit)})
}

fn static_rx(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static pattern"))
}

pub fn short_drive(sensor: &str) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static SERIAL: OnceLock<Regex> = OnceLock::new();
    let s = static_rx(&PREFIX, r"^(S\.M\.A\.R\.T\.|Drive):\s*").replace(sensor, "");
    static_rx(&SERIAL, r"\s*\([^)]*\)").replace_all(&s, "").into_owned() // drop the serial number
}

/// Readings that belong to the same physical drive: same assigned letter, else same name sans serial.
fn nest_key(item: &Value, r: &Reading) -> Result<String, String> {
    static LETTER: OnceLock<Regex> = OnceLock::new();
    if let Some(c) = static_rx(&LETTER, r"\[([A-Za-z]:[^\]]*)\]").captures(&r.sensor) {
        return Ok(c[1].trim().to_string());
    }
    match text(item, "nest_regex") {
        // case-sensitive, like the original re.sub
        Some(p) => Regex::new(p).map(|re| re.replace_all(&r.sensor, "").trim().to_string()).map_err(|e| e.to_string()),
        None => Ok(short_drive(&r.sensor)),
    }
}

pub fn build_rows(key: &str, item: &Value, readings: &[Reading], rx: &mut RxCache) -> Result<Vec<Value>, String> {
    let hits = find(item, readings, rx)?;
    let name = match item.get("name") {
        Some(Value::String(s)) => s.clone(),
        _ => item.get("label").and_then(Value::as_str).unwrap_or("?").to_string(),
    };
    if hits.is_empty() {
        return Ok(vec![Value::Object(missing_row(item, key, &name))]);
    }
    if truthy(item.get("expand"), false) {
        let nest = truthy(item.get("nest"), true);
        let mut groups: Vec<(String, Vec<&Reading>)> = Vec::new();
        for (i, r) in hits.iter().enumerate() {
            let gk = if nest { nest_key(item, r)? } else { i.to_string() };
            match groups.iter_mut().find(|(k, _)| *k == gk) {
                Some((_, v)) => v.push(r),
                None => groups.push((gk, vec![r])),
            }
        }
        let prim = rx.get(text(item, "primary"))?;
        let mut rows = Vec::new();
        for (gk, rs) in groups {
            let p = rs.iter().find(|r| prim.as_ref().is_some_and(|re| re.is_match(&r.label))).unwrap_or(&rs[0]);
            let rid = format!("{key}:{gk}");
            let mut m =
                row(item, &rid, &short_drive(&p.sensor), &p.unit, Some(p.cur), Some(p.min), Some(p.max), Some(p.avg));
            if rs.len() > 1 {
                let kids: Vec<Value> =
                    rs.iter().enumerate().map(|(i, r)| child(item, &format!("{rid}:{i}"), &r.label, r)).collect();
                m.insert("children".into(), Value::Array(kids));
            }
            rows.push(Value::Object(m));
        }
        return Ok(rows);
    }
    if truthy(item.get("group"), false) {
        // hottest member for current/max, coldest for min, mean for avg; expand in the UI to see each member
        let kids: Vec<Value> =
            hits.iter().enumerate().map(|(i, r)| child(item, &format!("{key}:{i}"), &r.label, r)).collect();
        let hot = hits.iter().fold(hits[0], |a, b| if b.cur > a.cur { b } else { a });
        let mn = hits.iter().map(|r| r.min).fold(f64::INFINITY, f64::min);
        let mx = hits.iter().map(|r| r.max).fold(f64::NEG_INFINITY, f64::max);
        let avg = hits.iter().map(|r| r.avg).sum::<f64>() / hits.len() as f64;
        let mut m = row(item, key, &name, &hits[0].unit, Some(hot.cur), Some(mn), Some(mx), Some(avg));
        m.insert("children".into(), Value::Array(kids));
        m.insert("hottest".into(), json!(hot.label));
        return Ok(vec![Value::Object(m)]);
    }
    let r = hits[0];
    Ok(vec![Value::Object(row(item, key, &name, &r.unit, Some(r.cur), Some(r.min), Some(r.max), Some(r.avg)))])
}

pub fn build_payload(cfg: &Value, readings: &[Reading], rx: &mut RxCache) -> Value {
    let empty = Map::new();
    let cat = cfg.get("catalog").and_then(Value::as_object).unwrap_or(&empty);
    let mut by_key: Vec<(String, Vec<Value>)> = Vec::with_capacity(cat.len());
    for (k, it) in cat {
        let rows = build_rows(k, it, readings, rx).unwrap_or_else(|e| {
            // a bad regex in views.json breaks only its own row
            let nm = it.get("name").and_then(Value::as_str).unwrap_or(k);
            vec![Value::Object(missing_row(&Value::Null, k, &format!("{nm} (bad pattern: {e})")))]
        });
        by_key.push((k.clone(), rows));
    }
    let rows_of = |k: &str| by_key.iter().find(|(key, _)| key == k).map(|(_, r)| r);

    let mut section_of: HashMap<String, String> = HashMap::new();
    let mut views = Vec::new();
    for v in cfg.get("views").and_then(Value::as_array).into_iter().flatten() {
        let mut sections = Vec::new();
        for sec in v.get("sections").and_then(Value::as_array).into_iter().flatten() {
            let title = sec.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let keys: Vec<&str> =
                sec.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            let (mut rows, mut missing) = (Vec::new(), Vec::new());
            for key in &keys {
                let unknown;
                let list: &[Value] = match rows_of(key) {
                    Some(r) => r,
                    None => {
                        unknown = vec![Value::Object(missing_row(&Value::Null, key, &format!("unknown item: {key}")))];
                        &unknown
                    }
                };
                for rw in list {
                    if rw.get("found") != Some(&Value::Bool(true)) {
                        missing.push(rw.get("name").cloned().unwrap_or(Value::Null));
                    }
                    rows.push(rw.clone());
                }
                section_of.entry(key.to_string()).or_insert_with(|| title.clone());
            }
            let items: Vec<Value> = keys
                .iter()
                .filter_map(|k| {
                    cat.get(*k).map(|it| json!({"key": k, "name": it.get("name").and_then(Value::as_str).unwrap_or(k)}))
                })
                .collect();
            sections.push(json!({"title": title, "rows": rows, "missing": missing, "items": items}));
        }
        views.push(json!({
            "id": v.get("id").cloned().unwrap_or(Value::Null),
            "title": v.get("title").cloned().unwrap_or(Value::Null),
            "columns": v.get("columns").cloned().unwrap_or_else(|| json!(["cur"])),
            "density": v.get("density").cloned().unwrap_or_else(|| json!("")),
            "width": v.get("width").cloned().unwrap_or_else(|| json!(0)),
            "align": v.get("align").cloned().unwrap_or_else(|| json!("center")),
            "sections": sections,
        }));
    }
    // flat list for the Builder's sensor picker (every catalog row, including nested children)
    let mut sensors = Vec::new();
    for (key, rows) in &by_key {
        let sec = section_of.get(key).cloned().unwrap_or_else(|| "Other".into());
        for rw in rows {
            if rw.get("found") != Some(&Value::Bool(true)) {
                continue;
            }
            let mut s = Map::new();
            for k in ["id", "name", "unit", "cur", "min", "max", "avg", "state"] {
                s.insert(k.into(), rw.get(k).cloned().unwrap_or(Value::Null));
            }
            s.insert("section".into(), json!(sec));
            sensors.push(Value::Object(s));
            let parent = rw.get("name").and_then(Value::as_str).unwrap_or("");
            for c in rw.get("children").and_then(Value::as_array).into_iter().flatten() {
                let mut s = Map::new();
                for k in ["id", "unit", "cur", "min", "max", "avg", "state"] {
                    s.insert(k.into(), c.get(k).cloned().unwrap_or(Value::Null));
                }
                let cn = c.get("name").and_then(Value::as_str).unwrap_or("");
                s.insert("name".into(), json!(format!("{parent} › {cn}")));
                s.insert("section".into(), json!(sec));
                sensors.push(Value::Object(s));
            }
        }
    }
    json!({"ok": true, "ts": now_epoch(), "views": views, "sensors": sensors})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd(sensor: &str, label: &str, unit: &str, cur: f64) -> Reading {
        Reading {
            sensor: sensor.into(),
            label: label.into(),
            label_orig: label.into(),
            unit: unit.into(),
            typ: 1,
            cur,
            min: cur - 1.0,
            max: cur + 1.0,
            avg: cur,
        }
    }

    #[test]
    fn group_takes_hottest() {
        let rs = vec![
            rd("AMD Ryzen 7 5800X: Enhanced", "Core 0 (Tdie)", "°C", 40.0),
            rd("AMD Ryzen 7 5800X: Enhanced", "Core 1 (Tdie)", "°C", 72.0),
        ];
        let item = json!({"name": "Cores", "sensor": "ryzen.*enhanced", "label": "^Core \\d+", "group": true, "warn": 70, "crit": 85});
        let rows = build_rows("c", &item, &rs, &mut RxCache::default()).unwrap();
        assert_eq!(rows[0]["cur"], json!(72.0));
        assert_eq!(rows[0]["state"], "warn");
        assert_eq!(rows[0]["hottest"], "Core 1 (Tdie)");
        assert_eq!(rows[0]["children"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn expand_nests_drives_by_letter() {
        let s = "S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]";
        let rs = vec![
            rd(s, "Drive Temperature", "°C", 42.0),
            rd(s, "Drive Temperature 2", "°C", 48.0),
            rd("S.M.A.R.T.: X (1) [C:]", "Drive Temperature", "°C", 30.0),
        ];
        let item = json!({"sensor": "^S\\.M\\.A\\.R\\.T\\.", "label": "^Drive Temperature", "expand": true, "primary": "^Drive Temperature$"});
        let rows = build_rows("drives", &item, &rs, &mut RxCache::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "XPG GAMMIX S70 BLADE [D:]");
        assert_eq!(rows[0]["id"], "drives:D:");
        assert_eq!(rows[0]["cur"], json!(42.0));
    }

    #[test]
    fn bad_pattern_breaks_only_its_row() {
        let cfg = json!({"catalog": {"bad": {"name": "Bad", "label": "("}, "ok": {"name": "Ok", "label": "^Temp$"}},
                         "views": [{"id": "v", "title": "V", "sections": [{"title": "S", "items": ["bad", "ok", "nope"]}]}]});
        let p = build_payload(&cfg, &[rd("A", "Temp", "°C", 1.0)], &mut RxCache::default());
        let rows = &p["views"][0]["sections"][0]["rows"];
        assert!(rows[0]["name"].as_str().unwrap().contains("bad pattern"));
        assert_eq!(rows[1]["found"], true);
        assert_eq!(rows[2]["name"], "unknown item: nope");
        assert_eq!(p["sensors"].as_array().map(Vec::len), Some(1));
    }
}
