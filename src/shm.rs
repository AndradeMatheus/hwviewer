//! HWiNFO64 shared memory (`Global\HWiNFO_SENS_SM2`): parsing, a resilient reader, and a mock source.
//!
//! HWiNFO can vanish at any moment (closed, crashed, 12 h limit, watchdog restart), so:
//! * every read opens the mapping, copies it and closes it again - we never keep a dead section alive;
//! * every byte read is bounds-checked against the mapped view (`VirtualQuery`);
//! * the header is validated (zero sensors/readings or a zero poll time = "starting up");
//! * failures back off exponentially (0.25 s -> 5 s) and are always an [`SmError`] with a [`Kind`].

use std::fmt;
use std::sync::OnceLock;
use std::time::Instant;

pub const SM_NAME: &str = "Global\\HWiNFO_SENS_SM2";
pub const HDR_SIZE: usize = 44; // <IIIqIIIIII: sig, ver, rev, poll_time, off/size/num sensors, off/size/num readings
pub const SENSOR_SIZE: usize = 264; // <II128s128s: id, instance, name_orig, name_user
pub const READING_SIZE: usize = 316; // <III128s128s16sdddd: type, sensor_idx, id, label_orig, label_user, unit, cur/min/max/avg
pub const DEAD_SIGNATURE: u32 = 0xDEAD_DEAD;
const MAX_SENSORS: u32 = 10_000;
const MAX_READINGS: u32 = 100_000;

pub const MSG_MISSING: &str = "HWiNFO shared memory not found - is HWiNFO running with Sensors open and \
    'Shared Memory Support' ticked? (The free version switches it off after 12 h.)";
pub const MSG_EXPIRED: &str = "HWiNFO shared memory is inactive (the free version's 12 h limit has probably expired - \
    re-tick Shared Memory Support in HWiNFO Settings, or let the watchdog restart HWiNFO)";
pub const MSG_STALE: &str = "HWiNFO stopped updating - if it has been ~12 h, re-tick Shared Memory Support \
    (HWiNFO Settings > General / User Interface).";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Missing,  // no mapping: HWiNFO not running / shared memory off / mid-restart
    Starting, // mapping exists but isn't populated yet
    Stale,    // mapping exists but HWiNFO stopped updating it
    Expired,  // free version's 12 h limit (signature DEAD)
    Invalid,  // unexpected layout
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Missing => "missing",
            Kind::Starting => "starting",
            Kind::Stale => "stale",
            Kind::Expired => "expired",
            Kind::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SmError {
    pub msg: String,
    pub kind: Kind,
}

impl SmError {
    pub fn new(msg: impl Into<String>, kind: Kind) -> Self {
        SmError { msg: msg.into(), kind }
    }
}

impl fmt::Display for SmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for SmError {}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub sensor: String,
    pub label: String,
    pub label_orig: String,
    pub unit: String,
    pub typ: u32,
    pub cur: f64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
}

/// (poll_time, readings)
pub type Snapshot = (i64, Vec<Reading>);

pub trait DataSource: Send {
    fn read(&mut self) -> Result<Snapshot, SmError>;
}

/// Seconds on a monotonic clock (process-relative).
pub fn mono() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Seconds since the Unix epoch.
pub fn now_epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// --------------------------------------------------------------------------- parsing
fn u32_at(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(a)
}

fn i64_at(b: &[u8], o: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    i64::from_le_bytes(a)
}

fn f64_at(b: &[u8], o: usize) -> f64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    f64::from_le_bytes(a)
}

/// Windows-1252 code points for 0x80..=0x9F (0 = undefined -> U+FFFD, like Python's "replace").
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017D, 0, 0,
    0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

/// NUL-terminated cp1252 string.
pub fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end]
        .iter()
        .map(|&c| match c {
            0x80..=0x9F => char::from_u32(CP1252_HIGH[(c - 0x80) as usize] as u32)
                .filter(|_| CP1252_HIGH[(c - 0x80) as usize] != 0)
                .unwrap_or('\u{FFFD}'),
            _ => c as char, // the rest of cp1252 is identical to Latin-1
        })
        .collect()
}

