@echo off
rem Stops and removes the HWiNFO watchdog scheduled task. HWiNFO itself keeps running.
cd /d "%~dp0"
python hwinfo_watchdog.py --uninstall
