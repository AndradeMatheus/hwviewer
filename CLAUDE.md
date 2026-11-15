# HWiNFO Viewer — working notes for Claude

Windows sensor dashboard for HWiNFO64. HWiNFO publishes readings in the shared-memory block
`Global\HWiNFO_SENS_SM2`; `hwviewer.exe` (Rust) parses it and shows it in a desktop window (tao + wry/WebView2),
a tray icon, and any browser via a local HTTP server. An elevated watchdog task restarts HWiNFO before the free
version's 12 h shared-memory limit. Owner: single developer. Target machine: Windows 11, Ryzen 5800X / B550 / RTX 3070 Ti.
Installed copy: `C:\Program Files\HWiNFO Viewer\` (data in `%LOCALAPPDATA%\HWiNFO Viewer`); its watchdog task
**is live** and manages the user's real HWiNFO.

Roadmap: R0–R4 done (Rust port, watchdog, desktop/tray, opacity, installer + wizard, CI). **R5: in-game OSD + FPS**
(plan: `~/.claude/plans/great-now-we-need-lovely-metcalfe.md`).

## Commands
```
cargo test                                         # unit tests (~0.1 s)
cargo clippy --all-targets -- -D warnings          # must be clean (CI enforces)
cargo fmt --check                                  # rustfmt.toml: 120 columns
cargo run -- serve --mock --port 8799              # browser UI with fake data; debug builds read web/ from disk
cargo run -- serve --mock --mock-outage 20 --port 8799   # + a 4 s simulated HWiNFO restart every 20 s
cargo run -- app --mock --port 8797                # desktop window + tray; `tray` = no window until asked
cargo run -- dump                                  # every real sensor/label -> <data>/dump.txt
cargo run --example transparency_spike -- --backdrop     # per-pixel transparency regression check
powershell -ExecutionPolicy Bypass -File packaging\build.ps1   # dist\: portable zip + installer
```
- Toolchain pinned in `rust-toolchain.toml` (MSVC host; VS Build Tools 2022 + Windows SDK rc.exe).
  In a fresh shell, `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"` if cargo isn't found.
- Debug builds use the repo's `data/`; release builds use `%LOCALAPPDATA%\HWiNFO Viewer`, or `.\data` with
  `portable.txt` beside the exe. **`HWV_DATA_DIR=<scratch>` for any test that writes.** Use ports 8795–8799 for
  tests: the user's app usually holds 8765.
- The exe is GUI-subsystem: from PowerShell, pipe CLI output (`| Out-String`) or it returns before printing.
- Commit messages: write them to a file and `git commit -F` (PowerShell 5.1 mangles embedded quotes).
  Keep `main` pushed after each milestone. Bump the version only in `Cargo.toml`.

## Layout
| Path | What |
|---|---|
| `src/main.rs` | CLI (clap): `app` (default) / `tray` / `serve` / `dump` / `watchdog …`; console attach; `fatal()` |
| `src/shm.rs` | HWiNFO SM2 structs, `read_raw()` (open→copy→close, VirtualQuery bounds), `SharedMemorySource` (backoff + stale), `MockSource`, `DataSource` trait |
| `src/views.rs` | readings + `views.json` catalog → UI payload (pure) |
| `src/storage.rs` | `atomic_write`, `Settings` (rev, deep-merge), `Profiles`, images |
| `src/server.rs` | `App` (snapshot cache, reconnect grace, layouts), tiny_http routes, guards |
| `src/desktop/` | tao loop + wry windows + tray (`mod.rs`), Win32 tweaks (`win32.rs`), icon (`icon.rs`) |
| `src/watchdog/` | loop + CLI + task install (`mod.rs`), Win32 processes (`process.rs`), HWiNFO64.INI (`ini.rs`) |
| `src/runtime.rs` · `src/system.rs` · `src/autostart.rs` · `src/log.rs` · `src/paths.rs` | single instance + runtime.json · `GET /api/system` (wizard) · HKCU Run · rotating logs · every path |
| `web/index.html` | the whole UI (vanilla JS/CSS, no build step), embedded in release builds |
| `defaults/views.json` | factory catalog (embedded); the editable copy is `<data>/views.json` |
| `packaging/` | `build.ps1`, `installer.iss` (AppId is fixed: never change it) |
| `docs/ARCHITECTURE.md`, `docs/DECISIONS.md` | how it fits together; why (append an entry for each new decision or crate) |

