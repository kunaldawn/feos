//! SHA-1 `x86`/`x86_64` backend built on the SHA-NI instructions.
//!
//! Computes a block's digest and spills the expanded message schedule, which
//! the collision detection needs. The spill is the difference from an
//! ordinary SHA-1: `sha1msg1`/`sha1msg2` keep the schedule in registers, and
//! the check needs all 80 words in memory.
//!
//! `sha1rnds4` does four rounds at a time, so the work is in groups of four.
//! Each group consumes the four schedule words that are ready, finishes the
//! four that the next group needs, and takes the first two of the three steps
//! that will finish the group after that. `sha1nexte` carries the fifth
//! working word between groups, which is why two of them alternate.

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
compile_error!("the sha_ni backend needs an x86 or x86_64 target");

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

use crate::Schedule;
use crate::block::rounds;
use crate::mem::{load_u8x16, load_u32x4, store_u32x4};
use crate::ubc_check::RecompressFrom;

/// SHA-NI holds the four words of a group in the opposite order to the
/// schedule, so every crossing between the two reverses them.
const REVERSE: i32 = 0b00_01_10_11;

/// Writes the four words a group is about to use into the schedule.
///
/// The group is held reversed, so it goes to the mirrored offset rather than
/// through a `pshufd` to put it in order. What that leaves is [`Schedule`]'s
/// backwards layout, which every reader of it already indexes through.
///
/// `I` is a const parameter, so a spill past the end of the schedule is a
/// compile error at the call site rather than a promise in a comment.
#[inline]
#[target_feature(enable = "sse2")]
fn spill<const I: usize>(w: &mut Schedule, msg: __m128i) {
    store_u32x4(w.window_mut::<I, 4>(), msg);
}

/// The four message words starting at byte `I`, in native order.
///
/// `I` is a const parameter, so a load past the end of the block is a compile
/// error at the call site.
#[inline]
#[target_feature(enable = "sse2,ssse3")]
fn msg<const I: usize>(block: &[u8; 64], swap: __m128i) -> __m128i {
    const { assert!(I + 16 <= 64, "the load runs past the block") }
    let raw = load_u8x16(block[I..I + 16].try_into().unwrap());
    _mm_shuffle_epi8(raw, swap)
}

/// The four schedule words of steps `T..T + 4`, exclusive-ored with the
/// partner's difference, in the order the rounds want them.
///
/// The read side of [`spill`], with `T` const for the same reason. The spill is already reversed, being mirrored
/// for that reason; the difference table is in step order, so it is the one
/// that turns round.
#[inline]
#[target_feature(enable = "sse2,ssse3")]
fn words<const T: usize>(m1: &Schedule, dm: &[u32; 80]) -> __m128i {
    const { assert!(T + 4 <= 80, "the group runs past the schedule") }
    let spilled = load_u32x4(m1.window::<T, 4>());
    let diff = load_u32x4(dm[T..T + 4].try_into().unwrap());
    _mm_xor_si128(spilled, _mm_shuffle_epi32(diff, REVERSE))
}

/// The `abcd` half of the state. The fifth word is carried on its own.
#[inline]
#[target_feature(enable = "sse2")]
fn load_abcd(state: &[u32; 5]) -> __m128i {
    _mm_shuffle_epi32(load_u32x4(state.first_chunk().unwrap()), REVERSE)
}

/// Writes `abcd` back, leaving the fifth word alone.
#[inline]
#[target_feature(enable = "sse2")]
fn store_abcd(state: &mut [u32; 5], abcd: __m128i) {
    store_u32x4(
        state.first_chunk_mut().unwrap(),
        _mm_shuffle_epi32(abcd, REVERSE),
    );
}

