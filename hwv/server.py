"""The viewer: App (data + config state) and a small stdlib HTTP server for the web UI and its JSON API."""
import argparse
import copy
import json
import mimetypes
import re
import threading
import time
import traceback
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from . import paths
from .shm import MockSource, SharedMemorySource, SMError
from .storage import IMAGE_EXT, MAX_IMAGE, Profiles, Settings, read_json, safe_name, save_image
from .views import build_payload

CACHE_FOR = 0.4        # seconds a snapshot is reused (several windows poll at 1 Hz)
GRACE = 30.0           # keep showing the last good data (marked "reconnecting") this long after HWiNFO drops
RESTART_GRACE = 120.0  # ... and this long while the watchdog reports it is restarting HWiNFO
WATCHDOG_FRESH = 150.0  # a status file older than this means the watchdog isn't running


class WatchdogStatus:
    """Reads data/watchdog_status.json (written by the watchdog process) when it changes."""

    def __init__(self, path=None):
        self.path, self.mtime, self.data = Path(path or paths.WATCHDOG_STATUS), None, None

    def get(self):
        try:
            m = self.path.stat().st_mtime
        except OSError:
            return None
        if m != self.mtime:
            self.mtime, self.data = m, read_json(self.path, None) if self.path.exists() else None
        d = self.data
        if not isinstance(d, dict) or time.time() - d.get("updated", 0) > WATCHDOG_FRESH:
            return None
        return {k: d.get(k) for k in ("state", "message", "restart_at", "last_restart", "restarts", "hwinfo_started")}


class App:
    def __init__(self, source, cfg_path=None, settings=None):
        self.source, self.cfg_path = source, Path(cfg_path or paths.VIEWS_FILE)
        self.settings = settings or Settings()
        self.profiles, self.layouts = Profiles(paths.PROFILES), Profiles(paths.LAYOUTS)
        self.watchdog = WatchdogStatus()
        self.lock = threading.Lock()
        self.cfg, self.cfg_mtime = None, 0
        self.last_readings = []
        self.layout_cache = (None, None, None)  # name, mtime, data
        self.cache, self.cache_t = None, 0.0
        self.good, self.good_t = None, 0.0  # last successful payload, for the reconnect grace window
        self.since = None  # when the current unbroken run of successful reads began

    def invalidate(self):
        self.cache_t = 0.0

    def config(self):
        m = self.cfg_path.stat().st_mtime
        if m != self.cfg_mtime:
            try:
                cfg = json.loads(self.cfg_path.read_text(encoding="utf-8"))
                if not isinstance(cfg.get("catalog"), dict) or not isinstance(cfg.get("views"), list):
                    raise ValueError("views.json needs a 'catalog' object and a 'views' list")
                self.cfg, self.cfg_mtime = cfg, m
                self.invalidate()
            except ValueError as e:
                if self.cfg is None:
                    raise
                print("views.json has an error, keeping the previous version:", e)
                self.cfg_mtime = m
        return self.cfg

    def active_layout(self):
        name = self.settings.get().get("active_layout")
        f = self.layouts.path(name) if name else None
        if not f or not f.exists():
            return None, None
        m = f.stat().st_mtime
        if self.layout_cache[:2] != (name, m):
            try:
                self.layout_cache = (name, m, json.loads(f.read_text("utf-8")))
            except (OSError, ValueError):
                return None, None
        return name, self.layout_cache[2]

    def effective(self):
        """Factory views.json, overlaid with the active sensor profile (if any)."""
        base = self.config()
        name, lay = self.active_layout()
        if not lay:
            return base, None
        views = []
        for v in base["views"]:
            ov = (lay.get("views") or {}).get(v["id"])
            views.append(dict(v, sections=ov["sections"]) if ov and ov.get("sections") is not None else v)
        return {"catalog": dict(base["catalog"], **(lay.get("catalog_extra") or {})), "views": views}, name

    def edit_layout(self, body):
        """Apply a view edit (order/add/remove) to the active profile, forking 'Custom' from Default if needed."""
        view, sections, add = body.get("view"), body.get("sections"), body.get("add") or {}
        base = self.config()
        if view not in [v["id"] for v in base["views"]] or not isinstance(sections, list) or not isinstance(add, dict):
            raise ValueError("bad edit")
        for it in add.values():
            for fld in ("sensor", "label", "unit"):
                if it.get(fld):
                    re.compile(it[fld])  # reject bad patterns early
        name, lay = self.active_layout()
        lay = copy.deepcopy(lay)  # never mutate the cached copy (a failed save would leave it wrong)
        if not name:
            n, i = "Custom", 1
            while self.layouts.path(n).exists():
                i += 1
                n = "Custom %d" % i
            name, lay = n, {"views": {}, "catalog_extra": {}}
        lay.setdefault("views", {})
        lay.setdefault("catalog_extra", {}).update(add)
        known = set(base["catalog"]) | set(lay["catalog_extra"])
        lay["views"][view] = {"sections": [
            {"title": str(s.get("title", "?"))[:40], "items": [k for k in s.get("items", []) if k in known]} for s in sections]}
        used = {k for v in lay["views"].values() for s in v["sections"] for k in s["items"]}
        used |= {k for v in base["views"] if v["id"] not in lay["views"] for s in v["sections"] for k in s["items"]}
        lay["catalog_extra"] = {k: v for k, v in lay["catalog_extra"].items() if k in used}
        self.layouts.save(name, lay)
        self.settings.patch({"active_layout": name})
        self.invalidate()
        return name

    def snapshot(self):
        with self.lock:
            now = time.monotonic()
            if self.cache is not None and now - self.cache_t < CACHE_FOR:
                return self.cache
            wd = self.watchdog.get()
            try:
                cfg, layout_name = self.effective()
                _poll, readings = self.source.read()
                self.last_readings = readings
                payload = build_payload(cfg, readings)
                payload["layout"] = layout_name
                self.since = self.since or now
                payload["uptime"] = now - self.since
                self.good, self.good_t = payload, now
            except SMError as e:
                self.since = None  # the next successful read starts a new run (e.g. a restarted HWiNFO)
                grace = RESTART_GRACE if wd and wd.get("state") == "restarting" else GRACE
                if self.good is not None and now - self.good_t < grace:
                    payload = dict(self.good, reconnecting=True, error=str(e), stale_for=now - self.good_t)
                else:
                    payload = {"ok": False, "error": str(e), "kind": e.kind, "ts": time.time()}
            except Exception as e:  # never let one bad poll take the server down
                traceback.print_exc()
                payload = {"ok": False, "error": "viewer error: %s" % e, "kind": "internal", "ts": time.time()}
            payload["rev"] = self.settings.rev
            payload["watchdog"] = wd
            self.cache, self.cache_t = payload, now
            return payload


