//! Win32 window tweaks the cross-platform crates don't cover: click-through, pin-to-desktop,
//! z-order, clipboard, opening URLs, message boxes, console attach.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, FindWindowW, GetForegroundWindow, GetWindowLongPtrW, MessageBoxW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA,
    MB_ICONERROR, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNORMAL, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

use crate::watchdog::process::wide;

pub fn hwnd_of(w: &tao::window::Window) -> HWND {
    use tao::platform::windows::WindowExtWindows;
    HWND(w.hwnd() as _)
}

/// The window that hosts the desktop icons (Progman, or a WorkerW when a slideshow wallpaper is active).
fn desktop_host() -> HWND {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let class = wide("SHELLDLL_DefView");
        // SAFETY: plain window lookup; `lp` points at the HWND slot owned by desktop_host's frame.
        unsafe {
            if FindWindowExW(h, HWND::default(), PCWSTR(class.as_ptr()), PCWSTR::null()).is_ok_and(|c| !c.is_invalid())
            {
                *(lp.0 as *mut HWND) = h;
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut found = HWND::default();
    // SAFETY: the callback only writes through the pointer to `found`, which outlives EnumWindows.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut found as *mut HWND as isize));
    }
    if !found.is_invalid() {
        return found;
    }
    let progman = wide("Progman");
    // SAFETY: plain window lookup by class name.
    unsafe { FindWindowW(PCWSTR(progman.as_ptr()), PCWSTR::null()) }.unwrap_or_default()
}

/// Z-order: pinned = just above the desktop (below everything else); else topmost / normal.
pub fn stack(hwnd: HWND, on_top: bool, pinned: bool) {
    let after = if pinned {
        let d = desktop_host();
        if d.is_invalid() {
            HWND(1 as _)
        } else {
            d
        } // HWND_BOTTOM fallback
    } else if on_top {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    // SAFETY: valid top-level HWND; only z-order changes (no move/size/activate).
    let r = unsafe { SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE) };
    if std::env::var_os("HWV_TRACE").is_some() {
        // SAFETY: reading our own window's extended style.
        let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        info!("stack {:?} on_top={on_top} pinned={pinned} after={:?} -> {r:?}, ex=0x{ex:X}", hwnd.0, after.0);
    }
}

/// Click-through needs WS_EX_LAYERED + WS_EX_TRANSPARENT (alpha stays 255: opacity is done in CSS).
pub fn set_click_through(hwnd: HWND, on: bool) {
    // SAFETY: reading/writing the extended style of our own window.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let bits = (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) as isize;
        let new = if on { ex | bits } else { ex & !bits };
        if new != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
            if on {
                let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA);
            }
        }
    }
}

pub fn is_topmost(hwnd: HWND) -> bool {
    // SAFETY: reading our own window's extended style.
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    ex & windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST.0 as isize != 0
}

pub fn foreground() -> HWND {
    // SAFETY: no arguments.
    unsafe { GetForegroundWindow() }
}

pub fn open_url(url: &str) {
    let (verb, file) = (wide("open"), wide(url));
    // SAFETY: NUL-terminated buffers that outlive the call.
    unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn copy_text(text: &str) -> Result<(), String> {
    let data = wide(text);
    let bytes = data.len() * 2;
    // SAFETY: standard clipboard sequence; the global block is handed to the clipboard on success
    // (which then owns it) and the clipboard is always closed.
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e| e.to_string())?;
        let r = (|| {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return Err("GlobalLock failed".to_string());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(13 /* CF_UNICODETEXT */, HANDLE(h.0)).map_err(|e| e.to_string())?;
            Ok(())
        })();
        let _ = CloseClipboard();
        r
    }
}

pub fn message_box(title: &str, text: &str) {
    let (t, m) = (wide(title), wide(text));
    // SAFETY: NUL-terminated buffers that outlive the (modal) call.
    unsafe {
        MessageBoxW(HWND::default(), PCWSTR(m.as_ptr()), PCWSTR(t.as_ptr()), MB_ICONERROR);
    }
}

/// Attach to the console of the shell that started us (GUI-subsystem exe), so CLI output shows up there.
pub fn attach_parent_console() -> bool {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: no pointers; fails harmlessly when there is no parent console.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok()
}
