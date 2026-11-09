//! `feos-math`: the floating-point functions `core` doesn't have.
//!
//! `core` gives `f64` its arithmetic, `abs` and `copysign`, but not roots,
//! rounding, logarithms, powers or trigonometry: those come from the platform's
//! C library under `std`. These are self-contained replacements for code that
//! needs them to be right, rather than close enough for an icon: an equaliser's
//! filter coefficients, or a visualiser preset's expressions, which feed their
//! own results back in every frame and so magnify any error.
//!
//! Everything is computed in `f64`, with the usual reductions (Cody-Waite for
//! the trigonometric functions, exponent splitting for logs and exponentials)
//! followed by short series. Results are within a few units in the last place
//! for ordinary arguments; the host tests compare against `std`. The `f32`
//! versions at the end round the `f64` result.

#![cfg_attr(not(test), no_std)]

use core::f64::consts::{FRAC_PI_2, FRAC_PI_6, LN_10, PI, SQRT_2};

const LN2_HI: f64 = 6.931_471_803_691_238e-1;
const LN2_LO: f64 = 1.908_214_929_270_587_7e-10;
const INV_LN2: f64 = core::f64::consts::LOG2_E;
/// π/2 in three parts, the first two with trailing zeros so that a small
/// multiple of them is exact (fdlibm's `pio2_1`, `pio2_1t`).
const PIO2_1: f64 = 1.570_796_326_734_125_6;
const PIO2_1T: f64 = 6.077_100_506_506_192e-11;
const INV_PIO2: f64 = core::f64::consts::FRAC_2_PI;
const SQRT_3: f64 = 1.732_050_807_568_877_2;
/// tan(π/12): above this, `atan` shifts its argument by π/6.
const TAN_PI_12: f64 = 0.267_949_192_431_122_7;
/// Doubles at or above this are integers already.
const INTEGRAL: f64 = 4_503_599_627_370_496.0;

/// The largest integer no greater than `x`.
pub fn floor(x: f64) -> f64 {
    if !(x.abs() < INTEGRAL) {
        return x;
    }
    let whole = x as i64 as f64;
    if whole > x {
        whole - 1.0
    } else {
        whole
    }
}

/// The smallest integer no less than `x`.
pub fn ceil(x: f64) -> f64 {
    -floor(-x)
}

/// `x` with its fraction dropped, toward zero.
pub fn trunc(x: f64) -> f64 {
    if !(x.abs() < INTEGRAL) {
        return x;
    }
    x as i64 as f64
}

/// The nearest integer to `x`, halves away from zero.
pub fn round(x: f64) -> f64 {
    if !(x.abs() < INTEGRAL) {
        return x;
    }
    let whole = trunc(x);
    let rest = x - whole;
    if rest >= 0.5 {
        whole + 1.0
    } else if rest <= -0.5 {
        whole - 1.0
    } else {
        whole
    }
}

/// The remainder of `x / y`, with the sign of `x`, as C's `fmod`.
///
/// Exact, by long division of the mantissas (as musl's), so `fmod(0.5, 0.001)`
/// is the tiny remainder the binary values really leave, not zero.
pub fn fmod(x: f64, y: f64) -> f64 {
    let mut ux = x.to_bits();
    let mut uy = y.to_bits();
    let mut ex = ((ux >> 52) & 0x7ff) as i64;
    let mut ey = ((uy >> 52) & 0x7ff) as i64;
    let sign = ux >> 63;
    if uy << 1 == 0 || y.is_nan() || ex == 0x7ff {
        return f64::NAN;
    }
    if ux << 1 <= uy << 1 {
        if ux << 1 == uy << 1 {
            return 0.0 * x;
        }
        return x;
    }
    // Normalize both mantissas, subnormals included.
    if ex == 0 {
        let mut probe = ux << 12;
        while probe >> 63 == 0 {
            ex -= 1;
            probe <<= 1;
        }
        ux <<= (1 - ex) as u64;
    } else {
        ux &= u64::MAX >> 12;
        ux |= 1u64 << 52;
    }
    if ey == 0 {
        let mut probe = uy << 12;
        while probe >> 63 == 0 {
            ey -= 1;
            probe <<= 1;
        }
        uy <<= (1 - ey) as u64;
    } else {
        uy &= u64::MAX >> 12;
        uy |= 1u64 << 52;
    }
    while ex > ey {
        let rest = ux.wrapping_sub(uy);
        if rest >> 63 == 0 {
            if rest == 0 {
                return 0.0 * x;
            }
            ux = rest;
        }
        ux <<= 1;
        ex -= 1;
    }
    let rest = ux.wrapping_sub(uy);
    if rest >> 63 == 0 {
        if rest == 0 {
            return 0.0 * x;
        }
        ux = rest;
    }
    while ux >> 52 == 0 {
        ux <<= 1;
        ex -= 1;
    }
    if ex > 0 {
        ux -= 1u64 << 52;
        ux |= (ex as u64) << 52;
    } else {
        ux >>= (1 - ex) as u64;
    }
    f64::from_bits(ux | (sign << 63))
}

