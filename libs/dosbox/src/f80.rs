//! The 80-bit extended real format and its software arithmetic.
//!
//! The `F80` register type comes from `rust-dos` (GPL-2.0-or-later), where it
//! was based on mwemu (source: https://github.com/sha0coder/mwemu); see
//! LICENSING.md. The arithmetic here is integer-only: `no_std` has no floating
//! point library, and the x87 has more precision than `f64` could carry anyway.
//!
//! Every operation takes an [`Env`], which supplies the rounding and precision
//! control and collects the exception flags. Results are always the x87's
//! masked responses. Transcendental functions work in [`Xf`], a private float
//! with a 128-bit significand, and are rounded to 80 bits only at the end.

use core::cmp::Ordering;

/// Invalid operation.
pub const IE: u16 = 0x0001;
/// Denormal operand.
pub const DE: u16 = 0x0002;
/// Zero divide.
pub const ZE: u16 = 0x0004;
/// Overflow.
pub const OE: u16 = 0x0008;
/// Underflow.
pub const UE: u16 = 0x0010;
/// Precision.
pub const PE: u16 = 0x0020;
/// Status word bit 9: C1, which an arithmetic result sets if it rounded up.
pub const C1: u16 = 0x0200;

/// Round to nearest, ties to even.
pub const RC_NEAREST: u8 = 0;
/// Round towards minus infinity.
pub const RC_DOWN: u8 = 1;
/// Round towards plus infinity.
pub const RC_UP: u8 = 2;
/// Round towards zero.
pub const RC_CHOP: u8 = 3;

const REAL_INDEFINITE: u128 = 0xFFFF_C000_0000_0000_0000;
const SIGN_SHIFT: u32 = 79;
const EXP_SHIFT: u32 = 64;
const EXP_MAX: u16 = 0x7FFF;
const INT_BIT: u64 = 1 << 63;
const QUIET_BIT: u64 = 1 << 62;
const F80_EXP_BIAS: i32 = 16383;
const BCD_LIMIT: u128 = 1_000_000_000_000_000_000;

/// The packed BCD indefinite that FBSTP stores for what it can't represent.
pub const BCD_INDEFINITE: [u8; 10] = [0, 0, 0, 0, 0, 0, 0, 0xC0, 0xFF, 0xFF];

/// The rounding environment of one operation, and the flags it raised.
pub struct Env {
    /// One of the `RC_*` constants.
    pub rc: u8,
    /// Significand bits to round to: 24, 53 or 64.
    pub prec: u32,
    /// Whether the underflow exception is unmasked, which reports every tiny
    /// result rather than only the inexact ones.
    pub underflow_unmasked: bool,
    /// The `IE..PE` bits raised, and [`C1`] if the result rounded up.
    pub flags: u16,
}

impl Env {
    /// An environment that rounds to the full 64 bits.
    pub fn new(rc: u8) -> Self {
        Self {
            rc,
            prec: 64,
            underflow_unmasked: false,
            flags: 0,
        }
    }
}

/// What an 80-bit pattern holds. `Unsupported` covers the patterns the 387
/// dropped: unnormals, pseudo-infinities and pseudo-NaNs. A pseudo-denormal
/// counts as `Denormal`, because the 387 still reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Zero,
    Denormal,
    Normal,
    Infinity,
    QNan,
    SNan,
    Unsupported,
}

// f80 emulation
// Source: https://github.com/sha0coder/mwemu
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct F80 {
    pub st: u128,
}

impl Default for F80 {
    fn default() -> Self {
        Self::new()
    }
}

impl F80 {
    pub const fn new() -> Self {
        F80 { st: 0 }
    }

    /// The QNaN indefinite: the masked response to an invalid operation.
    pub const INDEFINITE: F80 = F80 {
        st: REAL_INDEFINITE,
    };

    /// Builds a value from its sign, biased exponent and explicit significand.
    pub const fn from_parts(negative: bool, exponent: u16, mantissa: u64) -> Self {
        F80 {
            st: ((negative as u128) << SIGN_SHIFT)
                | (((exponent & EXP_MAX) as u128) << EXP_SHIFT)
                | mantissa as u128,
        }
    }

    pub fn set_bytes(&mut self, bytes: &[u8; 10]) {
        let mut value = 0u128;
        for (index, byte) in bytes.iter().enumerate() {
            value |= (*byte as u128) << (index * 8);
        }
        self.st = value;
    }

    pub fn get_bytes(&self) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (self.st >> (index * 8)) as u8;
        }
        bytes
    }

    pub fn sign(&self) -> bool {
        (self.st >> SIGN_SHIFT) & 1 != 0
    }

    pub fn exponent(&self) -> u16 {
        (self.st >> EXP_SHIFT) as u16 & EXP_MAX
    }

    pub fn mantissa(&self) -> u64 {
        self.st as u64
    }

    pub fn with_sign(&self, negative: bool) -> Self {
        Self::from_parts(negative, self.exponent(), self.mantissa())
    }

    pub fn is_nan(&self) -> bool {
        matches!(self.class(), Class::QNan | Class::SNan)
    }

    pub fn class(&self) -> Class {
        let exponent = self.exponent();
        let mantissa = self.mantissa();
        if exponent == 0 {
            if mantissa == 0 {
                Class::Zero
            } else {
                Class::Denormal
            }
        } else if mantissa & INT_BIT == 0 {
            Class::Unsupported
        } else if exponent != EXP_MAX {
            Class::Normal
        } else if mantissa << 1 == 0 {
            Class::Infinity
        } else if mantissa & QUIET_BIT != 0 {
            Class::QNan
        } else {
            Class::SNan
        }
    }

    /// The unbiased exponent and normalized significand of a finite non-zero
    /// value, which is `significand * 2^(exponent - 63)`.
    fn unpack(&self) -> (i32, u64) {
        let exponent = self.exponent().max(1) as i32 - F80_EXP_BIAS;
        let mantissa = self.mantissa();
        let shift = mantissa.leading_zeros();
        (exponent - shift as i32, mantissa.wrapping_shl(shift))
    }

    fn quieted(&self) -> Self {
        F80 {
            st: self.st | QUIET_BIT as u128,
        }
    }

    /// The tag a register holding this value gets: 0 valid, 1 zero, 2 special.
    pub fn tag(&self) -> u16 {
        match self.class() {
            Class::Normal => 0,
            Class::Zero => 1,
            _ => 2,
        }
    }
}

// ----------------------------------------------------------------------
// Rounding
// ----------------------------------------------------------------------

/// Shifts right, folding what falls off into the lowest bit.
fn shift_sticky(value: u128, shift: u32) -> u128 {
    if shift == 0 {
        value
    } else if shift >= 128 {
        u128::from(value != 0)
    } else {
        (value >> shift) | u128::from(value & ((1u128 << shift) - 1) != 0)
    }
}

