"""HWiNFO64 shared memory (Global\\HWiNFO_SENS_SM2): parsing, a resilient reader, and a mock source.

The producer (HWiNFO) can disappear at any moment - it is closed, crashes, hits the free version's
12 h limit, or is restarted by the watchdog (a 2-5 s gap, longer while it re-scans sensors). So:

* every read opens the mapping, copies it and closes it again. We never hold the section open
  between reads, which means a restarted HWiNFO always creates a fresh section instead of
  inheriting a stale one that only we kept alive;
* every byte we read is bounds-checked against the mapped view (VirtualQuery), so a half-built
  or shrunk section can't cause an access violation;
* the header is validated (zero sensors/readings or a zero poll time = "starting up");
* failures back off exponentially (0.25 s -> 5 s) and every failure is an SMError carrying a
  `kind`, so callers never see raw OSError/struct errors.
"""
import random
import re
import struct
import sys
import threading
import time
from collections import namedtuple

SM_NAME = "Global\\HWiNFO_SENS_SM2"
HDR = struct.Struct("<IIIqIIIIII")  # sig, ver, rev, poll_time, off/size/num sensors, off/size/num readings
SENSOR = struct.Struct("<II128s128s")  # id, instance, name_orig, name_user
READING = struct.Struct("<III128s128s16sdddd")  # type, sensor_idx, id, label_orig, label_user, unit, cur, min, max, avg
DEAD_SIGNATURE = 0xDEADDEAD
MAX_SENSORS, MAX_READINGS = 10000, 100000

Reading = namedtuple("Reading", "sensor label label_orig unit type cur min max avg")

# SMError.kind values
MISSING = "missing"    # no mapping: HWiNFO not running / shared memory off / mid-restart
STARTING = "starting"  # mapping exists but isn't populated yet
STALE = "stale"        # mapping exists but HWiNFO stopped updating it
EXPIRED = "expired"    # free version's 12 h limit (signature DEAD)
INVALID = "invalid"    # unexpected layout

MSG_MISSING = ("HWiNFO shared memory not found - is HWiNFO running with Sensors open and "
               "'Shared Memory Support' ticked? (The free version switches it off after 12 h.)")
MSG_EXPIRED = ("HWiNFO shared memory is inactive (the free version's 12 h limit has probably expired - "
               "re-tick Shared Memory Support in HWiNFO Settings, or let the watchdog restart HWiNFO)")
MSG_STALE = ("HWiNFO stopped updating - if it has been ~12 h, re-tick Shared Memory Support "
             "(HWiNFO Settings > General / User Interface).")


class SMError(Exception):
    def __init__(self, msg, kind=INVALID):
        super().__init__(msg)
        self.kind = kind


def cstr(b):
    return b.split(b"\0", 1)[0].decode("cp1252", "replace")


def norm_unit(u):
    u = u.strip()
    return "°C" if re.fullmatch(r".?C", u) else u


def check_header(head):
    """Validate the header and return (poll, total_bytes). Raises SMError for anything unusable."""
    if len(head) < HDR.size:
        raise SMError("HWiNFO shared memory is still being created", STARTING)
    sig, _ver, _rev, poll, so, ss, sn, ro, rs, rn = HDR.unpack_from(head, 0)
    if sig == DEAD_SIGNATURE:
        raise SMError(MSG_EXPIRED, EXPIRED)
    if sig == 0 or poll == 0 or sn == 0 or rn == 0:
        raise SMError("HWiNFO is starting up (shared memory not populated yet)", STARTING)
    if ss < SENSOR.size or rs < READING.size or sn > MAX_SENSORS or rn > MAX_READINGS \
            or so < HDR.size or ro < HDR.size:
        raise SMError("unexpected shared-memory layout (signature 0x%08X)" % sig, INVALID)
    return poll, max(so + ss * sn, ro + rs * rn)


def parse_buffer(buf):
    poll, total = check_header(buf[:HDR.size])
    if len(buf) < total:
        raise SMError("shared memory truncated", STARTING)
    _sig, _ver, _rev, _poll, so, ss, sn, ro, rs, rn = HDR.unpack_from(buf, 0)
    sensors = []
    for i in range(sn):
        _sid, _inst, orig, user = SENSOR.unpack_from(buf, so + i * ss)
        sensors.append(cstr(user) or cstr(orig))
    out = []
    for i in range(rn):
        t, si, _rid, lo, lu, unit, cur, vmin, vmax, vavg = READING.unpack_from(buf, ro + i * rs)
        orig = cstr(lo)
        out.append(Reading(sensors[si] if si < len(sensors) else "?", cstr(lu) or orig, orig,
                           norm_unit(cstr(unit)), t, cur, vmin, vmax, vavg))
    return poll, out


