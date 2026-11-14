//! Fast Adler-32 checksums (RFC 1950), as used by the zlib format.
//!
//! With SIMD implementations for x86 (SSE2, AVX2, AVX-VNNI, AVX-512 VNNI) and
//! AArch64 (NEON, NEON with dot product), and a portable fallback.
//!
//! With the `std` feature (the default), the fastest implementation the CPU
//! supports is picked at runtime, using std's cached CPU feature detection.
//! Without it, the choice is made at compile time from the enabled target
//! features, so build with e.g. `-C target-cpu=native` to get more than the
//! baseline. (The one exception is whether AVX-512 should use 512-bit
//! vectors, which depends on the CPU model and is checked once at runtime.)
//! Features enabled at compile time, such as dot product on Apple targets,
//! need no runtime check at all.
//!
//! Under Miri, only the portable implementation is used.
//!
//! ```
//! use adler32furious::{adler32, Adler32};
//!
//! assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
//!
//! // The same checksum, computed in pieces.
//! let mut a = Adler32::new();
//! a.update(b"Wiki");
//! a.update(b"pedia");
//! assert_eq!(a.checksum(), 0x11E6_0398);
//! ```

#![no_std]
#![warn(missing_docs)]

// Tests use `std` even without the feature, for heap allocations.
#[cfg(any(feature = "std", test))]
extern crate std;

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
mod arm;
mod scalar;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86;

const DIVISOR: u32 = 65521;

/// The most bytes that can be summed before `s2` could overflow a `u32`,
/// assuming both sums start at `DIVISOR - 1` and every byte is `0xFF`.
const MAX_CHUNK_LEN: usize = 5552;

/// Updates a checksum whose sums are both below `DIVISOR`.
///
/// Unsafe because most are `#[target_feature]` functions: callers must make
/// sure the CPU supports the features the function was compiled with. The
/// only source of them is `available`, which lists those the CPU supports;
/// `best` picks the first, and `select` returns it, cached where that helps.
type Adler32Fn = unsafe fn(u32, &[u8]) -> u32;

/// Computes the Adler-32 checksum of `data`.
///
/// To checksum data that arrives in pieces, use [`Adler32`].
#[inline]
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    let mut a = Adler32::new();
    a.update(data);
    a.checksum()
}

/// A running Adler-32 checksum.
///
/// Also implements [`Hasher`](core::hash::Hasher) and, with `std`,
/// [`io::Write`](std::io::Write), so it can be fed with `std::io::copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Adler32 {
    checksum: u32,
}

impl Adler32 {
    /// Starts a new checksum, of no data.
    #[must_use]
    pub const fn new() -> Self {
        Self { checksum: 1 }
    }

    /// Resumes a checksum from a value returned by [`checksum`](Self::checksum).
    ///
    /// Values that no data checksums to (with either 16-bit half at or above
    /// 65521) are reduced into range, as zlib does.
    #[must_use]
    pub const fn from_checksum(checksum: u32) -> Self {
        let s1 = (checksum & 0xFFFF) % DIVISOR;
        let s2 = (checksum >> 16) % DIVISOR;
        Self {
            checksum: (s2 << 16) | s1,
        }
    }

    /// Adds `data` to the checksum.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        // SAFETY: `select` only returns implementations from `available`.
        self.checksum = unsafe { select()(self.checksum, data) };
    }

    /// The checksum of all the data added so far.
    #[must_use]
    pub const fn checksum(&self) -> u32 {
        self.checksum
    }

    /// Extends this checksum as if the data `other` was computed over, which
    /// was `other_len` bytes long, had been added to it.
    ///
    /// This lets pieces of a buffer be checksummed independently, e.g. in
    /// parallel, and joined afterwards.
    ///
    /// ```
    /// use adler32furious::{adler32, Adler32};
    ///
    /// let (a, b) = (b"Wiki", b"pedia");
    /// let mut sum = Adler32::from_checksum(adler32(a));
    /// sum.combine(Adler32::from_checksum(adler32(b)), b.len() as u64);
    /// assert_eq!(sum.checksum(), adler32(b"Wikipedia"));
    /// ```
    pub fn combine(&mut self, other: Adler32, other_len: u64) {
        let d = u64::from(DIVISOR);
        let rem = other_len % d;
        let (s1a, s2a) = (
            u64::from(self.checksum & 0xFFFF),
            u64::from(self.checksum >> 16),
        );
        let (s1b, s2b) = (
            u64::from(other.checksum & 0xFFFF),
            u64::from(other.checksum >> 16),
        );
        // Both s1s count the initial 1, so subtract one of them. Each byte of
        // `other` also added this checksum's s1 (minus that 1) to s2.
        let s1 = (s1a + s1b + d - 1) % d;
        let s2 = (s2a + s2b + rem * s1a + d - rem) % d;
        self.checksum = ((s2 as u32) << 16) | s1 as u32;
    }

    /// Resets to the checksum of no data.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