def make_handler(app, port, lan):
    ok_hosts = {"127.0.0.1:%d" % port, "localhost:%d" % port}

    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def _send(self, code, body, ctype="application/json"):
            if isinstance(body, (dict, list)):
                body = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            self.wfile.write(body)

        def _guard(self, post=False):
            # blocks DNS-rebinding (Host check) and cross-site POSTs (custom header forces a CORS preflight)
            if not lan and self.headers.get("Host", "") not in ok_hosts:
                self._send(403, {"error": "bad host"})
                return False
            if post and self.headers.get("X-HWV") != "1":
                self._send(403, {"error": "missing X-HWV header"})
                return False
            if post and lan and self.client_address[0] not in ("127.0.0.1", "::1"):
                self._send(403, {"error": "read-only over the network - change settings on the PC itself"})
                return False
            return True

        def _body(self, limit=1 << 20):
            n = int(self.headers.get("Content-Length") or 0)
            if n < 0 or n > limit:
                raise ValueError("bad body size")
            return self.rfile.read(n)

        def do_GET(self):
            if not self._guard():
                return
            u = urlparse(self.path)
            q = parse_qs(u.query)
            if u.path == "/api/data":
                self._send(200, app.snapshot())
            elif u.path == "/api/settings":
                self._send(200, app.settings.get())
            elif u.path == "/api/profiles":
                self._send(200, {"names": app.profiles.names()})
            elif u.path == "/api/layouts":
                self._send(200, {"names": app.layouts.names(), "active": app.active_layout()[0]})
            elif u.path == "/api/readings":
                app.snapshot()
                seen, out = set(), []
                for r in app.last_readings:
                    k = (r.sensor, r.label, r.unit)
                    if k not in seen:
                        seen.add(k)
                        out.append({"sensor": r.sensor, "label": r.label, "unit": r.unit, "cur": r.cur})
                self._send(200, out)
            elif u.path == "/api/profile":
                try:
                    data = app.profiles.load((q.get("name") or [""])[0])
                    self._send(200 if data is not None else 404, data if data is not None else {"error": "not found"})
                except (OSError, ValueError):
                    self._send(404, {"error": "not found"})
            elif u.path.startswith("/assets/"):
                f = paths.ASSETS / Path(u.path).name
                if f.suffix.lower() in IMAGE_EXT and f.is_file():
                    self._send(200, f.read_bytes(), mimetypes.guess_type(f.name)[0] or "application/octet-stream")
                else:
                    self._send(404, b"not found", "text/plain")
            elif u.path in ("/", "/index.html"):
                self._send(200, paths.INDEX_HTML.read_bytes(), "text/html; charset=utf-8")
            else:
                self._send(404, b"not found", "text/plain")

        def do_POST(self):
            if not self._guard(post=True):
                return
            u = urlparse(self.path)
            try:
                if u.path == "/api/image":
                    name = (parse_qs(u.query).get("name") or [""])[0]
                    self._send(200, {"file": save_image(name, self._body(MAX_IMAGE))})
                    return
                body = json.loads(self._body() or b"{}")
                if not isinstance(body, dict):
                    raise ValueError("expected a JSON object")
                if u.path == "/api/settings":
                    self._send(200, app.settings.patch(body))
                elif u.path == "/api/profile":
                    self._send(200, {"name": app.profiles.save(body.get("name"), body.get("data"))})
                elif u.path == "/api/profile/delete":
                    app.profiles.delete(body.get("name"))
                    self._send(200, {"ok": True})
                elif u.path == "/api/layout/select":     # name=None -> back to the original views.json
                    n = safe_name(body.get("name")) if body.get("name") else None
                    if n and n not in app.layouts.names():
                        raise ValueError("no such profile")
                    app.settings.patch({"active_layout": n})
                    app.invalidate()
                    self._send(200, {"active": n})
                elif u.path == "/api/layout/edit":
                    self._send(200, {"active": app.edit_layout(body)})
                elif u.path == "/api/layout/save_as":    # copy the active profile (or Default) under a new name
                    _n, lay = app.active_layout()
                    n = app.layouts.save(body.get("name"), lay or {"views": {}, "catalog_extra": {}})
                    app.settings.patch({"active_layout": n})
                    app.invalidate()
                    self._send(200, {"active": n})
                elif u.path == "/api/layout/delete":
                    n = safe_name(body.get("name"))
                    if n:
                        app.layouts.delete(n)
                        if app.settings.get().get("active_layout") == n:
                            app.settings.patch({"active_layout": None})
                    app.invalidate()
                    self._send(200, {"ok": True})
                elif u.path == "/api/settings/reset_key":  # e.g. {"key": "theme"} -> back to defaults
                    k = body.get("key")
                    if k not in ("theme", "ui", "window"):
                        raise ValueError("bad key")
                    self._send(200, app.settings.replace_key(k, copy.deepcopy(Settings.DEFAULTS[k])))
                else:
                    self._send(404, {"error": "not found"})
            except (ValueError, OSError, re.error, KeyError, TypeError, AttributeError) as e:
                self._send(400, {"error": str(e)})

    return H


