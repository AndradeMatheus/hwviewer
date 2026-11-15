"""Stdlib-only tests for the parts that must not break: SHM parsing, reconnection, INI editing, storage, grace window.

    python -m unittest discover -s tests -v
"""
import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from hwv import server, shm, storage  # noqa: E402
from hwv.watchdog import update_ini  # noqa: E402


def make_buffer(sig=0x12345678, poll=1, readings=(("Sensor A", "Temp", "C", 42.0),)):
    sensors = sorted({r[0] for r in readings})
    so = shm.HDR.size
    ro = so + shm.SENSOR.size * len(sensors)
    buf = bytearray(shm.HDR.pack(sig, 2, 0, poll, so, shm.SENSOR.size, len(sensors), ro, shm.READING.size, len(readings)))
    for i, name in enumerate(sensors):
        buf += shm.SENSOR.pack(i, 0, name.encode(), b"")
    for i, (sname, label, unit, v) in enumerate(readings):
        buf += shm.READING.pack(1, sensors.index(sname), i, label.encode(), b"", unit.encode(), v, v - 1, v + 1, v)
    return bytes(buf)


class ParseTests(unittest.TestCase):
    def test_roundtrip(self):
        poll, rs = shm.parse_buffer(make_buffer())
        self.assertEqual(poll, 1)
        self.assertEqual((rs[0].sensor, rs[0].label, rs[0].unit, rs[0].cur), ("Sensor A", "Temp", "°C", 42.0))

    def test_dead_signature_is_expired(self):
        with self.assertRaises(shm.SMError) as c:
            shm.parse_buffer(make_buffer(sig=shm.DEAD_SIGNATURE))
        self.assertEqual(c.exception.kind, shm.EXPIRED)

    def test_unpopulated_header_is_starting(self):
        for buf in (b"", bytes(shm.HDR.size), make_buffer(poll=0), make_buffer(readings=())):
            with self.assertRaises(shm.SMError) as c:
                shm.parse_buffer(buf)
            self.assertEqual(c.exception.kind, shm.STARTING, buf[:8])

    def test_truncated(self):
        with self.assertRaises(shm.SMError):
            shm.parse_buffer(make_buffer()[:-10])

    def test_absurd_counts_rejected(self):
        buf = bytearray(make_buffer())
        struct.pack_into("<I", buf, 4 * 3 + 8 + 8, 10 ** 6)  # num sensors
        with self.assertRaises(shm.SMError) as c:
            shm.parse_buffer(bytes(buf))
        self.assertEqual(c.exception.kind, shm.INVALID)


class Clock:
    def __init__(self):
        self.t = 100.0

    def __call__(self):
        return self.t


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.clock, self.calls, self.script = Clock(), 0, []

    def reader(self):
        self.calls += 1
        r = self.script.pop(0)
        if isinstance(r, Exception):
            raise r
        return r, []

    def src(self):
        return shm.SharedMemorySource(stale_after=15, reader=self.reader, clock=self.clock)

    def test_backoff_skips_attempts_then_recovers(self):
        s = self.src()
        missing = shm.SMError("gone", shm.MISSING)
        self.script = [missing, missing, 7]
        with self.assertRaises(shm.SMError):
            s.read()                       # attempt 1 fails -> wait 0.25 s
        with self.assertRaises(shm.SMError):
            s.read()                       # inside the backoff: no attempt made
        self.assertEqual(self.calls, 1)
        self.clock.t += 0.3
        with self.assertRaises(shm.SMError):
            s.read()                       # attempt 2 fails -> wait 0.5 s
        self.assertEqual(s.backoff.delay, 0.5)
        self.clock.t += 0.6
        self.assertEqual(s.read()[0], 7)   # recovered, backoff reset
        self.assertEqual(s.backoff.delay, 0)

    def test_backoff_is_capped(self):
        s = self.src()
        self.script = [shm.SMError("x", shm.MISSING)] * 20
        for _ in range(20):
            try:
                s.read()
            except shm.SMError:
                pass
            self.clock.t += 10
        self.assertEqual(s.backoff.delay, shm.Backoff().cap)

    def test_stale_poll_detected(self):
        s = self.src()
        self.script = [5, 5, 6]
        s.read()
        self.clock.t += 16
        with self.assertRaises(shm.SMError) as c:
            s.read()
        self.assertEqual(c.exception.kind, shm.STALE)
        self.clock.t += 1
        self.assertEqual(s.read()[0], 6)   # a new poll value (restarted HWiNFO) clears it


class IniTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.p = self.dir / "HWiNFO64.INI"

    def test_key_added_to_middle_section(self):  # the case the old code silently skipped
        self.p.write_bytes(b"[Settings]\r\nFoo=1\r\n\r\n[Other]\r\nBar=2\r\n")
        self.assertTrue(update_ini(self.p, {"SensorsSM": "1"}))
        self.assertEqual(self.p.read_bytes(), b"[Settings]\r\nFoo=1\r\nSensorsSM=1\r\n\r\n[Other]\r\nBar=2\r\n")

    def test_existing_key_updated_case_insensitive(self):
        self.p.write_bytes(b"[settings]\nsensorssm=0\n")
        update_ini(self.p, {"SensorsSM": "1"})
        self.assertEqual(self.p.read_bytes(), b"[settings]\nsensorssm=1\n")

    def test_no_change_no_write(self):
        self.p.write_bytes(b"[Settings]\r\nSensorsSM=1\r\n")
        self.assertFalse(update_ini(self.p, {"SensorsSM": "1"}))
        self.assertFalse((self.dir / "HWiNFO64.INI.hwv-backup").exists())

    def test_ansi_bytes_preserved_and_backup_made(self):
        raw = b"[Settings]\r\nName=Ol\xe1 \xe7\r\n"
        self.p.write_bytes(raw)
        update_ini(self.p, {"SensorsOnly": "1"})
        self.assertEqual(self.p.read_bytes(), b"[Settings]\r\nName=Ol\xe1 \xe7\r\nSensorsOnly=1\r\n")
        self.assertEqual((self.dir / "HWiNFO64.INI.hwv-backup").read_bytes(), raw)

    def test_missing_file_and_section(self):
        update_ini(self.p, {"SensorsSM": "1"})
        self.assertEqual(self.p.read_bytes(), b"[Settings]\r\nSensorsSM=1\r\n")
        p2 = self.dir / "b.ini"
        p2.write_bytes(b"[Other]\nX=1\n")
        update_ini(p2, {"A": "1"})
        self.assertEqual(p2.read_bytes(), b"[Other]\nX=1\n\n[Settings]\nA=1\n")


class StorageTests(unittest.TestCase):
    def test_atomic_write_leaves_no_temp(self):
        d = Path(tempfile.mkdtemp())
        storage.atomic_write(d / "s.json", "{}")
        storage.atomic_write(d / "s.json", "{\"a\": 1}")
        self.assertEqual([p.name for p in d.iterdir()], ["s.json"])

    def test_corrupt_settings_kept_as_bad(self):
        d = Path(tempfile.mkdtemp())
        (d / "settings.json").write_text("{nope", "utf-8")
        s = storage.Settings(d / "settings.json")
        self.assertEqual(s.get()["window"]["opacity"], 1.0)
        self.assertTrue((d / "settings.json.bad").exists())

    def test_safe_name_blocks_traversal(self):
        self.assertEqual(storage.safe_name("../../evil"), "evil")
        self.assertIsNone(storage.safe_name("../"))


class FlakySource:
    def __init__(self):
        self.fail = False

    def read(self):
        if self.fail:
            raise shm.SMError("gone", shm.MISSING)
        return 1, [shm.Reading("AMD Ryzen 7 5800X: Enhanced", "CPU (Tctl/Tdie)", "CPU (Tctl/Tdie)", "°C", 1, 50, 40, 60, 45)]


class GraceTests(unittest.TestCase):
    def test_last_values_held_then_error(self):
        d = Path(tempfile.mkdtemp())
        src = FlakySource()
        app = server.App(src, settings=storage.Settings(d / "settings.json"))
        app.watchdog = server.WatchdogStatus(d / "none.json")
        self.assertTrue(app.snapshot()["ok"])
        src.fail = True
        app.cache_t = 0
        p = app.snapshot()
        self.assertTrue(p["ok"] and p["reconnecting"])          # inside the grace window: last values
        app.cache_t, app.good_t = 0, app.good_t - server.GRACE - 1
        p = app.snapshot()
        self.assertFalse(p["ok"])                                  # grace over: a real error
        self.assertEqual(p["kind"], shm.MISSING)
        src.fail = False
        app.cache_t = 0
        p = app.snapshot()
        self.assertTrue(p["ok"] and not p.get("reconnecting"))


if __name__ == "__main__":
    unittest.main()
