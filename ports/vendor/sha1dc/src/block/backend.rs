//! Selects between the hardware and scalar SHA-1 compression implementations.
//!
//! The three are [`sha_ni`], [`armv8`] and [`scalar`], one file each. All of
//! them leave the expanded schedule behind, which the detection needs and an
//! ordinary SHA-1 would not keep.
//!
//! [`Backend::scalar`] selects the scalar one. Tests use it to run that path
//! on a machine that has the instructions.

use crate::block::rounds;
use crate::ubc_check::RecompressFrom;
use crate::{BLOCK_SIZE, Schedule};

#[cfg(target_arch = "aarch64")]
mod armv8;
mod scalar;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod sha_ni;

/// Whether this CPU has the SHA-1 instructions the hardware backend needs.
///
/// A `std` build asks the OS at run time. Run-time detection needs `cpuid` or
/// a system call, so a `no_std` build uses `target_feature` only. Such a build
/// needs the features on the command line, for example
/// `-C target-feature=+sha2`.
#[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
fn has_sha1_instructions() -> bool {
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        std::arch::is_x86_feature_detected!("sha")
            && std::arch::is_x86_feature_detected!("sse2")
            && std::arch::is_x86_feature_detected!("ssse3")
            && std::arch::is_x86_feature_detected!("sse4.1")
    }
    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    {
        std::arch::is_aarch64_feature_detected!("sha2")
    }
    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        cfg!(all(
            target_feature = "sha",
            target_feature = "sse2",
            target_feature = "ssse3",
            target_feature = "sse4.1"
        ))
    }
    #[cfg(all(not(feature = "std"), target_arch = "aarch64"))]
    {
        cfg!(target_feature = "sha2")
    }
}

#[derive(Clone, Copy)]
enum Repr {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    ShaNi,
    #[cfg(target_arch = "aarch64")]
    Armv8,
    Scalar,
}

/// The implementation that computes the digest of a block. [`Backend::new`]
/// selects it once, from CPU feature detection.
#[derive(Clone, Copy)]
pub(crate) struct Backend(Repr);

impl Backend {
    pub(crate) fn new() -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
        if has_sha1_instructions() {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            return Self(Repr::ShaNi);
            #[cfg(target_arch = "aarch64")]
            return Self(Repr::Armv8);
        }

        Self(Repr::Scalar)
    }

    /// The scalar implementation, on every CPU.
    pub(crate) fn scalar() -> Self {
        Self(Repr::Scalar)
    }

    /// Digests `block` into `state` and writes the input of the checker to
    /// `m1`.
    ///
    /// A hardware backend leaves its own states at steps 60 and 64 in
    /// `state_58` and `state_65`. Call [`ensure_states`](Self::ensure_states)
    /// before you read them.
    #[inline]
    pub(crate) fn compress_spill(
        &self,
        state: &mut [u32; 5],
        block: &[u8; BLOCK_SIZE],
        m1: &mut Schedule,
        state_58: &mut [u32; 5],
        state_65: &mut [u32; 5],
    ) {
        match self.0 {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Repr::ShaNi => {
                // SAFETY: `Repr::ShaNi` means the features were checked.
                unsafe { sha_ni::compress_spill(state, block, m1, state_58, state_65) };
            }
            #[cfg(target_arch = "aarch64")]
            Repr::Armv8 => {
                // SAFETY: `Repr::Armv8` means the features were checked.
                unsafe { armv8::compress_spill(state, block, m1, state_58, state_65) };
            }
            // Only this arm decodes the block. The other two byte-swap in
            // their own loads, and the buffer stays here rather than moving
            // into `scalar` so that it is hoisted out of the loop over blocks
            // instead of being built again for each one. Moving it measures
            // 4% of the scalar backend.
            Repr::Scalar => {
                let mut block_u32 = [0u32; BLOCK_SIZE / 4];
                read_block(block, &mut block_u32);
                scalar::compress_spill(state, &block_u32, m1, state_58, state_65);
            }
        }
    }

    /// Updates the stored states this block's candidates will recompress
    /// from.
    ///
    /// The scalar backend stores them on the way past. A hardware backend
    /// works four steps at a time, so it stores its states at steps 60 and 64
    /// instead, and this takes them the rest of the way. It runs only for a
    /// flagged block. `need_58` says whether any candidate recompresses from
    /// step 58.
    pub(crate) fn ensure_states(
        &self,
        m1: &Schedule,
        need_58: bool,
        state_58: &mut [u32; 5],
        state_65: &mut [u32; 5],
    ) {
        match self.0 {
            // The compression stored them itself.
            Repr::Scalar => {
                let _ = (m1, need_58, state_58, state_65);
            }
            // As `sha1rnds4` holds them: reversed, with E + W on the end.
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Repr::ShaNi => {
                for (s, t) in [(&mut *state_58, 60), (&mut *state_65, 64)] {
                    *s = [s[3], s[2], s[1], s[0], s[4].wrapping_sub(m1[t])];
                }
                rounds::states_from_60_64(m1, need_58, state_58, state_65);
            }
            #[cfg(target_arch = "aarch64")]
            Repr::Armv8 => rounds::states_from_60_64(m1, need_58, state_58, state_65),
        }
    }

    /// Whether this candidate really is the attack it is a candidate for.
    ///
    /// A backend with SHA-1 instructions asks it forwards, from the chaining
    /// value an attack would have had to start from, since forwards is the
    /// only direction they go. One without keeps to the way back, cheaper in
    /// steps.
    ///
    /// Forwards runs all eighty rounds where the check reaches only step 58
    /// or 65. The steps between are a bijection, so the two are the same
    /// test, and the whole way needs no state taken out of the middle.
    #[inline]
    pub(crate) fn is_attack(
        &self,
        step: RecompressFrom,
        m1: &Schedule,
        dm: &[u32; 80],
        state: &[u32; 5],
        chaining_out: &[u32; 5],
    ) -> bool {
        match self.0 {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: `Repr::ShaNi` is chosen only where the instructions are.
            Repr::ShaNi => unsafe { sha_ni::recompress(step, m1, dm, state, chaining_out) },
            #[cfg(target_arch = "aarch64")]
            // SAFETY: `Repr::Armv8` is chosen only where the instructions are.
            Repr::Armv8 => unsafe { armv8::recompress(step, m1, dm, state, chaining_out) },
            Repr::Scalar => {
                let (mut ihv2, mut ends_on) = ([0u32; 5], [0u32; 5]);
                rounds::recompression_step(step, &mut ihv2, &mut ends_on, m1, dm, state);
                super::xor(&ends_on, chaining_out) == 0
            }
        }
    }
}

