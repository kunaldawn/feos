//! Integer sine and the inverse MDCT shared by Vorbis and AAC.
//!
//! The transform is
//! `y[n] = sum_k X[k] cos(2 pi / N (n + 1/2 + N/4) (k + 1/2))`, with `N/2`
//! inputs and `N` outputs, computed through an `N/4`-point complex FFT in Q31
//! arithmetic with rounding. A sample in fixed-point format comes out in that
//! same format, with an error of a few units in the last place.
//!
//! Values wrap instead of saturating. A valid stream cannot reach the limit
//! when its samples have headroom; a hostile one gets garbage, not a panic.

use alloc::vec::Vec;

use crate::bits::try_vec;
use crate::error::Result;

const HALF_PI_Q62: i128 = 7_244_019_458_077_122_842;

/// `sin(2 pi phase / 2^64)` in Q62, from a Taylor series in integers.
pub(crate) fn sin_turn(phase: u64) -> i64 {
    const QUARTER: u64 = 1 << 62;
    let quadrant = phase >> 62;
    let fraction = phase & (QUARTER - 1);
    let fraction = if quadrant & 1 != 0 {
        QUARTER - fraction
    } else {
        fraction
    };
    let x = ((fraction as i128 * HALF_PI_Q62) >> 62) as i64;
    // Up to (pi/2)^2, which needs the sign bit of a Q62 `u64`.
    let x2 = ((x as i128 * x as i128) >> 62) as u64;
    let mut term = x;
    let mut sum = x;
    let mut k = 1u64;
    while k <= 13 {
        let scale = x2 / (2 * k * (2 * k + 1));
        term = ((term as i128 * scale as i128) >> 62) as i64;
        if k & 1 != 0 {
            sum -= term;
        } else {
            sum += term;
        }
        k += 1;
    }
    if quadrant >= 2 {
        -sum
    } else {
        sum
    }
}

/// `cos(2 pi phase / 2^64)` in Q62.
pub(crate) fn cos_turn(phase: u64) -> i64 {
    sin_turn(phase.wrapping_add(1 << 62))
}

/// Converts Q62 to Q31 with rounding, keeping +1.0 representable-ish.
pub(crate) fn q62_to_q31(value: i64) -> i32 {
    ((value as i128 + (1 << 30)) >> 31).clamp(i32::MIN as i128, i32::MAX as i128) as i32
}

#[inline]
fn mul_q31(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64 + (1 << 30)) >> 31) as i32
}

/// `(ar + i ai)(br + i bi)` in Q31. The twiddle `b` has magnitude at most one,
/// so the sums of products cannot overflow an `i64`.
#[inline]
pub(crate) fn cmul(ar: i32, ai: i32, br: i32, bi: i32) -> (i32, i32) {
    let (ar, ai, br, bi) = (ar as i64, ai as i64, br as i64, bi as i64);
    (
        ((ar * br - ai * bi + (1 << 30)) >> 31) as i32,
        ((ar * bi + ai * br + (1 << 30)) >> 31) as i32,
    )
}

/// Tables for one transform size.
pub(crate) struct Imdct {
    n: usize,
    /// Pre- and post-rotation, `n/4` entries.
    rotate: Vec<(i32, i32)>,
    /// FFT twiddles, `n/8` entries.
    twiddle: Vec<(i32, i32)>,
    /// Bit reversal, `n/4` entries. Empty if `n/4` isn't a power of two, when `twiddle` has
    /// `n/4` entries instead, and the transform is the general one below.
    reverse: Vec<u16>,
    /// FFT working space, `n/4` entries.
    z: Vec<(i32, i32)>,
    /// The general transform's other working space.
    scratch: Vec<(i32, i32)>,
}

