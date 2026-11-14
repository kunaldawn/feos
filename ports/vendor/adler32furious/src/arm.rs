//! AArch64 Adler-32: NEON, and NEON with the dot-product extension.
//! Little-endian only.

use core::arch::aarch64::*;

use super::scalar::{self, chunk};
use super::{Adler32Fn, MAX_CHUNK_LEN};

/// Implementations usable on this CPU, best first.
#[inline]
pub(super) fn available(push: &mut impl FnMut((&'static str, Adler32Fn))) {
    if has_dotprod() {
        push(("neon_dotprod", neon_dotprod));
    }
    // NEON is part of the AArch64 baseline.
    push(("neon", neon));
}

#[cfg(feature = "std")]
#[inline]
fn has_dotprod() -> bool {
    std::arch::is_aarch64_feature_detected!("dotprod")
}

#[cfg(not(feature = "std"))]
#[inline]
fn has_dotprod() -> bool {
    cfg!(target_feature = "dotprod")
}

const MULTS: [u8; 64] = {
    let mut m = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        m[i] = (64 - i) as u8;
        i += 1;
    }
    m
};

/// Loaded at offset `n`, keeps the last `n` bytes of a 16-byte vector.
static TAIL_MASK: [u8; 32] = {
    let mut m = [0u8; 32];
    let mut i = 16;
    while i < 32 {
        m[i] = 0xFF;
        i += 1;
    }
    m
};

/// If the input is large and misaligned, sums bytes up to a 16-byte boundary
/// first; smaller inputs just take the misaligned-load penalty.
#[inline(always)]
fn align<'a>(s1: &mut u32, s2: &mut u32, p: &'a [u8]) -> &'a [u8] {
    let misalign = p.as_ptr().align_offset(16);
    if p.len() > 32768 && misalign != 0 {
        let (head, rest) = p.split_at(misalign);
        chunk(s1, s2, head);
        rest
    } else {
        p
    }
}

#[inline(always)]
fn load4(c: &[u8]) -> [uint8x16_t; 4] {
    let c: &[u8; 64] = c[..64].try_into().unwrap();
    // SAFETY: c is 64 bytes.
    unsafe {
        [
            vld1q_u8(c.as_ptr()),
            vld1q_u8(c.as_ptr().add(16)),
            vld1q_u8(c.as_ptr().add(32)),
            vld1q_u8(c.as_ptr().add(48)),
        ]
    }
}

#[inline(always)]
fn load1(c: &[u8]) -> uint8x16_t {
    let c: &[u8; 16] = c[..16].try_into().unwrap();
    // SAFETY: c is 16 bytes.
    unsafe { vld1q_u8(c.as_ptr()) }
}

/// Adds `c`, the last bytes of `whole`, to `v_s1` and `v_s2` 16 at a time.
/// `add` adds one vector's bytes to `v_s1`, and the bytes weighted 16 down
/// to 1 to `v_s2`.
///
/// `whole` must be at least 16 bytes: the last partial vector is its last 16
/// bytes, with those before `c` zeroed. The caller adds `s1 * c.len()` to
/// `s2`.
#[inline]
#[target_feature(enable = "neon")]
fn tail(
    whole: &[u8],
    mut c: &[u8],
    v_s1: &mut uint32x4_t,
    v_s2: &mut uint32x4_t,
    add: impl Fn(&mut uint32x4_t, &mut uint32x4_t, uint8x16_t),
) {
    while c.len() >= 16 {
        *v_s2 = vaddq_u32(*v_s2, vshlq_n_u32::<4>(*v_s1));
        add(v_s1, v_s2, load1(c));
        c = &c[16..];
    }
    if !c.is_empty() {
        let n = c.len();
        let last = load1(&whole[whole.len() - 16..]);
        *v_s2 = vaddq_u32(*v_s2, vmulq_n_u32(*v_s1, n as u32));
        add(v_s1, v_s2, vandq_u8(last, load1(&TAIL_MASK[n..])));
    }
}

