//! Fixed-point inverse DCT.
//!
//! This is the Loeffler, Ligtenberg and Moschytz factorisation with 12
//! multiplications per 1-D transform, the same one libjpeg calls "islow". It
//! uses no floating point, which matters on `x86_64-unknown-none`, where floats
//! are soft-float.
//!
//! The coefficients come from untrusted input, so [`dequantize`] bounds them to
//! `±LIMIT`. With that bound nothing below can overflow `i64`: the largest
//! intermediate is under `LIMIT * 64 * 2 * 2^26` (`2^49`). Both supported
//! targets are 64-bit, so `i64` costs no more than `i32` here.

const CONST_BITS: u32 = 13;
const PASS1_BITS: u32 = 2;

const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;

/// Largest dequantized magnitude. Valid 8-bit data stays far below this.
const LIMIT: i64 = 1 << 16;

/// Multiplies a coefficient by its quantizer, bounded to what the IDCT accepts.
#[inline]
pub(super) fn dequantize(value: i32, quantizer: u16) -> i32 {
    (value as i64 * quantizer as i64).clamp(-LIMIT, LIMIT) as i32
}

/// One 1-D transform. The results still carry the `CONST_BITS` scale.
#[inline]
fn transform(input: [i64; 8]) -> [i64; 8] {
    // Even part.
    let z1 = (input[2] + input[6]) * FIX_0_541196100;
    let even2 = z1 - input[6] * FIX_1_847759065;
    let even3 = z1 + input[2] * FIX_0_765366865;
    let even0 = (input[0] + input[4]) << CONST_BITS;
    let even1 = (input[0] - input[4]) << CONST_BITS;
    let tmp10 = even0 + even3;
    let tmp13 = even0 - even3;
    let tmp11 = even1 + even2;
    let tmp12 = even1 - even2;

    // Odd part.
    let (i7, i5, i3, i1) = (input[7], input[5], input[3], input[1]);
    let z5 = (i7 + i3 + i5 + i1) * FIX_1_175875602;
    let z1 = -(i7 + i1) * FIX_0_899976223;
    let z2 = -(i5 + i3) * FIX_2_562915447;
    let z3 = z5 - (i7 + i3) * FIX_1_961570560;
    let z4 = z5 - (i5 + i1) * FIX_0_390180644;
    let odd0 = i7 * FIX_0_298631336 + z1 + z3;
    let odd1 = i5 * FIX_2_053119869 + z2 + z4;
    let odd2 = i3 * FIX_3_072711026 + z2 + z3;
    let odd3 = i1 * FIX_1_501321110 + z1 + z4;

    [
        tmp10 + odd3,
        tmp11 + odd2,
        tmp12 + odd1,
        tmp13 + odd0,
        tmp13 - odd0,
        tmp12 - odd1,
        tmp11 - odd2,
        tmp10 - odd3,
    ]
}

#[inline]
fn descale(value: i64, bits: u32) -> i64 {
    (value + (1 << (bits - 1))) >> bits
}

#[inline]
fn to_sample(value: i64) -> u8 {
    (value + 128).clamp(0, 255) as u8
}

