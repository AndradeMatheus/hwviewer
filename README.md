# HWiNFO quick sensor viewer

## Setup
1. HWiNFO64 -> Settings (via "Main Settings" in the Sensors window) -> General / User Interface -> tick **Shared Memory Support**. Keep Sensors running.
   The free version switches this off after 12 h. Either re-tick it, install the watchdog (below), or get HWiNFO Pro.
2. Python 3.9+ on PATH.
   - **Desktop widget:** `pip install -r requirements.txt`, then double-click `start_widget.bat`.
   - **Browser only (no packages):** double-click `start_browser.bat` -> http://127.0.0.1:8765

## HWiNFO watchdog (optional)
Restarts HWiNFO silently at 11.5 h of uptime, so the free version's 12 h shared-memory limit never hits.
It also starts HWiNFO if it isn't running.

- Double-click `install_watchdog.bat`. It asks for admin once, then registers an elevated logon task and starts it.
  Admin is needed because HWiNFO runs elevated; the viewer itself never runs as admin.
- `python hwinfo_watchdog.py --status` shows HWiNFO's uptime and the next restart. The viewer's status line shows it too.
- Before each launch the watchdog writes a few `[Settings]` keys into `HWiNFO64.INI` (sensors-only, no welcome
  screen, shared memory on, no update check). A one-time backup is saved as `HWiNFO64.INI.hwv-backup`.
  HWiNFO is started with `-sensoronly`.
- Options are in `data/watchdog.json`: exe path, args, hours, INI keys, `keep_running`. Log: `data/logs/watchdog.log`.
- During a restart the viewer keeps showing the last values (dimmed, "reconnecting…") for the few seconds HWiNFO is gone.
- Remove it with `uninstall_watchdog.bat`.

## Views
- **Full / Compact** - tables defined in `views.json` (your original sensors = "Default"; never modified by the app).
- **Builder** - import an image (stored in `data/assets/`), add text boxes bound to any sensor, drag into place, Save. Builder profiles live in `data/profiles/`.

## Gear menu
- **View**: align, width (or drag the content edges), columns, order (Custom / Hottest first / Coolest first - RPM always last), per-view font size, collapse sections.
- **Sensors**: sensor profiles. *Edit sensors* lets you drag rows to reorder, remove with x, add any HWiNFO reading with +, add sections.
  Editing while on Default forks a profile called "Custom". *Return to default* switches back to the original. Profiles are stored in `data/layouts/`.
- **Look**: presets, font, size, colors, row padding, corner radius, bold values, section title case.
- **Window** (widget): hide title bar (restarts), always on top, pin to desktop, click-through, opacity, close button -> tray (default), start hidden, window size.
- The pop-out button next to the gear detaches the current view into its own window; the pop-out on a section title detaches just that section.
  Detached windows are remembered and restored on the next launch.

## Tray icon (right-click)
Show/hide, always on top, pin to desktop, click-through, close-to-tray, start hidden, opacity, Quit.
If you enable click-through you can no longer click the window - turn it off here.

## Handy
- `python hwviewer.py --dump` writes every sensor/label HWiNFO exposes to `data/dump.txt`.
- `python hwviewer.py --mock` / `python widget.py --mock` run with fake data; add `--mock-outage 20` to simulate HWiNFO restarts.
- `python hwviewer.py --host 0.0.0.0` lets other devices on your LAN view it (read-only from them).
- Reset a messed-up look: gear > Look > Reset look. Delete `data/settings.json` to reset everything (profiles are kept).
- Tests: `python -m unittest discover -s tests -v`.
