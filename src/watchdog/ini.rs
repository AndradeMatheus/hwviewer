//! Byte-preserving INI editing for HWiNFO64.INI.

use std::path::Path;

use crate::storage::atomic_write;

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Utf16Le,
    Utf16Be,
    Utf8Bom,
    Latin1, // ANSI read as Latin-1: every byte round-trips unchanged
}

fn decode(raw: &[u8]) -> (Enc, String) {
    let u16s = |b: &[u8], le: bool| -> String {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if raw.starts_with(&[0xFF, 0xFE]) {
        (Enc::Utf16Le, u16s(&raw[2..], true))
    } else if raw.starts_with(&[0xFE, 0xFF]) {
        (Enc::Utf16Be, u16s(&raw[2..], false))
    } else if let (true, Ok(s)) =
        (raw.starts_with(&[0xEF, 0xBB, 0xBF]), std::str::from_utf8(raw.get(3..).unwrap_or(&[])))
    {
        (Enc::Utf8Bom, s.to_string())
    } else {
        (Enc::Latin1, raw.iter().map(|&b| b as char).collect())
    }
}

fn encode(enc: Enc, s: &str) -> Vec<u8> {
    match enc {
        Enc::Utf16Le => [0xFF, 0xFE].into_iter().chain(s.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
        Enc::Utf16Be => [0xFE, 0xFF].into_iter().chain(s.encode_utf16().flat_map(u16::to_be_bytes)).collect(),
        Enc::Utf8Bom => [0xEF, 0xBB, 0xBF].into_iter().chain(s.bytes()).collect(),
        Enc::Latin1 => s.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect(),
    }
}

fn split_lines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            '\n' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Set key=value pairs in one section, preserving everything else byte for byte.
/// Keys match case-insensitively; a missing file or section is created. Returns true if the file changed
/// (a one-time `<name>.hwv-backup` copy of the original is kept).
pub fn update_ini(path: &Path, values: &[(String, String)], section: &str) -> std::io::Result<bool> {
    let raw = if path.exists() { std::fs::read(path)? } else { Vec::new() };
    let (enc, text) = decode(&raw);
    let nl = if text.contains("\r\n") || text.is_empty() { "\r\n" } else { "\n" };
    let mut lines = split_lines(&text);
    let header = format!("[{}]", section.to_lowercase());
    let start = match lines.iter().position(|l| l.trim().to_lowercase() == header) {
        Some(i) => i,
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.len() - 1
        }
    };
    let end = (start + 1..lines.len()).find(|&i| lines[i].trim().starts_with('[')).unwrap_or(lines.len());

    let mut todo: Vec<(String, String)> = values.to_vec();
    let mut changed = false;
    for line in lines.iter_mut().take(end).skip(start + 1) {
        let Some((key, val)) = line.split_once('=') else { continue };
        let k = key.trim().to_lowercase();
        if let Some(i) = todo.iter().position(|(tk, _)| tk.to_lowercase() == k) {
            let (_, v) = todo.remove(i);
            if val.trim() != v {
                *line = format!("{}={v}", key.trim());
                changed = true;
            }
        }
    }
    if !todo.is_empty() {
        let mut at = end;
        while at > start + 1 && lines[at - 1].trim().is_empty() {
            at -= 1; // keep the section's trailing blank lines after our keys
        }
        let new: Vec<String> = todo.iter().map(|(k, v)| format!("{k}={v}")).collect();
        lines.splice(at..at, new);
        changed = true;
    }
    if changed {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let backup = path.with_file_name(format!("{name}.hwv-backup"));
        if !raw.is_empty() && !backup.exists() {
            std::fs::write(&backup, &raw)?;
        }
        let mut out = lines.join(nl);
        out.push_str(nl);
        atomic_write(path, &encode(enc, &out))?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::temp_dir;
    use std::fs;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn key_added_to_middle_section() {
        let p = temp_dir("ini").join("HWiNFO64.INI");
        fs::write(&p, b"[Settings]\r\nFoo=1\r\n\r\n[Other]\r\nBar=2\r\n").unwrap();
        assert!(update_ini(&p, &kv(&[("SensorsSM", "1")]), "Settings").unwrap());
        assert_eq!(fs::read(&p).unwrap(), b"[Settings]\r\nFoo=1\r\nSensorsSM=1\r\n\r\n[Other]\r\nBar=2\r\n");
    }

    #[test]
    fn existing_key_updated_case_insensitive() {
        let p = temp_dir("ini").join("HWiNFO64.INI");
        fs::write(&p, b"[settings]\nsensorssm=0\n").unwrap();
        update_ini(&p, &kv(&[("SensorsSM", "1")]), "Settings").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[settings]\nsensorssm=1\n");
    }

    #[test]
    fn no_change_no_write() {
        let d = temp_dir("ini");
        let p = d.join("HWiNFO64.INI");
        fs::write(&p, b"[Settings]\r\nSensorsSM=1\r\n").unwrap();
        assert!(!update_ini(&p, &kv(&[("SensorsSM", "1")]), "Settings").unwrap());
        assert!(!d.join("HWiNFO64.INI.hwv-backup").exists());
    }

    #[test]
    fn ansi_bytes_preserved_and_backup_made() {
        let d = temp_dir("ini");
        let p = d.join("HWiNFO64.INI");
        let raw = b"[Settings]\r\nName=Ol\xe1 \xe7\r\n";
        fs::write(&p, raw).unwrap();
        update_ini(&p, &kv(&[("SensorsOnly", "1")]), "Settings").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[Settings]\r\nName=Ol\xe1 \xe7\r\nSensorsOnly=1\r\n");
        assert_eq!(fs::read(d.join("HWiNFO64.INI.hwv-backup")).unwrap(), raw);
    }

    #[test]
    fn missing_file_and_section() {
        let d = temp_dir("ini");
        let p = d.join("HWiNFO64.INI");
        update_ini(&p, &kv(&[("SensorsSM", "1")]), "Settings").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[Settings]\r\nSensorsSM=1\r\n");
        let p2 = d.join("b.ini");
        fs::write(&p2, b"[Other]\nX=1\n").unwrap();
        update_ini(&p2, &kv(&[("A", "1")]), "Settings").unwrap();
        assert_eq!(fs::read(&p2).unwrap(), b"[Other]\nX=1\n\n[Settings]\nA=1\n");
    }

    #[test]
    fn utf16_round_trip() {
        let p = temp_dir("ini").join("u.ini");
        fs::write(&p, encode(Enc::Utf16Le, "[Settings]\r\nA=0\r\n")).unwrap();
        update_ini(&p, &kv(&[("A", "1")]), "Settings").unwrap();
        assert_eq!(decode(&fs::read(&p).unwrap()).1, "[Settings]\r\nA=1\r\n");
    }
}