/// Transforms one block of dequantized coefficients into 8-bit samples.
///
/// `block` is in natural (row, column) order. Pass `has_ac = false` when every
/// coefficient but the first is zero; that block is a flat fill.
pub(super) fn idct(block: &[i32; 64], has_ac: bool, out: &mut [u8; 64]) {
    if !has_ac {
        out.fill(to_sample(descale(block[0] as i64, 3)));
        return;
    }

    let mut workspace = [0i32; 64];
    for x in 0..8 {
        let column = |y: usize| block[y * 8 + x] as i64;
        if (1..8).all(|y| block[y * 8 + x] == 0) {
            // Most columns of real images are DC-only.
            let value = (column(0) << PASS1_BITS) as i32;
            for y in 0..8 {
                workspace[y * 8 + x] = value;
            }
            continue;
        }
        let result = transform([
            column(0),
            column(1),
            column(2),
            column(3),
            column(4),
            column(5),
            column(6),
            column(7),
        ]);
        for y in 0..8 {
            workspace[y * 8 + x] = descale(result[y], CONST_BITS - PASS1_BITS) as i32;
        }
    }
    for y in 0..8 {
        let row = |x: usize| workspace[y * 8 + x] as i64;
        let result = transform([
            row(0),
            row(1),
            row(2),
            row(3),
            row(4),
            row(5),
            row(6),
            row(7),
        ]);
        for x in 0..8 {
            out[y * 8 + x] = to_sample(descale(result[x], CONST_BITS + PASS1_BITS + 3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 16384 * C(u) * cos((2x + 1)u*pi/16), the exact definition from T.81 A.3.3.
    const COS: [[i64; 8]; 8] = [
        [11585, 11585, 11585, 11585, 11585, 11585, 11585, 11585],
        [16069, 13623, 9102, 3196, -3196, -9102, -13623, -16069],
        [15137, 6270, -6270, -15137, -15137, -6270, 6270, 15137],
        [13623, -3196, -16069, -9102, 9102, 16069, 3196, -13623],
        [11585, -11585, -11585, 11585, 11585, -11585, -11585, 11585],
        [9102, -16069, 3196, 13623, -13623, -3196, 16069, -9102],
        [6270, -15137, 15137, -6270, -6270, 15137, -15137, 6270],
        [3196, -9102, 13623, -16069, 16069, -13623, 9102, -3196],
    ];

    fn reference(block: &[i32; 64]) -> [u8; 64] {
        let mut temp = [0i64; 64];
        for v in 0..8 {
            for x in 0..8 {
                temp[v * 8 + x] = (0..8).map(|u| COS[u][x] * block[v * 8 + u] as i64).sum();
            }
        }
        let scale = 4 * 16384 * 16384i64;
        let mut out = [0u8; 64];
        for y in 0..8 {
            for x in 0..8 {
                let sum: i64 = (0..8).map(|v| COS[v][y] * temp[v * 8 + x]).sum();
                let rounded = (2 * sum + scale).div_euclid(2 * scale);
                out[y * 8 + x] = to_sample(rounded);
            }
        }
        out
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn signed(&mut self, range: i32) -> i32 {
            (self.next() % (2 * range as u64 + 1)) as i32 - range
        }
    }

    #[test]
    fn matches_the_exact_transform_within_one_level() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut worst = 0i32;
        for round in 0..2000 {
            let mut block = [0i32; 64];
            block[0] = rng.signed(1000);
            // From sparse, image-like blocks through to dense ones.
            for _ in 0..round % 40 {
                let index = (rng.next() % 64) as usize;
                block[index] = rng.signed(200);
            }
            let has_ac = block[1..].iter().any(|&value| value != 0);
            let mut actual = [0u8; 64];
            idct(&block, has_ac, &mut actual);
            let expected = reference(&block);
            for i in 0..64 {
                worst = worst.max((actual[i] as i32 - expected[i] as i32).abs());
            }
        }
        assert!(worst <= 1, "worst IDCT error was {worst} levels");
    }

    #[test]
    fn dc_shortcut_matches_the_full_transform() {
        for dc in -1100..1100 {
            let mut block = [0i32; 64];
            block[0] = dc;
            let mut fast = [0u8; 64];
            let mut full = [0u8; 64];
            idct(&block, false, &mut fast);
            idct(&block, true, &mut full);
            assert_eq!(fast, full, "DC {dc}");
        }
    }

    #[test]
    fn extreme_coefficients_do_not_overflow() {
        for pattern in 0..4u32 {
            let mut block = [0i32; 64];
            for (i, value) in block.iter_mut().enumerate() {
                let negative = match pattern {
                    0 => false,
                    1 => true,
                    2 => i % 2 == 0,
                    _ => (i / 8 + i % 8) % 2 == 0,
                };
                *value = dequantize(if negative { i32::MIN } else { i32::MAX }, u16::MAX);
            }
            let mut out = [0u8; 64];
            idct(&block, true, &mut out);
        }
    }
}
