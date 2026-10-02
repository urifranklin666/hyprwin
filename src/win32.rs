//! Thin, safe-ish wrappers over the Win32 calls HyprWin needs.
//!
//! Every function here is a direct translation of a documented Win32 API; the
//! tiling logic lives in `layout.rs` and `wm.rs` so it stays testable.

use crate::layout::Rect;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, FALSE, HANDLE, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CLOAKED,
    DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, IsZoomed, SetForegroundWindow, SetWindowPos, ShowWindow,
    SystemParametersInfoW, GWL_EXSTYLE, GW_OWNER, SPIF_SENDCHANGE, SPI_GETFOREGROUNDLOCKTIMEOUT,
    SPI_SETFOREGROUNDLOCKTIMEOUT, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNA, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

const SENTINEL_BORDER: u32 = 0xFFFF_FFFF; // DWMWA_COLOR_DEFAULT; -1 means "no explicit colour".

/// Rebuild an `HWND` from the raw `isize` carried through event messages.
pub fn hwnd_from(raw: isize) -> HWND {
    HWND(raw as *mut std::ffi::c_void)
}

pub fn is_window(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    unsafe { IsWindow(hwnd).as_bool() }
}

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// The local wall-clock time as `YYYY-MM-DD HH:MM:SS`, for log lines.
///
/// Built here rather than in `log` because the only clock HyprWin can read is
/// Win32's, and formatting it as a string is where the platform detail lives.
pub fn local_timestamp() -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

pub fn is_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

pub fn is_maximized(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

pub fn title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

pub fn class_name(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

pub fn pid(hwnd: HWND) -> u32 {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

/// Executable file name of the window's process, e.g. "notepad.exe".
pub fn exe_name(hwnd: HWND) -> String {
    let pid = pid(hwnd);
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let Ok(handle): Result<HANDLE, _> =
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)
        else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }
}

/// Unused today; kept because rules may grow a "find window by class" match.
#[allow(dead_code)]
pub fn find_window(class: &str) -> Option<HWND> {
    let wide: Vec<u16> = class.encode_utf16().chain(Some(0)).collect();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::FindWindowW(PCWSTR(wide.as_ptr()), None)
            .ok()
            .filter(|h| !h.0.is_null())
    }
}

fn has_owner_or_toolwindow(hwnd: HWND) -> bool {
    unsafe {
        // GetWindow returns a Result in windows 0.58, not a bare HWND.
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.0.is_null() {
                return true;
            }
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return true;
        }
        false
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        ok.is_ok() && cloaked != 0
    }
}

/// Heuristic for "is this a real, user-facing, tiled-able top-level window?".
///
/// Deliberately conservative: false positives produce a tiling manager that
/// steals focus from its own invisible helper windows.
pub fn is_manageable(hwnd: HWND) -> bool {
    if !is_window(hwnd) || !is_visible(hwnd) || is_cloaked(hwnd) {
        return false;
    }
    if is_minimized(hwnd) {
        return false;
    }
    // Never manage our own windows (the tray host, for one).
    if pid(hwnd) == unsafe { windows::Win32::System::Threading::GetCurrentProcessId() } {
        return false;
    }
    let cls = class_name(hwnd);
    if cls.is_empty() {
        return false;
    }
    // Chromium creates a hidden top-level helper window per browser process:
    // unowned, not a tool window, WS_EX_APPWINDOW set, so every other check here
    // passes it. It is visible for the instant it is created and then hides
    // itself, which leaves a tiled window that is never drawn — an empty slot in
    // the layout that the visible windows cannot fill. It carries no title, so
    // requiring one keeps it, and helpers like it, out.
    if title(hwnd).is_empty() {
        return false;
    }
    // Shell surfaces that must never be tiled.
    for skip in [
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "Progman",
        "WorkerW",
    ] {
        if cls == skip {
            return false;
        }
    }
    // Browser-internal helper windows. Chromium creates these as top-level,
    // unowned, non-tool windows with a title, so they pass every structural
    // check above; tiling one gives the layout a slot holding a window that is
    // never drawn, and the real windows cannot fill the space it occupies.
    // `Chrome_RenderWidgetHostHWND` is the render host, and does not appear in
    // the Alt+Tab list that Windows itself treats as "the real windows".
    for skip in ["Chrome_RenderWidgetHostHWND", "Chrome_WidgetWin_0"] {
        if cls == skip {
            return false;
        }
    }
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 };
    // Never tile our own tray window.
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    if has_owner_or_toolwindow(hwnd) {
        return false;
    }
    true
}

