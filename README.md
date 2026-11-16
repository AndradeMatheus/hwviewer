# HWiNFO Viewer

A small, fast sensor dashboard for [HWiNFO64](https://www.hwinfo.com/): a desktop window you can make see-through,
a tray icon, and the same view in any browser on your PC (or, read-only, on your phone).
One ~4 MB Windows exe, written in Rust.

## Install
- **Installer** (`HWiNFO-Viewer-Setup-x.y.z.exe` from Releases): installs to Program Files, can start with Windows,
  and installs the **HWiNFO watchdog** (recommended for the free HWiNFO, see below).
- **Portable** (`HWiNFO-Viewer-x.y.z-portable.zip`): unzip anywhere and run `hwviewer.exe`. Everything is kept in
  `data\` next to it (that's what `portable.txt` does). The setup wizard can install the watchdog from there too.

On first start a **setup wizard** checks HWiNFO, offers the watchdog, autostart and network access.
You can reopen it later from the gear menu.

### HWiNFO
In HWiNFO: *Settings → General / User Interface → Shared Memory Support*, and keep the Sensors window running.
The free version switches shared memory off after 12 h. The **watchdog** fixes that: it restarts HWiNFO silently at
11.5 h (sensors only, no welcome screen) and starts it at logon. It is an elevated scheduled task (one admin prompt
when installed). HWiNFO Pro has no 12 h limit.

## Modes
| Command | What you get |
|---|---|
| `hwviewer` / `hwviewer app` | window + tray icon + local URL (default) |
| `hwviewer tray` | tray icon + local URL; the window opens on demand (lightest: ~15 MB RAM) |
| `hwviewer serve [--open] [--host 0.0.0.0]` | browser only |
| `hwviewer dump` | every sensor/label HWiNFO exposes → `dump.txt` (for writing `views.json` patterns) |
| `hwviewer watchdog install\|uninstall\|status\|restart-now` | the HWiNFO watchdog |

The URL (default `http://127.0.0.1:8765/`, next free port if taken) is in the tray menu and in gear › Window.

## Using it
- **Full / Compact**: tables defined in `views.json` (in the data folder; edit it and the app reloads it).
  **Builder**: put sensor text boxes over your own image.
- **Gear › Look**: presets, fonts, colors, and **opacity for background, boxes and text separately**. A see-through
  background needs the title bar hidden (gear › Window).
- **Gear › Window** (desktop app): hide title bar, always on top, pin to desktop, click-through, whole-window opacity.
- **OSD tab — in-game overlay**: pick sensors (+ FPS / frame time), position, font, colours and separate
  background/box/text opacity; toggle with **Ctrl+Shift+O** (configurable) or the tray. Engines:
  *RTSS* (if RivaTuner Statistics Server is running — draws inside games, also exclusive fullscreen) or the
  *native* overlay (no extra software; windowed/borderless games). *Auto* picks RTSS when available.
  FPS comes from RTSS, or from PresentMon via the watchdog when RTSS isn't running. Nothing is injected into games.
- **⧉** detaches a view or section into its own window. Detached windows come back on the next start.
- **Tray icon**: show/hide, open in browser, copy URL, window options, opacity, quit. If click-through locks you
  out of the window, turn it off here.

Data folder: `%LOCALAPPDATA%\HWiNFO Viewer` (installed) or `data\` (portable). Delete `settings.json` there to
reset the look and window options; profiles and layouts are kept.

## Building
Needs Rust (the toolchain is pinned in `rust-toolchain.toml`) and the Visual Studio Build Tools (MSVC).
```
cargo run -- app --mock          # fake data; debug builds read web/ from disk (edit + refresh)
cargo test
powershell -ExecutionPolicy Bypass -File packaging\build.ps1    # dist\: portable zip + installer (needs Inno Setup 6)
```
Pushing a `v*` tag builds both on GitHub Actions and attaches them to a Release.
See `CLAUDE.md` and `docs/` for how it's put together.
