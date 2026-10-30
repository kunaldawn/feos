//! Small `no_std` trigonometry helpers.
//!
//! `core` does not expose `f32::sin` or `f32::cos`, and pulling in `libm` just
//! for spinners and star icons is not worth it. These approximations use the
//! Bhaskara I sine formula with the standard correction term; they are accurate
//! to well under a pixel at icon sizes.

use core::f32::consts::PI;

/// Approximate sine.
pub(crate) fn sin(mut x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let tau = 2.0 * PI;
    x %= tau;
    if x > PI {
        x -= tau;
    } else if x < -PI {
        x += tau;
    }
    let b = 4.0 / PI;
    let c = -4.0 / (PI * PI);
    let y = b * x + c * x * x.abs();
    0.225 * (y * y.abs() - y) + y
}

/// Approximate cosine.
pub(crate) fn cos(x: f32) -> f32 {
    sin(x + PI / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn key_angles_match_the_real_functions() {
        assert!(close(sin(0.0), 0.0));
        assert!(close(sin(PI / 2.0), 1.0));
        assert!(close(sin(PI), 0.0));
        assert!(close(sin(-PI / 2.0), -1.0));
        assert!(close(cos(0.0), 1.0));
        assert!(close(cos(PI), -1.0));
        assert!(close(cos(PI / 2.0), 0.0));
    }

    #[test]
    fn large_and_invalid_inputs_stay_finite() {
        assert!(sin(1000.0).is_finite());
        assert_eq!(sin(f32::NAN), 0.0);
        assert_eq!(sin(f32::INFINITY), 0.0);
    }
}
