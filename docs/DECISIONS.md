# Decision log

Short entries, newest last. Format: **context → decision → consequence**. Add one whenever you make a
choice a future reader might want to undo.

### 1. Stdlib-only viewer (2026-11)
The browser version has to run on any Python install with nothing to install → stdlib `http.server`,
`ctypes` and `struct`. Only the widget uses pywebview/pystray/pillow.

### 2. Open → copy → close the shared memory on every read (2026-11)
Holding the mapping open keeps a dead HWiNFO's section alive: a restarted HWiNFO might reuse the stale
object, and we can only detect the problem through the 15 s stale timer. Re-opening at 1 Hz costs a few
syscalls and a ~100 KB copy. Consequence: if HWiNFO is gone, the next read fails immediately with `missing`.

### 3. Reconnect grace window in the server, not the UI (2026-11)
The 2–15 s gap during an HWiNFO restart shouldn't flash an error in every window or trigger tray
notifications. `App.snapshot` serves the last good payload marked `reconnecting` for 30 s (120 s while the
watchdog says `restarting`). One place serves every consumer (browser, widget windows, tray watcher).

### 4. Watchdog is a separate elevated scheduled task (2026-11)
HWiNFO runs elevated, so killing it, or relaunching it without UAC, needs admin. Running the viewer elevated
would put an HTTP server and a WebView at high integrity for no reason. So the watchdog is its own small
process, registered with `--install` as a logon task (`RunLevel Highest`, no execution time limit; Task
Scheduler's default 72 h limit would otherwise kill it). The viewer only reads the watchdog's status file.

### 5. Silent start through HWiNFO64.INI, edited only while HWiNFO is stopped (2026-11)
HWiNFO rewrites its INI on a clean exit and reads it only at start-up. The watchdog writes `[Settings]`
keys (`SensorsOnly`, `ShowWelcomeAndProgress=0`, `SensorsSM`, `MinimalizeSensors`, `AutoUpdate=0`) after the
forced kill and before the launch. Keys are configurable in `data/watchdog.json`. The file's encoding and
bytes are kept (latin-1 round-trip), and a one-time `.hwv-backup` copy is made. The `-sensoronly` argument is
passed as requested but not verified against HWiNFO's documentation; the INI key is the reliable mechanism.
Replaces the earlier in-widget `ensure_hwinfo_sensors_sm()` experiment, which had a bug that skipped the
write and made `start_widget.bat` self-elevate.

### 6. Testing the watchdog without touching HWiNFO (2026-11)
`Watchdog(cfg, status_path=...)` with `exe` = `C:\Windows\System32\PING.EXE`, `args` = `["-n","1000","127.0.0.1"]`,
a scratch `ini`, and `restart_after_hours = 10/3600` exercises the real kill/launch code path in about 30 s.
Process lookup is by the exe's file name, so HWiNFO is never matched.

### 7. Runtime files under data/ (2026-11)
Settings, profiles, layouts, assets, logs and watchdog files moved from the project root to `data/`.
`paths.migrate_legacy()` moves old root copies on start-up.

### 8. Rewrite in Rust (2026-11)
The goal is "flexible and lightweight" plus a self-contained exe. Python stays usable until the Rust port
reaches parity (R3), then it's removed. Crates were picked for size and to avoid an async runtime:
`windows` (Win32), `tiny_http` (server), `serde_json` with `preserve_order` (catalog/settings key order matters),
`regex`, `clap`. Planned later: `tao` + `wry` + `tray-icon`, which are Tauri's lower layers without the framework.
Measured on R1, with `serve` on live HWiNFO data polled at 2 Hz:

| | Python | Rust |
|---|---|---|
| RAM | 20.5 MB | 9.9 MB |
| CPU | ~2% | ~0.5% |
| Size | needs a Python install | 2.85 MB exe |

The JSON payload shape and every route's status codes matched the Python server exactly (scratch parity script).
Debug builds read `web/` from disk, so UI work needs no rebuild.
