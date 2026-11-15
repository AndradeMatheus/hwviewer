"""HWiNFO64 watchdog: keeps HWiNFO running and restarts it before the free version's 12 h shared-memory limit.

How it works
  * Every check_interval_s (and exactly at the due time) it finds HWiNFO64.exe and reads the process
    creation time. Once uptime reaches restart_after_hours (11.5 h) it kills the process (TerminateProcess,
    falling back to `taskkill /F`), writes the silent-start keys into HWiNFO64.INI (HWiNFO only reads the INI
    at start-up and rewrites it on a clean exit, so a forced kill is what makes the edit stick), relaunches it
    with the configured args (default `-sensoronly`), and waits for the shared memory to come back.
  * It also restarts early if the shared memory reports the "expired" signature, and starts HWiNFO when it
    isn't running (keep_running).
  * Progress is written to data/watchdog_status.json (the viewer reads it to show "restart in ..." and to
    hold the last values on screen during the gap) and logged to data/logs/watchdog.log.

HWiNFO runs elevated, so killing it and relaunching it without a UAC prompt needs an elevated watchdog.
`--install` registers a logon scheduled task with highest privileges; the viewer itself stays unelevated.
"""
import argparse
import codecs
import ctypes
import json
import logging
import logging.handlers
import os
import subprocess
import sys
import threading
import time
from collections import namedtuple
from pathlib import Path

from . import paths, shm
from .storage import atomic_write, deep_merge, read_json, write_json

TASK_NAME = "HWiNFO Viewer Watchdog"
MUTEX_NAME = "Local\\HWiNFO-Viewer-Watchdog"

DEFAULTS = {
    "exe": "",                    # path to HWiNFO64.exe; empty = detect (running process, last seen, Program Files)
    "args": ["-sensoronly"],
    "ini": "",                    # empty = HWiNFO64.INI next to the exe
    "ini_settings": {             # [Settings] keys written before every launch (silent, sensors-only start)
        "SensorsOnly": "1",
        "ShowWelcomeAndProgress": "0",
        "SensorsSM": "1",
        "MinimalizeSensors": "1",
        "AutoUpdate": "0",
    },
    "restart_after_hours": 11.5,
    "check_interval_s": 30,
    "keep_running": True,         # start HWiNFO when it isn't running
    "kill_timeout_s": 10,
    "ready_timeout_s": 90,        # how long to wait for the shared memory after a launch
    "settle_s": 1.0,              # pause between kill and relaunch (driver/handles release)
}

CANDIDATES = [r"%ProgramFiles%\HWiNFO64\HWiNFO64.exe", r"%ProgramFiles(x86)%\HWiNFO64\HWiNFO64.exe",
              r"%ProgramFiles%\HWiNFO\HWiNFO64.exe", r"%LOCALAPPDATA%\Programs\HWiNFO64\HWiNFO64.exe"]

log = logging.getLogger("hwv.watchdog")


class WatchdogError(Exception):
    pass


# --------------------------------------------------------------------------- Win32 process helpers
Proc = namedtuple("Proc", "pid created exe")  # created = epoch seconds; (pid, created) identifies a process

if sys.platform == "win32":
    from ctypes import wintypes

    _k = ctypes.WinDLL("kernel32", use_last_error=True)
    _INVALID = ctypes.c_void_p(-1).value
    PROCESS_TERMINATE, SYNCHRONIZE, QUERY_LIMITED = 0x0001, 0x00100000, 0x1000

    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD), ("th32ProcessID", wintypes.DWORD),
                    ("th32DefaultHeapID", ctypes.c_size_t), ("th32ModuleID", wintypes.DWORD),
                    ("cntThreads", wintypes.DWORD), ("th32ParentProcessID", wintypes.DWORD),
                    ("pcPriClassBase", ctypes.c_long), ("dwFlags", wintypes.DWORD), ("szExeFile", ctypes.c_wchar * 260)]

    _k.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    _k.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    _k.Process32FirstW.argtypes = _k.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESSENTRY32W)]
    _k.OpenProcess.restype = wintypes.HANDLE
    _k.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    _k.CloseHandle.argtypes = [wintypes.HANDLE]
    _k.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    _k.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
    _k.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
    _k.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    _k.CreateMutexW.restype = wintypes.HANDLE
    _k.CreateMutexW.argtypes = [ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR]