/// Rounds away the low `128 - prec` bits. Returns the rounded bits, whether it
/// carried out of the top, whether it rounded up and whether it was inexact.
fn round_at(value: u128, prec: u32, negative: bool, rc: u8) -> (u128, bool, bool, bool) {
    let low_bits = 128 - prec;
    let mask = (1u128 << low_bits) - 1;
    let rest = value & mask;
    let half = 1u128 << (low_bits - 1);
    let odd = (value >> low_bits) & 1 != 0;
    let up = rest != 0
        && match rc {
            RC_NEAREST => rest > half || (rest == half && odd),
            RC_DOWN => negative,
            RC_UP => !negative,
            _ => false,
        };
    let kept = value & !mask;
    if up {
        let (rounded, carry) = kept.overflowing_add(mask + 1);
        (rounded, carry, true, true)
    } else {
        (kept, false, false, rest != 0)
    }
}

/// The biggest finite value at a precision, or infinity, as overflow rounds to.
fn overflow_rounds_to_infinity(negative: bool, rc: u8) -> bool {
    match rc {
        RC_NEAREST => true,
        RC_DOWN => negative,
        RC_UP => !negative,
        _ => false,
    }
}

/// Rounds a normalized significand to an 80-bit value. The value is
/// `significand * 2^(exponent - 127)`; the lowest bit may be a sticky bit.
fn round_pack(env: &mut Env, negative: bool, exponent: i32, significand: u128) -> F80 {
    let prec = env.prec;
    let mut biased = exponent.wrapping_add(F80_EXP_BIAS);
    if biased <= 0 {
        // The x87 detects tininess after rounding, with the exponent unbounded.
        let (_, carried, _, _) = round_at(significand, prec, negative, env.rc);
        let tiny = biased < 0 || !carried;
        let shifted = shift_sticky(significand, 1u32.wrapping_sub(biased as u32).min(128));
        let (rounded, _, up, inexact) = round_at(shifted, prec, negative, env.rc);
        let mantissa = (rounded >> 64) as u64;
        if inexact {
            env.flags |= PE;
        }
        if up {
            env.flags |= C1;
        }
        if tiny && (inexact || env.underflow_unmasked) {
            env.flags |= UE;
        }
        return F80::from_parts(negative, u16::from(mantissa & INT_BIT != 0), mantissa);
    }
    let (mut rounded, carried, up, inexact) = round_at(significand, prec, negative, env.rc);
    if carried {
        rounded = 1u128 << 127;
        biased = biased.wrapping_add(1);
    }
    if biased >= EXP_MAX as i32 {
        env.flags |= OE | PE;
        if overflow_rounds_to_infinity(negative, env.rc) {
            env.flags |= C1;
            return F80::from_parts(negative, EXP_MAX, INT_BIT);
        }
        let largest = u64::MAX << (64 - prec);
        return F80::from_parts(negative, EXP_MAX - 1, largest);
    }
    if inexact {
        env.flags |= PE;
    }
    if up {
        env.flags |= C1;
    }
    F80::from_parts(negative, biased as u16, (rounded >> 64) as u64)
}

/// Normalizes then rounds `magnitude * 2^power`.
fn pack_integer(env: &mut Env, negative: bool, power: i32, magnitude: u128) -> F80 {
    if magnitude == 0 {
        return F80::from_parts(negative, 0, 0);
    }
    let shift = magnitude.leading_zeros();
    round_pack(
        env,
        negative,
        power.wrapping_add(127).wrapping_sub(shift as i32),
        magnitude << shift,
    )
}

// ----------------------------------------------------------------------
// Special values
// ----------------------------------------------------------------------

/// The result of an operation with a NaN or unsupported operand, if it has one.
fn special_pair(env: &mut Env, a: F80, b: F80) -> Option<F80> {
    let (class_a, class_b) = (a.class(), b.class());
    if class_a == Class::Unsupported || class_b == Class::Unsupported {
        env.flags |= IE;
        return Some(F80::INDEFINITE);
    }
    let nan_a = a.is_nan();
    let nan_b = b.is_nan();
    if !nan_a && !nan_b {
        return None;
    }
    if class_a == Class::SNan || class_b == Class::SNan {
        env.flags |= IE;
    }
    Some(if nan_a && nan_b {
        // A quiet operand beats a signalling one; otherwise the bigger
        // significand wins, and on a tie the positive one.
        match (class_a == Class::SNan, class_b == Class::SNan) {
            (true, false) => b,
            (false, true) => a,
            _ => {
                let (fa, fb) = (a.mantissa() | QUIET_BIT, b.mantissa() | QUIET_BIT);
                if fa > fb || (fa == fb && !a.sign()) {
                    a.quieted()
                } else {
                    b.quieted()
                }
            }
        }
    } else if nan_a {
        a.quieted()
    } else {
        b.quieted()
    })
}

/// The result of a one-operand operation on a NaN or unsupported value.
fn special_one(env: &mut Env, a: F80) -> Option<F80> {
    match a.class() {
        Class::Unsupported => {
            env.flags |= IE;
            Some(F80::INDEFINITE)
        }
        Class::SNan => {
            env.flags |= IE;
            Some(a.quieted())
        }
        Class::QNan => Some(a),
        _ => None,
    }
}

fn check_denormal(env: &mut Env, a: F80) {
    if a.class() == Class::Denormal {
        env.flags |= DE;
    }
}

fn invalid(env: &mut Env) -> F80 {
    env.flags |= IE;
    F80::INDEFINITE
}

fn infinity(negative: bool) -> F80 {
    F80::from_parts(negative, EXP_MAX, INT_BIT)
}

fn zero(negative: bool) -> F80 {
    F80::from_parts(negative, 0, 0)
}

// ----------------------------------------------------------------------
// Arithmetic
// ----------------------------------------------------------------------

pub fn add(env: &mut Env, a: F80, b: F80) -> F80 {
    add_signed(env, a, b, b.sign())
}

pub fn sub(env: &mut Env, a: F80, b: F80) -> F80 {
    add_signed(env, a, b, !b.sign())
}

fn add_signed(env: &mut Env, a: F80, b: F80, sign_b: bool) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let sign_a = a.sign();
    let (class_a, class_b) = (a.class(), b.class());
    if class_a == Class::Infinity || class_b == Class::Infinity {
        if class_a == class_b && sign_a != sign_b {
            return invalid(env);
        }
        check_denormal(env, a);
        check_denormal(env, b);
        return infinity(if class_a == Class::Infinity {
            sign_a
        } else {
            sign_b
        });
    }
    check_denormal(env, a);
    check_denormal(env, b);
    match (class_a == Class::Zero, class_b == Class::Zero) {
        (true, true) => {
            return zero(if sign_a == sign_b {
                sign_a
            } else {
                env.rc == RC_DOWN
            });
        }
        (true, false) => {
            let (exp_b, sig_b) = b.unpack();
            return round_pack(env, sign_b, exp_b, (sig_b as u128) << 64);
        }
        (false, true) => {
            let (exp_a, sig_a) = a.unpack();
            return round_pack(env, sign_a, exp_a, (sig_a as u128) << 64);
        }
        _ => {}
    }
    let (exp_a, sig_a) = a.unpack();
    let (exp_b, sig_b) = b.unpack();
    let a_big = (exp_a, sig_a) >= (exp_b, sig_b);
    let (exp_hi, sig_hi, sign_hi, exp_lo, sig_lo, sign_lo) = if a_big {
        (exp_a, sig_a, sign_a, exp_b, sig_b, sign_b)
    } else {
        (exp_b, sig_b, sign_b, exp_a, sig_a, sign_a)
    };
    let wide_hi = (sig_hi as u128) << 62;
    let wide_lo = shift_sticky((sig_lo as u128) << 62, (exp_hi - exp_lo) as u32);
    let result = if sign_hi == sign_lo {
        wide_hi + wide_lo
    } else {
        wide_hi - wide_lo
    };
    if result == 0 {
        return zero(env.rc == RC_DOWN);
    }
    let shift = result.leading_zeros();
    round_pack(env, sign_hi, exp_hi + 2 - shift as i32, result << shift)
}