/// The non-negative square root of `x`, or NaN when `x` is negative.
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 || x.is_infinite() {
        return x;
    }
    // Subnormals are lifted into the normal range first.
    let (x, adjust) = if x < f64::MIN_POSITIVE {
        (x * 18_014_398_509_481_984.0, 1.0 / 134_217_728.0) // 2^54, 2^-27
    } else {
        (x, 1.0)
    };
    // Halving the exponent is within a few percent; Newton doubles the digits.
    let mut y = f64::from_bits((x.to_bits() >> 1) + (1023u64 << 51));
    for _ in 0..5 {
        y = 0.5 * (y + x / y);
    }
    y * adjust
}

/// `x * 2^k`, without the intermediate overflowing.
fn scale(mut x: f64, mut k: i32) -> f64 {
    while k > 1023 {
        x *= f64::from_bits(2046u64 << 52); // 2^1023
        k -= 1023;
    }
    while k < -1022 {
        x *= f64::from_bits(1u64 << 52); // 2^-1022
        k += 1022;
    }
    x * f64::from_bits(((k + 1023) as u64) << 52)
}

/// e raised to `x`.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.782_712_893_384 {
        return f64::INFINITY;
    }
    if x < -745.133_219_101_941_1 {
        return 0.0;
    }
    let k = round(x * INV_LN2);
    let r = (x - k * LN2_HI) - k * LN2_LO;
    // |r| <= ln2 / 2: thirteen terms leave less than 1e-17.
    let mut sum = 1.0;
    let mut index = 13;
    while index > 0 {
        sum = 1.0 + r * sum / index as f64;
        index -= 1;
    }
    scale(sum, k as i32)
}

/// The natural logarithm of `x`.
pub fn ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return x;
    }
    let mut bits = x.to_bits();
    let mut exponent = ((bits >> 52) & 0x7ff) as i32;
    if exponent == 0 {
        bits = (x * 18_014_398_509_481_984.0).to_bits(); // 2^54
        exponent = ((bits >> 52) & 0x7ff) as i32 - 54;
    }
    exponent -= 1023;
    let mut mantissa = f64::from_bits((bits & ((1u64 << 52) - 1)) | (1023u64 << 52));
    if mantissa > SQRT_2 {
        mantissa *= 0.5;
        exponent += 1;
    }
    // ln(m) = 2 atanh(s), with |s| <= 0.172.
    let s = (mantissa - 1.0) / (mantissa + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = s;
    for k in 1..=12 {
        term *= s2;
        sum += term / (2 * k + 1) as f64;
    }
    let e = exponent as f64;
    e * LN2_HI + (2.0 * sum + e * LN2_LO)
}

/// The base-10 logarithm of `x`.
pub fn log10(x: f64) -> f64 {
    ln(x) / LN_10
}

/// The base-2 logarithm of `x`.
pub fn log2(x: f64) -> f64 {
    ln(x) * INV_LN2
}

/// `x` raised to `y`, as C's `pow` for finite arguments. A negative `x` has a
/// real power only for an integer `y`; otherwise this is NaN.
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    // Small integer powers by multiplication: exact where it can be, and the
    // common case (squares, cubes, reciprocals).
    if y.abs() <= 64.0 && trunc(y) == y {
        let mut n = y.abs() as u32;
        let mut base = x;
        let mut result = 1.0;
        while n > 0 {
            if n & 1 == 1 {
                result *= base;
            }
            base *= base;
            n >>= 1;
        }
        return if y < 0.0 { 1.0 / result } else { result };
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f64::INFINITY };
    }
    if x < 0.0 {
        if trunc(y) != y {
            return f64::NAN;
        }
        let magnitude = exp(y * ln(-x));
        let odd = y.abs() < INTEGRAL && fmod(y, 2.0) != 0.0;
        return if odd { -magnitude } else { magnitude };
    }
    exp(y * ln(x))
}