def find_pids(exe_name):
    snap = _k.CreateToolhelp32Snapshot(0x2, 0)  # TH32CS_SNAPPROCESS
    if not snap or snap == _INVALID:
        raise ctypes.WinError(ctypes.get_last_error())
    pids = []
    try:
        e = PROCESSENTRY32W()
        e.dwSize = ctypes.sizeof(e)
        ok = _k.Process32FirstW(snap, ctypes.byref(e))
        while ok:
            if e.szExeFile.lower() == exe_name.lower():
                pids.append(e.th32ProcessID)
            ok = _k.Process32NextW(snap, ctypes.byref(e))
    finally:
        _k.CloseHandle(snap)
    return pids


def _created(h):
    c, x, kt, ut = (wintypes.FILETIME() for _ in range(4))
    if not _k.GetProcessTimes(h, ctypes.byref(c), ctypes.byref(x), ctypes.byref(kt), ctypes.byref(ut)):
        return None
    return ((c.dwHighDateTime << 32) | c.dwLowDateTime) / 1e7 - 11644473600.0  # FILETIME (1601) -> epoch


def proc_info(pid):
    """Proc for pid, or None if it can't be opened (gone, or protected)."""
    h = _k.OpenProcess(QUERY_LIMITED, False, pid)
    if not h:
        return None
    try:
        created = _created(h)
        buf, n = ctypes.create_unicode_buffer(32768), wintypes.DWORD(32768)
        exe = buf.value if _k.QueryFullProcessImageNameW(h, 0, buf, ctypes.byref(n)) else ""
    finally:
        _k.CloseHandle(h)
    return Proc(pid, created, exe) if created else None


def alive(proc):
    p = proc_info(proc.pid)
    return bool(p and abs(p.created - proc.created) < 1.0)


def terminate(proc, timeout):
    """Kill proc (verifying it is still the same process, not a recycled PID). True once it is gone."""
    h = _k.OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE | QUERY_LIMITED, False, proc.pid)
    if h:
        try:
            created = _created(h)
            if created is None or abs(created - proc.created) >= 1.0:
                return True  # the PID now belongs to something else: ours is already gone
            if not _k.TerminateProcess(h, 1):
                log.warning("TerminateProcess(PID %d) failed (error %d)", proc.pid, ctypes.get_last_error())
            elif _k.WaitForSingleObject(h, int(timeout * 1000)) == 0:  # WAIT_OBJECT_0
                return True
        finally:
            _k.CloseHandle(h)
    else:
        log.warning("OpenProcess(PID %d) failed (error %d) - trying taskkill", proc.pid, ctypes.get_last_error())
    if alive(proc):
        r = subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)], capture_output=True, text=True,
                           creationflags=0x08000000, timeout=30)  # CREATE_NO_WINDOW
        log.info("taskkill: %s", (r.stdout or r.stderr).strip())
        end = time.monotonic() + timeout
        while alive(proc) and time.monotonic() < end:
            time.sleep(0.2)
    return not alive(proc)


