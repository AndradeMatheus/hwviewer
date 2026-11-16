//! Native OSD engine: a click-through, always-on-top, never-activated layered window. Each frame is drawn with
//! Direct2D/DirectWrite into a premultiplied 32-bpp DIB and pushed with UpdateLayeredWindow, so background, row
//! boxes and text each get real alpha - no WebView, nothing injected into the game. Shows over windowed and
//! borderless games; exclusive fullscreen needs the RTSS engine.
//!
//! Runs on its own thread (it owns the window, its message loop and the global hotkey).

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_METRICS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC,
    SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow, PeekMessageW,
    RegisterClassExW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow, HWND_TOPMOST, MSG, PM_REMOVE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_HOTKEY, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use super::model::{rgb, Anchor, Line, OsdCfg};

pub enum Cmd {
    Show(Vec<Line>, Box<OsdCfg>),
    Hide,
    Hotkey(String),
    Quit,
}

const HOTKEY_ID: i32 = 0x4857; // "HW"

/// "Ctrl+Shift+O", "Alt+F10", "Win+Ctrl+1" -> (modifiers, virtual key).
pub fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "SHIFT" => mods |= MOD_SHIFT,
            "ALT" => mods |= MOD_ALT,
            "WIN" => mods |= MOD_WIN,
            k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => {
                vk = k.chars().next().map(|c| c as u32)
            }
            k if k.starts_with('F') => {
                vk = k[1..].parse::<u32>().ok().filter(|n| (1..=24).contains(n)).map(|n| 0x6F + n)
            }
            _ => return None,
        }
    }
    vk.map(|v| (mods, v))
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: default handling for our own window.
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Gfx {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
}

fn color(hex: &str, a: f32) -> D2D1_COLOR_F {
    let (r, g, b) = rgb(hex);
    D2D1_COLOR_F { r, g, b, a }
}

fn layout(gfx: &Gfx, text: &str, cfg: &OsdCfg, px: f32, bold: bool) -> Option<(IDWriteTextLayout, f32, f32)> {
    let family = wide(&cfg.font);
    let weight = if bold { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
    // SAFETY: plain DirectWrite factory calls with valid NUL-terminated strings / slices.
    unsafe {
        let fmt = gfx
            .dw
            .CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px,
                w!("en-us"),
            )
            .ok()?;
        let t: Vec<u16> = text.encode_utf16().collect();
        let l = gfx.dw.CreateTextLayout(&t, &fmt, 8192.0, 8192.0).ok()?;
        let mut m = DWRITE_TEXT_METRICS::default();
        l.GetMetrics(&mut m).ok()?;
        Some((l, m.widthIncludingTrailingWhitespace.ceil(), m.height.ceil()))
    }
}

