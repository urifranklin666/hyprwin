//! Display enumeration and work-area lookup.

use crate::layout::Rect;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub handle: isize,
    /// Full bounds including the taskbar.
    pub rect: Rect,
    /// Usable area, excluding the taskbar and docked appbars.
    pub work: Rect,
    pub primary: bool,
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut info).as_bool() {
        out.push(Monitor {
            handle: hmon.0 as isize,
            rect: Rect::new(
                info.rcMonitor.left,
                info.rcMonitor.top,
                info.rcMonitor.right - info.rcMonitor.left,
                info.rcMonitor.bottom - info.rcMonitor.top,
            ),
            work: Rect::new(
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right - info.rcWork.left,
                info.rcWork.bottom - info.rcWork.top,
            ),
            primary: info.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
        });
    }
    TRUE
}

/// All displays, primary first.
pub fn enumerate() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_proc),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    out.sort_by_key(|m| !m.primary);
    out
}

/// The monitor a window currently sits on.
pub fn monitor_for_window(hwnd: HWND) -> Option<Monitor> {
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if hmon.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(hmon, &mut info) }.as_bool() {
        return None;
    }
    Some(Monitor {
        handle: hmon.0 as isize,
        rect: Rect::new(
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right - info.rcMonitor.left,
            info.rcMonitor.bottom - info.rcMonitor.top,
        ),
        work: Rect::new(
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right - info.rcWork.left,
            info.rcWork.bottom - info.rcWork.top,
        ),
        primary: info.dwFlags & 1 != 0,
    })
}

/// Device name of a monitor (e.g. `\\.\DISPLAY1`). Unused today, kept for rules.
#[allow(dead_code)]
pub fn device_name(hmon: HMONITOR) -> String {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        let ptr = &mut info as *mut MONITORINFOEXW as *mut MONITORINFO;
        if !GetMonitorInfoW(hmon, ptr).as_bool() {
            return String::new();
        }
    }
    let end = info
        .szDevice
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(info.szDevice.len());
    String::from_utf16_lossy(&info.szDevice[..end])
}