pub fn mul(env: &mut Env, a: F80, b: F80) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let negative = a.sign() != b.sign();
    let (class_a, class_b) = (a.class(), b.class());
    if class_a == Class::Infinity || class_b == Class::Infinity {
        if class_a == Class::Zero || class_b == Class::Zero {
            return invalid(env);
        }
        check_denormal(env, a);
        check_denormal(env, b);
        return infinity(negative);
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Zero || class_b == Class::Zero {
        return zero(negative);
    }
    let (exp_a, sig_a) = a.unpack();
    let (exp_b, sig_b) = b.unpack();
    let product = sig_a as u128 * sig_b as u128;
    let shift = product.leading_zeros();
    round_pack(
        env,
        negative,
        exp_a + exp_b + 1 - shift as i32,
        product << shift,
    )
}

pub fn div(env: &mut Env, a: F80, b: F80) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let negative = a.sign() != b.sign();
    let (class_a, class_b) = (a.class(), b.class());
    if (class_a == Class::Infinity && class_b == Class::Infinity)
        || (class_a == Class::Zero && class_b == Class::Zero)
    {
        return invalid(env);
    }
    if class_b == Class::Zero && class_a != Class::Infinity {
        env.flags |= ZE;
        return infinity(negative);
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Infinity {
        return infinity(negative);
    }
    if class_b == Class::Infinity || class_a == Class::Zero {
        return zero(negative);
    }
    let (exp_a, sig_a) = a.unpack();
    let (exp_b, sig_b) = b.unpack();
    let divisor = sig_b as u128;
    let whole = sig_a >= sig_b;
    let first = if whole { sig_a - sig_b } else { sig_a } as u128;
    let high = (first << 64) / divisor;
    let middle = (first << 64) % divisor;
    let low = (middle << 64) / divisor;
    let sticky = (middle << 64) % divisor != 0;
    let quotient = (high << 64) | low;
    let (significand, exponent) = if whole {
        (
            (1u128 << 127) | (quotient >> 1) | (quotient & 1),
            exp_a - exp_b,
        )
    } else {
        (quotient, exp_a - exp_b - 1)
    };
    round_pack(env, negative, exponent, significand | u128::from(sticky))
}

pub fn sqrt(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    match a.class() {
        Class::Zero => return a,
        Class::Infinity if !a.sign() => return a,
        _ => {}
    }
    if a.sign() {
        return invalid(env);
    }
    check_denormal(env, a);
    let (exponent, significand) = a.unpack();
    // The radicand is in [1, 4), held at 2^-126. One bit of root per two bits
    // of radicand, with two extra bits for rounding.
    let odd = exponent & 1 != 0;
    let mut radicand = (significand as u128) << if odd { 64 } else { 63 };
    let half_exponent = (exponent - i32::from(odd)) / 2;
    let mut root = 0u128;
    let mut remainder = 0u128;
    for _ in 0..66 {
        remainder = (remainder << 2) | (radicand >> 126);
        radicand <<= 2;
        let trial = (root << 2) | 1;
        root <<= 1;
        if remainder >= trial {
            remainder -= trial;
            root |= 1;
        }
    }
    let significand = (root << 62) | u128::from(remainder != 0);
    round_pack(env, false, half_exponent, significand)
}

/// Rounds a finite value's magnitude to an integer: `(integer, up, inexact)`.
/// The value must be below 2^64.
fn round_magnitude(exponent: i32, significand: u64, negative: bool, rc: u8) -> (u128, bool, bool) {
    if exponent >= 63 {
        return (
            (significand as u128) << (exponent - 63).clamp(0, 64) as u32,
            false,
            false,
        );
    }
    let shift = (63 - exponent) as u32;
    let wide = significand as u128;
    let (integer, rest, half) = if shift >= 65 {
        // Everything is below one half, and what's left only needs to be seen.
        (0, 1, 2)
    } else {
        (
            wide >> shift,
            wide & ((1u128 << shift) - 1),
            1u128 << (shift - 1),
        )
    };
    let up = rest != 0
        && match rc {
            RC_NEAREST => rest > half || (rest == half && integer & 1 != 0),
            RC_DOWN => negative,
            RC_UP => !negative,
            _ => false,
        };
    (integer + u128::from(up), up, rest != 0)
}

/// FRNDINT. Values that are already whole, and infinities, come back unchanged.
pub fn round_to_integer(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    match a.class() {
        Class::Zero | Class::Infinity => return a,
        _ => {}
    }
    check_denormal(env, a);
    let (exponent, significand) = a.unpack();
    if exponent >= 63 {
        return a;
    }
    let negative = a.sign();
    let (integer, up, inexact) = round_magnitude(exponent, significand, negative, env.rc);
    if inexact {
        env.flags |= PE;
    }
    if up {
        env.flags |= C1;
    }
    let mut full = Env::new(RC_NEAREST);
    pack_integer(&mut full, negative, 0, integer)
}

/// FIST: converts to a signed integer of `bits` bits, by the rounding control.
/// `None` is out of range or not a number: the caller stores the indefinite.
pub fn to_integer(env: &mut Env, a: F80, bits: u32) -> Option<i64> {
    if bits == 0 || bits > 64 {
        return None;
    }
    match a.class() {
        Class::Zero => return Some(0),
        Class::Denormal | Class::Normal => {}
        _ => {
            env.flags |= IE;
            return None;
        }
    }
    let (exponent, significand) = a.unpack();
    let negative = a.sign();
    if exponent >= 64 {
        env.flags |= IE;
        return None;
    }
    let (integer, up, inexact) = round_magnitude(exponent, significand, negative, env.rc);
    let limit = 1u128 << (bits - 1);
    if integer > limit || (integer == limit && !negative) {
        env.flags |= IE;
        return None;
    }
    if inexact {
        env.flags |= PE;
    }
    if up {
        env.flags |= C1;
    }
    let value = integer as u64;
    Some(if negative {
        (value as i64).wrapping_neg()
    } else {
        value as i64
    })
}

/// FBSTP: `None` is out of range or not a number.
pub fn to_bcd(env: &mut Env, a: F80) -> Option<[u8; 10]> {
    let negative = a.sign();
    let mut bytes = [0u8; 10];
    bytes[9] = u8::from(negative) << 7;
    match a.class() {
        Class::Zero => return Some(bytes),
        Class::Denormal | Class::Normal => {}
        _ => {
            env.flags |= IE;
            return None;
        }
    }
    let (exponent, significand) = a.unpack();
    if exponent >= 64 {
        env.flags |= IE;
        return None;
    }
    let (mut integer, up, inexact) = round_magnitude(exponent, significand, negative, env.rc);
    if integer >= BCD_LIMIT {
        env.flags |= IE;
        return None;
    }
    if inexact {
        env.flags |= PE;
    }
    if up {
        env.flags |= C1;
    }
    for byte in bytes.iter_mut().take(9) {
        let digits = (integer % 100) as u8;
        *byte = ((digits / 10) << 4) | (digits % 10);
        integer /= 100;
    }
    Some(bytes)
}

