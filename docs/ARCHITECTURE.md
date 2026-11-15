# Architecture

## Processes

```
 HWiNFO64.exe (elevated)                      hwinfo_watchdog.py (elevated, logon task, optional)
   │ writes Global\HWiNFO_SENS_SM2             │ reads process uptime, kills + relaunches HWiNFO at 11.5 h
   │                                           │ writes data/watchdog_status.json
   ▼                                           ▼
 hwviewer.py  or  widget.py  (unelevated) ─── one process: App + ThreadingHTTPServer on 127.0.0.1:8765
   │  shm.SharedMemorySource → views.build_payload → /api/data (JSON)
   ▼
 web/index.html (browser tab or pywebview windows) polls /api/data at 1 Hz
```

`widget.py` is the same server plus pywebview windows and a tray icon in one process; the UI calls
back into Python through `window.pywebview.api` (class `desktop.Api`).

## Data flow for one poll
1. `GET /api/data` → `App.snapshot()`. Results are cached for 0.4 s because several windows poll.
2. `App.effective()` = `views.json` (reloaded when its mtime changes) overlaid with the active sensor
   profile from `data/layouts/`.
3. `SharedMemorySource.read()` → `shm.read_raw()`: OpenFileMapping → MapViewOfFile → VirtualQuery (view
   size) → validate header → copy → unmap/close → `parse_buffer`.
4. `views.build_payload()` matches each catalog item (regexes on sensor/label/unit) and builds the
   rows the UI renders, plus a flat sensor list for the Builder.

## Failure handling (HWiNFO restarts, closes, expires)
| Layer | Behaviour |
|---|---|
| `shm.read_raw` | Any problem → `SMError(kind)`: `missing`, `starting` (empty/zero header, short view), `stale`, `expired` (DEAD), `invalid`. Reads are bounds-checked, so no access violations. |
| `SharedMemorySource` | Exponential backoff 0.25 → 5 s between attempts while failing. Stale if `poll_time` hasn't changed for 15 s. |
| `App.snapshot` | For up to 30 s after a failure (120 s while the watchdog reports `restarting`) it keeps serving the last good payload with `reconnecting: true`. After that it returns `ok: false`. Any other exception → `kind: internal`, and the server keeps running. |
| UI | `reconnecting` → last values dimmed, plus "reconnecting…" in the status line. `ok:false` → error banner. Server unreachable → its own banner. |
| Widget | Tray notification only when it goes from ok to `ok:false`, i.e. after the grace window. |

A watchdog restart therefore shows up as a few seconds of dimmed values, not an error.

## Watchdog cycle
`step()` runs every 30 s and again exactly when the restart is due:
1. Find `HWiNFO64.exe` (Toolhelp snapshot) and its creation time (`GetProcessTimes`; works unelevated).
2. Not running and `keep_running` → write INI keys, launch, wait up to 90 s for the shared memory.
3. Uptime ≥ `restart_after_hours`, or shared memory `expired` for over 20 s → **cycle**: TerminateProcess
   (checking the PID wasn't recycled; falls back to `taskkill /F /T`) → wait 1 s → write INI keys → launch
   with `args` (`-sensoronly`), with CREATE_BREAKAWAY_FROM_JOB so stopping the task doesn't kill HWiNFO →
   wait for the shared memory.
4. Write `data/watchdog_status.json`. Exceptions back off 5 s → 300 s; the loop never exits on error.

It is installed as a logon scheduled task with highest privileges: no time limit, restarts on failure,
single instance (plus a named mutex). The task needs elevation because HWiNFO runs elevated: an
unelevated process can't terminate it, and starting it unelevated triggers a UAC prompt.

## Persistence
All writes go through `storage.atomic_write` (temp file + fsync + `os.replace` with retries). A corrupt
JSON file is renamed to `*.bad` instead of being silently overwritten. `Settings.rev` increments on
every change; the UI compares it against its own copy to pick up changes made in other windows or the tray.
