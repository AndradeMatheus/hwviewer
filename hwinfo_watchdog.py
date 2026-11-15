#!/usr/bin/env python3
"""
HWiNFO64 watchdog - restarts HWiNFO silently before the free version's 12 h shared-memory limit.

    python hwinfo_watchdog.py --install       # register an elevated logon task and start it (one UAC prompt)
    python hwinfo_watchdog.py --status        # task state, HWiNFO uptime, next/last restart
    python hwinfo_watchdog.py --restart-now   # restart HWiNFO once (asks for admin)
    python hwinfo_watchdog.py --uninstall
    python hwinfo_watchdog.py                 # run in the foreground (Ctrl+C to stop; should be elevated)

Options (exe path, args, restart_after_hours, INI keys ...) are in data/watchdog.json, created on first run.
Log: data/logs/watchdog.log
"""
from hwv.watchdog import main

if __name__ == "__main__":
    main(doc=__doc__)