/// FBLD. Digits above nine are undefined on hardware; here they just count.
pub fn from_bcd(bytes: &[u8; 10]) -> F80 {
    let mut integer = 0u128;
    for byte in bytes.iter().take(9).rev() {
        integer = integer * 100 + (byte >> 4) as u128 * 10 + (byte & 15) as u128;
    }
    pack_integer(&mut Env::new(RC_NEAREST), bytes[9] & 0x80 != 0, 0, integer)
}

/// An exact integer conversion. Zero is positive, like FILD's.
pub fn from_i64(value: i64) -> F80 {
    pack_integer(
        &mut Env::new(RC_NEAREST),
        value < 0,
        0,
        value.unsigned_abs() as u128,
    )
}

/// Widens an IEEE single or double exactly. A signalling NaN stays signalling.
/// The flag says the operand was a denormal.
pub fn from_ieee(bits: u64, fraction_bits: u32, exponent_bits: u32) -> (F80, bool) {
    let negative = (bits >> (fraction_bits + exponent_bits)) & 1 != 0;
    let exponent_max = (1u64 << exponent_bits) - 1;
    let exponent = (bits >> fraction_bits) & exponent_max;
    let fraction = bits & ((1u64 << fraction_bits) - 1);
    let aligned = fraction << (63 - fraction_bits);
    if exponent == exponent_max {
        return (F80::from_parts(negative, EXP_MAX, INT_BIT | aligned), false);
    }
    let bias = (exponent_max >> 1) as i32;
    if exponent == 0 {
        if fraction == 0 {
            return (zero(negative), false);
        }
        let shift = aligned.leading_zeros();
        let biased = 1 - bias + F80_EXP_BIAS - shift as i32;
        return (
            F80::from_parts(negative, biased as u16, aligned << shift),
            true,
        );
    }
    let biased = exponent as i32 - bias + F80_EXP_BIAS;
    (
        F80::from_parts(negative, biased as u16, INT_BIT | aligned),
        false,
    )
}

/// Narrows to an IEEE single or double by the rounding control.
pub fn to_ieee(env: &mut Env, a: F80, fraction_bits: u32, exponent_bits: u32) -> u64 {
    let negative = a.sign();
    let sign = u64::from(negative) << (fraction_bits + exponent_bits);
    let exponent_max = (1u64 << exponent_bits) - 1;
    let infinity_bits = sign | (exponent_max << fraction_bits);
    let quiet = 1u64 << (fraction_bits - 1);
    match a.class() {
        Class::Zero => return sign,
        Class::Infinity => return infinity_bits,
        Class::Unsupported => {
            env.flags |= IE;
            return (1u64 << (fraction_bits + exponent_bits)) | infinity_bits | quiet;
        }
        Class::QNan | Class::SNan => {
            if a.class() == Class::SNan {
                env.flags |= IE;
            }
            let fraction = (a.mantissa() << 1) >> (64 - fraction_bits);
            return infinity_bits | quiet | fraction;
        }
        Class::Denormal | Class::Normal => {}
    }
    let (exponent, significand) = a.unpack();
    let prec = fraction_bits + 1;
    let wide = (significand as u128) << 64;
    let bias = (exponent_max >> 1) as i32;
    let mut biased = exponent + bias;
    if biased <= 0 {
        let (_, carried, _, _) = round_at(wide, prec, negative, env.rc);
        let tiny = biased < 0 || !carried;
        let shifted = shift_sticky(wide, (1 - biased) as u32);
        let (rounded, _, up, inexact) = round_at(shifted, prec, negative, env.rc);
        if inexact {
            env.flags |= PE;
        }
        if up {
            env.flags |= C1;
        }
        if tiny && (inexact || env.underflow_unmasked) {
            env.flags |= UE;
        }
        // A carry into the hidden bit lands on the exponent, as it should.
        return sign | (rounded >> (128 - prec)) as u64;
    }
    let (mut rounded, carried, up, inexact) = round_at(wide, prec, negative, env.rc);
    if carried {
        rounded = 1u128 << 127;
        biased += 1;
    }
    if biased as u64 >= exponent_max {
        env.flags |= OE | PE;
        if overflow_rounds_to_infinity(negative, env.rc) {
            env.flags |= C1;
            return infinity_bits;
        }
        return sign | ((exponent_max - 1) << fraction_bits) | ((1u64 << fraction_bits) - 1);
    }
    if inexact {
        env.flags |= PE;
    }
    if up {
        env.flags |= C1;
    }
    let fraction = ((rounded >> (128 - prec)) as u64) & ((1u64 << fraction_bits) - 1);
    sign | ((biased as u64) << fraction_bits) | fraction
}

/// Orders two values neither of which is a NaN or unsupported. The two zeros
/// are equal, and the infinities sit beyond every finite value.
pub fn compare(a: F80, b: F80) -> Ordering {
    let key = |value: F80| -> Option<(i32, u64)> {
        match value.class() {
            Class::Zero => None,
            Class::Infinity => Some((i32::MAX, 0)),
            _ => Some(value.unpack()),
        }
    };
    let (key_a, key_b) = (key(a), key(b));
    match (key_a, key_b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if b.sign() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(_), None) => {
            if a.sign() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(mag_a), Some(mag_b)) => match (a.sign(), b.sign()) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_a.cmp(&mag_b),
            (true, true) => mag_b.cmp(&mag_a),
        },
    }
}

/// What FPREM and FPREM1 leave behind.
pub struct Remainder {
    pub value: F80,
    /// The low three bits of the quotient.
    pub quotient: u8,
    /// The reduction is incomplete: the C2 flag.
    pub partial: bool,
}

