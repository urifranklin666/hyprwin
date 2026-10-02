//! The focused-tile accent: DWM's 1px line plus the thick overlay frame, and
//! the cross-fade between focus changes.
//!
//! Split out of `wm` because the geometry of the accent (radius, per-window
//! width and colour) is a self-contained concern that reads the layout but does
//! not change it. `use super::*` reaches the manager's private state.

use super::*;

impl Wm {
    /// Effective overlay corner radius: `border_radius`, or the DWM preference
    /// when it is -1.
    ///
    /// The DWM rounds a window with a fixed radius (about 8px at 100% DPI) that
    /// we cannot read back, so following it is a fixed default rather than a
    /// measurement. `rounding: 2` is the only setting that rounds.
    fn accent_radius(&self) -> i32 {
        if self.cfg.border_radius >= 0 {
            self.cfg.border_radius
        } else if self.cfg.rounding == 2 {
            8
        } else {
            0
        }
    }

    /// Build the accent frames that should be on screen right now.
    ///
    /// The focused tile takes `border_width` and the active colour; every other
    /// tiled window takes `border_width_inactive` and the inactive colour, and is
    /// omitted entirely when that width is 0 (the default), leaving its 1px DWM
    /// line. Floating and fullscreen windows are skipped: their real geometry is
    /// not the rect the layout holds, so an overlay frame would not match.
    fn accent_frames(&self) -> Vec<crate::overlay::Frame> {
        if !self.cfg.show_border {
            return Vec::new();
        }
        let active = win32::foreground();
        let a = crate::overlay::pixel(crate::config::parse_rgba(&self.cfg.border_color_active));
        // An unset second colour means a flat accent, which is a gradient whose
        // two ends happen to be equal.
        let alt = self.cfg.border_color_active_alt.trim();
        let a2 = if alt.is_empty() {
            a
        } else {
            crate::overlay::pixel(crate::config::parse_rgba(alt))
        };
        let inactive =
            crate::overlay::pixel(crate::config::parse_rgba(&self.cfg.border_color_inactive));
        let radius = self.accent_radius();
        let mut frames = Vec::new();
        for w in self.windows.values() {
            if w.floating || self.workspaces.is_fullscreen(w.id) {
                continue;
            }
            let Some(rect) = self.placed.get(&w.id) else {
                continue;
            };
            let focused = w.hwnd == active;
            let width = if focused {
                self.cfg.border_width
            } else {
                self.cfg.border_width_inactive
            };
            if width <= 0 {
                continue;
            }
            let Some(mon) = self.monitors.iter().find(|m| m.handle == w.monitor) else {
                continue;
            };
            frames.push(crate::overlay::Frame {
                monitor: w.monitor,
                local: Rect::new(
                    rect.x - mon.rect.x,
                    rect.y - mon.rect.y,
                    rect.w,
                    rect.h,
                ),
                width,
                radius,
                color_a: if focused { a } else { inactive },
                color_b: if focused { a2 } else { inactive },
            });
        }
        frames
    }

    pub fn apply_border_focus(&mut self) {
        if !self.cfg.show_border {
            self.overlays.hide_all();
            self.last_frames.clear();
            self.last_focus = None;
            self.border_fade = None;
            return;
        }
        // This sets the final colour outright, so any fade still painting a
        // previous target is stale: drop it and let this call be the only writer.
        self.finish_border_fade();
        let active = win32::foreground();
        let active_color = crate::config::parse_color(&self.cfg.border_color_active);
        let inactive_color = crate::config::parse_color(&self.cfg.border_color_inactive);
        for w in self.windows.values() {
            if w.floating {
                continue;
            }
            let color = if w.hwnd == active {
                active_color
            } else {
                inactive_color
            };
            win32::set_border(w.hwnd, color);
        }
        let frames = self.accent_frames();
        self.overlays.redraw(&self.monitors, &frames);
        self.last_frames = frames;
        self.last_focus = Some(active.0 as isize);
    }

    /// Apply the accent, cross-fading the colour over `animation_ms`.
    ///
    /// Use this when *focus* moved, so the new tile eases into the active colour
    /// instead of snapping. It is a no-op when the focused window has not changed,
    /// so a retile that only moves tiles does not pay for a fade it would not
    /// show. With `animation_ms: 0` it is exactly `apply_border_focus`.
    pub fn apply_border_focus_animated(&mut self) {
        let active = win32::foreground();
        let key = Some(active.0 as isize);
        let to = self.accent_frames();
        let duration = self.cfg.animation_ms;
        // Nothing to fade to, or nobody moved: draw and remember. The fade is
        // only for a focus *change*, so an unchanged focus takes the instant path.
        if duration == 0
            || self.last_focus == key
            || self.last_frames.len() != to.len()
            || self.last_frames.is_empty()
        {
            self.apply_border_focus();
            return;
        }
        // Pair each destination frame with the frame it is replacing. Frames are
        // built in `windows` order, which is stable between calls, and any layout
        // change that moved a window sends us down the instant path above.
        self.border_fade = Some(BorderFade {
            from: self.last_frames.clone(),
            to,
            easing: self.cfg.animation_easing.clone(),
            start: Instant::now(),
            duration_ms: duration,
        });
        // The focus already moved when this was called, so record it now: a
        // second call before the fade ends must take the instant path rather
        // than restart from a half-faded frame.
        self.last_focus = key;
    }

    /// End any running cross-fade at once, keeping the frame it was heading to.
    ///
    /// The overlay redraw is left to `apply_border_focus`, which every caller of
    /// this runs immediately afterwards, so there is one writer of the final
    /// colour rather than two racing ones.
    pub(super) fn finish_border_fade(&mut self) {
        if let Some(fade) = self.border_fade.take() {
            self.last_frames = fade.to;
        }
    }
}
