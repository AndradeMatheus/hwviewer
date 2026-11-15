//! The app icon (dark rounded square, orange border, three bars), rendered to RGBA at any size.
//! Same design as the old Pillow version; no image dependency.

const BG: [u8; 4] = [20, 22, 26, 255];
const ACCENT: [u8; 4] = [255, 122, 26, 255];

/// RGBA pixels, `size` x `size`. Coordinates follow the original 256 px design, scaled.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32 / 256.0;
    let (x0, y0, x1, y1, r, border) = (12.0 * s, 12.0 * s, 244.0 * s, 244.0 * s, 48.0 * s, (12.0 * s).max(1.0));
    let inside_round = |px: f32, py: f32, inset: f32| -> bool {
        let (ax, ay, bx, by, rr) = (x0 + inset, y0 + inset, x1 - inset, y1 - inset, (r - inset).max(0.0));
        if px < ax || px > bx || py < ay || py > by {
            return false;
        }
        let cx = px.clamp(ax + rr, bx - rr);
        let cy = py.clamp(ay + rr, by - rr);
        (px - cx).powi(2) + (py - cy).powi(2) <= rr * rr
    };
    let bars: Vec<(f32, f32, f32, f32)> = [88.0, 144.0, 112.0]
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let x = 56.0 + i as f32 * 56.0;
            (x * s, (208.0 - h) * s, (x + 32.0) * s, 208.0 * s)
        })
        .collect();
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let c = if !inside_round(px, py, 0.0) {
                continue;
            } else if !inside_round(px, py, border) // the border ring...
                || bars.iter().any(|&(a, b, c, d)| px >= a && px < c && py >= b && py < d)
            // ...or a bar
            {
                ACCENT
            } else {
                BG
            };
            let i = ((y * size + x) * 4) as usize;
            out[i..i + 4].copy_from_slice(&c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn corners_transparent_centre_dark_bars_orange() {
        let n = 64u32;
        let px = super::rgba(n);
        let at = |x: u32, y: u32| &px[((y * n + x) * 4) as usize..((y * n + x) * 4 + 4) as usize];
        assert_eq!(at(0, 0)[3], 0);
        assert_eq!(at(32, 8), &super::BG[..]);
        assert_eq!(at(32, 40), &super::ACCENT[..]); // middle bar (x 28..36, y 16..52 at 64 px)
        assert_eq!(at(24, 40), &super::BG[..]); // gap between bars
    }
}