/// Draw `lines` and push them to the window at the configured corner of the foreground window's monitor.
fn render(gfx: &Gfx, hwnd: HWND, lines: &[Line], cfg: &OsdCfg) -> Result<(), String> {
    // SAFETY: querying DPI for our own window.
    let scale = (unsafe { GetDpiForWindow(hwnd) }.max(96)) as f32 / 96.0;
    let px = cfg.size * scale;
    let (pad, gap, row_pad, radius) = (8.0 * scale, 10.0 * scale, 2.0 * scale, 6.0 * scale);
    let rows: Vec<_> = lines
        .iter()
        .filter_map(|l| Some((layout(gfx, &l.label, cfg, px, false)?, layout(gfx, &l.value, cfg, px, true)?, l)))
        .collect();
    if rows.is_empty() {
        return Err("nothing to draw".into());
    }
    let row_h = rows.iter().map(|(a, b, _)| a.2.max(b.2)).fold(0.0, f32::max) + 2.0 * row_pad;
    let label_w = rows.iter().map(|(a, _, _)| a.1).fold(0.0, f32::max);
    let value_w = rows.iter().map(|(_, b, _)| b.1).fold(0.0, f32::max);
    // (x, y, w) of each row box
    let mut boxes = Vec::new();
    let (w, h) = if cfg.horizontal {
        let mut x = pad;
        for (a, b, _) in &rows {
            let bw = a.1 + gap * 0.6 + b.1;
            boxes.push((x, pad, bw));
            x += bw + gap * 1.6;
        }
        (x - gap * 1.6 + pad, row_h + 2.0 * pad)
    } else {
        let bw = label_w + gap + value_w;
        for i in 0..rows.len() {
            boxes.push((pad, pad + i as f32 * row_h, bw));
        }
        (bw + 2.0 * pad, rows.len() as f32 * row_h + 2.0 * pad)
    };
    let (w, h) = (w.ceil() as i32, h.ceil() as i32);

    // SAFETY: standard GDI + D2D DC-render-target sequence; every GDI object created here is released below,
    // and `bits` is owned by the DIB section for its lifetime.
    unsafe {
        let screen = GetDC(HWND::default());
        let mem = CreateCompatibleDC(screen);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let result = (|| -> Result<(), String> {
            let bmp = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0).map_err(|e| e.to_string())?;
            let old = SelectObject(mem, bmp);
            let r = (|| -> Result<(), String> {
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                };
                let rt = gfx.d2d.CreateDCRenderTarget(&props).map_err(|e| e.to_string())?;
                rt.BindDC(mem, &RECT { left: 0, top: 0, right: w, bottom: h }).map_err(|e| e.to_string())?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE); // ClearType can't do alpha
                rt.BeginDraw();
                rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
                let brush = |hex: &str, a: f32| rt.CreateSolidColorBrush(&color(hex, a), None);
                if cfg.bg_alpha > 0.0 {
                    let b = brush(&cfg.bg, cfg.bg_alpha).map_err(|e| e.to_string())?;
                    let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 };
                    rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius }, &b);
                }
                let box_brush = brush(&cfg.bg, cfg.box_alpha).map_err(|e| e.to_string())?;
                let label_brush = brush(&cfg.label_color, cfg.text_alpha).map_err(|e| e.to_string())?;
                for ((label, value, line), (x, y, bw)) in rows.iter().zip(&boxes) {
                    if cfg.box_alpha > 0.0 {
                        let rect = D2D_RECT_F {
                            left: *x - 3.0 * scale,
                            top: *y,
                            right: x + bw + 3.0 * scale,
                            bottom: y + row_h,
                        };
                        rt.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT { rect, radiusX: radius * 0.6, radiusY: radius * 0.6 },
                            &box_brush,
                        );
                    }
                    let ty = y + row_pad;
                    rt.DrawTextLayout(
                        D2D_POINT_2F { x: *x, y: ty },
                        &label.0,
                        &label_brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                    let vx = if cfg.horizontal { x + label.1 + gap * 0.6 } else { x + bw - value.1 }; // values right-aligned
                    let vb = brush(&line.color, cfg.text_alpha).map_err(|e| e.to_string())?;
                    rt.DrawTextLayout(D2D_POINT_2F { x: vx, y: ty }, &value.0, &vb, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
                rt.EndDraw(None, None).map_err(|e| e.to_string())?;

                // tests: HWV_OSD_DUMP=<file.bmp> writes the frame to disk instead of showing it on screen
                if let Some(path) = std::env::var_os("HWV_OSD_DUMP") {
                    let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                    return dump_bmp(std::path::Path::new(&path), w, h, px).map_err(|e| e.to_string());
                }

                // position: chosen corner of the monitor the foreground window (the game) is on
                let mon = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
                let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(mon, &mut mi);
                let m = mi.rcMonitor;
                let (ox, oy) = ((cfg.offset.0 as f32 * scale) as i32, (cfg.offset.1 as f32 * scale) as i32);
                let x = match cfg.anchor {
                    Anchor::TopLeft | Anchor::BottomLeft => m.left + ox,
                    _ => m.right - w - ox,
                };
                let y = match cfg.anchor {
                    Anchor::TopLeft | Anchor::TopRight => m.top + oy,
                    _ => m.bottom - h - oy,
                };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    hwnd,
                    screen,
                    Some(&POINT { x, y }),
                    Some(&SIZE { cx: w, cy: h }),
                    mem,
                    Some(&POINT { x: 0, y: 0 }),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                )
                .map_err(|e| e.to_string())?;
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Ok(())
            })();
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
            r
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(HWND::default(), screen);
        result
    }
}