## Rules
- **Never crash on HWiNFO state.** Every shared-memory failure is `shm::SmError { msg, kind }`; never hold the
  mapping open between reads (DECISIONS #2). Worker threads catch panics; locks go through `server::lock()`
  (poison-tolerant), order `inner` → `settings`.
- **No `unwrap()`/`expect()` outside tests and build.rs**. `unsafe` only in Win32 wrappers, each block with a
  `// SAFETY:` comment. Pin new crates to versions at least 2 weeks old and record them in DECISIONS.
- **Never write files directly**: `storage::atomic_write` / `write_json`, paths from `paths.rs`.
- **The viewer runs unelevated.** Only the watchdog task is elevated.
- **Security:** keep the Host-header check, the `X-HWV` POST header and LAN read-only mode in `server.rs`. The UI
  escapes all dynamic text with `esc()` before `innerHTML`.
- **API contract:** `web/index.html` and `server.rs` change together. `/api/data` keys used by the UI:
  `ok, error, kind, reconnecting, stale_for, views, sensors, uptime, rev, watchdog, layout`.
- **UI ↔ window:** wry IPC, `shellSend({cmd})` → `Desktop::on_ipc`. **Never name a top-level JS binding `ipc`**
  (it collides with wry's `window.ipc` and the whole script fails to load). Shell-mode JS errors land in
  `<data>/logs/app.log`; `HWV_TRACE=1` logs HTTP requests.
- See-through backgrounds only for **frameless** windows (WS_EX_NOREDIRECTIONBITMAP, DECISIONS #10).
- **Sensor-monitoring retro-port:** the user is building new sensor monitoring in another session, to port in
  later. Keep `shm::DataSource`, `shm::Reading` and the `views.json` catalog format stable and small. A new
  source plugs in as another `DataSource`, not by changing the server or UI.
- Add or extend a `#[cfg(test)]` test for any parser, INI, storage, reconnection or OSD-formatting change.

## Testing without disturbing the user
- **UI pages:** render with headless Edge, never the real desktop:
  `msedge --headless=new --disable-gpu --user-data-dir=<scratch> --virtual-time-budget=4000 --window-size=760,520 --screenshot=<png> http://127.0.0.1:<port>/#setup`.
- **Native windows:** use the spike's `--backdrop --backdrop-only` window as the background and capture only that
  exact rectangle. Ask before popping windows while the user is working.
- **Watchdog:** use a stand-in. `HWV_DATA_DIR` = scratch; config `exe` = `C:\Windows\System32\PING.EXE`,
  `args` = `["-n","1000","127.0.0.1"]`, a scratch `ini`, `restart_after_hours` = `0.0027778`,
  `check_interval_s` = 5, `ready_timeout_s` = 3. Then run
  `target\debug\hwviewer.exe watchdog run --config <file> --allow-multiple` for about 30 s. Expect a new PING PID
  every 10 s.

## Do not do without asking
- Kill, start or restart the real HWiNFO64 (`watchdog restart-now`, `taskkill`), or install/uninstall the
  watchdog task, the installed app, or other software.
- Edit `C:\Program Files\HWiNFO64\HWiNFO64.INI`, or change `views.json` sensor matches (tuned to the user's hardware).
- Delete anything in `data/` or `%LOCALAPPDATA%\HWiNFO Viewer`.
- Screenshot anything beyond your own test windows.

## Gotchas
- HWiNFO free: shared memory switches off after 12 h (signature `0xDEADDEAD`); restarting HWiNFO resets it.
- HWiNFO reads its INI only at start-up and rewrites it on a clean exit: INI edits stick only while it's stopped.
- HWiNFO renamed the GPU sensor to `dGPU [#0]: …`; patterns use `^d?GPU`.
- Windows refuses a rename while another process has the file open: `atomic_write` retries.