/// Reduces `x` to `r` in [-π/4, π/4] and the quadrant `x` was in.
fn reduce(x: f64) -> (i64, f64) {
    if x.abs() < 1_000_000.0 {
        let k = round(x * INV_PIO2);
        (k as i64, (x - k * PIO2_1) - k * PIO2_1T)
    } else {
        // Far out, fold into one turn first. Precision suffers, as it must
        // without an exact π, but the result stays bounded and periodic.
        let folded = fmod(x, 2.0 * PI);
        let k = round(folded * INV_PIO2);
        (k as i64, folded - k * FRAC_PI_2)
    }
}

/// sin(r) for |r| <= π/4.
fn sin_kernel(r: f64) -> f64 {
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    let mut n = 1;
    while n < 17 {
        term *= -r2 / ((n + 1) * (n + 2)) as f64;
        sum += term;
        n += 2;
    }
    sum
}

/// cos(r) for |r| <= π/4.
fn cos_kernel(r: f64) -> f64 {
    let r2 = r * r;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut n = 0;
    while n < 18 {
        term *= -r2 / ((n + 1) * (n + 2)) as f64;
        sum += term;
        n += 2;
    }
    sum
}

/// The sine of `x` radians.
pub fn sin(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (k, r) = reduce(x);
    match k.rem_euclid(4) {
        0 => sin_kernel(r),
        1 => cos_kernel(r),
        2 => -sin_kernel(r),
        _ => -cos_kernel(r),
    }
}

/// The cosine of `x` radians.
pub fn cos(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (k, r) = reduce(x);
    match k.rem_euclid(4) {
        0 => cos_kernel(r),
        1 => -sin_kernel(r),
        2 => -cos_kernel(r),
        _ => sin_kernel(r),
    }
}