def make_server(app, host, port, retry=0.0):
    """Create the HTTP server; keep retrying the bind for `retry` seconds (used when relaunching)."""
    end = time.monotonic() + retry
    while True:
        try:
            return ThreadingHTTPServer((host, port), make_handler(app, port, host == "0.0.0.0"))
        except OSError:
            if time.monotonic() >= end:
                raise
            time.sleep(0.3)


def make_source(args):
    return MockSource(args.mock_outage) if args.mock else SharedMemorySource()


def add_common_args(ap):
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--config", default=str(paths.VIEWS_FILE), help="views.json to use")
    ap.add_argument("--mock", action="store_true", help="fake data instead of HWiNFO")
    ap.add_argument("--mock-outage", type=float, default=0, metavar="SECONDS",
                    help="with --mock: simulate a 4 s HWiNFO restart every SECONDS")


def dump(source, path):
    _poll, readings = source.read()
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write("sensor | label | unit | type | current\n")
        for r in readings:
            f.write("%s | %s | %s | %s | %s\n" % (r.sensor, r.label, r.unit, r.type, r.cur))
    print("wrote %d readings to %s" % (len(readings), path))


def main(argv=None, doc=None):
    ap = argparse.ArgumentParser(description=doc, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_common_args(ap)
    ap.add_argument("--host", default="127.0.0.1", help="use 0.0.0.0 to view from another device on your LAN (read-only there)")
    ap.add_argument("--dump", action="store_true", help="write all HWiNFO readings to data/dump.txt and exit")
    ap.add_argument("--open", action="store_true", help="open the browser")
    a = ap.parse_args(argv)
    paths.migrate_legacy()

    source = make_source(a)
    if a.dump:
        try:
            dump(source, paths.DUMP_FILE)
        except SMError as e:
            raise SystemExit(str(e))
        return
    app = App(source, a.config)
    try:
        srv = make_server(app, a.host, a.port)
    except OSError:
        raise SystemExit("Port %d is already in use - is the viewer (or the widget) already running?" % a.port)
    url = "http://%s:%d/" % ("127.0.0.1" if a.host == "0.0.0.0" else a.host, a.port)
    print("HWiNFO viewer running at", url, "(Ctrl+C to stop)" + ("  [MOCK DATA]" if a.mock else ""))
    if a.open:
        webbrowser.open(url)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
