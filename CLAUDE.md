# HWiNFO viewer — working notes for Claude

Local dashboard for HWiNFO64 sensor data on Windows. HWiNFO publishes readings in the shared-memory
block `Global\HWiNFO_SENS_SM2`; we parse it and serve a web UI (browser or pywebview desktop widget).
An optional elevated watchdog restarts HWiNFO before the free version's 12 h shared-memory limit.

Owner: single developer, test project. Target machine: Windows 11, Python 3.10, Ryzen 5800X / B550 / RTX 3070 Ti.

## Commands
```
python -m unittest discover -s tests -v       # run after every change (stdlib only, ~0.1 s)
python -m compileall -q hwv tests *.py        # syntax check everything
python hwviewer.py --mock --open              # UI with fake data, http://127.0.0.1:8765
python hwviewer.py --mock --mock-outage 20    # fake data + a 4 s simulated HWiNFO restart every 20 s
python hwviewer.py --dump                     # every real sensor/label -> data/dump.txt (find regexes for views.json)
python widget.py --mock                       # desktop widget (needs requirements.txt)
python hwinfo_watchdog.py --status            # read-only: task state + last watchdog status
```
Use `--port 8799` (or anything but 8765) for test servers: the user's widget usually holds 8765.

## Layout
| Path | What |
|---|---|
| `hwviewer.py`, `widget.py`, `hwinfo_watchdog.py` | Thin entry points only (docstring = user help). Logic lives in `hwv/`. |
| `hwv/shm.py` | Shared-memory structs, `read_raw()` (open→copy→close, bounds-checked), `SharedMemorySource` (backoff + stale detection), `MockSource`. |
| `hwv/views.py` | Pure functions: readings + catalog → UI payload. |
| `hwv/storage.py` | `atomic_write`, `Settings`, `Profiles`, image upload. |
| `hwv/server.py` | `App` (config hot-reload, layouts, reconnect grace window), HTTP handler, CLI. |
| `hwv/desktop.py` | pywebview windows, Win32 tweaks, tray icon. |
| `hwv/watchdog.py` | HWiNFO lifecycle (uptime, kill, INI, relaunch), scheduled-task install. |
| `hwv/paths.py` | Every path. Add new files here, never hard-code paths elsewhere. |
| `web/index.html` | Entire UI: vanilla JS + CSS in one file, no build step. |
| `views.json` | Factory sensor catalog + views (user-edited, hot-reloaded). |
| `data/` | Runtime state, git-ignored: settings, profiles, layouts, assets, logs, watchdog config/status. |
| `docs/ARCHITECTURE.md` | Data flow, process model, failure handling. Read before structural changes. |
| `docs/DECISIONS.md` | Why things are the way they are. Append an entry for any new design decision. |

## Rules
- **Dependencies:** the viewer (`hwviewer.py` + `hwv/` except `desktop.py`) is stdlib-only. Don't add packages to it.
  The widget may use pywebview/pystray/pillow only. Win32 goes through `ctypes` with explicit `argtypes`/`restype`.
- **Never crash on HWiNFO state.** Every shared-memory failure must become `shm.SMError(msg, kind)`.
  Don't hold the mapping open between reads (see DECISIONS #2).
- **Never write files directly.** Use `storage.atomic_write` / `write_json`, and paths from `hwv/paths.py`.
- **The viewer runs unelevated.** Only the watchdog may need admin. Never add self-elevation to
  `start_widget.bat` or the server.
- **Security:** keep the Host-header check, the `X-HWV` POST header and LAN read-only mode in `server.py`.
  The UI must escape all dynamic text with `esc()` before `innerHTML`.
- **HTTP API contract:** `web/index.html` and `server.py` change together. The `/api/data` payload keys
  `ok, error, kind, reconnecting, stale_for, views, sensors, uptime, rev, watchdog, layout` are used by the UI.
- **Style:** match the existing compact style (≤ ~120 columns, short docstrings that say *why*). No type-annotation
  churn or reformatting of untouched code.
- Add or extend a test in `tests/test_core.py` for any parser, INI, storage or reconnection change.

## Do not do without asking
- Kill, start or restart the real HWiNFO64 (`hwinfo_watchdog.py --restart-now`, `taskkill`), or run `--install`/`--uninstall`.
  To test the watchdog, point it at a stand-in exe (`PING.EXE`) with a scratch `ini` and `status_path`, as in DECISIONS #6.
- Edit `C:\Program Files\HWiNFO64\HWiNFO64.INI`.
- Change `views.json` sensor matches (they're tuned to the user's hardware) or delete anything in `data/`.

## Gotchas
- HWiNFO free: shared memory switches off after 12 h (signature `0xDEADDEAD`); restarting HWiNFO resets it.
- HWiNFO reads its INI only at start-up and rewrites it on a clean exit, so INI edits only stick while it's stopped.
- `pythonw` has no console: in the widget, errors must reach the user via `fatal()` / tray notifications.
- Windows `os.replace` fails while another process has the file open; `atomic_write` retries.