/// Compresses one block, spilling the message schedule into `w`.
///
/// Requires `sha`, `sse2`, `ssse3` and `sse4.1`, so a caller that cannot
/// prove the features needs an `unsafe` block.
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
pub(crate) fn compress_spill(
    state: &mut [u32; 5],
    block: &[u8; 64],
    w: &mut Schedule,
    at_60: &mut [u32; 5],
    at_64: &mut [u32; 5],
) {
    /// One group of four rounds, once the schedule is under way.
    ///
    /// `ready` holds the words these rounds use. `next` is finished here,
    /// and the two groups behind it take the first and second of their three
    /// steps. The working word arrives in `live` and the one for the next
    /// group is put aside in `held`.
    macro_rules! group {
        (
            $w:expr, $t:literal, $k:expr, $abcd:ident, $live:ident, $held:ident,
            $ready:ident, $next:ident, $second:ident, $first:ident
        ) => {{
            spill::<$t>($w, $ready);
            $live = _mm_sha1nexte_epu32($live, $ready);
            $held = $abcd;
            $next = _mm_sha1msg2_epu32($next, $ready);
            $abcd = _mm_sha1rnds4_epu32($abcd, $live, $k);
            $first = _mm_sha1msg1_epu32($first, $ready);
            $second = _mm_xor_si128($second, $ready);
        }};
        (
            $w:expr, $t:literal, $k:expr, $abcd:ident, $live:ident, $held:ident,
            $ready:ident, $next:ident, $second:ident, $first:ident => $at:ident
        ) => {{
            spill::<$t>($w, $ready);
            $live = _mm_sha1nexte_epu32($live, $ready);
            // As held: `abcd` reversed in the first four words, and E + W in
            // the fifth, from the top of a store one word further on.
            store_u32x4((&mut $at[1..5]).try_into().unwrap(), $live);
            store_u32x4($at.first_chunk_mut().unwrap(), $abcd);
            $held = $abcd;
            $next = _mm_sha1msg2_epu32($next, $ready);
            $abcd = _mm_sha1rnds4_epu32($abcd, $live, $k);
            $first = _mm_sha1msg1_epu32($first, $ready);
            $second = _mm_xor_si128($second, $ready);
        }};
    }

    // Turns the big-endian message into native order, reversing the four
    // words along with the bytes.
    let swap = _mm_set_epi64x(0x0001_0203_0405_0607, 0x0809_0A0B_0C0D_0E0F);

    let mut abcd = load_abcd(state);
    let abcd_in = abcd;
    let e_in = _mm_set_epi32(state[4] as i32, 0, 0, 0);

    let mut msg0 = msg::<0>(block, swap);
    let mut msg1 = msg::<16>(block, swap);
    let mut msg2 = msg::<32>(block, swap);
    let mut msg3 = msg::<48>(block, swap);

    // The first sixteen words are the block itself. The expansion starts as
    // soon as enough of them are in, so these four groups build up to the
    // steady shape of `group!`.
    spill::<0>(w, msg0);
    let mut e0 = _mm_add_epi32(e_in, msg0);
    let mut e1 = abcd;
    abcd = _mm_sha1rnds4_epu32(abcd, e0, 0);

    spill::<4>(w, msg1);
    e1 = _mm_sha1nexte_epu32(e1, msg1);
    e0 = abcd;
    abcd = _mm_sha1rnds4_epu32(abcd, e1, 0);
    msg0 = _mm_sha1msg1_epu32(msg0, msg1);

    spill::<8>(w, msg2);
    e0 = _mm_sha1nexte_epu32(e0, msg2);
    e1 = abcd;
    abcd = _mm_sha1rnds4_epu32(abcd, e0, 0);
    msg1 = _mm_sha1msg1_epu32(msg1, msg2);
    msg0 = _mm_xor_si128(msg0, msg2);

    spill::<12>(w, msg3);
    e1 = _mm_sha1nexte_epu32(e1, msg3);
    e0 = abcd;
    msg0 = _mm_sha1msg2_epu32(msg0, msg3);
    abcd = _mm_sha1rnds4_epu32(abcd, e1, 0);
    msg2 = _mm_sha1msg1_epu32(msg2, msg3);
    msg1 = _mm_xor_si128(msg1, msg3);

    // Steady state. The four message registers take turns, and so do the two
    // working words.
    group!(w, 16, 0, abcd, e0, e1, msg0, msg1, msg2, msg3);
    group!(w, 20, 1, abcd, e1, e0, msg1, msg2, msg3, msg0);
    group!(w, 24, 1, abcd, e0, e1, msg2, msg3, msg0, msg1);
    group!(w, 28, 1, abcd, e1, e0, msg3, msg0, msg1, msg2);
    group!(w, 32, 1, abcd, e0, e1, msg0, msg1, msg2, msg3);
    group!(w, 36, 1, abcd, e1, e0, msg1, msg2, msg3, msg0);
    group!(w, 40, 2, abcd, e0, e1, msg2, msg3, msg0, msg1);
    group!(w, 44, 2, abcd, e1, e0, msg3, msg0, msg1, msg2);
    group!(w, 48, 2, abcd, e0, e1, msg0, msg1, msg2, msg3);
    group!(w, 52, 2, abcd, e1, e0, msg1, msg2, msg3, msg0);
    group!(w, 56, 2, abcd, e0, e1, msg2, msg3, msg0, msg1);
    group!(w, 60, 3, abcd, e1, e0, msg3, msg0, msg1, msg2 => at_60);
    group!(w, 64, 3, abcd, e0, e1, msg0, msg1, msg2, msg3 => at_64);

    // The last words are already in hand, so the expansion stops one step at
    // a time.
    spill::<68>(w, msg1);
    e1 = _mm_sha1nexte_epu32(e1, msg1);
    e0 = abcd;
    msg2 = _mm_sha1msg2_epu32(msg2, msg1);
    abcd = _mm_sha1rnds4_epu32(abcd, e1, 3);
    msg3 = _mm_xor_si128(msg3, msg1);

    spill::<72>(w, msg2);
    e0 = _mm_sha1nexte_epu32(e0, msg2);
    e1 = abcd;
    msg3 = _mm_sha1msg2_epu32(msg3, msg2);
    abcd = _mm_sha1rnds4_epu32(abcd, e0, 3);

    spill::<76>(w, msg3);
    e1 = _mm_sha1nexte_epu32(e1, msg3);
    e0 = abcd;
    abcd = _mm_sha1rnds4_epu32(abcd, e1, 3);

    // Feed-forward.
    e0 = _mm_sha1nexte_epu32(e0, e_in);
    abcd = _mm_add_epi32(abcd, abcd_in);

    store_abcd(state, abcd);
    state[4] = _mm_extract_epi32(e0, 3) as u32;
}