/// HWiNFO writes the degree sign in several ways; normalise anything like "?C" to "°C".
pub fn norm_unit(u: &str) -> String {
    let u = u.trim();
    let n = u.chars().count();
    if u.ends_with('C') && (n == 1 || n == 2) {
        "°C".to_string()
    } else {
        u.to_string()
    }
}

struct Header {
    sig: u32,
    poll: i64,
    so: u32,
    ss: u32,
    sn: u32,
    ro: u32,
    rs: u32,
    rn: u32,
}

fn header(b: &[u8]) -> Header {
    Header {
        sig: u32_at(b, 0),
        poll: i64_at(b, 12),
        so: u32_at(b, 20),
        ss: u32_at(b, 24),
        sn: u32_at(b, 28),
        ro: u32_at(b, 32),
        rs: u32_at(b, 36),
        rn: u32_at(b, 40),
    }
}

/// Validate the header and return (poll, total bytes the block claims).
pub fn check_header(head: &[u8]) -> Result<(i64, usize), SmError> {
    if head.len() < HDR_SIZE {
        return Err(SmError::new("HWiNFO shared memory is still being created", Kind::Starting));
    }
    let h = header(head);
    if h.sig == DEAD_SIGNATURE {
        return Err(SmError::new(MSG_EXPIRED, Kind::Expired));
    }
    if h.sig == 0 || h.poll == 0 || h.sn == 0 || h.rn == 0 {
        return Err(SmError::new("HWiNFO is starting up (shared memory not populated yet)", Kind::Starting));
    }
    if (h.ss as usize) < SENSOR_SIZE
        || (h.rs as usize) < READING_SIZE
        || h.sn > MAX_SENSORS
        || h.rn > MAX_READINGS
        || (h.so as usize) < HDR_SIZE
        || (h.ro as usize) < HDR_SIZE
    {
        return Err(SmError::new(
            format!("unexpected shared-memory layout (signature 0x{:08X})", h.sig),
            Kind::Invalid,
        ));
    }
    let total = (h.so as u64 + h.ss as u64 * h.sn as u64).max(h.ro as u64 + h.rs as u64 * h.rn as u64);
    Ok((h.poll, total as usize))
}

pub fn parse_buffer(buf: &[u8]) -> Result<Snapshot, SmError> {
    let (poll, total) = check_header(&buf[..buf.len().min(HDR_SIZE)])?;
    if buf.len() < total {
        return Err(SmError::new("shared memory truncated", Kind::Starting));
    }
    let h = header(buf);
    let sensors: Vec<String> = (0..h.sn as usize)
        .map(|i| {
            let o = h.so as usize + i * h.ss as usize;
            let user = cstr(&buf[o + 136..o + 264]);
            if user.is_empty() {
                cstr(&buf[o + 8..o + 136])
            } else {
                user
            }
        })
        .collect();
    let out = (0..h.rn as usize)
        .map(|i| {
            let o = h.ro as usize + i * h.rs as usize;
            let si = u32_at(buf, o + 4) as usize;
            let orig = cstr(&buf[o + 12..o + 140]);
            let user = cstr(&buf[o + 140..o + 268]);
            Reading {
                sensor: sensors.get(si).cloned().unwrap_or_else(|| "?".into()),
                label: if user.is_empty() { orig.clone() } else { user },
                label_orig: orig,
                unit: norm_unit(&cstr(&buf[o + 268..o + 284])),
                typ: u32_at(buf, o),
                cur: f64_at(buf, o + 284),
                min: f64_at(buf, o + 292),
                max: f64_at(buf, o + 300),
                avg: f64_at(buf, o + 308),
            }
        })
        .collect();
    Ok((poll, out))
}