/// Reads a block as big-endian `u32` words.
#[inline(always)]
pub(crate) fn read_block(block: &[u8; BLOCK_SIZE], out: &mut [u32; BLOCK_SIZE / 4]) {
    for (o, chunk) in out.iter_mut().zip(block.chunks_exact(4)) {
        *o = u32::from_be_bytes(chunk.try_into().unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_hardware(backend: &Backend) -> bool {
        match backend.0 {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Repr::ShaNi => true,
            #[cfg(target_arch = "aarch64")]
            Repr::Armv8 => true,
            Repr::Scalar => false,
        }
    }

    #[cfg(feature = "std")]
    fn name(backend: &Backend) -> &'static str {
        match backend.0 {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Repr::ShaNi => "sha-ni",
            #[cfg(target_arch = "aarch64")]
            Repr::Armv8 => "armv8",
            Repr::Scalar => "scalar",
        }
    }

    /// `SHA1DC_EXPECT_BLOCK` lets a job say which implementation it is there
    /// to cover. A runner without the SHA-1 instructions falls back to the
    /// scalar one and looks no different, so without this `sha_ni` can go a
    /// whole release without running.
    #[cfg(feature = "std")]
    #[test]
    fn the_expected_implementation_was_selected() {
        let expected = std::env::var("SHA1DC_EXPECT_BLOCK").unwrap_or_default();
        let expected = expected.trim();
        if expected.is_empty() {
            return; // a job that does not pin one leaves it empty
        }
        assert_eq!(
            name(&Backend::new()),
            expected,
            "this job was meant to exercise a different implementation of `block`"
        );
    }

    /// Detects a silent fallback to scalar. If the compiler knows that the
    /// instructions are present, detection must agree.
    #[test]
    fn hardware_is_used_when_the_target_guarantees_it() {
        let guaranteed = cfg!(any(
            all(target_arch = "aarch64", target_feature = "sha2"),
            all(
                any(target_arch = "x86", target_arch = "x86_64"),
                target_feature = "sha",
                target_feature = "sse2",
                target_feature = "ssse3",
                target_feature = "sse4.1"
            )
        ));
        if guaranteed {
            assert!(
                is_hardware(&Backend::new()),
                "cpu feature detection missed instructions the target guarantees"
            );
        }
    }

    /// Property tests. These look for a disagreement anywhere and shrink a
    /// failure to a small case.
    ///
    /// `quickcheck` needs `std`, so a `no_std` build skips them.
    #[cfg(feature = "std")]
    mod properties {
        use super::*;
        use quickcheck::QuickCheck;

        /// A backend must accept exactly the chaining value the way back
        /// finds, and no other.
        ///
        /// Accepting the output the way back arrived at takes computing that
        /// value exactly. Rejecting anything else is what makes it mean
        /// something: a form that always accepted would pass on its own.
        #[test]
        fn a_backend_accepts_only_what_the_way_back_finds() {
            fn prop(words: [u32; 80], state: [u32; 5], nudge: u8) -> bool {
                let backend = Backend::new();
                let m1 = Schedule::from_words(words);
                crate::ubc_check::SHA1_DVS.iter().all(|dv| {
                    let (mut ihvin, mut ihvout) = ([0u32; 5], [0u32; 5]);
                    rounds::recompression_step(
                        dv.recompress_from,
                        &mut ihvin,
                        &mut ihvout,
                        &m1,
                        &dv.dm,
                        &state,
                    );

                    let accepts =
                        backend.is_attack(dv.recompress_from, &m1, &dv.dm, &state, &ihvout);

                    // Any other output belongs to some other chaining value.
                    let mut wrong = ihvout;
                    wrong[nudge as usize % 5] ^= 1 << u32::from(nudge % 32);
                    let rejects =
                        !backend.is_attack(dv.recompress_from, &m1, &dv.dm, &state, &wrong);

                    accepts && rejects
                })
            }
            QuickCheck::new()
                .tests(200)
                .quickcheck(prop as fn([u32; 80], [u32; 5], u8) -> bool);
        }

        /// The hardware backend and the scalar one must produce the same
        /// digest state and the same schedule for any block and any starting
        /// state, not only for a real message.
        #[test]
        fn hardware_agrees_with_scalar_on_any_block() {
            if !is_hardware(&Backend::new()) {
                return;
            }

            fn prop(block: [u8; BLOCK_SIZE], ihv: [u32; 5]) -> bool {
                let (mut hw_state, mut hw_w) = (ihv, Schedule::zeroed());
                let (mut hw_s58, mut hw_s65) = ([0u32; 5], [0u32; 5]);
                Backend::new().compress_spill(
                    &mut hw_state,
                    &block,
                    &mut hw_w,
                    &mut hw_s58,
                    &mut hw_s65,
                );

                let (mut sc_state, mut sc_w) = (ihv, Schedule::zeroed());
                let (mut s58, mut s65) = ([0u32; 5], [0u32; 5]);
                Backend(Repr::Scalar).compress_spill(
                    &mut sc_state,
                    &block,
                    &mut sc_w,
                    &mut s58,
                    &mut s65,
                );

                let mut m = [0u32; 16];
                read_block(&block, &mut m);
                let want = Schedule::expand(&m);

                Backend::new().ensure_states(&hw_w, true, &mut hw_s58, &mut hw_s65);

                hw_state == sc_state
                    && hw_w.words() == sc_w.words()
                    && hw_w.words() == want.words()
                    && hw_s58 == s58
                    && hw_s65 == s65
            }

            QuickCheck::new()
                .tests(1_000)
                .quickcheck(prop as fn([u8; BLOCK_SIZE], [u32; 5]) -> bool);
        }

        /// The stored states must match a plain run of the compression, both
        /// the scalar backend's and those a hardware backend's are finished
        /// into, for any block and starting state.
        #[test]
        fn state_recovery_agrees_on_any_block() {
            fn prop(m: [u32; 16], ihv: [u32; 5]) -> bool {
                let mut replayed = ihv;
                let (mut w, mut want_58, mut want_65) = (Schedule::zeroed(), [0u32; 5], [0u32; 5]);
                scalar::compress_spill(&mut replayed, &m, &mut w, &mut want_58, &mut want_65);

                // What a hardware backend stores, finished the way it is
                // finished there. Written out here so that every target runs
                // it, not only one with the instructions.
                let at = |t| rounds::state_at(&ihv, &w, t);
                let (mut from_60, mut from_64) = (at(60), at(64));
                rounds::states_from_60_64(&w, true, &mut from_60, &mut from_64);

                // Asking for step 65 alone must leave step 58 untouched and
                // still give step 65.
                let (mut skipped_58, mut only_65) = ([0xDEAD_BEEFu32; 5], at(64));
                rounds::states_from_60_64(&w, false, &mut skipped_58, &mut only_65);

                at(58) == want_58
                    && at(65) == want_65
                    && from_60 == want_58
                    && from_64 == want_65
                    && only_65 == want_65
                    && skipped_58 == [0xDEAD_BEEF; 5]
            }

            QuickCheck::new()
                .tests(1_000)
                .quickcheck(prop as fn([u32; 16], [u32; 5]) -> bool);
        }
    }
}