/// FPREM, or FPREM1 if `nearest`. Exponents further than 63 apart are reduced
/// by part of the distance per call, like hardware, so the time is bounded.
pub fn remainder(env: &mut Env, a: F80, b: F80, nearest: bool) -> Remainder {
    let done = |value: F80| Remainder {
        value,
        quotient: 0,
        partial: false,
    };
    if let Some(result) = special_pair(env, a, b) {
        return done(result);
    }
    let (class_a, class_b) = (a.class(), b.class());
    if class_a == Class::Infinity || class_b == Class::Zero {
        return done(invalid(env));
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Zero {
        return done(a);
    }
    let negative = a.sign();
    let (exp_a, sig_a) = a.unpack();
    if class_b == Class::Infinity {
        return done(pack_integer(env, negative, exp_a - 63, sig_a as u128));
    }
    let (exp_b, sig_b) = b.unpack();
    let distance = exp_a - exp_b;
    let divisor = sig_b as u128;
    if distance < 0 {
        // a < b. Only FPREM1 can do anything, and only when a > b/2.
        if nearest && distance == -1 && sig_a > sig_b {
            let value = (divisor << 1) - sig_a as u128;
            return Remainder {
                value: pack_integer(env, !negative, exp_a - 63, value),
                quotient: 1,
                partial: false,
            };
        }
        return done(pack_integer(env, negative, exp_a - 63, sig_a as u128));
    }
    let partial = distance >= 64;
    let steps = if partial {
        32 + distance % 32
    } else {
        distance
    };
    let mut rest = sig_a as u128;
    let mut quotient = 0u8;
    for step in 0..=steps {
        quotient <<= 1;
        if rest >= divisor {
            rest -= divisor;
            quotient |= 1;
        }
        if step != steps {
            rest <<= 1;
        }
    }
    let power = exp_a - steps - 63;
    if partial {
        return Remainder {
            value: pack_integer(env, negative, power, rest),
            quotient: 0,
            partial: true,
        };
    }
    let mut sign = negative;
    if nearest {
        let twice = rest << 1;
        if twice > divisor || (twice == divisor && quotient & 1 != 0) {
            rest = divisor - rest;
            quotient = quotient.wrapping_add(1);
            sign = !sign;
        }
    }
    if rest == 0 {
        sign = negative;
    }
    Remainder {
        value: pack_integer(env, sign, power, rest),
        quotient: quotient & 7,
        partial: false,
    }
}

/// FSCALE: `a * 2^trunc(b)`, with an exact scale.
pub fn scale(env: &mut Env, a: F80, b: F80) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let (class_a, class_b) = (a.class(), b.class());
    if class_b == Class::Infinity {
        let by_up = !b.sign();
        return match class_a {
            Class::Zero if by_up => invalid(env),
            Class::Infinity if !by_up => invalid(env),
            Class::Infinity | Class::Zero => a,
            _ => {
                check_denormal(env, a);
                if by_up {
                    infinity(a.sign())
                } else {
                    zero(a.sign())
                }
            }
        };
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Zero || class_a == Class::Infinity {
        return a;
    }
    let by = if class_b == Class::Zero {
        0
    } else {
        let (exponent, significand) = b.unpack();
        let magnitude = if exponent < 0 {
            0
        } else if exponent >= 20 {
            1 << 20
        } else {
            (significand >> (63 - exponent)) as i32
        };
        if b.sign() {
            -magnitude
        } else {
            magnitude
        }
    };
    let (exponent, significand) = a.unpack();
    round_pack(
        env,
        a.sign(),
        exponent.wrapping_add(by),
        (significand as u128) << 64,
    )
}

/// FXTRACT: `(exponent, significand)`.
pub fn extract(env: &mut Env, a: F80) -> (F80, F80) {
    if let Some(result) = special_one(env, a) {
        return (result, result);
    }
    match a.class() {
        Class::Zero => {
            env.flags |= ZE;
            return (infinity(true), a);
        }
        Class::Infinity => return (infinity(false), a),
        _ => {}
    }
    check_denormal(env, a);
    let (exponent, significand) = a.unpack();
    (
        from_i64(exponent as i64),
        F80::from_parts(a.sign(), F80_EXP_BIAS as u16, significand),
    )
}

// ----------------------------------------------------------------------
// Transcendentals, in a float with a 128-bit significand
// ----------------------------------------------------------------------

/// A working float: `significand * 2^(exponent - 127)`, with the top bit of a
/// non-zero significand set. Operations truncate, so a result is good to a few
/// units in the last of those 128 bits, well past the 64 the x87 returns.
#[derive(Clone, Copy)]
struct Xf {
    negative: bool,
    exponent: i32,
    significand: u128,
}

const PI: Xf = Xf::pos(1, 0xC90FDAA22168C234C4C6628B80DC1CD1);
const PI_2: Xf = Xf::pos(0, 0xC90FDAA22168C234C4C6628B80DC1CD1);
const PI_4: Xf = Xf::pos(-1, 0xC90FDAA22168C234C4C6628B80DC1CD1);
const PI3_4: Xf = Xf::pos(1, 0x96CBE3F9990E91A79394C9E8A0A5159C);
const LN2: Xf = Xf::pos(-1, 0xB17217F7D1CF79ABC9E3B39803F2F6AF);
const LOG2_E: Xf = Xf::pos(0, 0xB8AA3B295C17F0BBBE87FED0691D3E88);
const LOG2_10: Xf = Xf::pos(1, 0xD49A784BCD1B8AFE492BF6FF4DAFDB4C);
const LOG10_2: Xf = Xf::pos(-2, 0x9A209A84FBCFF7988F8959AC0B7C9178);
const SQRT2: u128 = 0xB504F333F9DE6484597D89B3754ABE9F;
const ONE: Xf = Xf::pos(0, 1u128 << 127);

/// `atan(k / 8)` for `k` from 1 to 8.
const ATAN_EIGHTHS: [Xf; 8] = [
    Xf::pos(-4, 0xFEADD4D5617B6E32C897989F3E888EF7),
    Xf::pos(-3, 0xFADBAFC96406EB156DC79EF5F7A217E5),
    Xf::pos(-2, 0xB7B0CA0F26F784738AA32122DCFE4483),
    Xf::pos(-2, 0xED63382B0DDA7B456FE445ECBC3A8D03),
    Xf::pos(-1, 0x8F005D5EF7F59F9B5C835E1665C43747),
    Xf::pos(-1, 0xA4BC7D1934F7092419A87F2A457DAC9E),
    Xf::pos(-1, 0xB8053E2BC2319E73CB2DA55210A4443D),
    PI_4,
];

/// The first 320 fractional bits of 2/pi, most significant limb first.
const TWO_OVER_PI: [u64; 5] = [
    0xA2F9_836E_4E44_1529,
    0xFC27_57D1_F534_DDC0,
    0xDB62_9599_3C43_9041,
    0xFE51_63AB_DEBB_C561,
    0xB724_6E3A_424D_D2E0,
];

fn mul_high(a: u128, b: u128) -> u128 {
    let (a_hi, a_lo) = (a >> 64, a & u64::MAX as u128);
    let (b_hi, b_lo) = (b >> 64, b & u64::MAX as u128);
    let low = a_lo * b_lo;
    let middle_a = a_hi * b_lo;
    let middle_b = a_lo * b_hi;
    let carry = ((low >> 64) + (middle_a & u64::MAX as u128) + (middle_b & u64::MAX as u128)) >> 64;
    (a_hi * b_hi) + (middle_a >> 64) + (middle_b >> 64) + carry
}

impl Xf {
    const fn pos(exponent: i32, significand: u128) -> Self {
        Self {
            negative: false,
            exponent,
            significand,
        }
    }

    fn normalized(negative: bool, exponent: i32, significand: u128) -> Self {
        let shift = significand.leading_zeros();
        Self {
            negative,
            exponent: exponent.wrapping_sub(shift as i32),
            significand: significand.checked_shl(shift).unwrap_or(0),
        }
    }

    /// A finite value's magnitude and sign.
    fn from_f80(value: F80) -> Self {
        if value.class() == Class::Zero {
            return Self::pos(0, 0);
        }
        let (exponent, significand) = value.unpack();
        Self {
            negative: value.sign(),
            exponent,
            significand: (significand as u128) << 64,
        }
    }

    fn from_int(value: i32) -> Self {
        Self::normalized(value < 0, 127, value.unsigned_abs() as u128)
    }

    fn is_zero(self) -> bool {
        self.significand == 0
    }

    fn negated(self) -> Self {
        Self {
            negative: !self.negative,
            ..self
        }
    }

    fn abs(self) -> Self {
        Self {
            negative: false,
            ..self
        }
    }

    fn add(self, other: Self) -> Self {
        if self.is_zero() {
            return other;
        }
        if other.is_zero() {
            return self;
        }
        let self_big = (self.exponent, self.significand) >= (other.exponent, other.significand);
        let (big, small) = if self_big {
            (self, other)
        } else {
            (other, self)
        };
        let distance = big.exponent.wrapping_sub(small.exponent) as u32;
        if distance >= 128 {
            return big;
        }
        let aligned = small.significand >> distance;
        if big.negative == small.negative {
            let (sum, carry) = big.significand.overflowing_add(aligned);
            if carry {
                Self {
                    negative: big.negative,
                    exponent: big.exponent.wrapping_add(1),
                    significand: (sum >> 1) | (1u128 << 127),
                }
            } else {
                Self {
                    significand: sum,
                    ..big
                }
            }
        } else {
            Self::normalized(big.negative, big.exponent, big.significand - aligned)
        }
    }

    fn sub(self, other: Self) -> Self {
        self.add(other.negated())
    }

    fn mul(self, other: Self) -> Self {
        Self::normalized(
            self.negative != other.negative,
            self.exponent.wrapping_add(other.exponent).wrapping_add(1),
            mul_high(self.significand, other.significand),
        )
    }

    fn div(self, other: Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::pos(0, 0);
        }
        let divisor = other.significand;
        let mut rest = self.significand;
        let mut exponent = self.exponent.wrapping_sub(other.exponent);
        let mut quotient = 1u128;
        if rest >= divisor {
            rest -= divisor;
        } else {
            exponent = exponent.wrapping_sub(1);
            rest = (rest << 1).wrapping_sub(divisor);
        }
        for _ in 0..127 {
            let top = rest >> 127 != 0;
            rest <<= 1;
            quotient <<= 1;
            if top || rest >= divisor {
                rest = rest.wrapping_sub(divisor);
                quotient |= 1;
            }
        }
        Self {
            negative: self.negative != other.negative,
            exponent,
            significand: quotient,
        }
    }

    fn div_small(self, by: u64) -> Self {
        let by = by.max(1) as u128;
        let high = (self.significand >> 64) / by;
        let rest = (self.significand >> 64) % by;
        let low = ((rest << 64) | (self.significand & u64::MAX as u128)) / by;
        Self::normalized(self.negative, self.exponent, (high << 64) | low)
    }

    /// Rounds to 80 bits. The value is taken to be inexact.
    fn round(self, env: &mut Env) -> F80 {
        if self.is_zero() {
            return zero(self.negative);
        }
        round_pack(env, self.negative, self.exponent, self.significand | 1)
    }
}

