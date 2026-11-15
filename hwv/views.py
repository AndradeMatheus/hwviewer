"""Turn raw readings + the views.json catalog into the JSON payload the UI renders.

Catalog item keys: name, sensor/label/unit (case-insensitive regexes), warn/crit (°C thresholds),
group (collapse all matches into one "hottest" row), expand (one row per matching drive),
nest/nest_regex/primary (how expanded rows are grouped).
"""
import functools
import re
import time


@functools.lru_cache(maxsize=1024)
def _rx(p):
    return re.compile(p, re.I) if p else None


def find(item, readings):
    sre, lre, ure = _rx(item.get("sensor")), _rx(item.get("label")), _rx(item.get("unit"))
    hits = []
    for r in readings:
        if sre and not sre.search(r.sensor):
            continue
        if lre and not (lre.search(r.label) or lre.search(r.label_orig)):
            continue
        if ure and not ure.search(r.unit):
            continue
        hits.append(r)
    return hits


def state_of(item, value, unit):
    if value is None or not unit.endswith("C"):
        return ""
    if "crit" in item and value >= item["crit"]:
        return "crit"
    if "warn" in item and value >= item["warn"]:
        return "warn"
    return ""


def row(item, rid, name, unit, r_cur, r_min, r_max, r_avg, **extra):
    d = {"id": rid, "name": name, "unit": unit, "found": r_cur is not None,
         "cur": r_cur, "min": r_min, "max": r_max, "avg": r_avg, "state": state_of(item, r_cur, unit or "")}
    d.update(extra)
    return d


def child(item, rid, name, r):
    return {"id": rid, "name": name, "unit": r.unit, "cur": r.cur, "min": r.min, "max": r.max, "avg": r.avg,
            "state": state_of(item, r.cur, r.unit)}


def short_drive(sensor):
    s = re.sub(r"^(S\.M\.A\.R\.T\.|Drive):\s*", "", sensor)
    return re.sub(r"\s*\([^)]*\)", "", s)  # drop the serial number


def nest_key(item, r):
    """Readings that belong to the same physical drive: same assigned letter, else same name sans serial."""
    m = re.search(r"\[([A-Za-z]:[^\]]*)\]", r.sensor)
    if m:
        return m.group(1).strip()
    rx = item.get("nest_regex")
    return re.sub(rx, "", r.sensor).strip() if rx else short_drive(r.sensor)


def build_rows(key, item, readings):
    hits = find(item, readings)
    name = item.get("name", item.get("label", "?"))
    if not hits:
        return [row(item, key, name, "", None, None, None, None)]
    if item.get("expand"):
        groups = {}
        for r in hits:
            groups.setdefault(nest_key(item, r) if item.get("nest", True) else id(r), []).append(r)
        rows = []
        prim = _rx(item.get("primary"))
        for gk, rs in groups.items():
            p = next((r for r in rs if prim and prim.search(r.label)), rs[0])
            rid = "%s:%s" % (key, gk)
            kids = [child(item, "%s:%d" % (rid, i), r.label, r) for i, r in enumerate(rs)] if len(rs) > 1 else None
            extra = {"children": kids} if kids else {}
            rows.append(row(item, rid, short_drive(p.sensor), p.unit, p.cur, p.min, p.max, p.avg, **extra))
        return rows
    if item.get("group"):
        # hottest member for current/max, coldest for min, mean for avg; expand in the UI to see each member
        kids = [child(item, "%s:%d" % (key, i), r.label, r) for i, r in enumerate(hits)]
        hot = max(hits, key=lambda r: r.cur)
        return [row(item, key, name, hits[0].unit, hot.cur, min(r.min for r in hits), max(r.max for r in hits),
                    sum(r.avg for r in hits) / len(hits), children=kids, hottest=hot.label)]
    r = hits[0]
    return [row(item, key, name, r.unit, r.cur, r.min, r.max, r.avg)]


def build_payload(cfg, readings):
    cat = cfg["catalog"]
    by_key = {}
    for k, it in cat.items():
        try:
            by_key[k] = build_rows(k, it, readings)
        except re.error as e:  # a bad regex in views.json breaks only its own row
            by_key[k] = [row({}, k, "%s (bad pattern: %s)" % (it.get("name", k), e), "", None, None, None, None)]
    section_of = {}
    views = []
    for v in cfg["views"]:
        sections = []
        for sec in v["sections"]:
            rows, missing = [], []
            for key in sec["items"]:
                for rw in by_key.get(key) or [row({}, key, "unknown item: " + key, "", None, None, None, None)]:
                    rows.append(rw)
                    if not rw["found"]:
                        missing.append(rw["name"])
                    section_of.setdefault(key, sec["title"])
            items = [{"key": k, "name": cat[k].get("name", k)} for k in sec["items"] if k in cat]
            sections.append({"title": sec["title"], "rows": rows, "missing": missing, "items": items})
        views.append({"id": v["id"], "title": v["title"], "columns": v.get("columns", ["cur"]),
                      "density": v.get("density", ""), "width": v.get("width", 0), "align": v.get("align", "center"),
                      "sections": sections})
    # flat list for the Builder's sensor picker (every catalog row, including nested children)
    sensors = []
    for key, rows in by_key.items():
        sec = section_of.get(key, "Other")
        for rw in rows:
            if not rw["found"]:
                continue
            sensors.append(dict({k: rw[k] for k in ("id", "name", "unit", "cur", "min", "max", "avg", "state")}, section=sec))
            for c in rw.get("children") or []:
                sensors.append({k: c[k] for k in ("id", "unit", "cur", "min", "max", "avg", "state")} |
                               {"name": "%s › %s" % (rw["name"], c["name"]), "section": sec})
    return {"ok": True, "ts": time.time(), "views": views, "sensors": sensors}
