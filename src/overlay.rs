//! A per-monitor layered window that draws a thick accent frame around the
//! focused tile.
//!
//! The DWM paints exactly one pixel of accent around a window and offers no way
//! to thicken it — `DWMWA_VISIBLE_FRAME_BORDER_THICKNESS` exists but is
//! read-only — so a thicker border has to be drawn by us. This module owns that
//! drawing surface: one click-through, never-focused, taskbar-less window per
//! monitor, painted with per-pixel alpha through `UpdateLayeredWindow`.
//!
//! It is deliberately *not* a second geometry authority. It is handed a
//! rectangle that `wm` already computed and draws a frame inside it; a monitor
//! with no frame is simply hidden. Nothing here writes application geometry, so
//! the rule that `retile_monitor` is the only writer stays intact.

use crate::layout::Rect;
use crate::monitor::Monitor;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, ShowWindow, UpdateLayeredWindow,
    HTTRANSPARENT, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_NCHITTEST, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER};

const CLASS_NAME: windows::core::PCWSTR = w!("HyprWinOverlayWnd");

/// Pack an `(r, g, b, a)` colour into the BGRA word a 32-bit DIBSection stores.
pub fn pixel(rgba: (u8, u8, u8, u8)) -> u32 {
    let (r, g, b, a) = rgba;
    ((a as u32) << 24) | ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Register the window class once, returning the module handle to create with.
fn class_instance() -> Option<HINSTANCE> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: HINSTANCE(module.0),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // Re-registering an existing class succeeds; there is one overlay per
    // monitor, and each call would otherwise race the first.
    unsafe {
        RegisterClassW(&wc);
    }
    Some(HINSTANCE(module.0))
}