impl core::hash::Hasher for Adler32 {
    fn finish(&self) -> u64 {
        u64::from(self.checksum)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

#[cfg(feature = "std")]
impl std::io::Write for Adler32 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.update(buf);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One of the ways this crate can compute Adler-32, such as with AVX2 or with
/// portable code. All of them give the same results.
///
/// Exposed so this crate's tests, benchmarks and fuzz target can check each
/// one. Not part of the public API: it may change in any release.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct Implementation {
    name: &'static str,
    f: Adler32Fn,
}

impl Implementation {
    /// A short name for the implementation, e.g. `"avx2"` or `"generic"`.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Adds `data` to `sum`, as [`Adler32::update`] does.
    #[inline]
    pub fn update(&self, sum: &mut Adler32, data: &[u8]) {
        // SAFETY: `Implementation`s are only made by `implementations`, from
        // `available`.
        sum.checksum = unsafe { (self.f)(sum.checksum, data) };
    }
}

impl core::fmt::Debug for Implementation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Implementation").field(&self.name).finish()
    }
}

/// Every implementation usable on this CPU, best first, ending with the
/// portable `"generic"` one. The first is the one [`Adler32`] uses.
///
/// Not part of the public API: it may change in any release.
#[doc(hidden)]
pub fn implementations() -> impl Iterator<Item = Implementation> {
    let mut out: [Option<Implementation>; 8] = [None; 8];
    let mut i = 0;
    available(&mut |(name, f)| {
        out[i] = Some(Implementation { name, f });
        i += 1;
    });
    out.into_iter().flatten()
}