// --------------------------------------------------------------------------- Win32 access
/// One complete open -> copy -> close cycle.
#[cfg(windows)]
pub fn read_raw() -> Result<Snapshot, SmError> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ, MEMORY_BASIC_INFORMATION,
    };

    let name: Vec<u16> = SM_NAME.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the call.
    let h = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) }
        .map_err(|_| SmError::new(MSG_MISSING, Kind::Missing))?;
    let copied = (|| {
        // SAFETY: `h` is a valid section handle; mapping the whole section read-only.
        let view = unsafe { MapViewOfFile(h, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            return Err(SmError::new("could not map HWiNFO shared memory", Kind::Missing));
        }
        let r = (|| {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: querying our own freshly mapped view into a correctly sized struct.
            let n =
                unsafe { VirtualQuery(Some(view.Value), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
            let size = if n == 0 { 0 } else { mbi.RegionSize };
            if size < HDR_SIZE {
                return Err(SmError::new("HWiNFO shared memory is still being created", Kind::Starting));
            }
            // SAFETY: the view is at least `size` bytes (VirtualQuery) and HDR_SIZE <= size.
            let head = unsafe { std::slice::from_raw_parts(view.Value as *const u8, HDR_SIZE) };
            let (_, total) = check_header(head)?;
            if total > size {
                return Err(SmError::new(
                    format!("shared memory smaller than its header claims ({total} > {size} bytes)"),
                    Kind::Starting,
                ));
            }
            // SAFETY: total <= size, so the whole range lies inside the mapped view.
            Ok(unsafe { std::slice::from_raw_parts(view.Value as *const u8, total) }.to_vec())
        })();
        // SAFETY: unmapping the view we mapped above; nothing references it after this.
        unsafe {
            let _ = UnmapViewOfFile(view);
        }
        r
    })();
    // SAFETY: closing the handle we opened.
    unsafe {
        let _ = CloseHandle(h);
    }
    parse_buffer(&copied?)
}

#[cfg(not(windows))]
pub fn read_raw() -> Result<Snapshot, SmError> {
    Err(SmError::new("live mode needs Windows (HWiNFO shared memory) - use --mock", Kind::Missing))
}

// --------------------------------------------------------------------------- resilient source
/// Exponential retry gate: after a failure, attempts are skipped until the delay has passed.
pub struct Backoff {
    first: f64,
    pub cap: f64,
    pub delay: f64,
    next_at: f64,
    pub err: Option<SmError>,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { first: 0.25, cap: 5.0, delay: 0.0, next_at: 0.0, err: None }
    }
}

impl Backoff {
    pub fn blocked(&self, now: f64) -> bool {
        self.err.is_some() && now < self.next_at
    }
    pub fn fail(&mut self, now: f64, err: SmError) {
        self.delay = self.cap.min(self.first.max(self.delay * 2.0));
        self.next_at = now + self.delay;
        self.err = Some(err);
    }
    pub fn ok(&mut self) {
        self.delay = 0.0;
        self.next_at = 0.0;
        self.err = None;
    }
}

type Reader = Box<dyn FnMut() -> Result<Snapshot, SmError> + Send>;
type Clock = Box<dyn Fn() -> f64 + Send>;

pub struct SharedMemorySource {
    reader: Reader,
    clock: Clock,
    stale_after: f64,
    pub backoff: Backoff,
    last_poll: Option<i64>,
    last_change: f64,
}

impl SharedMemorySource {
    pub fn live() -> Self {
        Self::with(Box::new(read_raw), Box::new(mono), 15.0)
    }

    pub fn with(reader: Reader, clock: Clock, stale_after: f64) -> Self {
        let last_change = clock();
        SharedMemorySource { reader, clock, stale_after, backoff: Backoff::default(), last_poll: None, last_change }
    }
}

impl DataSource for SharedMemorySource {
    fn read(&mut self) -> Result<Snapshot, SmError> {
        let now = (self.clock)();
        if self.backoff.blocked(now) {
            if let Some(e) = &self.backoff.err {
                return Err(e.clone());
            }
        }
        let r = (self.reader)().and_then(|(poll, readings)| {
            if self.last_poll != Some(poll) {
                self.last_poll = Some(poll);
                self.last_change = now;
                Ok((poll, readings))
            } else if now - self.last_change > self.stale_after {
                // HWiNFO paused/hung, or a 3rd-party reader keeps a dead section alive
                Err(SmError::new(MSG_STALE, Kind::Stale))
            } else {
                Ok((poll, readings))
            }
        });
        match &r {
            Ok(_) => self.backoff.ok(),
            Err(e) => self.backoff.fail(now, e.clone()),
        }
        r
    }
}

// --------------------------------------------------------------------------- mock
const CPU: &str = "CPU [#0]: AMD Ryzen 7 5800X: Enhanced";
const ITE: &str = "GIGABYTE B550 AORUS ELITE V2 (ITE IT8688E)";
const GPU: &str = "GPU [#0]: NVIDIA GeForce RTX 3070 Ti: GIGABYTE AORUS";
const C: &str = "°C";

/// Plausible fake data (names from a real 5800X / B550 / 3070 Ti setup).
/// `outage_every = N` simulates a 4 s HWiNFO restart every N seconds, to exercise reconnection.
pub struct MockSource {
    spec: Vec<(String, String, &'static str, u32, f64, f64)>,
    state: Vec<Option<(f64, f64, f64, u32)>>, // min, max, sum, n
    outage_every: f64,
    t0: f64,
    rng: u64,
}

impl MockSource {
    pub const OUTAGE: f64 = 4.0;

    pub fn new(outage_every: f64) -> Self {
        let mut spec: Vec<(String, String, &'static str, u32, f64, f64)> = Vec::new();
        let mut add = |s: &str, l: &str, u: &'static str, t: u32, base: f64, jit: f64| {
            spec.push((s.to_string(), l.to_string(), u, t, base, jit))
        };
        add(CPU, "CPU (Tctl/Tdie)", C, 1, 43.0, 4.0);
        add(CPU, "CPU Die (average)", C, 1, 40.0, 3.0);
        add(CPU, "CPU CCD1 (Tdie)", C, 1, 42.0, 3.0);
        for i in 0..8 {
            add(CPU, &format!("Core {i} (Tdie)"), C, 1, 36.0 + (i % 3) as f64, 3.0);
        }
        add(CPU, "L3 Cache 0", C, 1, 36.0, 2.0);
        add(CPU, "Core 0 Power", "W", 5, 0.8, 0.3);
        add(CPU, "L3 Clocks", "MHz", 6, 3700.0, 40.0);
        add(CPU, "CPU Core Voltage (SVI2 TFN)", "V", 2, 1.0, 0.03);
        add(CPU, "CPU Core VID (Effective)", "V", 2, 1.0, 0.03);
        add(CPU, "CPU Package Power", "W", 5, 30.0, 8.0);
        add(CPU, "Power Reporting Deviation (Accuracy)", "%", 8, 85.0, 8.0);
        for (l, b) in
            [("System1", 33.0), ("System2", 33.0), ("CPU", 41.0), ("PCIEX16", 41.0), ("VRM MOS", 41.0), ("PCH", 42.0)]
        {
            add(ITE, l, C, 1, b, 1.0);
        }
        add(ITE, "CPU VCORE SoC", "V", 2, 1.188, 0.004);
        add(ITE, "System 1", "RPM", 3, 560.0, 25.0);
        for (s, l, b, j) in [
            ("S.M.A.R.T.: ST2000DM001-1E6164 (Z1E6RBQZ) [F:]", "Drive Temperature", 34.0, 1.0),
            ("S.M.A.R.T.: ST1000DM003-1ER162 (Z4Y9XRDY) [G:]", "Drive Temperature", 36.0, 1.0),
            ("S.M.A.R.T.: KINGSTON SA400S37960G (50026B77849761C0) [E:]", "Drive Temperature", 31.0, 1.0),
            ("S.M.A.R.T.: KINGSTON SA400S37240G (50026B7783D833BC) [H:]", "Drive Temperature", 30.0, 1.0),
            ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature", 42.0, 2.0),
            ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature 2", 48.0, 2.0),
            ("S.M.A.R.T.: XPG GAMMIX S70 BLADE (2M042L28J7UR) [D:]", "Drive Temperature 3", 44.0, 2.0),
            ("S.M.A.R.T.: XPG SPECTRIX S40G (2J3720095834) [C:]", "Drive Temperature", 38.0, 2.0),
        ] {
            add(s, l, C, 1, b, j);
        }
        add("Drive: XPG SPECTRIX S40G (2J3720095834) [C:]", "Read Activity", "%", 7, 1.0, 1.0);
        add(GPU, "GPU Temperature", C, 1, 50.0, 5.0);
        add(GPU, "GPU Memory Junction Temperature", C, 1, 56.0, 5.0);
        add(GPU, "GPU Hot Spot Temperature", C, 1, 62.0, 5.0);
        add(GPU, "GPU Core Voltage", "V", 2, 0.956, 0.05);
        add(GPU, "GPU Fan1", "RPM", 3, 600.0, 80.0);
        add(GPU, "GPU Fan2", "RPM", 3, 600.0, 80.0);
        add(GPU, "GPU Clock", "MHz", 6, 1875.0, 120.0);
        add(GPU, "GPU Fan1", "%", 7, 20.0, 3.0);
        let n = spec.len();
        let seed = (now_epoch() * 1e6) as u64 | 1;
        MockSource { spec, state: vec![None; n], outage_every, t0: mono(), rng: seed }
    }

    fn uniform(&mut self, jit: f64) -> f64 {
        // xorshift64: plenty for jitter, and no dependency
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * jit
    }
}

impl DataSource for MockSource {
    fn read(&mut self) -> Result<Snapshot, SmError> {
        if self.outage_every > 0.0 {
            let phase = (mono() - self.t0) % self.outage_every;
            if phase > self.outage_every - Self::OUTAGE {
                self.state.iter_mut().for_each(|s| *s = None); // a restarted HWiNFO resets min/max/avg
                return Err(SmError::new(format!("{MSG_MISSING} [mock outage]"), Kind::Missing));
            }
        }
        let mut out = Vec::with_capacity(self.spec.len());
        for i in 0..self.spec.len() {
            let (base, jit) = (self.spec[i].4, self.spec[i].5);
            let v = base + self.uniform(jit);
            let s = self.state[i].get_or_insert((v, v, 0.0, 0));
            s.0 = s.0.min(v);
            s.1 = s.1.max(v);
            s.2 += v;
            s.3 += 1;
            let (mn, mx, avg) = (s.0, s.1, s.2 / s.3 as f64);
            let (sensor, label, unit, typ, _, _) = &self.spec[i];
            out.push(Reading {
                sensor: sensor.clone(),
                label: label.clone(),
                label_orig: label.clone(),
                unit: unit.to_string(),
                typ: *typ,
                cur: v,
                min: mn,
                max: mx,
                avg,
            });
        }
        Ok(((now_epoch() * 1000.0) as i64, out))
    }
}

// --------------------------------------------------------------------------- tests
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub fn make_buffer(sig: u32, poll: i64, readings: &[(&str, &str, &str, f64)]) -> Vec<u8> {
        let mut sensors: Vec<&str> = readings.iter().map(|r| r.0).collect();
        sensors.sort();
        sensors.dedup();
        let so = HDR_SIZE as u32;
        let ro = so + (SENSOR_SIZE * sensors.len()) as u32;
        let mut b = Vec::new();
        for v in [sig, 2, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&poll.to_le_bytes());
        for v in [so, SENSOR_SIZE as u32, sensors.len() as u32, ro, READING_SIZE as u32, readings.len() as u32] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let fixed = |s: &str, n: usize| {
            let mut v = s.as_bytes().to_vec();
            v.resize(n, 0);
            v
        };
        for (i, s) in sensors.iter().enumerate() {
            b.extend_from_slice(&(i as u32).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend(fixed(s, 128));
            b.extend(fixed("", 128));
        }
        for (i, (s, label, unit, v)) in readings.iter().enumerate() {
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&(sensors.iter().position(|x| x == s).unwrap_or(0) as u32).to_le_bytes());
            b.extend_from_slice(&(i as u32).to_le_bytes());
            b.extend(fixed(label, 128));
            b.extend(fixed("", 128));
            b.extend(fixed(unit, 16));
            for x in [*v, v - 1.0, v + 1.0, *v] {
                b.extend_from_slice(&x.to_le_bytes());
            }
        }
        b
    }

    fn one() -> Vec<u8> {
        make_buffer(0x1234_5678, 1, &[("Sensor A", "Temp", "C", 42.0)])
    }

    #[test]
    fn roundtrip() {
        let (poll, rs) = parse_buffer(&one()).unwrap();
        assert_eq!(poll, 1);
        assert_eq!(
            (rs[0].sensor.as_str(), rs[0].label.as_str(), rs[0].unit.as_str(), rs[0].cur),
            ("Sensor A", "Temp", "°C", 42.0)
        );
    }

    #[test]
    fn dead_signature_is_expired() {
        let e = parse_buffer(&make_buffer(DEAD_SIGNATURE, 1, &[("A", "T", "C", 1.0)])).unwrap_err();
        assert_eq!(e.kind, Kind::Expired);
    }

    #[test]
    fn unpopulated_header_is_starting() {
        for buf in [vec![], vec![0u8; HDR_SIZE], make_buffer(1, 0, &[("A", "T", "C", 1.0)]), make_buffer(1, 1, &[])] {
            assert_eq!(parse_buffer(&buf).unwrap_err().kind, Kind::Starting);
        }
    }

    #[test]
    fn truncated() {
        let b = one();
        assert!(parse_buffer(&b[..b.len() - 10]).is_err());
    }

    #[test]
    fn absurd_counts_rejected() {
        let mut b = one();
        b[28..32].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert_eq!(parse_buffer(&b).unwrap_err().kind, Kind::Invalid);
    }

    #[test]
    fn cp1252_and_units() {
        assert_eq!(cstr(b"Ol\xe1 \x80\x81\0junk"), "Olá €\u{FFFD}");
        assert_eq!(norm_unit(" \u{b0}C "), "°C");
        assert_eq!(norm_unit("RPM"), "RPM");
    }

    /// A source driven by a script of results and a manual clock.
    fn scripted(script: Vec<Result<i64, SmError>>) -> (SharedMemorySource, Arc<Mutex<f64>>, Arc<Mutex<usize>>) {
        let clock = Arc::new(Mutex::new(100.0));
        let calls = Arc::new(Mutex::new(0usize));
        let script = Arc::new(Mutex::new(script));
        let (c2, k2) = (clock.clone(), calls.clone());
        let src = SharedMemorySource::with(
            Box::new(move || {
                *k2.lock().unwrap() += 1;
                script.lock().unwrap().remove(0).map(|p| (p, vec![]))
            }),
            Box::new(move || *c2.lock().unwrap()),
            15.0,
        );
        (src, clock, calls)
    }

    #[test]
    fn backoff_skips_attempts_then_recovers() {
        let gone = || Err(SmError::new("gone", Kind::Missing));
        let (mut s, clock, calls) = scripted(vec![gone(), gone(), Ok(7)]);
        assert!(s.read().is_err()); // attempt 1 fails -> wait 0.25 s
        assert!(s.read().is_err()); // inside the backoff: no attempt
        assert_eq!(*calls.lock().unwrap(), 1);
        *clock.lock().unwrap() += 0.3;
        assert!(s.read().is_err()); // attempt 2 fails -> wait 0.5 s
        assert_eq!(s.backoff.delay, 0.5);
        *clock.lock().unwrap() += 0.6;
        assert_eq!(s.read().unwrap().0, 7);
        assert_eq!(s.backoff.delay, 0.0);
    }

    #[test]
    fn backoff_is_capped() {
        let (mut s, clock, _) = scripted((0..20).map(|_| Err(SmError::new("x", Kind::Missing))).collect());
        for _ in 0..20 {
            let _ = s.read();
            *clock.lock().unwrap() += 10.0;
        }
        assert_eq!(s.backoff.delay, Backoff::default().cap);
    }

    #[test]
    fn stale_poll_detected() {
        let (mut s, clock, _) = scripted(vec![Ok(5), Ok(5), Ok(6)]);
        s.read().unwrap();
        *clock.lock().unwrap() += 16.0;
        assert_eq!(s.read().unwrap_err().kind, Kind::Stale);
        *clock.lock().unwrap() += 1.0;
        assert_eq!(s.read().unwrap().0, 6); // a new poll value (restarted HWiNFO) clears it
    }

    #[test]
    fn mock_produces_readings() {
        let mut m = MockSource::new(0.0);
        let (_, rs) = m.read().unwrap();
        assert!(rs.len() > 30 && rs.iter().all(|r| r.min <= r.cur && r.cur <= r.max));
    }
}
