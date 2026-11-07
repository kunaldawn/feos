//! Inverse transforms (spec 8.5.10 to 8.5.13). Coefficients are in raster order.

use super::pixel::Pixel;

#[inline(always)]
fn clip<P: Pixel>(value: i32, max: i32) -> P {
    P::new(value.clamp(0, max))
}

/// Adds the 4x4 inverse transform of `c` to the block at `at`.
pub(crate) fn idct4_add<P: Pixel>(dst: &mut [P], at: usize, stride: usize, c: &[i32], max: i32) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let d = &c[i * 4..i * 4 + 4];
        let e0 = d[0] + d[2];
        let e1 = d[0] - d[2];
        let e2 = (d[1] >> 1) - d[3];
        let e3 = d[1] + (d[3] >> 1);
        t[i * 4] = e0 + e3;
        t[i * 4 + 1] = e1 + e2;
        t[i * 4 + 2] = e1 - e2;
        t[i * 4 + 3] = e0 - e3;
    }
    for x in 0..4 {
        let e0 = t[x] + t[8 + x];
        let e1 = t[x] - t[8 + x];
        let e2 = (t[4 + x] >> 1) - t[12 + x];
        let e3 = t[4 + x] + (t[12 + x] >> 1);
        let f = [e0 + e3, e1 + e2, e1 - e2, e0 - e3];
        for y in 0..4 {
            let p = &mut dst[at + y * stride + x];
            *p = clip(p.get() + ((f[y] + 32) >> 6), max);
        }
    }
}

#[inline(always)]
fn idct8_line(d: [i32; 8]) -> [i32; 8] {
    let a0 = d[0] + d[4];
    let a1 = -d[3] + d[5] - d[7] - (d[7] >> 1);
    let a2 = d[0] - d[4];
    let a3 = d[1] + d[7] - d[3] - (d[3] >> 1);
    let a4 = (d[2] >> 1) - d[6];
    let a5 = -d[1] + d[7] + d[5] + (d[5] >> 1);
    let a6 = d[2] + (d[6] >> 1);
    let a7 = d[3] + d[5] + d[1] + (d[1] >> 1);
    let b0 = a0 + a6;
    let b1 = a1 + (a7 >> 2);
    let b2 = a2 + a4;
    let b3 = a3 + (a5 >> 2);
    let b4 = a2 - a4;
    let b5 = (a3 >> 2) - a5;
    let b6 = a0 - a6;
    let b7 = a7 - (a1 >> 2);
    [
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]
}

/// Adds the 8x8 inverse transform of `c` to the block at `at`.
pub(crate) fn idct8_add<P: Pixel>(dst: &mut [P], at: usize, stride: usize, c: &[i32], max: i32) {
    let mut t = [0i32; 64];
    for i in 0..8 {
        let r = &c[i * 8..i * 8 + 8];
        let out = idct8_line([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
        t[i * 8..i * 8 + 8].copy_from_slice(&out);
    }
    for x in 0..8 {
        let out = idct8_line([
            t[x],
            t[8 + x],
            t[16 + x],
            t[24 + x],
            t[32 + x],
            t[40 + x],
            t[48 + x],
            t[56 + x],
        ]);
        for y in 0..8 {
            let p = &mut dst[at + y * stride + x];
            *p = clip(p.get() + ((out[y] + 32) >> 6), max);
        }
    }
}

/// The 4x4 Hadamard of Intra16x16 DC levels, in raster order.
pub(crate) fn hadamard4(c: &mut [i32; 16]) {
    for i in 0..4 {
        let (a, b, d, e) = (c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]);
        c[i * 4] = a + b + d + e;
        c[i * 4 + 1] = a + b - d - e;
        c[i * 4 + 2] = a - b - d + e;
        c[i * 4 + 3] = a - b + d - e;
    }
    for x in 0..4 {
        let (a, b, d, e) = (c[x], c[4 + x], c[8 + x], c[12 + x]);
        c[x] = a + b + d + e;
        c[4 + x] = a + b - d - e;
        c[8 + x] = a - b - d + e;
        c[12 + x] = a - b + d - e;
    }
}

/// The 2x2 transform of 4:2:0 chroma DC levels.
pub(crate) fn chroma_dc_420(c: &mut [i32]) {
    let (a, b, d, e) = (c[0], c[1], c[2], c[3]);
    c[0] = a + b + d + e;
    c[1] = a - b + d - e;
    c[2] = a + b - d - e;
    c[3] = a - b - d + e;
}

/// The 2x4 transform of 4:2:2 chroma DC levels. `c` holds them in parsing order
/// and comes back in the raster order of the chroma blocks.
pub(crate) fn chroma_dc_422(c: &mut [i32]) {
    // 8.5.11.1: rows of the 4x2 matrix c, from the coded list.
    let m = [
        [c[0], c[2]],
        [c[1], c[5]],
        [c[3], c[6]],
        [c[4], c[7]],
    ];
    for col in 0..2 {
        let (a, b, d, e) = (m[0][col], m[1][col], m[2][col], m[3][col]);
        let v = [a + b + d + e, a + b - d - e, a - b - d + e, a - b + d - e];
        for row in 0..4 {
            c[row * 2 + col] = v[row];
        }
    }
    for row in 0..4 {
        let (a, b) = (c[row * 2], c[row * 2 + 1]);
        c[row * 2] = a + b;
        c[row * 2 + 1] = a - b;
    }
}
