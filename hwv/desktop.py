"""Desktop widget: pywebview windows (main + detached views/sections) and a pystray tray icon, around the viewer server."""
import argparse
import subprocess
import sys
import threading
import time
from urllib.parse import quote

from . import paths
from .server import App, add_common_args, make_server, make_source
from .storage import Settings

TITLE = "HWiNFO viewer"
MAIN_GEO = {"w": 660, "h": 980}
DETACHED_GEO = {"w": 520, "h": 640}
OPTS = ("frameless", "on_top", "pinned", "opacity", "click_through", "close_to_tray", "start_hidden")


def fatal(msg):
    print(msg, file=sys.stderr)
    if sys.platform == "win32":  # pythonw has no console, so also show a dialog
        try:
            import ctypes
            ctypes.windll.user32.MessageBoxW(0, msg, TITLE, 0x10)
        except Exception:
            pass
    sys.exit(1)


# --------------------------------------------------------------------------- icon (shared by tray + window)
def make_icon_image():
    from PIL import Image, ImageDraw
    img = Image.new("RGBA", (256, 256), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((12, 12, 244, 244), 48, fill=(20, 22, 26, 255), outline=(255, 122, 26, 255), width=12)
    for i, h in enumerate((88, 144, 112)):  # three little bars
        x = 56 + i * 56
        d.rectangle((x, 208 - h, x + 32, 208), fill=(255, 122, 26, 255))
    return img


def ensure_icon_file():
    try:
        if not paths.ICON_FILE.exists():
            make_icon_image().save(paths.ICON_FILE, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
        return True
    except Exception as e:
        print("Could not create icon.ico:", e)
        return False


# --------------------------------------------------------------------------- Win32 window tweaks (no-ops elsewhere)
class Win32:
    ok = sys.platform == "win32"
    GWL_EXSTYLE = -20
    WS_EX_LAYERED, WS_EX_TRANSPARENT = 0x80000, 0x20
    SWP = 0x0001 | 0x0002 | 0x0010  # NOSIZE | NOMOVE | NOACTIVATE
    HWND_TOPMOST, HWND_NOTOPMOST = -1, -2

    def __init__(self):
        if not self.ok:
            return
        import ctypes
        from ctypes import wintypes
        self.ct, self.wt = ctypes, wintypes
        u = self.u = ctypes.windll.user32
        u.FindWindowW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR]
        u.FindWindowW.restype = wintypes.HWND
        u.FindWindowExW.argtypes = [wintypes.HWND, wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR]
        u.FindWindowExW.restype = wintypes.HWND
        u.GetForegroundWindow.restype = wintypes.HWND
        u.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.UINT]
        u.SetLayeredWindowAttributes.argtypes = [wintypes.HWND, wintypes.COLORREF, ctypes.c_ubyte, wintypes.DWORD]
        u.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
        u.LoadImageW.argtypes = [wintypes.HINSTANCE, wintypes.LPCWSTR, wintypes.UINT, ctypes.c_int, ctypes.c_int, wintypes.UINT]
        u.LoadImageW.restype = wintypes.HANDLE
        self.get_ex = getattr(u, "GetWindowLongPtrW", u.GetWindowLongW)
        self.set_ex = getattr(u, "SetWindowLongPtrW", u.SetWindowLongW)
        self.get_ex.argtypes = [wintypes.HWND, ctypes.c_int]
        self.get_ex.restype = ctypes.c_ssize_t
        self.set_ex.argtypes = [wintypes.HWND, ctypes.c_int, ctypes.c_ssize_t]
        self.icons = {}

    def find(self, title):
        return self.u.FindWindowW(None, title) if self.ok else None

    def _desktop(self):
        """The top-level window that hosts the desktop icons (Progman, or a WorkerW when a slideshow wallpaper is active)."""
        found = []

        @self.ct.WINFUNCTYPE(self.wt.BOOL, self.wt.HWND, self.wt.LPARAM)
        def cb(h, _):
            if self.u.FindWindowExW(h, None, "SHELLDLL_DefView", None):
                found.append(h)
                return False
            return True
        self.u.EnumWindows(cb, 0)
        return found[0] if found else self.u.FindWindowW("Progman", None)

    def apply(self, hwnd, o):
        if not (self.ok and hwnd):
            return
        ex = self.get_ex(hwnd, self.GWL_EXSTYLE)
        opacity = float(o.get("opacity", 1.0))
        layered = opacity < 0.999 or o.get("click_through")
        ex = (ex | self.WS_EX_LAYERED) if layered else (ex & ~self.WS_EX_LAYERED)
        ex = (ex | self.WS_EX_TRANSPARENT) if o.get("click_through") else (ex & ~self.WS_EX_TRANSPARENT)
        self.set_ex(hwnd, self.GWL_EXSTYLE, ex)
        if layered:
            self.u.SetLayeredWindowAttributes(hwnd, 0, max(30, min(255, int(255 * opacity))), 2)  # LWA_ALPHA
        self.stack(hwnd, o)

    def stack(self, hwnd, o):
        if o.get("pinned"):
            self.u.SetWindowPos(hwnd, self._desktop() or 1, 0, 0, 0, 0, self.SWP)  # just above the desktop, below everything else
        else:
            self.u.SetWindowPos(hwnd, self.HWND_TOPMOST if o.get("on_top") else self.HWND_NOTOPMOST, 0, 0, 0, 0, self.SWP)

    def foreground(self):
        return self.u.GetForegroundWindow() if self.ok else None

    def set_icon(self, hwnd, path):
        if not (self.ok and hwnd and path.exists()):
            return
        for kind, size in ((0, 16), (1, 32)):  # ICON_SMALL, ICON_BIG
            h = self.u.LoadImageW(None, str(path), 1, size, size, 0x10)  # IMAGE_ICON, LR_LOADFROMFILE
            if h:
                self.icons[(hwnd, kind)] = h
                self.u.SendMessageW(hwnd, 0x80, kind, h)  # WM_SETICON