def is_admin():
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def launch(exe, args):
    """Start HWiNFO detached from us (and from the scheduled task's job, so stopping the task doesn't kill it)."""
    si = subprocess.STARTUPINFO()
    si.dwFlags |= 0x1                          # STARTF_USESHOWWINDOW
    si.wShowWindow = 7                         # SW_SHOWMINNOACTIVE
    base = 0x00000008 | 0x00000200             # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    cmd = [str(exe)] + [str(a) for a in args]
    for flags in (base | 0x01000000, base):    # try CREATE_BREAKAWAY_FROM_JOB first
        try:
            return subprocess.Popen(cmd, cwd=str(Path(exe).parent), creationflags=flags, startupinfo=si,
                                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                    close_fds=True).pid
        except OSError as e:
            if getattr(e, "winerror", None) == 740:  # ERROR_ELEVATION_REQUIRED: we are not elevated
                log.warning("HWiNFO needs admin and the watchdog isn't elevated - a UAC prompt will appear. "
                            "Run `hwinfo_watchdog.py --install` for silent restarts.")
                params = subprocess.list2cmdline([str(a) for a in args])
                r = ctypes.windll.shell32.ShellExecuteW(None, "runas", str(exe), params, str(Path(exe).parent), 7)
                if r <= 32:
                    raise WatchdogError("could not start HWiNFO (ShellExecute error %d)" % r)
                return None
            if flags & 0x01000000 and getattr(e, "winerror", None) == 5:
                continue  # our job object forbids breakaway: start inside it
            raise WatchdogError("could not start %s: %s" % (exe, e))


def wait_ready(timeout, stop):
    """Wait until HWiNFO's shared memory is populated. Returns seconds taken, or None on timeout."""
    t0 = time.monotonic()
    last = None
    while time.monotonic() - t0 < timeout and not stop.is_set():
        try:
            shm.read_raw()
            return time.monotonic() - t0
        except shm.SMError as e:
            last = e
        stop.wait(0.5)
    if last:
        log.warning("shared memory not ready after %.0f s: %s", timeout, last)
    return None


# --------------------------------------------------------------------------- HWiNFO64.INI
def update_ini(path, values, section="Settings"):
    """Set key=value pairs in one INI section, preserving everything else byte for byte.

    The file's encoding is kept (UTF-16/UTF-8 BOM, otherwise ANSI read as latin-1, which round-trips any
    byte unchanged). Keys are matched case-insensitively. Returns True if the file was changed.
    """
    path = Path(path)
    raw = path.read_bytes() if path.exists() else b""
    if raw.startswith(codecs.BOM_UTF16_LE) or raw.startswith(codecs.BOM_UTF16_BE):
        enc = "utf-16"
    elif raw.startswith(codecs.BOM_UTF8):
        enc = "utf-8-sig"
    else:
        enc = "latin-1"
    text = raw.decode(enc)
    nl = "\r\n" if "\r\n" in text or not text else "\n"
    lines = text.splitlines()
    header = "[%s]" % section.lower()
    start = next((i for i, l in enumerate(lines) if l.strip().lower() == header), None)
    if start is None:
        if lines and lines[-1].strip():
            lines.append("")
        lines.append("[%s]" % section)
        start = len(lines) - 1
    end = next((i for i in range(start + 1, len(lines)) if lines[i].strip().startswith("[")), len(lines))

    todo = {k.lower(): (k, str(v)) for k, v in values.items()}
    changed = False
    for i in range(start + 1, end):
        key, sep, val = lines[i].partition("=")
        if sep and key.strip().lower() in todo:
            k, v = todo.pop(key.strip().lower())
            if val.strip() != v:
                lines[i] = "%s=%s" % (key.strip(), v)
                changed = True
    if todo:
        at = end
        while at > start + 1 and not lines[at - 1].strip():  # keep the section's trailing blank lines after our keys
            at -= 1
        lines[at:at] = ["%s=%s" % kv for kv in todo.values()]
        changed = True
    if changed:
        backup = path.with_name(path.name + ".hwv-backup")
        if raw and not backup.exists():
            backup.write_bytes(raw)
        atomic_write(path, (nl.join(lines) + nl).encode(enc))
    return changed


# --------------------------------------------------------------------------- the watchdog
def load_config(path=None):
    path = Path(path or paths.WATCHDOG_CONFIG)
    cfg = json.loads(json.dumps(DEFAULTS))
    user = read_json(path, None)
    if isinstance(user, dict):
        deep_merge(cfg, user)
    elif not path.exists():
        try:
            write_json(path, DEFAULTS)  # leave an editable copy with every option
        except OSError:
            pass
    return cfg