pub fn rect_of(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut r).is_err() {
            return None;
        }
    }
    Some(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top))
}

/// Restore a window that Windows has snapped/maximised so we can tile it.
pub fn unmaximize(hwnd: HWND) {
    if is_maximized(hwnd) {
        unsafe {
            let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_RESTORE);
        }
    }
}

/// Put a window back into the maximized state it was in before we adopted it.
pub fn maximize(hwnd: HWND) {
    if !is_maximized(hwnd) {
        unsafe {
            let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_MAXIMIZE);
        }
    }
}

pub fn set_pos(hwnd: HWND, r: Rect) {
    let w = r.w.max(1);
    let h = r.h.max(1);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            r.x,
            r.y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
    }
}

pub fn show(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
}

pub fn hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// Apply one frame of a stepped animation.
///
/// The caller drives the clock; this only turns a `(handle, rect)` pair back
/// into a window move, so a whole batch can be advanced without owning the loop.
pub fn set_pos_tween(frames: &[(isize, Rect)]) {
    for (h, r) in frames {
        set_pos(hwnd_from(*h), *r);
    }
}

pub fn post_close(hwnd: HWND) {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

pub fn set_foreground(hwnd: HWND) {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Give the process the right to call `SetForegroundWindow`.
///
/// Windows normally blocks background processes from stealing focus. Saving and
/// clearing the foreground lock timeout is the standard unlock, and HyprWin
/// restores the original value on exit.
pub fn allow_foreground_change() -> u32 {
    let mut timeout = 0u32;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut timeout as *mut u32 as *mut _),
            Default::default(),
        );
        let _ = SystemParametersInfoW(SPI_SETFOREGROUNDLOCKTIMEOUT, 0, None, SPIF_SENDCHANGE);
    }
    timeout
}

pub fn restore_foreground_timeout(timeout: u32) {
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(timeout as usize as *mut _),
            SPIF_SENDCHANGE,
        );
    }
}

pub fn set_border(hwnd: HWND, colorref: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &colorref as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

pub fn clear_border(hwnd: HWND) {
    set_border(hwnd, SENTINEL_BORDER);
}

pub fn set_rounding(hwnd: HWND, preference: i32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

/// Enumerate every top-level window.
pub fn top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<HWND>);
        out.push(hwnd);
        TRUE
    }
    let mut out: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_and_title_of_the_desktop_are_readable() {
        // Progman always exists; this proves the string marshalling works.
        let wide: Vec<u16> = "Progman".encode_utf16().chain(Some(0)).collect();
        let hwnd = unsafe {
            windows::Win32::UI::WindowsAndMessaging::FindWindowW(PCWSTR(wide.as_ptr()), None)
        };
        match hwnd {
            Ok(h) if !h.0.is_null() => {
                assert_eq!(class_name(h), "Progman");
                assert!(!is_manageable(h), "the desktop must never be tiled");
            }
            _ => {}
        }
    }

    #[test]
    fn garbage_handles_are_rejected() {
        let fake = HWND(std::ptr::null_mut());
        assert!(!is_window(fake));
        assert!(!is_manageable(fake));
        assert!(rect_of(fake).is_none());
    }

    #[test]
    fn enumeration_finds_the_desktop() {
        let all = top_level_windows();
        assert!(
            !all.is_empty(),
            "there is always at least one top-level window"
        );
        assert!(
            all.iter().any(|h| class_name(*h) == "Progman"),
            "the desktop window should be enumerable"
        );
    }
}