class Api:
    """Methods the web UI can call via window.pywebview.api (underscore attributes are not exposed)."""

    def __init__(self, ctl, key):
        self._ctl, self._key = ctl, key

    def get_env(self):
        return {"window": self._ctl.opts()}

    def set_window(self, patch):
        return self._ctl.set_opts(patch)

    def resize(self, w, h):
        self._ctl.resize(self._key, int(w), int(h))

    def detach(self, view, section=""):
        self._ctl.detach(str(view), str(section or ""))

    def close_detached(self, key):
        self._ctl.close_window(str(key))

    def hide(self):
        self._ctl.hide()

    def quit(self):
        self._ctl.quit()


class Controller:
    def __init__(self, args):
        try:
            import webview
        except Exception:
            fatal("pywebview is not installed.\n\nRun:  pip install -r requirements.txt")
        self.webview, self.args = webview, args
        self.w32 = Win32()
        self.settings = Settings()
        self.quitting = False
        self.icon = None
        self.wins, self.titles = {}, {}
        self.visible = True
        self._geo_timer, self._pending, self._geo_lock = None, {}, threading.Lock()
        self.has_ico = ensure_icon_file()

        self.app = App(make_source(args), args.config, self.settings)
        try:  # a relaunch (title-bar change) may race the old process releasing the port
            self.srv = make_server(self.app, "127.0.0.1", args.port, retry=8 if args.relaunch else 0)
        except OSError:
            fatal("Port %d is already in use - is the viewer already running? (check the tray icon, or pass --port)" % args.port)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.url = "http://127.0.0.1:%d/" % args.port

    # ---------------------------------------------------------------- options
    def opts(self):
        return dict(self.settings.get()["window"])

    def set_opts(self, patch):
        patch = {k: v for k, v in (patch or {}).items() if k in OPTS}
        if "opacity" in patch:
            patch["opacity"] = max(0.2, min(1.0, float(patch["opacity"])))
        before = self.opts()
        self.settings.patch({"window": patch})
        after = self.opts()
        if after["frameless"] != before["frameless"]:
            self.relaunch()
        else:
            self.apply_all()
            if self.icon:
                self.icon.update_menu()
        return after

    def apply_all(self):
        if not self.w32.ok:
            return
        o = self.opts()
        for key in list(self.wins):
            h = self.w32.find(self.titles.get(key, ""))
            if h:
                self.w32.apply(h, o)
                if self.has_ico:
                    self.w32.set_icon(h, paths.ICON_FILE)

    def pin_watch(self):
        """Keep pinned windows at desktop level (clicking one raises it; push it back once focus moves away)."""
        while not self.quitting:
            time.sleep(1.0)
            try:
                o = self.opts()
                if not o["pinned"]:
                    continue
                fg = self.w32.foreground()
                for key in list(self.wins):
                    h = self.w32.find(self.titles.get(key, ""))
                    if h and h != fg:
                        self.w32.stack(h, o)
            except Exception:
                pass

    # ---------------------------------------------------------------- windows
    def _geo(self, key):
        return dict(self.settings.get().get("geometry", {}).get(key) or {})

    def _onscreen(self, x, y):
        try:
            return any(s.x - 50 <= x < s.x + s.width - 50 and s.y - 50 <= y < s.y + s.height - 50 for s in self.webview.screens)
        except Exception:
            return True

    def create(self, key, url, title, default_geo, hidden=False):
        o = self.opts()
        g = dict(default_geo)
        g.update(self._geo(key))
        x, y = g.get("x"), g.get("y")
        if x is None or y is None or not self._onscreen(x, y):
            x = y = None
        w = self.webview.create_window(
            title, url, js_api=Api(self, key), width=int(g["w"]), height=int(g["h"]), x=x, y=y,
            frameless=bool(o["frameless"]), easy_drag=False, on_top=bool(o["on_top"]), hidden=hidden,
            background_color="#14161a", min_size=(240, 160))
        w.events.closing += lambda *a: self._on_closing(key)
        w.events.moved += lambda x_, y_: self._save_geo(key, x=int(x_), y=int(y_))
        w.events.resized += lambda w_, h_: self._save_geo(key, w=int(w_), h=int(h_))
        w.events.shown += lambda *a: threading.Timer(0.6, self.apply_all).start()
        self.wins[key], self.titles[key] = w, title
        return w

    def detach(self, view, section=""):
        key = view + ("/" + section if section else "")
        if key in self.wins:
            self.wins[key].show()
            return
        url = self.url + "?solo=" + quote(key, safe="/")
        self.create(key, url, "%s - %s" % (TITLE, section or view), DETACHED_GEO)
        self._persist_detached()

    def _persist_detached(self):
        keys = [k for k in self.wins if k != "main"]
        self.settings.replace_key("detached", [{"view": k.split("/", 1)[0], "section": k.split("/", 1)[1] if "/" in k else ""} for k in keys])

    def close_window(self, key):
        w = self.wins.get(key)
        if w:
            w.destroy()

    def resize(self, key, w, h):
        if key in self.wins:
            self.wins[key].resize(max(240, w), max(160, h))

    def hide(self):
        self.visible = False
        self.wins["main"].hide()

    def show(self):
        self.visible = True
        self.wins["main"].show()
        try:
            self.wins["main"].restore()
        except Exception:
            pass

    def toggle(self):
        (self.hide if self.visible else self.show)()

    def _on_closing(self, key):
        if self.quitting:
            return True
        if key == "main":
            if self.opts()["close_to_tray"] and self.icon:
                self.hide()
                return False  # keep running in the tray
            self.quit()
            return True
        self.wins.pop(key, None)  # a detached window the user closed: forget it
        self.titles.pop(key, None)
        self._persist_detached()
        return True

    def _save_geo(self, key, **kw):
        with self._geo_lock:
            self._pending.setdefault(key, {}).update(kw)
            if self._geo_timer:
                self._geo_timer.cancel()
            self._geo_timer = threading.Timer(0.6, self._flush_geo)
            self._geo_timer.daemon = True
            self._geo_timer.start()

    def _flush_geo(self):
        with self._geo_lock:
            p, self._pending = self._pending, {}
        if p:
            try:
                self.settings.patch({"geometry": p})
            except OSError as e:
                print("could not save window geometry:", e)

    # ---------------------------------------------------------------- lifecycle
    def relaunch(self):
        self._flush_geo()
        argv = [a for a in sys.argv[1:] if a != "--relaunch"]
        flags = 0x00000008 | 0x00000200 if sys.platform == "win32" else 0  # DETACHED_PROCESS | NEW_PROCESS_GROUP
        subprocess.Popen([sys.executable, str(paths.ROOT / "widget.py"), "--relaunch"] + argv, creationflags=flags, cwd=str(paths.ROOT))
        self.quit()

    def quit(self):
        if self.quitting:
            return
        self.quitting = True
        self._flush_geo()
        if self.icon:
            try:
                self.icon.stop()
            except Exception:
                pass
        for w in list(self.webview.windows):
            try:
                w.destroy()
            except Exception:
                pass

    # ---------------------------------------------------------------- tray
    def make_tray(self):
        try:
            import pystray
            image = make_icon_image().resize((64, 64))
        except Exception as e:
            print("Tray icon unavailable (%s). Install: pip install pystray pillow" % e)
            return None

        label = {"on_top": "Always on top", "pinned": "Pin to desktop", "click_through": "Click-through (lock)",
                 "close_to_tray": "Close button minimizes to tray", "start_hidden": "Start hidden"}

        def flag(key):
            return pystray.MenuItem(label[key], lambda *_: self.set_opts({key: not self.opts()[key]}), checked=lambda *_: bool(self.opts()[key]))

        def opacity(p):
            return pystray.MenuItem("%d%%" % p, lambda *_: self.set_opts({"opacity": p / 100}),
                                    checked=lambda *_: round(self.opts()["opacity"] * 100) == p, radio=True)
        menu = pystray.Menu(
            pystray.MenuItem("Show / hide", lambda *_: self.toggle(), default=True),
            pystray.Menu.SEPARATOR,
            flag("on_top"), flag("pinned"), flag("click_through"), flag("close_to_tray"), flag("start_hidden"),
            pystray.MenuItem("Opacity", pystray.Menu(*[opacity(p) for p in (100, 90, 80, 70, 60, 50, 30)])),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem("Quit", lambda *_: self.quit()),
        )
        return pystray.Icon("hwviewer", image, TITLE, menu)

    def watch(self):
        """Tray notification when HWiNFO data is lost for longer than the reconnect grace window."""
        was_ok = None
        while not self.quitting:
            try:
                p = self.app.snapshot()
            except Exception:
                p = {"ok": False, "error": "viewer error"}
            if was_ok and not p.get("ok") and self.icon:
                try:
                    self.icon.notify(p.get("error", "HWiNFO stopped updating"), TITLE)
                except Exception:
                    pass
            was_ok = p.get("ok")
            time.sleep(5)

    def run(self):
        self.icon = self.make_tray()
        o = self.opts()
        hidden = bool(o["start_hidden"] and self.icon)
        self.visible = not hidden
        self.create("main", self.url, TITLE, MAIN_GEO, hidden=hidden)
        for d in self.settings.get().get("detached", []):
            try:
                self.detach(d.get("view", "full"), d.get("section", ""))
            except Exception as e:
                print("could not restore detached window:", e)
        if self.icon:
            self.icon.run_detached()
        for fn in (self.watch, self.pin_watch):
            threading.Thread(target=fn, daemon=True).start()
        try:
            self.webview.start(icon=str(paths.ICON_FILE) if self.has_ico else None)  # icon= is honoured on Qt/GTK; Windows uses WM_SETICON in apply_all()
        except TypeError:
            self.webview.start()
        self.quitting = True
        self._flush_geo()
        if self.icon:
            try:
                self.icon.stop()
            except Exception:
                pass


def main(argv=None, doc=None):
    ap = argparse.ArgumentParser(description=doc, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_common_args(ap)
    ap.add_argument("--relaunch", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    paths.migrate_legacy()
    Controller(a).run()