# --------------------------------------------------------------------------- Win32 access
_api = None


def _win32():
    """kernel32 bindings, created once on first use."""
    global _api
    if _api is None:
        import ctypes
        from ctypes import wintypes

        class MBI(ctypes.Structure):  # MEMORY_BASIC_INFORMATION (ctypes inserts the 64-bit padding)
            _fields_ = [("BaseAddress", ctypes.c_void_p), ("AllocationBase", ctypes.c_void_p),
                        ("AllocationProtect", wintypes.DWORD), ("RegionSize", ctypes.c_size_t),
                        ("State", wintypes.DWORD), ("Protect", wintypes.DWORD), ("Type", wintypes.DWORD)]

        k = ctypes.WinDLL("kernel32", use_last_error=True)
        k.OpenFileMappingW.restype = wintypes.HANDLE
        k.OpenFileMappingW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
        k.MapViewOfFile.restype = ctypes.c_void_p
        k.MapViewOfFile.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.c_size_t]
        k.UnmapViewOfFile.argtypes = [ctypes.c_void_p]
        k.CloseHandle.argtypes = [wintypes.HANDLE]
        k.VirtualQuery.restype = ctypes.c_size_t
        k.VirtualQuery.argtypes = [ctypes.c_void_p, ctypes.POINTER(MBI), ctypes.c_size_t]
        _api = (ctypes, k, MBI)
    return _api


def read_raw():
    """One complete open -> copy -> close cycle. Returns (poll, readings) or raises SMError."""
    if sys.platform != "win32":
        raise SMError("live mode needs Windows (HWiNFO shared memory) - use --mock", MISSING)
    ctypes, k, MBI = _win32()
    h = k.OpenFileMappingW(4, False, SM_NAME)  # FILE_MAP_READ
    if not h:
        raise SMError(MSG_MISSING, MISSING)
    try:
        ptr = k.MapViewOfFile(h, 4, 0, 0, 0)
        if not ptr:
            raise SMError("could not map HWiNFO shared memory", MISSING)
        try:
            mbi = MBI()
            view = mbi.RegionSize if k.VirtualQuery(ptr, ctypes.byref(mbi), ctypes.sizeof(mbi)) else 0
            if view < HDR.size:
                raise SMError("HWiNFO shared memory is still being created", STARTING)
            _poll, total = check_header(ctypes.string_at(ptr, HDR.size))
            if total > view:
                raise SMError("shared memory smaller than its header claims (%d > %d bytes)" % (total, view), STARTING)
            buf = ctypes.string_at(ptr, total)
        except OSError as e:  # ctypes turns an access violation into OSError
            raise SMError("could not read HWiNFO shared memory: %s" % e, STARTING)
        finally:
            k.UnmapViewOfFile(ptr)
    finally:
        k.CloseHandle(h)
    return parse_buffer(buf)


class Backoff:
    """Exponential retry gate: after a failure, attempts are skipped until the delay has passed."""

    def __init__(self, first=0.25, cap=5.0):
        self.first, self.cap = first, cap
        self.delay, self.next_at, self.err = 0.0, 0.0, None

    def blocked(self, now):
        return self.err is not None and now < self.next_at

    def fail(self, now, err):
        self.delay = min(self.cap, max(self.first, self.delay * 2))
        self.next_at, self.err = now + self.delay, err

    def ok(self):
        self.delay, self.next_at, self.err = 0.0, 0.0, None


class SharedMemorySource:
    def __init__(self, stale_after=15.0, reader=read_raw, clock=time.monotonic):
        if sys.platform != "win32" and reader is read_raw:
            raise SystemExit("Live mode needs Windows (HWiNFO shared memory). Use --mock to try the UI.")
        self.stale_after, self._reader, self._clock = stale_after, reader, clock
        self.backoff = Backoff()
        self.lock = threading.Lock()
        self.last_poll, self.last_change = None, clock()

    def read(self):
        with self.lock:
            now = self._clock()
            if self.backoff.blocked(now):
                e = self.backoff.err
                raise SMError(str(e), e.kind)  # fresh instance: re-raising one object grows its traceback
            try:
                poll, readings = self._reader()
                if poll != self.last_poll:
                    self.last_poll, self.last_change = poll, now
                elif now - self.last_change > self.stale_after:
                    raise SMError(MSG_STALE, STALE)  # HWiNFO paused/hung, or a 3rd-party reader keeps a dead section alive
            except SMError as e:
                self.backoff.fail(now, e)
                raise
            self.backoff.ok()
            return poll, readings


# --------------------------------------------------------------------------- mock
_CPU = "CPU [#0]: AMD Ryzen 7 5800X: Enhanced"
_ITE = "GIGABYTE B550 AORUS ELITE V2 (ITE IT8688E)"
_GPU = "GPU [#0]: NVIDIA GeForce RTX 3070 Ti: GIGABYTE AORUS"
_C = "°C"


