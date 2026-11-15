"""Persistent state: settings.json, profile folders, uploaded images, and a crash-safe file writer."""
import copy
import json
import os
import re
import secrets
import tempfile
import threading
import time
from pathlib import Path

from . import paths

IMAGE_EXT = {".png", ".jpg", ".jpeg", ".webp", ".gif"}
MAX_IMAGE = 25 * 1024 * 1024


def deep_merge(dst, src):
    for k, v in src.items():
        if isinstance(v, dict) and isinstance(dst.get(k), dict):
            deep_merge(dst[k], v)
        else:
            dst[k] = v
    return dst


def atomic_write(path, data, attempts=6):
    """Write text/bytes to path via temp file + rename, so readers never see a half-written file.

    On Windows os.replace fails with PermissionError while another process (an editor, antivirus,
    backup tool) has the target open, so retry briefly; the temp file is always cleaned up.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=path.name + ".", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data.encode("utf-8") if isinstance(data, str) else data)
            f.flush()
            os.fsync(f.fileno())
        for i in range(attempts):
            try:
                os.replace(tmp, path)
                return
            except PermissionError:
                if i == attempts - 1:
                    raise
                time.sleep(0.05 * 2 ** i)
    finally:
        try:
            os.remove(tmp)
        except OSError:
            pass  # already renamed into place


def write_json(path, obj):
    atomic_write(path, json.dumps(obj, indent=1))


def read_json(path, default=None):
    """Load JSON; a corrupt file is kept as <name>.bad (instead of being silently overwritten later)."""
    path = Path(path)
    try:
        return json.loads(path.read_text("utf-8"))
    except FileNotFoundError:
        return default
    except (OSError, ValueError) as e:
        print("%s is unreadable (%s); keeping a copy as %s.bad" % (path.name, e, path.name))
        try:
            os.replace(path, path.with_name(path.name + ".bad"))
        except OSError:
            pass
        return default


class Settings:
    """settings.json: UI prefs per view, widget mode, window geometry, active profiles."""
    DEFAULTS = {"active_profile": None, "active_layout": None, "show_unmatched": False, "ui": {}, "theme": {},
                "window": {"frameless": False, "on_top": False, "pinned": False, "opacity": 1.0,
                           "click_through": False, "close_to_tray": True, "start_hidden": False},
                "geometry": {}, "detached": []}

    def __init__(self, path=None):
        self.path, self.lock = Path(path or paths.SETTINGS_FILE), threading.RLock()
        self.rev = 0
        stored = read_json(self.path, {})
        self.data = deep_merge(copy.deepcopy(self.DEFAULTS), stored if isinstance(stored, dict) else {})

    def get(self):
        with self.lock:
            d = copy.deepcopy(self.data)
            d["rev"] = self.rev
            return d

    def _commit(self):
        self.rev += 1
        write_json(self.path, self.data)
        return self.get()

    def patch(self, patch):
        if not isinstance(patch, dict):
            raise ValueError("settings patch must be an object")
        patch.pop("rev", None)
        with self.lock:
            deep_merge(self.data, patch)
            return self._commit()

    def replace_key(self, key, value):
        """Set a key wholesale (deep_merge can't delete dict keys or shrink lists)."""
        with self.lock:
            self.data[key] = value
            return self._commit()


def safe_name(name):
    n = re.sub(r"[^\w\- ]", "", str(name or ""), flags=re.A).strip()[:40]
    return n or None


class Profiles:
    """A folder of <name>.json documents with sanitised names (no path traversal)."""

    def __init__(self, folder):
        self.dir = Path(folder)

    def path(self, name):
        n = safe_name(name)
        return (self.dir / (n + ".json")) if n else None

    def names(self):
        return sorted(p.stem for p in self.dir.glob("*.json")) if self.dir.exists() else []

    def load(self, name):
        p = self.path(name)
        return json.loads(p.read_text("utf-8")) if p else None

    def save(self, name, data):
        p = self.path(name)
        if not p or not isinstance(data, dict):
            raise ValueError("bad profile")
        write_json(p, data)
        return p.stem

    def delete(self, name):
        p = self.path(name)
        if p and p.exists():
            p.unlink()


def save_image(name, blob):
    ext = Path(name or "").suffix.lower()
    if ext not in IMAGE_EXT:
        raise ValueError("unsupported image type (use png, jpg, webp or gif)")
    stem = re.sub(r"[^\w\-]", "_", Path(name).stem, flags=re.A)[:30] or "image"
    fname = "%s-%s%s" % (stem, secrets.token_hex(3), ext)
    atomic_write(paths.ASSETS / fname, blob)
    return fname