class Watchdog:
    def __init__(self, cfg, status_path=None):
        self.cfg = cfg
        self.status_path = Path(status_path or paths.WATCHDOG_STATUS)
        prev = read_json(self.status_path, {}) or {}
        self.exe = cfg.get("exe") or prev.get("exe") or ""
        self.exe_name = Path(cfg.get("exe") or "HWiNFO64.exe").name
        self.limit = float(cfg["restart_after_hours"]) * 3600
        self.interval = max(5.0, float(cfg["check_interval_s"]))
        self.stop = threading.Event()
        self.state = {"pid": os.getpid(), "state": "starting", "exe": self.exe, "restarts": prev.get("restarts", 0),
                      "last_restart": prev.get("last_restart"), "hwinfo_pid": None, "hwinfo_started": None,
                      "restart_at": None, "message": ""}
        self.first_seen = {}
        self.expired_since = None

    # ---- status
    def publish(self, **kw):
        self.state.update(kw, updated=time.time())
        try:
            write_json(self.status_path, self.state)
        except OSError as e:
            log.warning("could not write status: %s", e)

    # ---- helpers
    def resolve_exe(self):
        for c in [self.exe] + [os.path.expandvars(p) for p in CANDIDATES]:
            if c and Path(c).is_file():
                return str(Path(c))
        raise WatchdogError("%s not found - set \"exe\" in %s" % (self.exe_name, paths.WATCHDOG_CONFIG))

    def ini_path(self, exe):
        return Path(self.cfg.get("ini") or Path(exe).with_name("HWiNFO64.INI"))

    def running(self):
        procs = []
        for pid in find_pids(self.exe_name):
            p = proc_info(pid)
            if p is None:  # couldn't open it (not elevated?) - fall back to when we first saw it
                p = Proc(pid, self.first_seen.setdefault(pid, time.time()), "")
            procs.append(p)
        return sorted(procs, key=lambda p: p.created)

    def start_hwinfo(self):
        exe = self.resolve_exe()
        self.exe = exe
        if self.cfg.get("ini_settings"):
            ini = self.ini_path(exe)
            try:
                if update_ini(ini, self.cfg["ini_settings"]):
                    log.info("updated %s: %s", ini, self.cfg["ini_settings"])
            except OSError as e:
                log.warning("could not update %s (%s) - HWiNFO may show its start-up dialogs", ini, e)
        log.info("starting %s %s", exe, " ".join(self.cfg.get("args") or []))
        self.publish(state="restarting", exe=exe, message="starting HWiNFO")
        launch(exe, self.cfg.get("args") or [])
        took = wait_ready(float(self.cfg["ready_timeout_s"]), self.stop)
        if took is not None:
            log.info("shared memory is up after %.1f s", took)
        return took

    def cycle(self, proc, why):
        log.info("restarting HWiNFO (PID %d, up %.2f h): %s", proc.pid, (time.time() - proc.created) / 3600, why)
        self.publish(state="restarting", message=why)
        t0 = time.monotonic()
        if not terminate(proc, float(self.cfg["kill_timeout_s"])):
            raise WatchdogError("could not terminate HWiNFO (PID %d) - is the watchdog elevated?" % proc.pid)
        self.stop.wait(float(self.cfg["settle_s"]))
        took = self.start_hwinfo()
        restarts = self.state.get("restarts", 0) + 1
        self.publish(restarts=restarts, last_restart=time.time())
        if took is not None:
            log.info("restart #%d complete, data gap %.1f s", restarts, time.monotonic() - t0)

    # ---- loop
    def step(self):
        """One check. Returns how long to sleep before the next one."""
        procs = self.running()
        if not procs:
            self.expired_since = None
            other = read_json(self.status_path, None) or {}
            if other.get("pid") != os.getpid() and other.get("state") == "restarting" \
                    and time.time() - other.get("updated", 0) < float(self.cfg["ready_timeout_s"]) + 30:
                return 5.0  # a one-shot --restart-now is mid-restart: don't launch a second copy
            if not self.cfg.get("keep_running", True):
                self.publish(state="waiting", hwinfo_pid=None, restart_at=None, message="HWiNFO is not running")
                return self.interval
            log.info("HWiNFO is not running")
            self.start_hwinfo()
            return 2.0  # pick up the new process quickly
        p = procs[0]
        if p.exe:
            self.exe = p.exe
        restart_at = p.created + self.limit
        now = time.time()
        if now >= restart_at:
            self.cycle(p, "uptime limit (%.1f h) reached" % (self.limit / 3600))
            return 2.0
        try:
            shm.read_raw()
            self.expired_since = None
        except shm.SMError as e:
            if e.kind == shm.EXPIRED:
                self.expired_since = self.expired_since or now
                if now - self.expired_since > 20:
                    self.cycle(p, "shared memory expired early")
                    return 2.0
        self.publish(state="monitoring", exe=self.exe, hwinfo_pid=p.pid, hwinfo_started=p.created,
                     restart_at=restart_at, message="")
        return max(1.0, min(self.interval, restart_at - now))

    def run(self):
        log.info("watchdog started (PID %d, elevated=%s, restart after %.2f h)", os.getpid(), is_admin(), self.limit / 3600)
        if not is_admin():
            log.warning("not elevated: killing/starting HWiNFO will fail or show UAC prompts - use --install")
        backoff = 0.0
        while not self.stop.is_set():
            try:
                wait = self.step()
                backoff = 0.0
            except Exception as e:  # keep watching whatever happens; back off so a persistent error doesn't spin
                backoff = min(300.0, max(5.0, backoff * 2))
                log.error("%s (retrying in %.0f s)", e, backoff, exc_info=not isinstance(e, WatchdogError))
                self.publish(state="error", message=str(e))
                wait = backoff
            self.stop.wait(wait)
        self.publish(state="stopped", message="")
        log.info("watchdog stopped")