impl Imdct {
    /// `n` is from 16 to 8192, a multiple of 8 with no factor but 2, 3 and 5; the callers
    /// guarantee it.
    pub(crate) fn new(n: usize) -> Result<Self> {
        debug_assert!((16..=8192).contains(&n) && n.is_multiple_of(8));
        let n4 = n / 4;
        let bits = n4.trailing_zeros();
        let general = !n.is_power_of_two();
        let mut rotate = try_vec(n4, (0, 0))?;
        let mut twiddle = try_vec(if general { n4 } else { n4 / 2 }, (0, 0))?;
        let mut reverse = try_vec(if general { 0 } else { n4 }, 0u16)?;
        for (k, slot) in rotate.iter_mut().enumerate() {
            // (k + 1/8) / n of a turn, in 2^64 per turn.
            let phase = ((((8 * k as u64 + 1) as u128) << 61) / n as u128) as u64;
            // -cos(a + pi/2), -sin(a + pi/2): the signs that make this
            // transform equal to the formula in the module comment.
            *slot = (
                q62_to_q31(sin_turn(phase)),
                q62_to_q31(-cos_turn(phase)),
            );
        }
        for (k, slot) in twiddle.iter_mut().enumerate() {
            let phase = (((k as u128) << 64) / n4 as u128) as u64;
            *slot = (q62_to_q31(cos_turn(phase)), q62_to_q31(sin_turn(phase)));
        }
        for (k, slot) in reverse.iter_mut().enumerate() {
            *slot = ((k as u32).reverse_bits() >> (32 - bits)) as u16;
        }
        Ok(Self {
            n,
            rotate,
            twiddle,
            reverse,
            z: try_vec(n4, (0, 0))?,
            scratch: try_vec(if general { n4 } else { 0 }, (0, 0))?,
        })
    }

    /// Transforms `input` (`n/2` values) to `output` (`n` values).
    pub(crate) fn run(&mut self, input: &[i32], output: &mut [i32]) {
        let (n, n2, n4, n8) = (self.n, self.n / 2, self.n / 4, self.n / 8);
        let (Some(input), Some(output)) = (input.get(..n2), output.get_mut(..n)) else {
            debug_assert!(false, "IMDCT buffers are too short");
            return;
        };
        let z = &mut self.z[..n4];

        if self.reverse.is_empty() {
            for k in 0..n4 {
                let (tc, ts) = self.rotate[k];
                z[k] = cmul(input[n2 - 1 - 2 * k], input[2 * k], tc, ts);
            }
            general_fft(z, &self.twiddle, &mut self.scratch);
        } else {
            for k in 0..n4 {
                let (tc, ts) = self.rotate[k];
                z[self.reverse[k] as usize] = cmul(input[n2 - 1 - 2 * k], input[2 * k], tc, ts);
            }
        }

        // In-place inverse FFT (positive exponent, no scaling).
        let mut size = 2;
        while size <= n4 && !self.reverse.is_empty() {
            let half = size / 2;
            let step = n4 / size;
            for block in z.chunks_exact_mut(size) {
                let (low, high) = block.split_at_mut(half);
                for (j, (a, b)) in low.iter_mut().zip(high.iter_mut()).enumerate() {
                    let (wr, wi) = self.twiddle[j * step];
                    let (tr, ti) = cmul(b.0, b.1, wr, wi);
                    *b = (a.0.wrapping_sub(tr), a.1.wrapping_sub(ti));
                    *a = (a.0.wrapping_add(tr), a.1.wrapping_add(ti));
                }
            }
            size *= 2;
        }

        // Post-rotate, writing the middle half of the output.
        for k in 0..n8 {
            let (lo, hi) = (n8 - k - 1, n8 + k);
            let (tc0, ts0) = self.rotate[lo];
            let (tc1, ts1) = self.rotate[hi];
            let (r0, i1) = cmul(z[lo].1, z[lo].0, ts0, tc0);
            let (r1, i0) = cmul(z[hi].1, z[hi].0, ts1, tc1);
            output[n4 + 2 * lo] = r0;
            output[n4 + 2 * lo + 1] = i0;
            output[n4 + 2 * hi] = r1;
            output[n4 + 2 * hi + 1] = i1;
        }
        // The two outer quarters mirror the middle half.
        for k in 0..n4 {
            output[k] = output[n2 - k - 1].wrapping_neg();
            output[n - k - 1] = output[n2 + k];
        }
    }
}

