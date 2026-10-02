//! Best-effort animation: we cannot interpolate inside a compositor, so we step
//! `SetWindowPos` ourselves over a short interval with an easing curve.
//!
//! The maths here is pure and unit-tested; only `win32::animate_to` touches the OS.

use crate::layout::Rect;
use std::time::Instant;

/// Normalised easing curve. `t` is clamped to 0..=1.
pub fn ease(kind: &str, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match kind.trim().to_ascii_lowercase().as_str() {
        "linear" => t,
        "quad" => 1.0 - (1.0 - t) * (1.0 - t),
        "expo" => {
            if t >= 1.0 {
                1.0
            } else {
                1.0 - 2f32.powf(-10.0 * t)
            }
        }
        // "cubic" and anything unrecognised: ease-out cubic.
        _ => 1.0 - (1.0 - t).powi(3),
    }
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn lerp_rect(from: Rect, to: Rect, t: f32) -> Rect {
    Rect {
        x: lerp(from.x as f32, to.x as f32, t).round() as i32,
        y: lerp(from.y as f32, to.y as f32, t).round() as i32,
        w: lerp(from.w as f32, to.w as f32, t).round() as i32,
        h: lerp(from.h as f32, to.h as f32, t).round() as i32,
    }
}

/// The frame interval the runner steps at, in milliseconds.
pub const FRAME_MS: u32 = 8;

/// A batch of windows moving towards new rectangles over a fixed duration.
///
/// This holds the whole animation as data rather than running it: the caller
/// samples a frame, applies it, and comes back for the next one. That is what
/// lets the message loop keep servicing input while windows are in flight — the
/// old blocking loop slept the whole duration and did not answer a keypress
/// until it was over.
///
/// Handles are plain `isize` so this type stays testable without a Windows
/// message pump; `wm` maps them to real windows.
#[derive(Debug, Clone)]
pub struct Tween {
    items: Vec<(isize, Rect, Rect)>,
    easing: String,
    duration_ms: u32,
    frame_ms: u32,
    start: Instant,
}

impl Tween {
    pub fn new(
        items: Vec<(isize, Rect, Rect)>,
        duration_ms: u32,
        easing: &str,
        frame_ms: u32,
    ) -> Self {
        Tween {
            items,
            easing: easing.to_string(),
            duration_ms,
            frame_ms: frame_ms.max(1),
            start: Instant::now(),
        }
    }

    /// Add windows to an in-flight tween, or re-aim ones already moving.
    ///
    /// One tween covers every monitor in a multi-monitor retile, so the whole
    /// desktop lands together instead of each monitor paying its own duration.
    pub fn merge(&mut self, items: Vec<(isize, Rect, Rect)>) {
        for (key, from, to) in items {
            match self.items.iter_mut().find(|(k, _, _)| *k == key) {
                Some(slot) => {
                    slot.1 = from;
                    slot.2 = to;
                }
                None => self.items.push((key, from, to)),
            }
        }
    }

    pub fn easing_matches(&self, easing: &str) -> bool {
        self.easing == easing
    }

    pub fn duration(&self) -> u32 {
        self.duration_ms
    }

    /// The rectangles to apply this frame, and whether the tween has finished.
    ///
    /// The clock is passed in rather than read from `self.start`, so the whole
    /// curve is unit-testable without waiting on wall time.
    pub fn sample_at(&self, elapsed_ms: u64) -> (Vec<(isize, Rect)>, bool) {
        let finished = elapsed_ms >= self.duration_ms as u64;
        let t = if self.duration_ms == 0 {
            1.0
        } else {
            (elapsed_ms as f32 / self.duration_ms as f32).min(1.0)
        };
        // The final frame lands exactly on the target; easing towards 1.0 leaves
        // a sub-pixel remainder that rounding could otherwise keep.
        let eased = if finished { 1.0 } else { ease(&self.easing, t) };
        let frames = self
            .items
            .iter()
            .map(|(k, from, to)| (*k, lerp_rect(*from, *to, eased)))
            .collect();
        (frames, finished)
    }

    pub fn sample(&self) -> (Vec<(isize, Rect)>, bool) {
        self.sample_at(self.start.elapsed().as_millis() as u64)
    }

    /// How long to wait before the next frame is due, or 0 once it is finished.
    pub fn wait_ms(&self) -> u64 {
        let elapsed = self.start.elapsed().as_millis() as u64;
        if elapsed >= self.duration_ms as u64 {
            return 0;
        }
        let due = elapsed.saturating_sub(elapsed % self.frame_ms as u64) + self.frame_ms as u64;
        due.saturating_sub(elapsed)
    }

    /// Milliseconds the caller should block before asking for another frame.
    ///
    /// Never 0 for a live tween: a caller that blocks forever would freeze the
    /// animation, and one that never blocks would spin.
    pub fn sleep_ms(&self) -> u64 {
        self.wait_ms().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curve_starts_at_zero_and_ends_at_one() {
        for kind in ["linear", "quad", "cubic", "expo", "nonsense"] {
            assert!(ease(kind, 0.0).abs() < 1e-6, "{kind} at 0");
            assert!((ease(kind, 1.0) - 1.0).abs() < 1e-6, "{kind} at 1");
        }
    }

    #[test]
    fn curves_are_monotonic_and_ease_out_faster_than_linear() {
        for kind in ["quad", "cubic", "expo"] {
            let mut prev = -1.0;
            for i in 0..=20 {
                let t = i as f32 / 20.0;
                let v = ease(kind, t);
                assert!(v >= prev, "{kind} must not go backwards");
                prev = v;
            }
            assert!(
                ease(kind, 0.5) > 0.5,
                "{kind} is ease-out, so it passes halfway early"
            );
        }
    }

    #[test]
    fn input_is_clamped() {
        assert_eq!(ease("linear", -5.0), 0.0);
        assert_eq!(ease("linear", 5.0), 1.0);
    }

    #[test]
    fn lerp_rect_interpolates_both_ends() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(100, 200, 300, 400);
        assert_eq!(lerp_rect(a, b, 0.0), a);
        assert_eq!(lerp_rect(a, b, 1.0), b);
        assert_eq!(lerp_rect(a, b, 0.5), Rect::new(50, 100, 200, 250));
    }

    fn tween(duration: u32) -> Tween {
        Tween::new(
            vec![(7, Rect::new(0, 0, 100, 100), Rect::new(200, 0, 100, 100))],
            duration,
            "linear",
            8,
        )
    }

    #[test]
    fn a_tween_interpolates_and_lands_exactly_on_its_target() {
        let t = tween(100);
        // At the halfway point linear easing puts the window in the middle.
        let (frames, done) = t.sample_at(50);
        assert_eq!(frames[0].1.x, 100);
        assert!(!done);
        // The end of the duration snaps to the target rather than near it, even
        // though easing towards 1.0 would leave rounding behind.
        let (frames, done) = t.sample_at(100);
        assert_eq!(frames[0].1, Rect::new(200, 0, 100, 100));
        assert!(done);
        // Past the end is still the target, and still finished.
        let (frames, done) = t.sample_at(5000);
        assert_eq!(frames[0].1, Rect::new(200, 0, 100, 100));
        assert!(done);
    }

    #[test]
    fn a_zero_duration_tween_is_finished_immediately() {
        let t = tween(0);
        let (frames, done) = t.sample_at(0);
        assert!(done, "no duration means the first sample is the last");
        assert_eq!(frames[0].1, Rect::new(200, 0, 100, 100));
    }

    #[test]
    fn merging_re_aims_a_window_that_is_already_moving() {
        let mut t = tween(100);
        // A second retile arrives mid-flight for the same window from wherever it
        // now is to a new place; the item is updated, not duplicated.
        t.merge(vec![(
            7,
            Rect::new(120, 0, 100, 100),
            Rect::new(400, 0, 100, 100),
        )]);
        assert_eq!(t.sample_at(0).0.len(), 1);
        let (frames, _) = t.sample_at(100);
        assert_eq!(frames[0].1.x, 400);
        // A brand-new window joins the same tween rather than starting its own.
        t.merge(vec![(9, Rect::new(0, 0, 50, 50), Rect::new(10, 0, 50, 50))]);
        let (frames, _) = t.sample_at(0);
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn the_next_frame_wait_is_never_zero_for_a_live_tween() {
        let t = tween(100);
        // Right at the start the whole interval is still due; the point is only
        // that a live tween never asks its caller to wait zero, which would spin.
        assert!(t.sleep_ms() >= 1);
        assert!(t.sleep_ms() <= 8);
        // Once finished there is nothing left to wait for.
        let t = tween(0);
        assert_eq!(t.wait_ms(), 0);
    }
}