# --------------------------------------------------------------------------- install / CLI
def setup_logging(console):
    paths.LOGS.mkdir(parents=True, exist_ok=True)
    fmt = logging.Formatter("%(asctime)s %(levelname)s %(message)s")
    fh = logging.handlers.RotatingFileHandler(paths.LOGS / "watchdog.log", maxBytes=512 * 1024, backupCount=2, encoding="utf-8")
    fh.setFormatter(fmt)
    log.addHandler(fh)
    if console and sys.stderr:
        sh = logging.StreamHandler()
        sh.setFormatter(fmt)
        log.addHandler(sh)
    log.setLevel(logging.INFO)


def _ps(s):
    return "'" + str(s).replace("'", "''") + "'"


def _powershell(script):
    r = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
                       capture_output=True, text=True)
    return r.returncode, (r.stdout + r.stderr).strip()


def _elevate(argv):
    params = subprocess.list2cmdline([str(paths.ROOT / "hwinfo_watchdog.py")] + argv + ["--pause"])
    r = ctypes.windll.shell32.ShellExecuteW(None, "runas", sys.executable, params, str(paths.ROOT), 1)
    if r <= 32:
        raise SystemExit("elevation was cancelled or failed (%d)" % r)
    print("Continuing in the elevated window...")


def install(user):
    pyw = Path(sys.executable).with_name("pythonw.exe")
    exe = pyw if pyw.exists() else Path(sys.executable)
    script = paths.ROOT / "hwinfo_watchdog.py"
    ps = "\n".join([
        "$ErrorActionPreference = 'Stop'",
        "$a = New-ScheduledTaskAction -Execute %s -Argument %s -WorkingDirectory %s" % (_ps(exe), _ps('"%s"' % script), _ps(paths.ROOT)),
        "$t = New-ScheduledTaskTrigger -AtLogOn -User %s" % _ps(user),
        "$s = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries "
        "-DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew "
        "-RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)",
        "$p = New-ScheduledTaskPrincipal -UserId %s -LogonType Interactive -RunLevel Highest" % _ps(user),
        "Register-ScheduledTask -TaskName %s -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null" % _ps(TASK_NAME),
        "Start-ScheduledTask -TaskName %s" % _ps(TASK_NAME),
    ])
    code, out = _powershell(ps)
    if code:
        raise SystemExit("could not register the task:\n" + out)
    print("Installed and started scheduled task '%s' for %s (runs at logon, elevated)." % (TASK_NAME, user))
    print("Log: %s" % (paths.LOGS / "watchdog.log"))