/// `sin(y)` and `cos(y)` for `|y| <= pi/4`, by Taylor series.
fn sin_cos_kernel(y: Xf) -> (Xf, Xf) {
    let square = y.mul(y);
    let mut sine = ONE;
    let mut cosine = ONE;
    for term in (2..=34u64).rev().step_by(2) {
        // 1 - y^2 / (n (n + 1)) * (the rest), from the small end up.
        sine = ONE.sub(square.mul(sine).div_small(term * (term + 1)));
        cosine = ONE.sub(square.mul(cosine).div_small(term * (term - 1)));
    }
    (y.mul(sine), cosine)
}

/// Reduces a positive value below 2^63 modulo pi/2: `(quadrant, remainder)`,
/// using enough bits of 2/pi that a value next to a multiple of pi/2 keeps its
/// accuracy.
fn reduce_pi_2(value: F80) -> (u8, Xf) {
    let (exponent, significand) = value.unpack();
    if exponent < -1 {
        return (0, Xf::from_f80(value.with_sign(false)));
    }
    // significand * TWO_OVER_PI, limb by limb, most significant limb first.
    let mut product = [0u64; 6];
    let mut carry = 0u128;
    for index in (0..5).rev() {
        let wide = significand as u128 * TWO_OVER_PI[index] as u128 + carry;
        product[index + 1] = wide as u64;
        carry = wide >> 64;
    }
    product[0] = carry as u64;
    // There are exponent + 1 integer bits: at most 63.
    let shift = (exponent + 1).clamp(0, 63) as u32;
    let bits = |index: usize| -> u64 {
        let high = product.get(index).copied().unwrap_or(0);
        let low = product.get(index + 1).copied().unwrap_or(0);
        if shift == 0 {
            high
        } else {
            (high << shift) | (low >> (64 - shift))
        }
    };
    let integer = if shift == 0 {
        0
    } else {
        product[0] >> (64 - shift)
    };
    let mut fraction = [bits(0), bits(1), bits(2)];
    let mut quadrant = integer as u8 & 3;
    let negative = fraction[0] >> 63 != 0;
    if negative {
        quadrant = quadrant.wrapping_add(1) & 3;
        let mut borrow_in = true;
        for limb in fraction.iter_mut().rev() {
            let (value, overflow) = (!*limb).overflowing_add(u64::from(borrow_in));
            *limb = value;
            borrow_in = overflow;
        }
    }
    // Take the leading 128 bits of the 192, however many zeros start them.
    let high = ((fraction[0] as u128) << 64) | fraction[1] as u128;
    let zeros = high.leading_zeros();
    let wide = if zeros == 0 {
        high
    } else if zeros < 64 {
        (high << zeros) | (fraction[2] >> (64 - zeros)) as u128
    } else if zeros < 128 {
        (high << zeros) | ((fraction[2] as u128) << (zeros - 64))
    } else {
        (fraction[2] as u128) << 64
    };
    let reduced = Xf::normalized(negative, -1 - zeros as i32, wide);
    (quadrant, reduced.mul(PI_2))
}

/// `(sin, cos)` of a finite value below 2^63 in magnitude.
fn sin_cos(value: F80) -> (Xf, Xf) {
    let (quadrant, reduced) = reduce_pi_2(value);
    let (sine, cosine) = sin_cos_kernel(reduced);
    let (sine, cosine) = match quadrant {
        0 => (sine, cosine),
        1 => (cosine, sine.negated()),
        2 => (sine.negated(), cosine.negated()),
        _ => (cosine.negated(), sine),
    };
    if value.sign() {
        (sine.negated(), cosine)
    } else {
        (sine, cosine)
    }
}

/// Whether FSIN, FCOS, FSINCOS and FPTAN refuse a value, setting C2.
pub fn trig_out_of_range(value: F80) -> bool {
    matches!(value.class(), Class::Normal) && value.exponent() as i32 - F80_EXP_BIAS >= 63
}

/// FSIN of a value that isn't out of range.
pub fn sin(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    match a.class() {
        Class::Zero => return a,
        Class::Infinity => return invalid(env),
        _ => {}
    }
    check_denormal(env, a);
    sin_cos(a).0.round(env)
}

