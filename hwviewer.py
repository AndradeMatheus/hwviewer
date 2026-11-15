#!/usr/bin/env python3
"""
HWiNFO quick sensor viewer - browser version (standard library only; live data needs Windows).

Reads HWiNFO64's Shared Memory (HWiNFO Settings -> General / User Interface -> "Shared Memory Support")
and serves a local web UI.

    python hwviewer.py --open                  # live data in your browser, http://127.0.0.1:8765
    python hwviewer.py --dump                  # write every sensor/label HWiNFO exposes to data/dump.txt
    python hwviewer.py --mock                  # fake data, for trying the UI without HWiNFO
    python hwviewer.py --mock --mock-outage 30 # fake data with a simulated HWiNFO restart every 30 s

Desktop widget: widget.py. Automatic HWiNFO restarts before the free 12 h limit: hwinfo_watchdog.py.
Which sensors each table view shows lives in views.json (hot-reloaded on save).
"""
from hwv.server import main

if __name__ == "__main__":
    main(doc=__doc__)