/// Top-down 32-bpp BGRA pixels -> a .bmp (premultiplied alpha composited over a checkerboard, so the
/// see-through parts are visible in any image viewer).
fn dump_bmp(path: &std::path::Path, w: i32, h: i32, px: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(54 + px.len());
    let size = 54 + (w * h * 4) as u32;
    out.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&(-h).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, (w * h * 4) as u32, 2835, 2835, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for (i, p) in px.chunks_exact(4).enumerate() {
        let (x, y) = (i as i32 % w, i as i32 / w);
        let bg = if (x / 8 + y / 8) % 2 == 0 { 200u32 } else { 120 };
        let a = p[3] as u32;
        for c in &p[..3] {
            out.push((*c as u32 + bg * (255 - a) / 255).min(255) as u8); // premultiplied "over"
        }
        out.push(255);
    }
    crate::storage::atomic_write(path, &out)
}

fn create_window() -> Result<HWND, String> {
    // SAFETY: registering a window class and creating a top-level popup owned by this thread.
    unsafe {
        let inst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = w!("HWiNFOViewerOSD");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // 0 when already registered (a restarted thread): fine
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("HWiNFO Viewer OSD"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            HWND::default(),
            None,
            inst,
            None,
        )
        .map_err(|e| e.to_string())
    }
}

/// The overlay thread: owns the window and the hotkey. `hotkey_tx` gets a message whenever the hotkey fires.
pub fn run(rx: Receiver<Cmd>, hotkey_tx: Sender<()>) {
    // SAFETY: COM-free factory creation (Direct2D/DirectWrite need no CoInitialize).
    let gfx = unsafe {
        match (
            D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None),
            DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED),
        ) {
            (Ok(d2d), Ok(dw)) => Gfx { d2d, dw },
            (a, b) => {
                error!("OSD: Direct2D/DirectWrite unavailable: {:?} {:?}", a.err(), b.err());
                return;
            }
        }
    };
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            error!("OSD: could not create the overlay window: {e}");
            return;
        }
    };
    let mut visible = false;
    let mut hotkey = String::new();
    let mut next_top = Instant::now();
    loop {
        // pump this thread's messages (the hotkey arrives as WM_HOTKEY)
        let mut msg = MSG::default();
        // SAFETY: standard message pump for windows/hotkeys owned by this thread.
        unsafe {
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_ID as usize {
                    let _ = hotkey_tx.send(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Cmd::Show(lines, cfg)) => match render(&gfx, hwnd, &lines, &cfg) {
                Ok(()) => visible = true,
                Err(e) => warn!("OSD render: {e}"),
            },
            Ok(Cmd::Hide) if visible => {
                // SAFETY: hiding our own window.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                visible = false;
            }
            Ok(Cmd::Hide) => {}
            Ok(Cmd::Hotkey(k)) if k != hotkey => {
                // SAFETY: (un)registering a hotkey for our own window.
                unsafe {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
                    match parse_hotkey(&k) {
                        Some((mods, vk)) => {
                            if let Err(e) = RegisterHotKey(hwnd, HOTKEY_ID, mods, vk) {
                                warn!("OSD hotkey {k:?} unavailable (taken by another app?): {e}");
                            }
                        }
                        None => warn!("OSD hotkey {k:?} not understood (e.g. Ctrl+Shift+O, Alt+F10)"),
                    }
                }
                hotkey = k;
            }
            Ok(Cmd::Hotkey(_)) => {}
            Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if visible && Instant::now() >= next_top {
            // games and other topmost windows push us down: re-assert, without stealing focus
            next_top = Instant::now() + Duration::from_secs(2);
            // SAFETY: z-order change of our own window.
            unsafe {
                let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }
    // SAFETY: tearing down our own hotkey and window.
    unsafe {
        let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_parse() {
        assert_eq!(parse_hotkey("Ctrl+Shift+O"), Some((MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT, 'O' as u32)));
        assert_eq!(parse_hotkey("alt + f10"), Some((MOD_NOREPEAT | MOD_ALT, 0x79)));
        assert_eq!(parse_hotkey("Ctrl+Shift"), None);
        assert_eq!(parse_hotkey("Ctrl+Space"), None);
    }
}