class MockSource:
    """Plausible fake data (names taken from a real 5800X / B550 / 3070 Ti setup).

    outage_every=N simulates a 4 s HWiNFO restart every N seconds, to exercise reconnection.
    """
    SPEC = [
        (_CPU, "CPU (Tctl/Tdie)", _C, 1, 43, 4), (_CPU, "CPU Die (average)", _C, 1, 40, 3),
        (_CPU, "CPU CCD1 (Tdie)", _C, 1, 42, 3),
        *[(_CPU, "Core %d (Tdie)" % i, _C, 1, 36 + i % 3, 3) for i in range(8)],
        (_CPU, "L3 Cache 0", _C, 1, 36, 2), (_CPU, "Core 0 Power", "W", 5, 0.8, 0.3),
        (_CPU, "L3 Clocks", "MHz", 6, 3700, 40),
        (_CPU, "CPU Core Voltage (SVI2 TFN)", "V", 2, 1.0, 0.03), (_CPU, "CPU Core VID (Effective)", "V", 2, 1.0, 0.03),
        (_CPU, "CPU Package Power", "W", 5, 30, 8), (_CPU, "Power Reporting Deviation (Accuracy)", "%", 8, 85, 8),
        (_ITE, "System1", _C, 1, 33, 1), (_ITE, "System2", _C, 1, 33, 1), (_ITE, "CPU", _C, 1, 41, 2),
        (_ITE, "PCIEX16", _C, 1, 41, 1), (_ITE, "VRM MOS", _C, 1, 41, 1), (_ITE, "PCH", _C, 1, 42, 1),
        (_ITE, "CPU VCORE SoC", "V", 2, 1.188, 0.004), (_ITE, "System 1", "RPM", 3, 560, 25),
        ("S.M.A.R.T.: ST2000DM001-1E6164 (Z1E6RBQZ) [F:]", "Drive Temperature", _C, 1, 34, 1),
        ("S.M.A.R.T.: ST1000DM003-1ER162 (Z4Y9XRDY) [G:]", "Drive Temperature", _C, 1, 36, 1),
        ("S.M.A.R.T.: KINGSTON SA400S37960G (50026B77849761C0) [E:]", "Drive Temperature", _C, 1, 31, 1),
        ("S.M.A.R.T.: KINGSTON SA400S37240G (50026B7783D833BC) [H:]", "Drive Temperature", _C, 1, 30, 1),
        ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature", _C, 1, 42, 2),
        ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature 2", _C, 1, 48, 2),
        ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature 3", _C, 1, 44, 2),
        ("S.M.A.R.T.: XPG SPECTRIX S40G (2J3720095834) [C:]", "Drive Temperature", _C, 1, 38, 2),
        ("Drive: XPG SPECTRIX S40G (2J3720095834) [C:]", "Read Activity", "%", 7, 1, 1),
        (_GPU, "GPU Temperature", _C, 1, 50, 5), (_GPU, "GPU Memory Junction Temperature", _C, 1, 56, 5),
        (_GPU, "GPU Hot Spot Temperature", _C, 1, 62, 5), (_GPU, "GPU Core Voltage", "V", 2, 0.956, 0.05),
        (_GPU, "GPU Fan1", "RPM", 3, 600, 80), (_GPU, "GPU Fan2", "RPM", 3, 600, 80),
        (_GPU, "GPU Clock", "MHz", 6, 1875, 120), (_GPU, "GPU Fan1", "%", 7, 20, 3),
    ]
    OUTAGE = 4.0

    def __init__(self, outage_every=0):
        self.state = [None] * len(self.SPEC)
        self.outage_every, self.t0 = outage_every, time.monotonic()

    def read(self):
        if self.outage_every:
            phase = (time.monotonic() - self.t0) % self.outage_every
            if phase > self.outage_every - self.OUTAGE:
                self.state = [None] * len(self.SPEC)  # a restarted HWiNFO resets min/max/avg
                raise SMError(MSG_MISSING + " [mock outage]", MISSING)
        out = []
        for i, (sensor, label, unit, typ, base, jit) in enumerate(self.SPEC):
            v = base + random.uniform(-jit, jit)
            s = self.state[i]
            if s is None:
                s = self.state[i] = {"min": v, "max": v, "sum": 0.0, "n": 0}
            s["min"], s["max"] = min(s["min"], v), max(s["max"], v)
            s["sum"] += v
            s["n"] += 1
            out.append(Reading(sensor, label, label, unit, typ, v, s["min"], s["max"], s["sum"] / s["n"]))
        return time.time(), out
