# Changelog

The release workflow publishes the section matching the tag (`## 0.4.0` for `v0.4.0`) as the GitHub Release notes.

## 0.4.0
First public release, rewritten in Rust (previously a Python prototype).

**Viewer**
- Live HWiNFO64 sensors as Full / Compact tables (`views.json`) or your own Builder layouts over an image.
- Three modes: desktop window + tray (`hwviewer`), tray only (`hwviewer tray`, ~15 MB RAM), browser only (`hwviewer serve`).
- Same view in any browser at the local URL. LAN access is read-only and opt-in.
- Separate opacity for background, boxes and text. A see-through widget needs the title bar hidden.
- Always on top, pin to desktop, click-through, detachable views and sections, close to tray.
- Holds the last values through brief HWiNFO restarts instead of flashing an error.

**In-game overlay (OSD)**
- Sensors, FPS and frame time, toggled with Ctrl+Shift+O. Position, font, colours and opacity are configurable,
  with a live preview.
- RTSS engine (inside games, also exclusive fullscreen) or a native click-through overlay (windowed and
  borderless games). Nothing is injected into games.
- FPS comes from RTSS, or from bundled PresentMon 2.6.0 through the watchdog.

**HWiNFO watchdog**
- Restarts the free HWiNFO silently before its 12 h shared-memory limit (sensors only, no welcome screen), and
  starts it at logon. Installed with the app as an elevated scheduled task.

**Install**
- Installer (Program Files, optional start with Windows, watchdog included) or portable zip (data next to the exe).
- A setup wizard on first start checks HWiNFO and sets up the watchdog, autostart and network access.
