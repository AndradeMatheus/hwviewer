"""Every file location the app uses, in one place.

Factory config (views.json, web/) lives in the project root; everything the app writes at runtime
lives under data/ so the root stays clean and data/ can be backed up or deleted as a unit.
"""
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / "web"
INDEX_HTML = WEB / "index.html"
VIEWS_FILE = ROOT / "views.json"
ICON_FILE = ROOT / "icon.ico"

DATA = ROOT / "data"
SETTINGS_FILE = DATA / "settings.json"
PROFILES = DATA / "profiles"         # Builder profiles (image + text boxes)
LAYOUTS = DATA / "layouts"           # sensor profiles (overlays on views.json)
ASSETS = DATA / "assets"             # images imported in the Builder
LOGS = DATA / "logs"
DUMP_FILE = DATA / "dump.txt"
WATCHDOG_CONFIG = DATA / "watchdog.json"
WATCHDOG_STATUS = DATA / "watchdog_status.json"


def migrate_legacy():
    """Move runtime files from the pre-data/ layout (project root) into data/. Safe to call every start."""
    DATA.mkdir(exist_ok=True)
    for name in ("settings.json", "profiles", "layouts", "assets", "dump.txt"):
        old, new = ROOT / name, DATA / name
        if old.exists() and not new.exists():
            try:
                shutil.move(str(old), str(new))
            except OSError as e:
                print("could not move %s to data/: %s" % (name, e))