def uninstall():
    code, out = _powershell("Stop-ScheduledTask -TaskName %s -ErrorAction SilentlyContinue; "
                            "Unregister-ScheduledTask -TaskName %s -Confirm:$false" % (_ps(TASK_NAME), _ps(TASK_NAME)))
    print("Removed scheduled task '%s'." % TASK_NAME if code == 0 else "could not remove the task:\n" + out)


def status():
    code, out = _powershell("(Get-ScheduledTask -TaskName %s -ErrorAction Stop).State" % _ps(TASK_NAME))
    print("scheduled task:", out if code == 0 else "not installed")
    st = read_json(paths.WATCHDOG_STATUS, None)
    if not st:
        print("no status yet (%s)" % paths.WATCHDOG_STATUS)
        return
    fmt = lambda t: time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(t)) if t else "-"
    age = time.time() - st.get("updated", 0)
    print("watchdog: %s%s (updated %.0f s ago)" % (st.get("state"), " - " + st["message"] if st.get("message") else "", age))
    print("HWiNFO:   PID %s, started %s" % (st.get("hwinfo_pid") or "-", fmt(st.get("hwinfo_started"))))
    print("restart:  next %s, last %s, total %s" % (fmt(st.get("restart_at")), fmt(st.get("last_restart")), st.get("restarts", 0)))


def main(argv=None, doc=None):
    ap = argparse.ArgumentParser(description=doc, formatter_class=argparse.RawDescriptionHelpFormatter)
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--install", action="store_true", help="register + start the elevated logon task (asks for admin)")
    g.add_argument("--uninstall", action="store_true", help="stop and remove the scheduled task")
    g.add_argument("--status", action="store_true", help="show task state and the last status")
    g.add_argument("--restart-now", action="store_true", help="restart HWiNFO once, then exit")
    ap.add_argument("--config", help="watchdog.json to use (default data/watchdog.json)")
    ap.add_argument("--for-user", help=argparse.SUPPRESS)
    ap.add_argument("--pause", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    if sys.platform != "win32":
        raise SystemExit("the watchdog is Windows-only")
    paths.DATA.mkdir(exist_ok=True)
    try:
        if a.status:
            return status()
        if a.install or a.uninstall or a.restart_now:
            if not is_admin():
                user = "%s\\%s" % (os.environ.get("USERDOMAIN", "."), os.environ.get("USERNAME", ""))
                fwd = ["--install", "--for-user", user] if a.install else ["--uninstall"] if a.uninstall else ["--restart-now"]
                return _elevate(fwd + (["--config", a.config] if a.config else []))
            if a.install:
                return install(a.for_user or "%s\\%s" % (os.environ.get("USERDOMAIN", "."), os.environ.get("USERNAME", "")))
            if a.uninstall:
                return uninstall()
        setup_logging(console=True)
        wd = Watchdog(load_config(a.config))
        if a.restart_now:  # one-shot; a running watchdog simply picks up the new process
            procs = wd.running()
            wd.cycle(procs[0], "manual restart") if procs else wd.start_hwinfo()
            return
        mutex = _k.CreateMutexW(None, False, MUTEX_NAME)
        if ctypes.get_last_error() == 183:  # ERROR_ALREADY_EXISTS
            raise SystemExit("the watchdog is already running")
        try:
            wd.run()
        except KeyboardInterrupt:
            wd.publish(state="stopped", message="")
        finally:
            _k.CloseHandle(mutex)
    finally:
        if a.pause:
            input("\nPress Enter to close...")