/// An inverse FFT of any size made of 2s, 3s and 5s, without scaling: Stockham's, going
/// back and forth between `z` and `scratch`. `twiddle` is `z.len()` entries, a whole turn.
fn general_fft(z: &mut [(i32, i32)], twiddle: &[(i32, i32)], scratch: &mut [(i32, i32)]) {
    let total = z.len();
    let mut in_z = true;
    let mut size = total;
    let mut stride = 1;
    while size > 1 {
        let radix = [2usize, 3, 5].into_iter().find(|p| size.is_multiple_of(*p)).unwrap_or(size).min(5);
        let m = size / radix;
        let (from, to): (&[(i32, i32)], &mut [(i32, i32)]) =
            if in_z { (&*z, &mut *scratch) } else { (&*scratch, &mut *z) };
        for p in 0..m {
            for q in 0..stride {
                for t in 0..radix {
                    let (mut sum_r, mut sum_i) = (0i64, 0i64);
                    for i in 0..radix {
                        let (ar, ai) = from[q + stride * (p + i * m)];
                        let (wr, wi) = twiddle[(i * t * total / radix) % total];
                        let (r, im) = cmul(ar, ai, wr, wi);
                        sum_r += r as i64;
                        sum_i += im as i64;
                    }
                    let (wr, wi) = twiddle[(p * t * stride) % total];
                    to[q + stride * (radix * p + t)] = cmul(sum_r as i32, sum_i as i32, wr, wi);
                }
            }
        }
        in_z = !in_z;
        size = m;
        stride *= radix;
    }
    if !in_z {
        z.copy_from_slice(scratch);
    }
}

/// Multiplies a Q24-ish sample by a Q31 window value.
#[inline]
pub(crate) fn window(sample: i32, weight: i32) -> i32 {
    mul_q31(sample, weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_is_accurate() {
        assert_eq!(sin_turn(0), 0);
        assert!((sin_turn(1 << 62) - (1i64 << 62)).abs() < 64);
        assert!((sin_turn(1 << 63)).abs() < 64);
        assert!((sin_turn(3 << 62) + (1i64 << 62)).abs() < 64);
        // sin(pi/6) = 1/2, at a twelfth of a turn.
        let twelfth = (u64::MAX / 12) + 1;
        assert!((sin_turn(twelfth) - (1i64 << 61)).abs() < 1 << 10);
        // cos^2 + sin^2 = 1 around the circle.
        for k in 0u64..1000 {
            let phase = k.wrapping_mul(0x0123_4567_89ab_cdef);
            let s = q62_to_q31(sin_turn(phase)) as i64;
            let c = q62_to_q31(cos_turn(phase)) as i64;
            assert!((s * s + c * c - (1 << 62)).abs() < 1 << 34);
        }
    }

    /// Compares with the defining sum, using the same integer cosine.
    #[test]
    fn matches_the_direct_formula() {
        for n in [16usize, 64, 240, 256, 1024, 1920] {
            let mut seed = 0x1234_5678u32;
            let input: Vec<i32> = (0..n / 2)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((seed >> 8) as i32 - (1 << 23)) / (n as i32)
                })
                .collect();
            let mut output = alloc::vec![0i32; n];
            Imdct::new(n).unwrap().run(&input, &mut output);
            for (m, &value) in output.iter().enumerate() {
                let mut sum = 0i128;
                for (k, &x) in input.iter().enumerate() {
                    // (2m + 1 + n/2)(2k + 1) / (4n) of a turn, in 2^64 per turn.
                    let product = ((2 * m + 1 + n / 2) * (2 * k + 1)) as u128;
                    let phase = ((product << 62) / n as u128) as u64;
                    sum += x as i128 * cos_turn(phase) as i128;
                }
                let expected = (sum >> 62) as i64;
                assert!(
                    (expected - value as i64).abs() <= 64,
                    "n={n} m={m} {expected} {value}"
                );
            }
        }
    }
}
