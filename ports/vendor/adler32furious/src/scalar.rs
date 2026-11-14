//! Portable Adler-32, also used by the vectorized versions for inputs under
//! 16 bytes and to align large ones.
//!
//! Treats a machine word as a vector of 16-bit lanes ("SWAR"): each word of
//! input is split into its even and odd bytes, which are summed per byte
//! position into lanes, and only folded into `s2` with their weights once per
//! block of words.

use super::{DIVISOR, MAX_CHUNK_LEN};

type Word = usize;
const WORD_BYTES: usize = size_of::<Word>();
/// 1 in each 16-bit lane.
const LANE_ONES: Word = Word::MAX / 0xFFFF;
/// The low byte of each 16-bit lane.
const LOW_BYTES: Word = LANE_ONES * 0xFF;
/// The most words whose bytes can be summed into 16-bit lanes, one byte per
/// lane per word, without overflow.
const MAX_BLOCK_WORDS: usize = 0xFFFF / 0xFF;

/// Adds `p` to `s1` and `s2`, then reduces both mod `DIVISOR`.
///
/// The caller guarantees that neither sum can overflow, i.e. that `p.len()`
/// plus any bytes summed since the last reduction is at most `MAX_CHUNK_LEN`.
#[inline(always)]
pub(super) fn chunk(s1: &mut u32, s2: &mut u32, p: &[u8]) {
    let (words, rest) = p.as_chunks::<WORD_BYTES>();
    for block in words.chunks(MAX_BLOCK_WORDS) {
        // s1 before each word, summed; each word adds it to s2 WORD_BYTES
        // times.
        let mut s1_sums = 0u32;
        // Sums of the bytes at each even and odd position within a word.
        let (mut even_sums, mut odd_sums) = (0, 0);
        for &w in block {
            let w = Word::from_le_bytes(w);
            let even = w & LOW_BYTES;
            let odd = (w >> 8) & LOW_BYTES;
            even_sums += even;
            odd_sums += odd;
            s1_sums += *s1;
            *s1 += lane_sum(even + odd);
        }
        *s2 += WORD_BYTES as u32 * s1_sums + weighted_sum(even_sums, odd_sums);
    }
    for &b in rest {
        *s1 += u32::from(b);
        *s2 += *s1;
    }
    *s1 %= DIVISOR;
    *s2 %= DIVISOR;
}

/// The sum of the 16-bit lanes of `x`, which must fit in 16 bits.
#[inline(always)]
fn lane_sum(x: Word) -> u32 {
    // Multiplying by LANE_ONES adds every lane into the top one.
    (x.wrapping_mul(LANE_ONES) >> (Word::BITS - 16)) as u32
}

/// Each byte position's sum times its weight within a word: `WORD_BYTES`
/// for the first byte, down to 1 for the last.
#[inline(always)]
fn weighted_sum(even_sums: Word, odd_sums: Word) -> u32 {
    let mut sum = 0;
    for lane in 0..WORD_BYTES / 2 {
        let even = (even_sums >> (16 * lane)) as u32 & 0xFFFF;
        let odd = (odd_sums >> (16 * lane)) as u32 & 0xFFFF;
        let weight = (WORD_BYTES - 2 * lane) as u32;
        sum += weight * even + (weight - 1) * odd;
    }
    sum
}

pub(super) fn adler32_generic(adler: u32, data: &[u8]) -> u32 {
    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    for p in data.chunks(MAX_CHUNK_LEN & !(WORD_BYTES - 1)) {
        chunk(&mut s1, &mut s2, p);
    }
    (s2 << 16) | s1
}