/// Whether this candidate is the attack it is a candidate for.
///
/// The `x86` half of [`Backend::is_attack`], which says why it runs this way
/// round. The chaining value it starts from is worked out here rather than
/// handed in, so it never leaves the vector registers.
///
/// [`Backend::is_attack`]: crate::block::Backend::is_attack
#[inline]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
pub(crate) fn recompress(
    step: RecompressFrom,
    m1: &Schedule,
    dm: &[u32; 80],
    state: &[u32; 5],
    chaining_out: &[u32; 5],
) -> bool {
    let target = rounds::partner_start(step, m1, dm, state, chaining_out);

    let mut abcd = load_abcd(&target);
    let abcd_in = abcd;
    let e_in = _mm_set_epi32(target[4] as i32, 0, 0, 0);

    // The first group has no round behind it to carry the working word in,
    // so it adds the chaining value's fifth word itself.
    let mut e0 = _mm_add_epi32(e_in, words::<0>(m1, dm));
    let mut e1 = abcd;
    abcd = _mm_sha1rnds4_epu32(abcd, e0, 0);

    /// Four rounds. The working word arrives in `live` and the one the next
    /// four need is put aside in `held`, which is why two alternate.
    ///
    /// `compress_spill`'s group carries the schedule too; here there is none
    /// to carry, only words to read.
    macro_rules! group {
        ($g:expr, $k:expr, $live:ident, $held:ident) => {{
            let ready = words::<{ 4 * $g }>(m1, dm);
            $live = _mm_sha1nexte_epu32($live, ready);
            $held = abcd;
            abcd = _mm_sha1rnds4_epu32(abcd, $live, $k);
        }};
    }

    group!(1, 0, e1, e0);
    group!(2, 0, e0, e1);
    group!(3, 0, e1, e0);
    group!(4, 0, e0, e1);

    group!(5, 1, e1, e0);
    group!(6, 1, e0, e1);
    group!(7, 1, e1, e0);
    group!(8, 1, e0, e1);
    group!(9, 1, e1, e0);

    group!(10, 2, e0, e1);
    group!(11, 2, e1, e0);
    group!(12, 2, e0, e1);
    group!(13, 2, e1, e0);
    group!(14, 2, e0, e1);

    group!(15, 3, e1, e0);
    group!(16, 3, e0, e1);
    group!(17, 3, e1, e0);
    group!(18, 3, e0, e1);
    group!(19, 3, e1, e0);

    // Feed-forward.
    e0 = _mm_sha1nexte_epu32(e0, e_in);
    abcd = _mm_add_epi32(abcd, abcd_in);

    let mut ends_on = [0u32; 5];
    store_abcd(&mut ends_on, abcd);
    ends_on[4] = _mm_extract_epi32(e0, 3) as u32;

    crate::block::xor(&ends_on, chaining_out) == 0
}