/// FCOS of a value that isn't out of range.
pub fn cos(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    match a.class() {
        Class::Zero => return from_i64(1),
        Class::Infinity => return invalid(env),
        _ => {}
    }
    check_denormal(env, a);
    sin_cos(a).1.round(env)
}

/// FPTAN of a value that isn't out of range.
pub fn tan(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    match a.class() {
        Class::Zero => return a,
        Class::Infinity => return invalid(env),
        _ => {}
    }
    check_denormal(env, a);
    let (sine, cosine) = sin_cos(a);
    sine.div(cosine).round(env)
}

/// F2XM1: `2^a - 1` for `|a| <= 1`. Intel leaves anything else undefined; Intel
/// hardware returns the operand and reports precision, and so does this.
pub fn f2xm1(env: &mut Env, a: F80) -> F80 {
    if let Some(result) = special_one(env, a) {
        return result;
    }
    if a.class() == Class::Zero {
        return a;
    }
    if a.class() == Class::Infinity {
        return if a.sign() { from_i64(-1) } else { a };
    }
    check_denormal(env, a);
    let (exponent, significand) = a.unpack();
    if exponent > 0 || (exponent == 0 && significand != INT_BIT) {
        env.flags |= PE;
        return a;
    }
    if exponent == 0 {
        // Exactly plus or minus one. Hardware reports these as inexact anyway.
        env.flags |= PE;
        return if a.sign() {
            F80::from_parts(true, F80_EXP_BIAS as u16 - 1, INT_BIT)
        } else {
            from_i64(1)
        };
    }
    let z = Xf::from_f80(a).mul(LN2);
    // z (1 + z/2 (1 + z/3 (...)))
    let mut sum = ONE;
    for term in (2..=32u64).rev() {
        sum = ONE.add(z.mul(sum).div_small(term));
    }
    z.mul(sum).round(env)
}

/// `2 atanh(y)` for `|y|` up to about 0.18.
fn two_atanh(y: Xf) -> Xf {
    let square = y.mul(y);
    let mut sum = ONE.div_small(59);
    for term in (1..=57u64).rev().step_by(2) {
        sum = ONE.div_small(term).add(square.mul(sum));
    }
    let mut result = y.mul(sum);
    result.exponent = result.exponent.wrapping_add(1);
    result
}

/// `log2(x)` for a positive `x`.
fn log2(x: Xf) -> Xf {
    // x = m * 2^k with m in [sqrt(1/2), sqrt(2)).
    let (m_exponent, k) = if x.significand < SQRT2 {
        (0, x.exponent)
    } else {
        (-1, x.exponent.wrapping_add(1))
    };
    let m = Xf {
        negative: false,
        exponent: m_exponent,
        significand: x.significand,
    };
    let y = m.sub(ONE).div(m.add(ONE));
    Xf::from_int(k).add(two_atanh(y).mul(LOG2_E))
}

/// FYL2X: `b * log2(a)`, where `a` is ST(0) and `b` is ST(1).
pub fn yl2x(env: &mut Env, a: F80, b: F80) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let (class_a, class_b) = (a.class(), b.class());
    if a.sign() && class_a != Class::Zero {
        return invalid(env);
    }
    let sign_b = b.sign();
    if class_a == Class::Zero {
        if class_b == Class::Zero {
            return invalid(env);
        }
        if class_b != Class::Infinity {
            env.flags |= ZE;
        }
        return infinity(!sign_b);
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Infinity {
        if class_b == Class::Zero {
            return invalid(env);
        }
        return infinity(sign_b);
    }
    let x = Xf::from_f80(a);
    let is_power = x.significand == 1u128 << 127;
    if class_b == Class::Infinity {
        if is_power && x.exponent == 0 {
            return invalid(env);
        }
        // log2(a) < 0 for a below one.
        return infinity(sign_b != (x.exponent < 0));
    }
    if class_b == Class::Zero {
        return zero(sign_b != (x.exponent < 0));
    }
    if is_power {
        // An exact logarithm: the only rounding is the product's. Hardware
        // reports it as inexact anyway, and a tiny product as underflow, unless
        // the logarithm is zero.
        if x.exponent != 0 {
            env.flags |= PE;
            env.underflow_unmasked = true;
        }
        return mul(env, from_i64(x.exponent as i64), b);
    }
    log2(x).mul(Xf::from_f80(b)).round(env)
}

/// FYL2XP1: `b * log2(a + 1)`, accurate for a small `a`. The documented domain is
/// `|a| < 1 - sqrt(2)/2`; beyond it this keeps following the logarithm, as far as
/// it has one, which is what Intel hardware does.
pub fn yl2xp1(env: &mut Env, a: F80, b: F80) -> F80 {
    if let Some(result) = special_pair(env, a, b) {
        return result;
    }
    let (class_a, class_b) = (a.class(), b.class());
    let sign_a = a.sign();
    let sign_b = b.sign();
    if class_a == Class::Infinity {
        return yl2x(env, a, b);
    }
    check_denormal(env, a);
    check_denormal(env, b);
    if class_a == Class::Zero {
        if class_b == Class::Infinity {
            return invalid(env);
        }
        return zero(sign_a != sign_b);
    }
    let x = Xf::from_f80(a);
    if class_b == Class::Zero {
        return zero(sign_a != sign_b);
    }
    if class_b == Class::Infinity {
        return infinity(sign_a != sign_b);
    }
    if sign_a && x.exponent >= 0 {
        // Below the logarithm's domain Intel hardware hands back the operand.
        env.flags |= PE;
        return a;
    }
    let logarithm = if x.exponent < -2 {
        // Keeps the bits a small `a` would lose when added to one.
        let y = x.div(x.add(Xf::pos(1, 1u128 << 127)));
        two_atanh(y).mul(LOG2_E)
    } else {
        log2(x.add(ONE))
    };
    logarithm.mul(Xf::from_f80(b)).round(env)
}

/// `atan(t)` for `t` in `[0, 1]`.
fn atan_unit(t: Xf) -> Xf {
    // Pick k/8 near t: atan(t) = atan(k/8) + atan((t - k/8) / (1 + t k/8)).
    let scaled = if t.exponent >= 0 {
        8
    } else if t.exponent < -4 {
        0
    } else {
        let bits = t.significand >> (123 - t.exponent) as u32;
        (bits.wrapping_add(1) >> 1) as usize
    };
    let (base, rest) = if scaled == 0 {
        (Xf::pos(0, 0), t)
    } else {
        let mut k = Xf::from_int(scaled as i32);
        k.exponent = k.exponent.wrapping_sub(3);
        let index = (scaled - 1).min(ATAN_EIGHTHS.len() - 1);
        let top = t.sub(k);
        let bottom = ONE.add(t.mul(k));
        (ATAN_EIGHTHS[index], top.div(bottom))
    };
    let square = rest.mul(rest);
    let mut sum = ONE.div_small(41);
    for term in (1..=39u64).rev().step_by(2) {
        sum = ONE.div_small(term).sub(square.mul(sum));
    }
    base.add(rest.mul(sum))
}

