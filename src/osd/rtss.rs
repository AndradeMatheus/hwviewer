//! RivaTuner Statistics Server shared memory (`RTSSSharedMemoryV2`, SDK `RTSSSharedMemory.h` v2.x):
//! write our text into one OSD slot (RTSS draws it inside the game, also in exclusive fullscreen), and read the
//! foreground game's frame rate from its application entry.
//!
//! The protocol logic works on a byte slice so it can be tested against a synthetic block; `with_mapping`
//! supplies the real one. Like `shm.rs`, the mapping is opened per call and every access is bounds-checked.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use super::model::Fps;

pub const NAME: &str = "RTSSSharedMemoryV2";
pub const OWNER: &str = "HWiNFO Viewer";
const SIGNATURE: u32 = 0x5254_5353; // 'RTSS'
                                    // header offsets (v2.x)
const H_SIG: usize = 0;
const H_VER: usize = 4;
const H_APP_ENTRY: usize = 8;
const H_APP_OFF: usize = 12;
const H_APP_N: usize = 16;
const H_OSD_ENTRY: usize = 20;
const H_OSD_OFF: usize = 24;
const H_OSD_N: usize = 28;
const H_OSD_FRAME: usize = 32;
const H_BUSY: usize = 36; // v2.14+
const H_LAST_FG_PID: usize = 68; // v2.16+
                                 // OSD entry: szOSD[256], szOSDOwner[256], szOSDEx[4096] (v2.7+)
const O_TEXT: usize = 0;
const O_OWNER: usize = 256;
const O_TEXT_EX: usize = 512;
// app entry: dwProcessID, szName[260], dwFlags, dwTime0, dwTime1, dwFrames, dwFrameTime (µs)
const A_PID: usize = 0;
const A_TIME0: usize = 268;
const A_TIME1: usize = 272;
const A_FRAMES: usize = 276;
const A_FRAMETIME: usize = 280;

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

struct Layout {
    ver: u32,
    app: (usize, usize, usize), // entry size, array offset, count
    osd: (usize, usize, usize),
}

fn layout(b: &[u8]) -> Result<Layout, String> {
    if u32_at(b, H_SIG) != Some(SIGNATURE) {
        return Err("RTSS shared memory not initialised".into());
    }
    let g = |o| u32_at(b, o).map(|v| v as usize).ok_or("RTSS header truncated");
    let ver = g(H_VER)? as u32;
    if ver >> 16 != 2 {
        return Err(format!("unsupported RTSS shared memory version {:#x}", ver));
    }
    let l = Layout {
        ver,
        app: (g(H_APP_ENTRY)?, g(H_APP_OFF)?, g(H_APP_N)?),
        osd: (g(H_OSD_ENTRY)?, g(H_OSD_OFF)?, g(H_OSD_N)?),
    };
    for (size, off, n) in [l.app, l.osd] {
        if n > 4096 || off.saturating_add(size.saturating_mul(n)) > b.len() {
            return Err("RTSS shared memory layout out of bounds".into());
        }
    }
    if l.osd.0 < O_TEXT_EX || l.app.0 < A_FRAMETIME + 4 {
        return Err("RTSS entries smaller than expected".into());
    }
    Ok(l)
}

fn cstr(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

/// Frame rate of `pid`'s application entry (falls back to RTSS's last foreground app).
pub fn read_fps(b: &[u8], pid: u32) -> Option<Fps> {
    let l = layout(b).ok()?;
    let entry = |want: u32| {
        (0..l.app.2).map(|i| l.app.1 + i * l.app.0).find(|&o| want != 0 && u32_at(b, o + A_PID) == Some(want))
    };
    let o = entry(pid).or_else(|| if l.ver >= 0x0002_0010 { entry(u32_at(b, H_LAST_FG_PID)?) } else { None })?;
    let (t0, t1, frames, ft) =
        (u32_at(b, o + A_TIME0)?, u32_at(b, o + A_TIME1)?, u32_at(b, o + A_FRAMES)?, u32_at(b, o + A_FRAMETIME)?);
    let span = t1.wrapping_sub(t0);
    if span == 0 || frames == 0 {
        return None;
    }
    let fps = frames as f64 * 1000.0 / span as f64;
    let frametime_ms = if ft > 0 { ft as f64 / 1000.0 } else { 1000.0 / fps };
    Some(Fps { fps, frametime_ms })
}

/// cp1252 bytes for RTSS's char fields (unmappable characters become '?').
pub fn to_cp1252(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if u < 0x80 || (0xA0..0x100).contains(&u) {
                u as u8
            } else {
                crate::shm::CP1252_HIGH.iter().position(|&x| x as u32 == u && x != 0).map_or(b'?', |i| 0x80 + i as u8)
            }
        })
        .collect()
}