/// Plain NEON: accumulate each of the 64 byte positions into 16-bit
/// counters, and multiply them by their weights once per chunk.
#[target_feature(enable = "neon")]
fn neon(adler: u32, data: &[u8]) -> u32 {
    if data.len() < 16 {
        return scalar::adler32_generic(adler, data);
    }
    let mut mults16 = [0u16; 64];
    for (m, &b) in mults16.iter_mut().zip(&MULTS) {
        *m = u16::from(b);
    }
    // SAFETY: mults16 is 64 u16s.
    let mults: [uint16x8_t; 8] =
        core::array::from_fn(|i| unsafe { vld1q_u16(mults16.as_ptr().add(8 * i)) });
    let mults_tail = load1(&MULTS[48..]);
    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;

    let mut p = align(&mut s1, &mut s2, data);
    while !p.is_empty() {
        let (mut c, rest) = p.split_at(p.len().min(MAX_CHUNK_LEN & !63));
        p = rest;

        if c.len() >= 16 {
            let whole = c;
            let mut v_s1 = vdupq_n_u32(0);
            let mut v_s2 = vdupq_n_u32(0);
            // Sum of the bytes at index i across all 64-byte segments.
            let mut v_byte_sums = [vdupq_n_u16(0); 8];

            s2 += s1 * c.len() as u32;

            while c.len() >= 64 {
                let [data_a, data_b, data_c, data_d] = load4(c);

                // Accumulate the previous s1 counters into s2; the
                // multiplication by 64 is delayed until after the loop.
                v_s2 = vaddq_u32(v_s2, v_s1);

                let mut tmp = vpaddlq_u8(data_a);
                v_byte_sums[0] = vaddw_u8(v_byte_sums[0], vget_low_u8(data_a));
                v_byte_sums[1] = vaddw_u8(v_byte_sums[1], vget_high_u8(data_a));
                tmp = vpadalq_u8(tmp, data_b);
                v_byte_sums[2] = vaddw_u8(v_byte_sums[2], vget_low_u8(data_b));
                v_byte_sums[3] = vaddw_u8(v_byte_sums[3], vget_high_u8(data_b));
                tmp = vpadalq_u8(tmp, data_c);
                v_byte_sums[4] = vaddw_u8(v_byte_sums[4], vget_low_u8(data_c));
                v_byte_sums[5] = vaddw_u8(v_byte_sums[5], vget_high_u8(data_c));
                tmp = vpadalq_u8(tmp, data_d);
                v_byte_sums[6] = vaddw_u8(v_byte_sums[6], vget_low_u8(data_d));
                v_byte_sums[7] = vaddw_u8(v_byte_sums[7], vget_high_u8(data_d));
                v_s1 = vpadalq_u16(v_s1, tmp);

                c = &c[64..];
            }

            // s2 = 64*s2 + (64*bytesum0 + 63*bytesum1 + ... + 1*bytesum63)
            v_s2 = vqshlq_n_u32::<6>(v_s2);
            for (sums, m) in v_byte_sums.iter().zip(&mults) {
                v_s2 = vmlal_u16(v_s2, vget_low_u16(*sums), vget_low_u16(*m));
                v_s2 = vmlal_high_u16(v_s2, *sums, *m);
            }

            tail(whole, c, &mut v_s1, &mut v_s2, |v_s1, v_s2, data| {
                *v_s1 = vpadalq_u16(*v_s1, vpaddlq_u8(data));
                let low = vmull_u8(vget_low_u8(data), vget_low_u8(mults_tail));
                *v_s2 = vpadalq_u16(*v_s2, vmlal_high_u8(low, data, mults_tail));
            });
            c = &[];

            s1 += vaddvq_u32(v_s1);
            s2 += vaddvq_u32(v_s2);
        }
        chunk(&mut s1, &mut s2, c);
    }
    (s2 << 16) | s1
}

/// `acc + dot(a, b)` in groups of 4 bytes: `vdotq_u32`, which is still
/// unstable in `core::arch`.
#[inline]
#[target_feature(enable = "neon,dotprod")]
fn udot(acc: uint32x4_t, a: uint8x16_t, b: uint8x16_t) -> uint32x4_t {
    let mut acc = acc;
    // SAFETY: udot only reads and writes these registers; dotprod is
    // enabled for this function.
    unsafe {
        core::arch::asm!(
            "udot {acc:v}.4s, {a:v}.16b, {b:v}.16b",
            acc = inout(vreg) acc,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    acc
}

/// NEON with `udot`, which sums groups of 4 byte products into 32-bit lanes.
#[target_feature(enable = "neon,dotprod")]
fn neon_dotprod(adler: u32, data: &[u8]) -> u32 {
    if data.len() < 16 {
        return scalar::adler32_generic(adler, data);
    }
    let mults = load4(&MULTS);
    let ones = vdupq_n_u8(1);
    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;

    let mut p = align(&mut s1, &mut s2, data);
    while !p.is_empty() {
        let (mut c, rest) = p.split_at(p.len().min(MAX_CHUNK_LEN & !63));
        p = rest;

        if c.len() >= 16 {
            let whole = c;
            let zero = vdupq_n_u32(0);
            let mut v_s1 = [zero; 4];
            let mut v_s2 = [zero; 4];
            let mut v_s1_sums = [zero; 4];

            s2 += s1 * c.len() as u32;

            while c.len() >= 64 {
                let data = load4(c);
                for i in 0..4 {
                    v_s1_sums[i] = vaddq_u32(v_s1_sums[i], v_s1[i]);
                    v_s1[i] = udot(v_s1[i], data[i], ones);
                    v_s2[i] = udot(v_s2[i], data[i], mults[i]);
                }
                c = &c[64..];
            }

            let sum4 = |v: [uint32x4_t; 4]| vaddq_u32(vaddq_u32(v[0], v[1]), vaddq_u32(v[2], v[3]));
            let mut v_s1 = sum4(v_s1);
            let mut v_s2 = vaddq_u32(sum4(v_s2), vqshlq_n_u32::<6>(sum4(v_s1_sums)));

            tail(whole, c, &mut v_s1, &mut v_s2, |v_s1, v_s2, data| {
                *v_s1 = udot(*v_s1, data, ones);
                *v_s2 = udot(*v_s2, data, mults[3]);
            });
            c = &[];

            s1 += vaddvq_u32(v_s1);
            s2 += vaddvq_u32(v_s2);
        }
        chunk(&mut s1, &mut s2, c);
    }
    (s2 << 16) | s1
}
