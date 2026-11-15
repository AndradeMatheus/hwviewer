@echo off
rem Browser version, no extra packages needed
cd /d "%~dp0"
start "HWiNFO viewer" /min python hwviewer.py --open