fn put_str(b: &mut [u8], off: usize, cap: usize, s: &[u8]) {
    let n = s.len().min(cap - 1);
    b[off..off + n].copy_from_slice(&s[..n]);
    b[off + n] = 0;
}

/// Take RTSS's busy bit (v2.14+). `None` when another client holds it for too long: skip this update.
fn lock(b: &mut [u8], ver: u32) -> Option<bool> {
    if ver < 0x0002_000E {
        return Some(false);
    }
    // SAFETY: H_BUSY is 4-byte aligned inside the (page-aligned) block and lives as long as `b`.
    let busy = unsafe { AtomicI32::from_ptr(b.as_mut_ptr().add(H_BUSY) as *mut i32) };
    for _ in 0..50 {
        if busy.fetch_or(1, Ordering::AcqRel) & 1 == 0 {
            return Some(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

fn unlock(b: &mut [u8], locked: bool) {
    if locked {
        // SAFETY: as in lock().
        let busy = unsafe { AtomicI32::from_ptr(b.as_mut_ptr().add(H_BUSY) as *mut i32) };
        busy.fetch_and(!1, Ordering::AcqRel); // never leave it set: it would freeze every RTSS client
    }
}

fn bump_frame(b: &mut [u8]) {
    // SAFETY: H_OSD_FRAME is 4-byte aligned inside the block.
    let f = unsafe { AtomicU32::from_ptr(b.as_mut_ptr().add(H_OSD_FRAME) as *mut u32) };
    f.fetch_add(1, Ordering::AcqRel);
}

/// Put `text` into our OSD slot (claiming a free one by owner name). `None` text releases the slot.
pub fn write_osd(b: &mut [u8], text: Option<&str>) -> Result<(), String> {
    let l = layout(b)?;
    let Some(locked) = lock(b, l.ver) else { return Err("RTSS shared memory busy".into()) };
    let r = (|| {
        let (size, off, n) = l.osd;
        let entry = |i: usize| off + i * size;
        // slot 0 belongs to RTSS itself
        let mine = (1..n).find(|&i| cstr(&b[entry(i) + O_OWNER..entry(i) + O_TEXT_EX]) == OWNER.as_bytes());
        let slot = match (mine, text) {
            (Some(i), _) => i,
            (None, None) => return Ok(()), // nothing to release
            (None, Some(_)) => {
                (1..n).find(|&i| b[entry(i) + O_OWNER] == 0).ok_or_else(|| "no free RTSS OSD slot".to_string())?
            }
        };
        let e = entry(slot);
        match text {
            Some(t) => {
                let bytes = to_cp1252(t);
                put_str(b, e + O_OWNER, 256, OWNER.as_bytes());
                if l.ver >= 0x0002_0007 {
                    put_str(b, e + O_TEXT_EX, 4096, &bytes);
                    put_str(b, e + O_TEXT, 256, b""); // RTSS shows szOSDEx when set
                } else {
                    put_str(b, e + O_TEXT, 256, &bytes);
                }
            }
            None => {
                for (o, cap) in [(O_TEXT, 256), (O_OWNER, 256), (O_TEXT_EX, 4096)] {
                    put_str(b, e + o, cap, b"");
                }
            }
        }
        bump_frame(b);
        Ok(())
    })();
    unlock(b, locked);
    r
}

/// Open RTSS's mapping (read or read/write), run `f` over it, close it. Err when RTSS isn't running.
#[cfg(windows)]
pub fn with_mapping<T>(write: bool, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS, FILE_MAP_READ,
        MEMORY_BASIC_INFORMATION,
    };
    let access = if write { FILE_MAP_ALL_ACCESS } else { FILE_MAP_READ };
    let name = crate::watchdog::process::wide(NAME);
    // SAFETY: NUL-terminated name; the handle is closed below on every path.
    let h = unsafe { OpenFileMappingW(access.0, false, PCWSTR(name.as_ptr())) }
        .map_err(|_| "RTSS is not running".to_string())?;
    // SAFETY: mapping the whole section of a valid handle.
    let view = unsafe { MapViewOfFile(h, access, 0, 0, 0) };
    let r = if view.Value.is_null() {
        Err("could not map RTSS shared memory".to_string())
    } else {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: querying our own view into a correctly sized struct.
        let n = unsafe { VirtualQuery(Some(view.Value), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        let size = if n == 0 { 0 } else { mbi.RegionSize };
        // SAFETY: `size` bytes are mapped (VirtualQuery); writes only happen with FILE_MAP_ALL_ACCESS, and every
        // offset used by the protocol functions is bounds-checked against this length.
        let buf = unsafe { std::slice::from_raw_parts_mut(view.Value as *mut u8, size) };
        let out = f(buf);
        // SAFETY: unmapping the view mapped above; `buf` is not used after this.
        unsafe {
            let _ = UnmapViewOfFile(view);
        }
        Ok(out)
    };
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = CloseHandle(h);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v2.21-style block with `apps` app entries and 8 OSD slots; slot 0 owned by RTSS.
    fn block(apps: &[(u32, u32, u32, u32, u32)]) -> Vec<u8> {
        let (app_size, osd_size) = (300usize, 512 + 4096);
        let app_off = 256;
        let osd_off = app_off + app_size * apps.len().max(1);
        let mut b = vec![0u8; osd_off + osd_size * 8];
        let mut put = |o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        for (o, v) in [
            (H_SIG, SIGNATURE),
            (H_VER, 0x0002_0015),
            (H_APP_ENTRY, app_size as u32),
            (H_APP_OFF, app_off as u32),
            (H_APP_N, apps.len() as u32),
            (H_OSD_ENTRY, osd_size as u32),
            (H_OSD_OFF, osd_off as u32),
            (H_OSD_N, 8),
            (H_LAST_FG_PID, 42),
        ] {
            put(o, v);
        }
        for (i, &(pid, t0, t1, frames, ft)) in apps.iter().enumerate() {
            let o = app_off + i * app_size;
            for (k, v) in [(A_PID, pid), (A_TIME0, t0), (A_TIME1, t1), (A_FRAMES, frames), (A_FRAMETIME, ft)] {
                put(o + k, v);
            }
        }
        b[osd_off + O_OWNER..osd_off + O_OWNER + 4].copy_from_slice(b"RTSS");
        b
    }

    #[test]
    fn fps_for_pid_and_last_foreground_fallback() {
        let b = block(&[(7, 1000, 2000, 144, 6944), (42, 0, 500, 30, 0)]);
        let f = read_fps(&b, 7).unwrap();
        assert!((f.fps - 144.0).abs() < 1e-9 && (f.frametime_ms - 6.944).abs() < 1e-9);
        let g = read_fps(&b, 999).unwrap(); // unknown pid -> RTSS's last foreground app (pid 42)
        assert!((g.fps - 60.0).abs() < 1e-9 && (g.frametime_ms - 1000.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn write_claims_one_slot_then_reuses_and_releases_it() {
        let mut b = block(&[]);
        let osd_off = u32_at(&b, H_OSD_OFF).unwrap() as usize;
        let slot = |i: usize| osd_off + i * (512 + 4096);
        write_osd(&mut b, Some("FPS 144 — 45 °C")).unwrap();
        assert_eq!(cstr(&b[slot(1) + O_OWNER..]), OWNER.as_bytes()); // slot 0 (RTSS's) untouched
        assert_eq!(cstr(&b[slot(1) + O_TEXT_EX..]), b"FPS 144 \x97 45 \xb0C");
        write_osd(&mut b, Some("again")).unwrap();
        assert_eq!(b[slot(2) + O_OWNER], 0, "the same slot is reused");
        assert_eq!(u32_at(&b, H_OSD_FRAME), Some(2));
        assert_eq!(u32_at(&b, H_BUSY), Some(0), "busy bit released");
        write_osd(&mut b, None).unwrap();
        assert_eq!(b[slot(1) + O_OWNER], 0);
    }

    #[test]
    fn rejects_foreign_or_broken_blocks() {
        let mut b = block(&[]);
        b[0] = 0;
        assert!(write_osd(&mut b, Some("x")).is_err());
        let mut b = block(&[]);
        b[H_OSD_N..H_OSD_N + 4].copy_from_slice(&100_000u32.to_le_bytes());
        assert!(read_fps(&b, 1).is_none() && write_osd(&mut b, Some("x")).is_err());
    }
}