/// FPATAN: `atan2(y, x)`, where `y` is ST(1) and `x` is ST(0).
pub fn atan2(env: &mut Env, y: F80, x: F80) -> F80 {
    if let Some(result) = special_pair(env, y, x) {
        return result;
    }
    let negative = y.sign();
    let (class_y, class_x) = (y.class(), x.class());
    check_denormal(env, y);
    check_denormal(env, x);
    let signed = |value: Xf| Xf { negative, ..value };
    if class_y == Class::Zero {
        return if x.sign() { signed(PI).round(env) } else { y };
    }
    if class_x == Class::Zero {
        return signed(PI_2).round(env);
    }
    if class_y == Class::Infinity {
        return match (class_x == Class::Infinity, x.sign()) {
            (true, false) => signed(PI_4).round(env),
            (true, true) => signed(PI3_4).round(env),
            _ => signed(PI_2).round(env),
        };
    }
    if class_x == Class::Infinity {
        return if x.sign() {
            signed(PI).round(env)
        } else {
            zero(negative)
        };
    }
    let (top, bottom) = (Xf::from_f80(y).abs(), Xf::from_f80(x).abs());
    let steep = (top.exponent, top.significand) > (bottom.exponent, bottom.significand);
    let t = if steep {
        bottom.div(top)
    } else {
        top.div(bottom)
    };
    let mut angle = atan_unit(t);
    if steep {
        angle = PI_2.sub(angle);
    }
    if x.sign() {
        angle = PI.sub(angle);
    }
    signed(angle).round(env)
}

/// The five constants the x87 loads, by opcode low bits.
#[derive(Clone, Copy)]
pub enum Constant {
    Log2Of10,
    Log2OfE,
    Pi,
    Log10Of2,
    LnOf2,
}

/// Loads a constant by the rounding control. These don't raise flags.
pub fn constant(rc: u8, which: Constant) -> F80 {
    let value = match which {
        Constant::Log2Of10 => LOG2_10,
        Constant::Log2OfE => LOG2_E,
        Constant::Pi => PI,
        Constant::Log10Of2 => LOG10_2,
        Constant::LnOf2 => LN2,
    };
    value.round(&mut Env::new(rc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env::new(RC_NEAREST)
    }

    /// A value from an integer numerator and a power of two.
    fn scaled(value: i64, power: i32) -> F80 {
        let unit = from_i64(value);
        scale(&mut env(), unit, from_i64(power as i64))
    }

    #[test]
    fn bytes_round_trip() {
        let mut value = F80::new();
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 0xAA];
        value.set_bytes(&bytes);
        assert_eq!(value.get_bytes(), bytes);
    }

    #[test]
    fn classes() {
        assert_eq!(F80::new().class(), Class::Zero);
        assert_eq!(F80::from_parts(false, 0, 1).class(), Class::Denormal);
        assert_eq!(F80::from_parts(false, 0, INT_BIT).class(), Class::Denormal);
        assert_eq!(from_i64(3).class(), Class::Normal);
        assert_eq!(F80::from_parts(false, 5, 1).class(), Class::Unsupported);
        assert_eq!(infinity(true).class(), Class::Infinity);
        assert_eq!(F80::INDEFINITE.class(), Class::QNan);
        assert_eq!(
            F80::from_parts(false, EXP_MAX, INT_BIT | 1).class(),
            Class::SNan
        );
    }

    #[test]
    fn an_integer_keeps_all_its_bits() {
        for value in [1i64, -1, 3, i64::MAX, i64::MIN, 0x1234_5678_9ABC_DEF0] {
            let mut e = env();
            assert_eq!(to_integer(&mut e, from_i64(value), 64), Some(value));
            assert_eq!(e.flags, 0);
        }
    }

    #[test]
    fn single_and_double_round_trip() {
        for bits in [
            0u64,
            0x3F80_0000,
            0xC000_0000,
            0x0000_0001,
            0x007F_FFFF,
            0x7F7F_FFFF,
            0x7F80_0000,
            0xFFC0_0000,
        ] {
            let (value, _) = from_ieee(bits, 23, 8);
            assert_eq!(to_ieee(&mut env(), value, 23, 8), bits);
        }
        for bits in [
            0u64,
            0x3FF0_0000_0000_0000,
            0xC000_0000_0000_0000,
            1,
            0x000F_FFFF_FFFF_FFFF,
            0x7FEF_FFFF_FFFF_FFFF,
            0x7FF0_0000_0000_0000,
            0xFFF8_0000_0000_0000,
        ] {
            let (value, _) = from_ieee(bits, 52, 11);
            assert_eq!(to_ieee(&mut env(), value, 52, 11), bits);
        }
    }

    #[test]
    fn sums_differences_products_and_quotients() {
        let mut e = env();
        assert_eq!(add(&mut e, from_i64(2), from_i64(3)), from_i64(5));
        assert_eq!(sub(&mut e, from_i64(2), from_i64(3)), from_i64(-1));
        assert_eq!(mul(&mut e, from_i64(-6), from_i64(7)), from_i64(-42));
        assert_eq!(div(&mut e, from_i64(42), from_i64(-6)), from_i64(-7));
        assert_eq!(e.flags, 0);
        let third = div(&mut e, from_i64(1), from_i64(3));
        assert_eq!(third, F80::from_parts(false, 0x3FFD, 0xAAAA_AAAA_AAAA_AAAB));
        assert_ne!(e.flags & PE, 0);
    }

    #[test]
    fn exact_cancellation_is_a_positive_zero_unless_rounding_down() {
        let (a, b) = (from_i64(7), from_i64(7));
        assert_eq!(sub(&mut env(), a, b), zero(false));
        assert_eq!(sub(&mut Env::new(RC_DOWN), a, b), zero(true));
    }

    #[test]
    fn sqrt_is_exact_where_it_can_be() {
        let mut e = env();
        assert_eq!(sqrt(&mut e, from_i64(144)), from_i64(12));
        assert_eq!(e.flags, 0);
        let root = sqrt(&mut e, scaled(1, 200));
        assert_eq!(root, scaled(1, 100));
        let two = sqrt(&mut e, from_i64(2));
        assert_eq!(two, F80::from_parts(false, 0x3FFF, 0xB504_F333_F9DE_6484));
    }

    #[test]
    fn compare_orders_infinities() {
        assert_eq!(compare(infinity(false), from_i64(1)), Ordering::Greater);
        assert_eq!(compare(infinity(true), from_i64(1)), Ordering::Less);
        assert_eq!(compare(infinity(true), infinity(true)), Ordering::Equal);
        assert_eq!(compare(zero(true), zero(false)), Ordering::Equal);
        assert_eq!(compare(from_i64(-1), from_i64(-2)), Ordering::Greater);
    }

    #[test]
    fn constants_are_full_precision() {
        assert_eq!(
            constant(RC_NEAREST, Constant::Pi),
            F80::from_parts(false, 0x4000, 0xC90F_DAA2_2168_C235)
        );
        assert_eq!(
            constant(RC_CHOP, Constant::Pi),
            F80::from_parts(false, 0x4000, 0xC90F_DAA2_2168_C234)
        );
    }
}