/// Fill an axis-aligned rectangle of `bits` with one BGRA word.
///
/// Coordinates are clipped to the surface, so a frame that runs off the edge —
/// or a monitor that reports a rectangle larger than the surface — can never
/// write out of bounds.
unsafe fn fill_rect(bits: *mut u32, w: i32, h: i32, r: Rect, px: u32) {
    let x0 = r.x.max(0);
    let y0 = r.y.max(0);
    let x1 = (r.x + r.w).min(w);
    let y1 = (r.y + r.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for y in y0..y1 {
        let row = (y as usize) * (w as usize);
        for x in x0..x1 {
            *bits.add(row + x as usize) = px;
        }
    }
}

/// One monitor's overlay surface.
struct Overlay {
    handle: isize,
    hwnd: HWND,
    bounds: Rect,
    mem_dc: HDC,
    bitmap: HBITMAP,
    saved: HGDIOBJ,
    /// Top-down BGRA pixels, owned by the DIBSection for the surface's lifetime.
    bits: *mut u32,
    /// BGRA word the frame is painted with; set before each redraw.
    pixel: u32,
    shown: bool,
}

impl Overlay {
    fn create(handle: isize, bounds: Rect, instance: HINSTANCE) -> Option<Overlay> {
        let w = bounds.w.max(1);
        let h = bounds.h.max(1);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                CLASS_NAME,
                w!("HyprWin overlay"),
                WS_POPUP,
                bounds.x,
                bounds.y,
                w,
                h,
                None,
                None,
                instance,
                None,
            )
        }
        .ok()?;

        let screen = unsafe { GetDC(None) };
        let mem_dc = unsafe { CreateCompatibleDC(screen) };
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative height selects a top-down surface, so row 0 is the
                // top of the screen and the frame maths needs no flip.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        };
        unsafe { ReleaseDC(None, screen) };
        let bitmap = match bitmap {
            Ok(b) => b,
            Err(_) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                    let _ = DeleteDC(mem_dc);
                }
                return None;
            }
        };
        let saved = unsafe { SelectObject(mem_dc, bitmap) };
        let mut ov = Overlay {
            handle,
            hwnd,
            bounds,
            mem_dc,
            bitmap,
            saved,
            bits: bits as *mut u32,
            pixel: 0,
            shown: false,
        };
        ov.clear();
        Some(ov)
    }

    fn clear(&mut self) {
        let len = (self.bounds.w.max(1) as usize) * (self.bounds.h.max(1) as usize);
        unsafe {
            std::ptr::write_bytes(self.bits, 0, len);
        }
    }

    /// Draw a frame `width` pixels thick, just inside `local` (monitor-relative).
    fn draw_frame(&mut self, local: Rect, width: i32) -> bool {
        let w = self.bounds.w.max(1);
        let h = self.bounds.h.max(1);
        // A frame can never be thicker than half the shorter side, or the four
        // bands would overlap into a solid block.
        let t = width.clamp(0, (local.w.min(local.h) / 2).max(0));
        if t <= 0 {
            return false;
        }
        let px = self.pixel;
        unsafe {
            let bits = self.bits;
            fill_rect(bits, w, h, Rect::new(local.x, local.y, local.w, t), px);
            fill_rect(
                bits,
                w,
                h,
                Rect::new(local.x, local.y + local.h - t, local.w, t),
                px,
            );
            fill_rect(bits, w, h, Rect::new(local.x, local.y, t, local.h), px);
            fill_rect(
                bits,
                w,
                h,
                Rect::new(local.x + local.w - t, local.y, t, local.h),
                px,
            );
        }
        true
    }

    fn set_color(&mut self, px: u32) {
        self.pixel = px;
    }

    /// Blit the surface to the screen with per-pixel alpha.
    fn present(&mut self) {
        let size = SIZE {
            cx: self.bounds.w.max(1),
            cy: self.bounds.h.max(1),
        };
        let dst = POINT {
            x: self.bounds.x,
            y: self.bounds.y,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let ok = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                HDC::default(),
                Some(&dst),
                Some(&size),
                self.mem_dc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        };
        if ok.is_ok() && !self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
            self.shown = true;
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            SelectObject(self.mem_dc, self.saved);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

/// Owns one overlay per monitor and keeps them in step with the display set.
#[derive(Default)]
pub struct Overlays {
    windows: Vec<Overlay>,
}

impl Overlays {
    /// Match the overlay windows to `monitors`, recreating them only when the
    /// display set changes. A monitor's bounds can only move when it is
    /// re-enumerated, so an unchanged set needs no work.
    pub fn reconcile(&mut self, monitors: &[Monitor]) {
        if self.windows.len() == monitors.len()
            && self
                .windows
                .iter()
                .zip(monitors)
                .all(|(ov, m)| ov.handle == m.handle && ov.bounds == m.rect)
        {
            return;
        }
        self.windows.clear();
        let Some(instance) = class_instance() else {
            return;
        };
        for m in monitors {
            if let Some(ov) = Overlay::create(m.handle, m.rect, instance) {
                self.windows.push(ov);
            }
        }
    }

    /// Repaint every monitor: the focused tile's frame gets `frame`, everything
    /// else is cleared and hidden.
    ///
    /// `frame` is `(monitor handle, absolute rect)` for the one tile that should
    /// carry the accent. Drawing a single frame means the unfocused tiles keep
    /// only the 1px DWM line beneath, which is the phased behaviour we chose.
    pub fn redraw(
        &mut self,
        monitors: &[Monitor],
        frame: Option<(isize, Rect)>,
        width: i32,
        color: u32,
    ) {
        self.reconcile(monitors);
        for m in monitors {
            let Some(ov) = self.windows.iter_mut().find(|o| o.handle == m.handle) else {
                continue;
            };
            ov.clear();
            ov.set_color(color);
            let keep = match frame {
                Some((mon, r)) if mon == m.handle && width > 0 => {
                    let local = Rect::new(r.x - m.rect.x, r.y - m.rect.y, r.w, r.h);
                    ov.draw_frame(local, width)
                }
                _ => false,
            };
            if keep {
                ov.present();
            } else {
                ov.hide();
            }
        }
    }

    /// Take the overlays off the screen without destroying them.
    pub fn hide_all(&mut self) {
        for ov in &mut self.windows {
            ov.hide();
        }
    }
}
