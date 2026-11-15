#!/usr/bin/env python3
"""
HWiNFO sensor viewer as a desktop widget.

    pip install -r requirements.txt
    python widget.py            # real data
    python widget.py --mock     # fake data

Window behavior (title bar, always-on-top, pin to desktop, opacity, click-through, close-to-tray,
start hidden) is set from the gear menu (Window section) or the tray icon's right-click menu and
remembered in data/settings.json. Each view or section can be detached into its own window (the
button next to the gear / on a section title); detached windows are restored next launch.
"""
from hwv.desktop import main

if __name__ == "__main__":
    main(doc=__doc__)
