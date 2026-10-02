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

/// Pack an `(r, g, b, a)` colour into the word a 32-bit DIBSection stores.
///
/// A DIBSection holds its bytes in B, G, R, A order, so on the little-endian word
/// red sits in the high colour byte and blue in the low one. Getting this
/// backwards swaps them, which a flat green accent cannot reveal (green is equal
/// in both) but any other colour shows.
pub fn pixel(rgba: (u8, u8, u8, u8)) -> u32 {
    let (r, g, b, a) = rgba;
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Blend `t` of the way from `a` to `b`, both packed BGRA words.
///
/// Used for the focus cross-fade; alpha is carried through unchanged, so a
/// translucent accent stays translucent while it changes colour.
pub fn blend_pixel(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |shift: u32| -> u32 {
        let ca = ((a >> shift) & 0xFF) as f32;
        let cb = ((b >> shift) & 0xFF) as f32;
        (ca + (cb - ca) * t).round() as u32
    };
    (mix(24) << 24) | (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// One accent frame to paint: the area to cover, how thick, and which colour.
///
/// The colour may be a gradient: `color_a` is at the top-left corner and
/// `color_b` at the bottom-right, and the two are interpolated per pixel. When
/// they are equal the result is a flat frame, so the plain case needs no special
/// path.
#[derive(Clone, Copy)]
pub struct Frame {
    /// Monitor handle whose surface this frame belongs to.
    pub monitor: isize,
    /// Monitor-relative rectangle of the tile the frame wraps.
    pub local: Rect,
    /// Thickness in pixels.
    pub width: i32,
    /// Corner radius in pixels.
    pub radius: i32,
    /// Colour at the top-left, as a packed BGRA word.
    pub color_a: u32,
    /// Colour at the bottom-right.
    pub color_b: u32,
}

/// Is `p` inside the rounded rectangle `r` with corner radius `radius`?
///
/// The point is projected onto the rectangle formed by the four corner centres;
/// the distance from that projection is zero everywhere except a corner square,
/// so a single circle test rounds exactly the four corners and leaves the edges
/// straight. The projection is also what bounds the shape: a point past an edge
/// projects to a corner centre and lands outside the circle.
fn in_rounded(p: (i32, i32), r: Rect, radius: i32) -> bool {
    if r.w <= 0 || r.h <= 0 {
        return false;
    }
    // Leave at least one pixel between opposing corner centres, so the corner
    // circles never cross and the projection below stays well ordered.
    let rad = radius.clamp(0, (r.w.min(r.h) - 1).max(0) / 2);
    if rad == 0 {
        return p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h;
    }
    let cx = p.0.clamp(r.x + rad, r.x + r.w - 1 - rad);
    let cy = p.1.clamp(r.y + rad, r.y + r.h - 1 - rad);
    let (dx, dy) = (p.0 - cx, p.1 - cy);
    dx * dx + dy * dy <= rad * rad
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

    /// Draw one accent frame (band thickness `f.width`, corners `f.radius`).
    ///
    /// A pixel is painted when it lies inside the frame's rounded outer edge but
    /// not inside the rounded inner edge. Doing it by test rather than by four
    /// bands is what makes the corners round: the overlap between the two shapes
    /// in each corner square is exactly the arc.
    fn draw(&mut self, f: Frame) -> bool {
        let w = self.bounds.w.max(1);
        let h = self.bounds.h.max(1);
        let outer = f.local;
        // A frame can never be thicker than the tile allows, or the inner shape
        // would vanish and the whole rectangle would fill in.
        let t = f
            .width
            .clamp(0, (outer.w.min(outer.h)).max(0) / 2);
        if t <= 0 || outer.w <= 0 || outer.h <= 0 {
            return false;
        }
        // The inner edge keeps the same corner radius, so the band is an even
        // thickness all the way around the arc. Its radius shrinks by the
        // thickness, bottoming out at a square corner rather than inverting.
        let inner = Rect::new(
            outer.x + t,
            outer.y + t,
            outer.w - 2 * t,
            outer.h - 2 * t,
        );
        let outer_rad = f.radius.clamp(0, (outer.w.min(outer.h) - 1).max(0) / 2);
        let inner_rad = (outer_rad - t).max(0);
        let (a, b) = (f.color_a, f.color_b);
        // Gradient runs corner to corner, so a wide tile does not wash out; the
        // span is never zero, or the divisor below would be.
        let span = ((outer.w + outer.h) as f32).max(1.0);
        let (ox, oy) = (outer.x, outer.y);
        let sw = w as usize;
        // Only the frame's own bounding box needs visiting.
        let x0 = ox.max(0);
        let y0 = oy.max(0);
        let x1 = (ox + outer.w).min(w);
        let y1 = (oy + outer.h).min(h);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        unsafe {
            for y in y0..y1 {
                let row = (y as usize) * sw;
                for x in x0..x1 {
                    let p = (x, y);
                    if !in_rounded(p, outer, outer_rad) || in_rounded(p, inner, inner_rad) {
                        continue;
                    }
                    let g = ((x - ox) as f32 + (y - oy) as f32) / span;
                    *self.bits.add(row + x as usize) = blend_pixel(a, b, g);
                }
            }
        }
        true
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

    /// Repaint every monitor from `frames`, one frame per tiled window to accent.
    ///
    /// A monitor with no frames is cleared and hidden, which is what keeps the
    /// unfocused tiles to just their 1px DWM line when only the focused accent is
    /// configured. Frames may be listed in any order; each is clipped to its own
    /// monitoring surface by the drawing code.
    pub fn redraw(&mut self, monitors: &[Monitor], frames: &[Frame]) {
        self.reconcile(monitors);
        for m in monitors {
            let Some(ov) = self.windows.iter_mut().find(|o| o.handle == m.handle) else {
                continue;
            };
            ov.clear();
            let mut painted = false;
            for f in frames.iter().filter(|f| f.monitor == m.handle) {
                if ov.draw(*f) {
                    painted = true;
                }
            }
            if painted {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(r: u8, g: u8, b: u8, a: u8) -> u32 {
        pixel((r, g, b, a))
    }

    #[test]
    fn pixel_places_red_high_and_blue_low() {
        // The word is memory [B, G, R, A] read as little-endian, so red is the
        // high colour byte and blue the low one. An asymmetric triple pins it;
        // a green one would pass either way round.
        assert_eq!(pixel((0x11, 0x22, 0x33, 0x44)), 0x4411_2233);
        assert_eq!(pixel((0xFF, 0x00, 0x00, 0xFF)), 0xFFFF_0000, "red");
        assert_eq!(pixel((0x00, 0x00, 0xFF, 0xFF)), 0xFF00_00FF, "blue");
    }

    #[test]
    fn blend_hits_both_ends_and_the_middle() {
        let a = pack(0, 0, 0, 255);
        let b = pack(200, 100, 50, 255);
        assert_eq!(blend_pixel(a, b, 0.0), a);
        assert_eq!(blend_pixel(a, b, 1.0), b);
        let mid = blend_pixel(a, b, 0.5);
        assert_eq!(((mid >> 16) & 0xFF) as u8, 100, "red channel halves");
        assert_eq!(((mid >> 8) & 0xFF) as u8, 50, "green channel halves");
        assert_eq!(((mid >> 24) & 0xFF) as u8, 255, "alpha is preserved");
    }

    #[test]
    fn blend_clamps_t_outside_the_unit_range() {
        let a = pack(10, 10, 10, 255);
        let b = pack(20, 20, 20, 255);
        assert_eq!(blend_pixel(a, b, -3.0), a);
        assert_eq!(blend_pixel(a, b, 7.0), b);
    }

    #[test]
    fn a_zero_radius_shape_is_the_plain_rectangle() {
        let r = Rect::new(5, 5, 10, 10);
        assert!(in_rounded((5, 5), r, 0), "top-left corner is inside");
        assert!(in_rounded((14, 14), r, 0), "bottom-right pixel is inside");
        assert!(!in_rounded((4, 5), r, 0), "one pixel left is outside");
        assert!(!in_rounded((15, 5), r, 0), "one past the right edge is outside");
    }

    #[test]
    fn rounding_cuts_the_corners_but_not_the_edges() {
        let r = Rect::new(0, 0, 20, 20);
        let rad = 5;
        // The extreme corner pixel falls outside the arc.
        assert!(!in_rounded((0, 0), r, rad));
        // The edge midpoints stay: rounding must not eat the straight sides.
        assert!(in_rounded((0, 10), r, rad), "left edge midpoint");
        assert!(in_rounded((10, 0), r, rad), "top edge midpoint");
        // The point diagonally in from the corner, at the arc's centre, is in.
        assert!(in_rounded((rad, rad), r, rad));
    }

    #[test]
    fn a_frame_has_bands_and_not_a_solid_fill() {
        // 20x20 tile, 4px band: the centre must stay unpainted, the border must
        // be painted, so the inner shape really is cut out.
        let outer = Rect::new(0, 0, 20, 20);
        let t = 4;
        let inner = Rect::new(t, t, 20 - 2 * t, 20 - 2 * t);
        assert!(in_rounded((0, 10), outer, 0) && !in_rounded((0, 10), inner, 0));
        assert!(
            !in_rounded((10, 10), outer, 0) || in_rounded((10, 10), inner, 0),
            "the middle of the tile is inside the inner shape, so unpainted"
        );
    }
}