/// Both at once, for the price of one reduction.
pub fn sin_cos(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let (k, r) = reduce(x);
    let (s, c) = (sin_kernel(r), cos_kernel(r));
    match k.rem_euclid(4) {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// The tangent of `x` radians.
pub fn tan(x: f64) -> f64 {
    let (s, c) = sin_cos(x);
    s / c
}

/// The arctangent of `x`, in [-π/2, π/2].
pub fn atan(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let negative = x < 0.0;
    let mut t = x.abs();
    let inverted = t > 1.0;
    if inverted {
        t = 1.0 / t;
    }
    let mut offset = 0.0;
    if t > TAN_PI_12 {
        t = (t * SQRT_3 - 1.0) / (t + SQRT_3);
        offset = FRAC_PI_6;
    }
    // |t| <= tan(π/12): the series is below 1e-17 by t^27.
    let t2 = t * t;
    let mut power = t;
    let mut sum = t;
    let mut n = 3;
    while n <= 27 {
        power *= -t2;
        sum += power / n as f64;
        n += 2;
    }
    let mut result = offset + sum;
    if inverted {
        result = FRAC_PI_2 - result;
    }
    if negative {
        -result
    } else {
        result
    }
}

/// The angle of the point (`x`, `y`), in [-π, π].
pub fn atan2(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 {
        return if y > 0.0 {
            FRAC_PI_2
        } else if y < 0.0 {
            -FRAC_PI_2
        } else if x.is_sign_negative() {
            PI.copysign(y)
        } else {
            0.0f64.copysign(y)
        };
    }
    if x.is_infinite() || y.is_infinite() {
        let (yy, xx) = (
            if y.is_infinite() { y.signum() } else { 0.0 },
            if x.is_infinite() { x.signum() } else { 0.0 },
        );
        if xx == 0.0 {
            return FRAC_PI_2.copysign(y);
        }
        if yy == 0.0 {
            return if xx > 0.0 { 0.0f64.copysign(y) } else { PI.copysign(y) };
        }
        return atan2(yy, xx);
    }
    let base = atan(y / x);
    if x > 0.0 {
        base
    } else if !y.is_sign_negative() {
        base + PI
    } else {
        base - PI
    }
}

/// The arcsine of `x`, in [-π/2, π/2], or NaN outside [-1, 1].
pub fn asin(x: f64) -> f64 {
    if !(-1.0..=1.0).contains(&x) {
        return f64::NAN;
    }
    atan2(x, sqrt((1.0 - x) * (1.0 + x)))
}

/// The arccosine of `x`, in [0, π], or NaN outside [-1, 1].
pub fn acos(x: f64) -> f64 {
    if !(-1.0..=1.0).contains(&x) {
        return f64::NAN;
    }
    atan2(sqrt((1.0 - x) * (1.0 + x)), x)
}

/// The hyperbolic tangent of `x`.
pub fn tanh(x: f64) -> f64 {
    if x > 20.0 {
        return 1.0;
    }
    if x < -20.0 {
        return -1.0;
    }
    let e = exp(2.0 * x);
    (e - 1.0) / (e + 1.0)
}

/// `f32` versions, computed in `f64` and rounded.
pub mod f32 {
    /// See [`super::floor`].
    pub fn floor(x: f32) -> f32 {
        super::floor(x as f64) as f32
    }
    /// See [`super::ceil`].
    pub fn ceil(x: f32) -> f32 {
        super::ceil(x as f64) as f32
    }
    /// See [`super::sqrt`].
    pub fn sqrt(x: f32) -> f32 {
        super::sqrt(x as f64) as f32
    }
    /// See [`super::sin`].
    pub fn sin(x: f32) -> f32 {
        super::sin(x as f64) as f32
    }
    /// See [`super::cos`].
    pub fn cos(x: f32) -> f32 {
        super::cos(x as f64) as f32
    }
    /// See [`super::sin_cos`].
    pub fn sin_cos(x: f32) -> (f32, f32) {
        let (s, c) = super::sin_cos(x as f64);
        (s as f32, c as f32)
    }
    /// See [`super::atan2`].
    pub fn atan2(y: f32, x: f32) -> f32 {
        super::atan2(y as f64, x as f64) as f32
    }
    /// See [`super::pow`].
    pub fn pow(x: f32, y: f32) -> f32 {
        super::pow(x as f64, y as f64) as f32
    }
    /// See [`super::exp`].
    pub fn exp(x: f32) -> f32 {
        super::exp(x as f64) as f32
    }
    /// See [`super::ln`].
    pub fn ln(x: f32) -> f32 {
        super::ln(x as f64) as f32
    }
    /// See [`super::log10`].
    pub fn log10(x: f32) -> f32 {
        super::log10(x as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Within `ulps` units in the last place, or `floor` absolutely near zero.
    fn close(got: f64, want: f64, tolerance: f64) -> bool {
        if want.is_nan() {
            return got.is_nan();
        }
        if want.is_infinite() {
            return got == want;
        }
        (got - want).abs() <= tolerance * want.abs().max(1.0)
    }

    fn samples() -> impl Iterator<Item = f64> {
        // A spread of magnitudes and signs, with awkward values among them.
        let mut values = std::vec![
            0.0, -0.0, 1e-300, 1e-10, 0.1, 0.5, 0.7853981633974483, 1.0, 1.5707963267948966,
            2.0, 3.141592653589793, 10.0, 123.456, 1e5, 999_999.9, 3e6, -0.3, -2.5, -1000.25,
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let unit = (seed >> 11) as f64 / (1u64 << 53) as f64;
            let exponent = ((seed & 0x3f) as i32) - 24;
            let sign = if seed & 0x40 == 0 { 1.0 } else { -1.0 };
            values.push(sign * unit * 2f64.powi(exponent));
        }
        values.into_iter()
    }

    #[test]
    fn rounding_matches_std() {
        for x in samples().chain([2.5, -2.5, 0.49999, -0.5, 1e17, f64::INFINITY]) {
            assert_eq!(floor(x), x.floor(), "floor({x})");
            assert_eq!(ceil(x), x.ceil(), "ceil({x})");
            assert_eq!(trunc(x), x.trunc(), "trunc({x})");
            assert_eq!(round(x), x.round(), "round({x})");
        }
        assert!(floor(f64::NAN).is_nan());
    }

    #[test]
    fn remainders_match_std() {
        for x in samples() {
            for y in [1.0, 2.0, -3.0, 0.37, 6.283185307179586, 1e-3] {
                let want = x % y;
                assert!(fmod(x, y) == want || close(fmod(x, y), want, 1e-12), "fmod({x}, {y}) = {} not {want}", fmod(x, y));
            }
        }
        assert!(fmod(1.0, 0.0).is_nan());
    }

    #[test]
    fn roots_match_std() {
        for x in samples().map(f64::abs).chain([4.0, 2.0, 1e300, 5e-324, f64::INFINITY]) {
            assert!(close(sqrt(x), x.sqrt(), 1e-15), "sqrt({x}) = {} not {}", sqrt(x), x.sqrt());
        }
        assert!(sqrt(-1.0).is_nan());
        assert_eq!(sqrt(9.0), 3.0);
    }

    #[test]
    fn exponentials_and_logs_match_std() {
        for x in samples().filter(|x| x.abs() < 700.0) {
            assert!(close(exp(x), x.exp(), 1e-14), "exp({x}) = {} not {}", exp(x), x.exp());
        }
        for x in samples().map(f64::abs).filter(|x| *x > 0.0).chain([5e-324, 1e308]) {
            assert!(close(ln(x), x.ln(), 1e-14), "ln({x}) = {} not {}", ln(x), x.ln());
            assert!(close(log10(x), x.log10(), 1e-14), "log10({x})");
        }
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(exp(-1000.0), 0.0);
        assert_eq!(ln(0.0), f64::NEG_INFINITY);
        assert!(ln(-1.0).is_nan());
    }

    #[test]
    fn powers_match_std() {
        let bases: [f64; 9] = [0.0, 0.5, 1.0, 2.0, 3.7, 10.0, 1e-5, -2.0, -0.5];
        let exponents = [0.0, 1.0, 2.0, -1.0, 0.5, 1.5, 3.0, -2.5, 10.0, 0.333, 100.0];
        for x in bases {
            for y in exponents {
                let want = x.powf(y);
                let got = pow(x, y);
                assert!(close(got, want, 1e-12), "pow({x}, {y}) = {got} not {want}");
            }
        }
    }

    #[test]
    fn trigonometry_matches_std() {
        for x in samples().filter(|x| x.abs() < 1e6) {
            assert!(close(sin(x), x.sin(), 1e-13), "sin({x}) = {} not {}", sin(x), x.sin());
            assert!(close(cos(x), x.cos(), 1e-13), "cos({x}) = {} not {}", cos(x), x.cos());
            let (s, c) = sin_cos(x);
            assert!(close(s, x.sin(), 1e-13) && close(c, x.cos(), 1e-13));
            if x.cos().abs() > 1e-3 {
                assert!(close(tan(x), x.tan(), 1e-10), "tan({x})");
            }
            assert!(close(atan(x), x.atan(), 1e-15), "atan({x}) = {} not {}", atan(x), x.atan());
        }
        // Far out, only boundedness is promised.
        assert!(sin(1e12).abs() <= 1.0 && cos(-3e15).abs() <= 1.0);
        assert!(sin(f64::INFINITY).is_nan());
    }

    #[test]
    fn inverse_trigonometry_matches_std() {
        for x in samples() {
            for y in [1.0, -1.0, 0.0, 0.25, -7.5, 1e-9] {
                assert!(close(atan2(x, y), x.atan2(y), 1e-14), "atan2({x}, {y})");
                assert!(close(atan2(y, x), y.atan2(x), 1e-14), "atan2({y}, {x})");
            }
        }
        for step in 0..=200 {
            let x = -1.0 + step as f64 / 100.0;
            assert!(close(asin(x), x.asin(), 1e-14), "asin({x})");
            assert!(close(acos(x), x.acos(), 1e-14), "acos({x})");
        }
        assert!(asin(1.5).is_nan());
    }

    #[test]
    fn single_precision_rounds_the_double_result() {
        assert_eq!(f32::sqrt(2.0), 2.0f32.sqrt());
        assert!((f32::sin(1.0) - 1.0f32.sin()).abs() < 1e-7);
        assert_eq!(f32::floor(-1.5), -2.0);
    }
}
