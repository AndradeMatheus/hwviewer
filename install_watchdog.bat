@echo off
rem Registers the HWiNFO watchdog as an elevated logon task (asks for admin once) and starts it.
cd /d "%~dp0"
python hwinfo_watchdog.py --install
