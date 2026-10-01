//! Best-effort animation: we cannot interpolate inside a compositor, so we step
//! `SetWindowPos` ourselves over a short interval with an easing curve.
//!
//! The maths here is pure and unit-tested; only `win32::animate_to` touches the OS.

use crate::layout::Rect;

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

/// Number of frames for a duration at a fixed frame interval.
pub fn frame_count(duration_ms: u32, frame_ms: u32) -> u32 {
    if duration_ms == 0 || frame_ms == 0 {
        return 1;
    }
    (duration_ms / frame_ms).max(1)
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

    #[test]
    fn frame_count_never_returns_zero() {
        assert_eq!(frame_count(120, 8), 15);
        assert_eq!(frame_count(0, 8), 1);
        assert_eq!(frame_count(120, 0), 1);
        assert_eq!(frame_count(4, 16), 1);
    }
}