/// Every implementation usable on this CPU, best first, ending with the
/// generic one. The only place that hands out `Adler32Fn`s.
#[inline]
fn available(push: &mut impl FnMut((&'static str, Adler32Fn))) {
    // Miri cannot run SIMD code (the dot-product version uses inline asm).
    if !cfg!(miri) {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        x86::available(push);
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        arm::available(push);
    }
    push(("generic", scalar::adler32_generic));
}

/// The best implementation for this CPU, cached.
///
/// On x86 with std, choosing takes several feature checks, which cost a
/// couple of nanoseconds per call even with std's cache, so the choice is
/// cached and each call is a single load.
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
#[inline]
fn select() -> Adler32Fn {
    use core::sync::atomic::{AtomicPtr, Ordering};
    static BEST: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

    #[cold]
    fn init() -> Adler32Fn {
        let f = best();
        BEST.store(f as *mut (), Ordering::Relaxed);
        f
    }

    let f = BEST.load(Ordering::Relaxed);
    if f.is_null() {
        init()
    } else {
        // SAFETY: `init` only stores `Adler32Fn`s.
        unsafe { core::mem::transmute::<*mut (), Adler32Fn>(f) }
    }
}

/// The best implementation for this CPU.
///
/// Cheap enough to call for every update without caching: here the checks
/// are compile-time `cfg!`s, which fold to a direct call, a single cached std
/// check, or cached themselves (`allow_512bit_vectors`).
#[cfg(not(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64"))))]
#[inline]
fn select() -> Adler32Fn {
    best()
}

#[inline]
fn best() -> Adler32Fn {
    let mut best = None;
    available(&mut |(_, f)| {
        best.get_or_insert(f);
    });
    best.unwrap_or(scalar::adler32_generic)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The textbook definition, one byte at a time.
    fn reference(adler: u32, data: &[u8]) -> u32 {
        let (mut s1, mut s2) = (adler & 0xFFFF, adler >> 16);
        for &b in data {
            s1 = (s1 + u32::from(b)) % DIVISOR;
            s2 = (s2 + s1) % DIVISOR;
        }
        (s2 << 16) | s1
    }

    /// Runs one implementation from a raw checksum.
    fn run(imp: Implementation, adler: u32, data: &[u8]) -> u32 {
        let mut sum = Adler32::from_checksum(adler);
        assert_eq!(sum.checksum(), adler, "test checksums must be in range");
        imp.update(&mut sum, data);
        sum.checksum()
    }

    fn pattern(len: usize, seed: u32) -> [u8; 70_000] {
        let mut buf = [0u8; 70_000];
        let mut x = seed.wrapping_mul(2_654_435_761) | 1;
        for b in &mut buf[..len] {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        buf
    }

    #[test]
    fn known_values() {
        for imp in implementations() {
            assert_eq!(run(imp, 1, b""), 1, "{}", imp.name());
            assert_eq!(run(imp, 1, b"Wikipedia"), 0x11E6_0398, "{}", imp.name());
        }
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    #[cfg_attr(miri, ignore = "slow, and Miri only runs the generic version")]
    fn all_lengths_and_offsets() {
        let buf = pattern(70_000, 7);
        let lens = (0..=300).chain([
            1000, 4095, 4096, 4097, 5503, 5504, 5552, 5553, 11_000, 65_537, 69_000,
        ]);
        for len in lens {
            for offset in [0, 1, 3, 17, 63] {
                let data = &buf[offset..offset + len];
                let want = reference(0x1234_5678, data);
                for imp in implementations() {
                    assert_eq!(
                        run(imp, 0x1234_5678, data),
                        want,
                        "{} len={len} offset={offset}",
                        imp.name()
                    );
                }
            }
        }
    }

    /// Each input ends exactly where its allocation does, so that reading
    /// past it is caught by AddressSanitizer, which CI runs these tests under.
    #[test]
    #[cfg_attr(miri, ignore = "slow, and the generic version has no unsafe code")]
    fn reads_stay_in_bounds() {
        let buf = pattern(70_000, 5);
        for len in (0..=300).chain([1000, 5553, 65_537]) {
            // Misalign the start too.
            for offset in [0, 1, 31] {
                let alloc = buf[..offset + len].to_vec();
                let data = &alloc[offset..];
                let want = reference(1, data);
                for imp in implementations() {
                    assert_eq!(run(imp, 1, data), want, "{} len={len}", imp.name());
                }
            }
        }
    }

    #[test]
    fn worst_case_bytes_do_not_overflow() {
        let data = [0xFFu8; 3 * MAX_CHUNK_LEN + 70];
        let start = ((DIVISOR - 1) << 16) | (DIVISOR - 1);
        let want = reference(start, &data);
        for imp in implementations() {
            assert_eq!(run(imp, start, &data), want, "{}", imp.name());
        }
    }

    #[test]
    fn incremental_matches_one_shot() {
        let buf = pattern(20_000, 3);
        let want = adler32(&buf[..20_000]);
        for step in [1, 7, 64, 5552, 9999] {
            let mut a = Adler32::new();
            for piece in buf[..20_000].chunks(step) {
                a.update(piece);
            }
            assert_eq!(a.checksum(), want, "step={step}");
            let resumed = Adler32::from_checksum(a.checksum());
            assert_eq!(resumed, a);
        }
    }

    #[test]
    fn from_checksum_reduces_out_of_range_values() {
        let a = Adler32::from_checksum(0xFFFF_FFFF);
        assert_eq!(a.checksum(), 0x000E_000E);
    }

    #[test]
    #[cfg_attr(miri, ignore = "slow, and Miri only runs the generic version")]
    fn combine_matches_concatenation() {
        let buf = pattern(70_000, 11);
        for split in [0, 1, 100, 5552, 65_520, 65_521, 65_522, 69_999, 70_000] {
            let (a, b) = buf.split_at(split);
            let mut sum = Adler32::from_checksum(adler32(a));
            sum.combine(Adler32::from_checksum(adler32(b)), b.len() as u64);
            assert_eq!(sum.checksum(), adler32(&buf), "split={split}");
        }
    }

    #[test]
    fn combine_with_lengths_beyond_u32() {
        // 0xFF repeated `len` times, in closed form: s1 = 1 + 255 len and
        // s2 = len + 255 len (len + 1) / 2.
        let ones = |len: u64| {
            let d = u64::from(DIVISOR);
            let s1 = (1 + 255 * (len % d)) % d;
            let s2 = (len + 255 * ((len as u128 * (len as u128 + 1) / 2) % d as u128) as u64) % d;
            Adler32::from_checksum(((s2 as u32) << 16) | s1 as u32)
        };
        for (la, lb) in [(3, 5), (1 << 33, 7), (12_345, (1 << 40) + 3)] {
            let mut sum = ones(la);
            sum.combine(ones(lb), lb);
            assert_eq!(sum, ones(la + lb), "{la} + {lb}");
        }
    }

    #[test]
    fn hasher() {
        use core::hash::Hasher;
        let mut h = Adler32::default();
        h.write(b"Wikipedia");
        assert_eq!(h.finish(), 0x11E6_0398);
        h.reset();
        assert_eq!(h.finish(), 1);
    }
}
