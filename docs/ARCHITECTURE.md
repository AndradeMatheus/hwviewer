# Architecture

## Processes
```
 HWiNFO64.exe (elevated)                          hwviewer.exe watchdog run   (elevated logon task)
   │ writes Global\HWiNFO_SENS_SM2                 │ uptime check, kill + relaunch at 11.5 h, INI keys
   │                                               │ writes <data>/watchdog_status.json
   ▼                                               ▼
 hwviewer.exe app | tray | serve   (unelevated, one per session: mutex + <data>/runtime.json)
   ├─ HTTP server thread pool (tiny_http, 127.0.0.1:8765+, or 0.0.0.0 read-only with LAN on)
   │    App::snapshot() → SharedMemorySource::read() → views::build_payload() → /api/data
   └─ main thread: tao event loop (app/tray) → wry windows (WebView2) + tray icon
         web/index.html polls /api/data at 1 Hz; window commands go back over wry IPC
```
`serve` has no event loop: just the server. `tray` creates no WebView until a window is opened
(~15 MB RAM against ~300 MB with WebView2).

## One poll
1. `GET /api/data` → `App::snapshot()`. The result is cached for 0.4 s because several windows poll.
2. `App::effective()` = `<data>/views.json` (reloaded when its mtime changes), overlaid with the active sensor
   profile from `<data>/layouts/`.
3. `SharedMemorySource::read()` → `shm::read_raw()`: OpenFileMapping → MapViewOfFile → VirtualQuery (view size) →
   validate header → copy → unmap/close → `parse_buffer`.
4. `views::build_payload()` matches catalog items (case-insensitive regexes on sensor/label/unit) into rows,
   plus a flat sensor list for the Builder.

## Failure handling
| Layer | Behaviour |
|---|---|
| `shm::read_raw` | every problem → `SmError(kind)`: `missing`, `starting`, `stale`, `expired` (DEAD), `invalid`; reads are bounds-checked |
| `SharedMemorySource` | backoff 0.25 → 5 s between attempts while failing; `stale` if `poll_time` is frozen 15 s |
| `App::snapshot` | serves the last good payload with `reconnecting: true` for 30 s after a failure (120 s while the watchdog says `restarting`), then `ok: false`; panics → `kind: internal` |
| server | each request runs under `catch_unwind`; poisoned locks are recovered |
| UI | `reconnecting` → last values dimmed; `ok:false` → banner; server gone → its own banner |
| watchdog | every check runs under `catch_unwind`; errors back off 5 → 300 s; the loop never exits |

## Watchdog cycle
`step()` runs every 30 s and exactly when a restart is due:
1. Find `HWiNFO64.exe` (Toolhelp) and its creation time (`GetProcessTimes`; works unelevated).
2. Not running → write INI keys, launch, and wait up to 90 s for the shared memory.
3. Uptime ≥ 11.5 h, or shared memory `expired` for more than 20 s → TerminateProcess (PID-recycling check,
   `taskkill` fallback) → INI keys → launch with `-sensoronly` and CREATE_BREAKAWAY_FROM_JOB → wait.
4. Publish `<data>/watchdog_status.json`. The viewer shows "restart in …" and holds values through the gap.

## Desktop shell
- Windows are created by `Desktop::create`: main plus detached `view/section` keys, each URL carrying
  `?shell=desktop&win=<key>`. An init script sets `window.__HWV`.
- Frameless windows use WS_EX_NOREDIRECTIONBITMAP, which gives real per-pixel alpha: background, box and text
  opacity are CSS alphas, and whole-window opacity is CSS `opacity`.
- A 1 s timer flushes the debounced geometry, keeps pinned windows at desktop level and re-asserts topmost.
- The tray menu and IPC both end up in `set_opts()`. A title-bar change recreates the windows in place.

## Persistence and packaging
- **Writes:** all go through `storage::atomic_write` (temp + fsync + rename with retries). Corrupt JSON is kept as
  `*.bad`. `Settings.rev` increments on each change; windows compare it to pick up changes made elsewhere.
- **Shipping:** one exe; the UI and the factory `views.json` are embedded. The Inno installer registers the
  watchdog task, which `PrepareToInstall` stops on upgrade. Portable mode is `portable.txt`.
- **CI:** fmt, clippy and tests on push; `v*` tags publish a Release.
