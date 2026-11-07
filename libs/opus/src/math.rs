//! What `std` adds to floats, for a crate without it. All of these go through `f64`, so
//! `f32` results are as good as rounding allows.

const PI: f64 = core::f64::consts::PI;

fn sqrt64(x: f64) -> f64 {
    if x <= 0.0 || x.is_nan() || x.is_infinite() {
        return if x == 0.0 || x == f64::INFINITY { x } else { f64::NAN };
    }
    // Half the exponent, and then Newton: each step doubles the good bits.
    let bits = x.to_bits();
    let mut y = f64::from_bits((bits >> 1) + (1023u64 << 51));
    for _ in 0..5 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// Sine and cosine on an angle within a quarter turn of zero, by their series.
fn sin_small(x: f64) -> f64 {
    let z = x * x;
    x * (1.0
        + z * (-1.0 / 6.0
            + z * (1.0 / 120.0
                + z * (-1.0 / 5040.0
                    + z * (1.0 / 362880.0
                        + z * (-1.0 / 39916800.0
                            + z * (1.0 / 6227020800.0 + z * (-1.0 / 1307674368000.0))))))))
}

fn cos_small(x: f64) -> f64 {
    let z = x * x;
    1.0 + z * (-0.5
        + z * (1.0 / 24.0
            + z * (-1.0 / 720.0
                + z * (1.0 / 40320.0
                    + z * (-1.0 / 3628800.0
                        + z * (1.0 / 479001600.0 + z * (-1.0 / 87178291200.0)))))))
}

/// The quadrant, and what's left over.
fn reduce(x: f64) -> (i64, f64) {
    let turns = x / (PI / 2.0);
    let quadrant = if turns >= 0.0 { (turns + 0.5) as i64 } else { (turns - 0.5) as i64 };
    (quadrant, x - quadrant as f64 * (PI / 2.0))
}

fn sin64(x: f64) -> f64 {
    let (quadrant, rest) = reduce(x);
    match quadrant & 3 {
        0 => sin_small(rest),
        1 => cos_small(rest),
        2 => -sin_small(rest),
        _ => -cos_small(rest),
    }
}

fn cos64(x: f64) -> f64 {
    let (quadrant, rest) = reduce(x);
    match quadrant & 3 {
        0 => cos_small(rest),
        1 => -sin_small(rest),
        2 => -cos_small(rest),
        _ => sin_small(rest),
    }
}

/// `2^x`.
fn exp2_64(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let x = x.clamp(-1000.0, 1000.0);
    let whole = if x >= 0.0 { x as i64 } else { (x as i64) - 1 };
    let part = (x - whole as f64) * core::f64::consts::LN_2;
    // e^part with part in [0, ln 2).
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..18 {
        term *= part / n as f64;
        sum += term;
    }
    sum * f64::from_bits(((whole + 1023) as u64) << 52)
}

/// The float methods this crate uses that are in `std`, not `core`.
#[allow(dead_code)]
pub trait Float: Sized {
    fn sqrt(self) -> Self;
    fn sin_cos(self) -> (Self, Self);
    fn sin(self) -> Self;
    fn cos(self) -> Self;
    fn round(self) -> Self;
    fn powi(self, n: i32) -> Self;
    /// Only 2 is ever raised to a fractional power here, so that's all this does.
    fn powf(self, exponent: Self) -> Self;
}

impl Float for f64 {
    fn sqrt(self) -> f64 {
        sqrt64(self)
    }
    fn sin_cos(self) -> (f64, f64) {
        (sin64(self), cos64(self))
    }
    fn sin(self) -> f64 {
        sin64(self)
    }
    fn cos(self) -> f64 {
        cos64(self)
    }
    fn round(self) -> f64 {
        if self.abs() >= 4.5e15 || self.is_nan() {
            return self;
        }
        let whole = (self.abs() + 0.5) as i64 as f64;
        if self < 0.0 { -whole } else { whole }
    }
    fn powi(self, n: i32) -> f64 {
        let mut value = 1.0;
        for _ in 0..n.unsigned_abs() {
            value *= self;
        }
        if n < 0 { 1.0 / value } else { value }
    }
    fn powf(self, exponent: f64) -> f64 {
        debug_assert!(self == 2.0);
        exp2_64(exponent)
    }
}

impl Float for f32 {
    fn sqrt(self) -> f32 {
        sqrt64(self as f64) as f32
    }
    fn sin_cos(self) -> (f32, f32) {
        (sin64(self as f64) as f32, cos64(self as f64) as f32)
    }
    fn sin(self) -> f32 {
        sin64(self as f64) as f32
    }
    fn cos(self) -> f32 {
        cos64(self as f64) as f32
    }
    fn round(self) -> f32 {
        Float::round(self as f64) as f32
    }
    fn powi(self, n: i32) -> f32 {
        Float::powi(self as f64, n) as f32
    }
    fn powf(self, exponent: f32) -> f32 {
        debug_assert!(self == 2.0);
        exp2_64(exponent as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::Float;

    fn close(a: f64, b: f64, tolerance: f64) {
        assert!((a - b).abs() <= tolerance, "{a} against {b}");
    }

    #[test]
    fn the_same_as_std() {
        for i in -2000..2000 {
            let x = i as f64 * 0.0173;
            close(Float::sin(x), f64::sin(x), 1e-13);
            close(Float::cos(x), f64::cos(x), 1e-13);
            let y = (i as f64 + 2000.5) * 0.37;
            close(Float::sqrt(y), f64::sqrt(y), 1e-13);
            close(Float::powf(2.0f64, x / 3.0), f64::powf(2.0, x / 3.0), f64::powf(2.0, x / 3.0) * 1e-13);
            assert_eq!(Float::round(x), f64::round(x));
            assert_eq!(Float::round(x as f32), f32::round(x as f32));
        }
        assert_eq!(Float::round(0.49999997f32), 0.0);
        assert_eq!(Float::round(-2.5f32), -3.0);
        assert_eq!(Float::sqrt(0.0f32), 0.0);
        close(Float::powi(0.9f64, 3), 0.729, 1e-12);
    }
}
