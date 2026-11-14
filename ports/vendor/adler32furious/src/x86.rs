//! x86 Adler-32: SSE2 and AVX2, and VNNI dot-product versions for AVX-VNNI
//! and AVX-512 (256- and 512-bit vectors).

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

use super::scalar::{self, chunk};
use super::{Adler32Fn, DIVISOR, MAX_CHUNK_LEN};

/// Implementations usable on this CPU, best first.
#[inline]
pub(super) fn available(push: &mut impl FnMut((&'static str, Adler32Fn))) {
    let f = features();
    if f.avx512 && f.zmm {
        push(("avx512_vl512_vnni", avx512_vl512_vnni));
    }
    if f.avx512 {
        push(("avx512_vl256_vnni", avx512_vl256_vnni));
    }
    if f.avx2 && f.avxvnni {
        push(("avx2_vnni", avx2_vnni));
    }
    if f.avx2 {
        push(("avx2", avx2));
    }
    if f.sse2 {
        push(("sse2", sse2));
    }
}

struct Features {
    sse2: bool,
    avx2: bool,
    avxvnni: bool,
    /// AVX512BW + AVX512VL + AVX512VNNI.
    avx512: bool,
    /// Whether 512-bit vectors are worth using (see `allow_512bit_vectors`).
    zmm: bool,
}

#[cfg(feature = "std")]
#[inline]
fn features() -> Features {
    // Never use AVX-512 on macOS: older Darwin kernels could corrupt the
    // opmask registers.
    let avx512 = !cfg!(target_os = "macos")
        && std::is_x86_feature_detected!("avx512bw")
        && std::is_x86_feature_detected!("avx512vl")
        && std::is_x86_feature_detected!("avx512vnni");
    Features {
        sse2: std::is_x86_feature_detected!("sse2"),
        avx2: std::is_x86_feature_detected!("avx2"),
        avxvnni: std::is_x86_feature_detected!("avxvnni"),
        avx512,
        zmm: avx512 && allow_512bit_vectors(),
    }
}

#[cfg(not(feature = "std"))]
#[inline]
fn features() -> Features {
    let avx512 = cfg!(all(
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vnni"
    ));
    Features {
        sse2: cfg!(target_feature = "sse2"),
        avx2: cfg!(target_feature = "avx2"),
        avxvnni: cfg!(target_feature = "avxvnni"),
        avx512,
        zmm: avx512 && allow_512bit_vectors(),
    }
}

/// Don't use 512-bit vectors on Intel CPUs before Rocket Lake and Sapphire
/// Rapids: they downclock too eagerly, which hurts workloads that only use
/// ZMM registers occasionally.
///
/// Cached, unlike the other checks here: it runs `cpuid`, which is slow (and
/// traps to the hypervisor in VMs), while std caches its feature detection
/// itself.
#[inline]
fn allow_512bit_vectors() -> bool {
    use core::sync::atomic::{AtomicU8, Ordering};
    const UNKNOWN: u8 = 0;
    const NO: u8 = 1;
    const YES: u8 = 2;
    static CACHE: AtomicU8 = AtomicU8::new(UNKNOWN);
    match CACHE.load(Ordering::Relaxed) {
        NO => false,
        YES => true,
        _ => {
            let allow = detect_512bit_vectors();
            CACHE.store(if allow { YES } else { NO }, Ordering::Relaxed);
            allow
        }
    }
}

#[cold]
fn detect_512bit_vectors() -> bool {
    // SGX enclaves cannot run `cpuid`.
    if cfg!(target_env = "sgx") {
        return false;
    }
    // SAFETY: only called once AVX-512 is known to be present, so the CPU
    // supports the `cpuid` instruction.
    #[allow(unused_unsafe)]
    let (vendor, version) = unsafe { (__cpuid(0), __cpuid(1)) };
    let intel = vendor.ebx == u32::from_le_bytes(*b"Genu")
        && vendor.edx == u32::from_le_bytes(*b"ineI")
        && vendor.ecx == u32::from_le_bytes(*b"ntel");
    if !intel {
        return true;
    }
    let family = (version.eax >> 8) & 0xF;
    let model = ((version.eax >> 4) & 0xF) | ((version.eax >> 12) & 0xF0);
    if family != 6 {
        return true;
    }
    // Skylake-SP/Cascade Lake/Cooper Lake, Ice Lake server and client,
    // Tiger Lake.
    !matches!(model, 85 | 106 | 108 | 126 | 140 | 141)
}

/// Sums the 32-bit lanes of `v_s1` into `s1` and of `v_s2` into `s2`.
///
/// When `v_s1` came from `psadbw`, its odd lanes are zero and are skipped.
#[inline]
#[target_feature(enable = "sse2")]
fn reduce128(v_s1: __m128i, v_s2: __m128i, s1_odd_lanes: bool, s1: &mut u32, s2: &mut u32) {
    let mut v_s1 = v_s1;
    if s1_odd_lanes {
        v_s1 = _mm_add_epi32(v_s1, _mm_shuffle_epi32::<0x31>(v_s1));
    }
    let v_s2 = _mm_add_epi32(v_s2, _mm_shuffle_epi32::<0x31>(v_s2));
    let v_s1 = _mm_add_epi32(v_s1, _mm_shuffle_epi32::<0x02>(v_s1));
    let v_s2 = _mm_add_epi32(v_s2, _mm_shuffle_epi32::<0x02>(v_s2));
    *s1 = s1.wrapping_add(_mm_cvtsi128_si32(v_s1) as u32);
    *s2 = s2.wrapping_add(_mm_cvtsi128_si32(v_s2) as u32);
}

#[inline]
#[target_feature(enable = "avx2")]
fn reduce256(v_s1: __m256i, v_s2: __m256i, s1_odd_lanes: bool, s1: &mut u32, s2: &mut u32) {
    let (v_s1, v_s2) = narrow256(v_s1, v_s2);
    reduce128(v_s1, v_s2, s1_odd_lanes, s1, s2);
}

/// Adds the halves of each of `v_s1` and `v_s2`.
#[inline]
#[target_feature(enable = "avx2")]
fn narrow256(v_s1: __m256i, v_s2: __m256i) -> (__m128i, __m128i) {
    let half = |v| _mm_add_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
    (half(v_s1), half(v_s2))
}

#[inline]
#[target_feature(enable = "sse2")]
fn narrow128(v_s1: __m128i, v_s2: __m128i) -> (__m128i, __m128i) {
    (v_s1, v_s2)
}

#[inline]
#[target_feature(enable = "sse2")]
fn load128(p: &[u8]) -> __m128i {
    let p: &[u8; 16] = p[..16].try_into().unwrap();
    // SAFETY: p is 16 bytes.
    unsafe { _mm_loadu_si128(p.as_ptr().cast()) }
}

/// A mask for the last `n` of `N` bytes, for a vector loaded so that it ends
/// with the `n` bytes wanted.
#[inline(always)]
fn tail_mask<const N: usize>(n: usize) -> &'static [u8; N] {
    static MASKS: [u8; 64] = {
        let mut m = [0u8; 64];
        let mut i = 32;
        while i < 64 {
            m[i] = 0xFF;
            i += 1;
        }
        m
    };
    MASKS[32 - N + n..][..N].try_into().unwrap()
}

/// The bytes of `data`, weighted 16 down to 1, summed into 32-bit lanes.
#[inline]
#[target_feature(enable = "sse2")]
fn weighted128(data: __m128i) -> __m128i {
    static MULTS: [[u16; 8]; 2] = [[16, 15, 14, 13, 12, 11, 10, 9], [8, 7, 6, 5, 4, 3, 2, 1]];
    let zeroes = _mm_setzero_si128();
    let [lo, hi] = MULTS.each_ref().map(|m| {
        // SAFETY: m is 8 u16s.
        unsafe { _mm_loadu_si128(m.as_ptr().cast()) }
    });
    _mm_add_epi32(
        _mm_madd_epi16(_mm_unpacklo_epi8(data, zeroes), lo),
        _mm_madd_epi16(_mm_unpackhi_epi8(data, zeroes), hi),
    )
}

/// Adds `c`, the last bytes of `whole`, to `v_s1` and `v_s2` 16 at a time.
///
/// `whole` must be at least 16 bytes: the last partial vector is its last 16
/// bytes, with those before `c` masked off. `v_s1` must have only even lanes
/// set, as from `psadbw`. The caller adds `s1 * c.len()` to `s2`.
#[inline]
#[target_feature(enable = "sse2")]
fn tail128(whole: &[u8], mut c: &[u8], v_s1: &mut __m128i, v_s2: &mut __m128i) {
    let zeroes = _mm_setzero_si128();
    while c.len() >= 16 {
        let data = load128(c);
        *v_s2 = _mm_add_epi32(*v_s2, _mm_slli_epi32::<4>(*v_s1));
        *v_s1 = _mm_add_epi32(*v_s1, _mm_sad_epu8(data, zeroes));
        *v_s2 = _mm_add_epi32(*v_s2, weighted128(data));
        c = &c[16..];
    }
    if !c.is_empty() {
        let n = c.len();
        let last = load128(&whole[whole.len() - 16..]);
        let data = _mm_and_si128(last, load128(tail_mask::<16>(n)));
        // With only even lanes set, a 32x32->64-bit multiply of those covers
        // all of v_s1, and the products fit in 32 bits.
        *v_s2 = _mm_add_epi32(*v_s2, _mm_mul_epu32(*v_s1, _mm_set1_epi32(n as i32)));
        *v_s1 = _mm_add_epi32(*v_s1, _mm_sad_epu8(data, zeroes));
        *v_s2 = _mm_add_epi32(*v_s2, weighted128(data));
    }
}

#[inline]
#[target_feature(enable = "avx512f,avx2")]
fn reduce512(v_s1: __m512i, v_s2: __m512i, s1: &mut u32, s2: &mut u32) {
    let v_s1 = _mm256_add_epi32(
        _mm512_castsi512_si256(v_s1),
        _mm512_extracti64x4_epi64::<1>(v_s1),
    );
    let v_s2 = _mm256_add_epi32(
        _mm512_castsi512_si256(v_s2),
        _mm512_extracti64x4_epi64::<1>(v_s2),
    );
    reduce256(v_s1, v_s2, true, s1, s2);
}

/// If the input is large and misaligned, sums bytes up to a `vl`-byte
/// boundary first; smaller inputs just take the misaligned-load penalty.
#[inline(always)]
fn align<'a>(s1: &mut u32, s2: &mut u32, p: &'a [u8], vl: usize) -> &'a [u8] {
    let misalign = p.as_ptr().align_offset(vl);
    if p.len() > 65536 && misalign != 0 {
        let (head, rest) = p.split_at(misalign);
        chunk(s1, s2, head);
        rest
    } else {
        p
    }
}

/// SSE2 / AVX2: sum bytes with `psadbw`, and accumulate each byte position
/// into 16-bit counters, multiplied by their weights with `pmaddwd` once per
/// chunk. Processes `2 * VL` bytes per iteration.
macro_rules! adler32_legacy {
    (
        $name:ident, $feat:literal, $vl:literal, $mults:expr,
        $loadu:ident, $setzero:ident, $add16:ident, $add32:ident,
        $unpacklo8:ident, $unpackhi8:ident, $sad8:ident, $madd16:ident,
        $slli32:ident, $log2_2vl:literal, $narrow:ident
    ) => {
        #[target_feature(enable = $feat)]
        pub(super) fn $name(adler: u32, data: &[u8]) -> u32 {
            #[repr(align(32))]
            struct Mults([[u16; $vl / 2]; 4]);
            static MULTS: Mults = Mults($mults);
            if data.len() < 16 {
                return scalar::adler32_generic(adler, data);
            }
            let [mults_a, mults_b, mults_c, mults_d] = MULTS.0.each_ref().map(|m| {
                // SAFETY: MULTS is 4 vectors of plain data.
                unsafe { $loadu(m.as_ptr().cast()) }
            });
            let zeroes = $setzero();
            let mut s1 = adler & 0xFFFF;
            let mut s2 = adler >> 16;

            let mut p = align(&mut s1, &mut s2, data, $vl);

            // Keep s1 and s2 below 2^32, and every 16-bit byte counter at or
            // below i16::MAX since pmaddwd multiplies signed values.
            let max = (2 * $vl * (i16::MAX as usize / u8::MAX as usize)).min(MAX_CHUNK_LEN)
                & !(2 * $vl - 1);
            while !p.is_empty() {
                let (mut c, rest) = p.split_at(p.len().min(max));
                p = rest;
                if c.len() >= 16 {
                    let whole = c;
                    s2 += s1 * c.len() as u32;
                    let (mut v_s1, mut v_s2) = if c.len() >= 2 * $vl {
                        let mut v_s1 = zeroes;
                        let mut v_s1_sums = zeroes;
                        let mut v_byte_sums_a = zeroes;
                        let mut v_byte_sums_b = zeroes;
                        let mut v_byte_sums_c = zeroes;
                        let mut v_byte_sums_d = zeroes;

                        while c.len() >= 2 * $vl {
                            // SAFETY: c holds at least 2 * VL bytes.
                            let (data_a, data_b) = unsafe {
                                (
                                    $loadu(c.as_ptr().cast()),
                                    $loadu(c.as_ptr().add($vl).cast()),
                                )
                            };
                            v_s1_sums = $add32(v_s1_sums, v_s1);
                            v_byte_sums_a = $add16(v_byte_sums_a, $unpacklo8(data_a, zeroes));
                            v_byte_sums_b = $add16(v_byte_sums_b, $unpackhi8(data_a, zeroes));
                            v_byte_sums_c = $add16(v_byte_sums_c, $unpacklo8(data_b, zeroes));
                            v_byte_sums_d = $add16(v_byte_sums_d, $unpackhi8(data_b, zeroes));
                            v_s1 =
                                $add32(v_s1, $add32($sad8(data_a, zeroes), $sad8(data_b, zeroes)));
                            c = &c[2 * $vl..];
                        }

                        // v_s2 = 2*VL * v_s1_sums + [2*VL, ..., 1] * v_byte_sums
                        let v_s2 = $add32(
                            $add32(
                                $slli32::<$log2_2vl>(v_s1_sums),
                                $madd16(v_byte_sums_a, mults_a),
                            ),
                            $add32(
                                $add32(
                                    $madd16(v_byte_sums_b, mults_b),
                                    $madd16(v_byte_sums_c, mults_c),
                                ),
                                $madd16(v_byte_sums_d, mults_d),
                            ),
                        );
                        $narrow(v_s1, v_s2)
                    } else {
                        (_mm_setzero_si128(), _mm_setzero_si128())
                    };
                    // The last 0 <= n < 2*VL bytes.
                    tail128(whole, c, &mut v_s1, &mut v_s2);
                    c = &[];
                    reduce128(v_s1, v_s2, false, &mut s1, &mut s2);
                }
                chunk(&mut s1, &mut s2, c);
            }
            (s2 << 16) | s1
        }
    };
}

/// VNNI: `vpdpbusd` multiplies data bytes by `[VL, ..., 1]` for s2 and by
/// all-ones for s1, accumulating in 32-bit lanes. Processes `4 * VL` bytes
/// per iteration.
macro_rules! adler32_vnni {
    (
        $name:ident, $feat:literal, $vl:literal, $vec:ty,
        $loadu:ident, $setzero:ident, $add8:ident, $add32:ident,
        $dpbusd:ident, $set1_8:ident, $set1_32:ident, $mullo32:ident, $slli32:ident,
        [$log2_vl:literal, $log2_2vl:literal, $log2_4vl:literal],
        $(below $min_len:literal: $fallback:path,)?
        |$tail:ident, $n:ident, $upto:ident, $mults:ident, $base:ident| $tail_load:expr,
        |$v1:ident, $v2:ident, $r1:ident, $r2:ident| $reduce:expr
    ) => {
        #[target_feature(enable = $feat)]
        pub(super) fn $name(adler: u32, data: &[u8]) -> u32 {
            #[repr(align(64))]
            struct Mults([u8; $vl]);
            static MULTS: Mults = Mults({
                let mut m = [0u8; $vl];
                let mut i = 0;
                while i < $vl {
                    m[i] = ($vl - i) as u8;
                    i += 1;
                }
                m
            });
            $(
                if data.len() < $min_len {
                    return $fallback(adler, data);
                }
            )?
            let ones = $set1_8(1);
            let zeroes = $setzero();
            let mut s1 = adler & 0xFFFF;
            let mut s2 = adler >> 16;

            let mut p = align(&mut s1, &mut s2, data, $vl);
            let load = |c: &[u8], i: usize| -> $vec {
                debug_assert!(c.len() >= (i + 1) * $vl);
                // SAFETY: callers check that c holds (i + 1) * VL bytes.
                unsafe { $loadu(c.as_ptr().add(i * $vl).cast()) }
            };

            while !p.is_empty() {
                let (mut c, rest) = p.split_at(p.len().min(MAX_CHUNK_LEN & !(4 * $vl - 1)));
                p = rest;
                // SAFETY: MULTS is VL bytes.
                let mut mults: $vec = unsafe { $loadu(MULTS.0.as_ptr().cast()) };
                let mut v_s1 = zeroes;
                let mut v_s2 = zeroes;

                s2 += s1 * c.len() as u32;

                if c.len() >= 4 * $vl {
                    let mut v_s1_b = zeroes;
                    let mut v_s1_c = zeroes;
                    let mut v_s1_d = zeroes;
                    let mut v_s2_b = zeroes;
                    let mut v_s2_c = zeroes;
                    let mut v_s2_d = zeroes;
                    let mut v_s1_sums = zeroes;
                    let mut v_s1_sums_b = zeroes;
                    let mut v_s1_sums_c = zeroes;
                    let mut v_s1_sums_d = zeroes;

                    while c.len() >= 4 * $vl {
                        let data_a = load(c, 0);
                        let data_b = load(c, 1);
                        let data_c = load(c, 2);
                        let data_d = load(c, 3);

                        v_s2 = $dpbusd(v_s2, data_a, mults);
                        v_s2_b = $dpbusd(v_s2_b, data_b, mults);
                        v_s2_c = $dpbusd(v_s2_c, data_c, mults);
                        v_s2_d = $dpbusd(v_s2_d, data_d, mults);

                        v_s1_sums = $add32(v_s1_sums, v_s1);
                        v_s1_sums_b = $add32(v_s1_sums_b, v_s1_b);
                        v_s1_sums_c = $add32(v_s1_sums_c, v_s1_c);
                        v_s1_sums_d = $add32(v_s1_sums_d, v_s1_d);

                        v_s1 = $dpbusd(v_s1, data_a, ones);
                        v_s1_b = $dpbusd(v_s1_b, data_b, ones);
                        v_s1_c = $dpbusd(v_s1_c, data_c, ones);
                        v_s1_d = $dpbusd(v_s1_d, data_d, ones);

                        c = &c[4 * $vl..];
                    }

                    // v_s2 = v_s2 + v_s2_b + v_s2_c + v_s2_d
                    //        + 4*VL * (all v_s1_sums)
                    //        + 3*VL * v_s1 + 2*VL * v_s1_b + VL * v_s1_c
                    // v_s1 = v_s1 + v_s1_b + v_s1_c + v_s1_d
                    let tmp0 = $add32(v_s1, v_s1_b);
                    let tmp1 = $add32(v_s1, v_s1_c);
                    let sums = $add32(
                        $add32(v_s1_sums, v_s1_sums_b),
                        $add32(v_s1_sums_c, v_s1_sums_d),
                    );
                    v_s1 = $add32($add32(tmp0, v_s1_c), v_s1_d);
                    v_s2 = $add32(
                        $add32(
                            $add32($slli32::<$log2_4vl>(sums), $slli32::<$log2_2vl>(tmp0)),
                            $add32($slli32::<$log2_vl>(tmp1), v_s2),
                        ),
                        $add32($add32(v_s2_b, v_s2_c), v_s2_d),
                    );
                }

                // The last 0 <= n < 4*VL bytes of the chunk.
                if c.len() >= 2 * $vl {
                    let data_a = load(c, 0);
                    let data_b = load(c, 1);
                    v_s2 = $add32(v_s2, $slli32::<$log2_2vl>(v_s1));
                    v_s1 = $dpbusd(v_s1, data_a, ones);
                    v_s1 = $dpbusd(v_s1, data_b, ones);
                    v_s2 = $dpbusd(v_s2, data_a, $set1_8($vl));
                    v_s2 = $dpbusd(v_s2, data_a, mults);
                    v_s2 = $dpbusd(v_s2, data_b, mults);
                    c = &c[2 * $vl..];
                }
                if !c.is_empty() {
                    // The last 0 < n < 2*VL bytes. Shift the weights so the
                    // last byte gets weight 1.
                    let n = c.len();
                    v_s2 = $add32(v_s2, $mullo32(v_s1, $set1_32(n as i32)));
                    mults = $add8(mults, $set1_8(n as i8 - $vl as i8));
                    if n > $vl {
                        let data = load(c, 0);
                        v_s1 = $dpbusd(v_s1, data, ones);
                        v_s2 = $dpbusd(v_s2, data, mults);
                        c = &c[$vl..];
                        mults = $add8(mults, $set1_8(-($vl as i8)));
                    }
                    // The last 0 < n <= VL bytes, and their weights.
                    let (data, mults) = {
                        let $tail = c;
                        let $n = c.len();
                        // The input up to the end of this chunk.
                        let $upto = &data[..data.len() - p.len()];
                        let $mults = mults;
                        // SAFETY: MULTS is VL bytes.
                        let $base: $vec = unsafe { $loadu(MULTS.0.as_ptr().cast()) };
                        $tail_load
                    };
                    v_s1 = $dpbusd(v_s1, data, ones);
                    v_s2 = $dpbusd(v_s2, data, mults);
                }

                {
                    let ($v1, $v2, $r1, $r2) = (v_s1, v_s2, &mut s1, &mut s2);
                    $reduce;
                }
                s1 %= DIVISOR;
                s2 %= DIVISOR;
            }
            (s2 << 16) | s1
        }
    };
}

mod sse2_impl {
    use super::*;
    adler32_legacy!(
        sse2,
        "sse2",
        16,
        [
            [32, 31, 30, 29, 28, 27, 26, 25],
            [24, 23, 22, 21, 20, 19, 18, 17],
            [16, 15, 14, 13, 12, 11, 10, 9],
            [8, 7, 6, 5, 4, 3, 2, 1],
        ],
        _mm_loadu_si128,
        _mm_setzero_si128,
        _mm_add_epi16,
        _mm_add_epi32,
        _mm_unpacklo_epi8,
        _mm_unpackhi_epi8,
        _mm_sad_epu8,
        _mm_madd_epi16,
        _mm_slli_epi32,
        5,
        narrow128
    );
}
use sse2_impl::sse2;

#[rustfmt::skip]
mod avx2_impl {
    use super::*;
    // The odd ordering matches how vpunpck{l,h}bw work on each 128-bit lane.
    adler32_legacy!(
        avx2,
        "avx2",
        32,
        [
            [64, 63, 62, 61, 60, 59, 58, 57, 48, 47, 46, 45, 44, 43, 42, 41],
            [56, 55, 54, 53, 52, 51, 50, 49, 40, 39, 38, 37, 36, 35, 34, 33],
            [32, 31, 30, 29, 28, 27, 26, 25, 16, 15, 14, 13, 12, 11, 10, 9],
            [24, 23, 22, 21, 20, 19, 18, 17, 8, 7, 6, 5, 4, 3, 2, 1],
        ],
        _mm256_loadu_si256,
        _mm256_setzero_si256,
        _mm256_add_epi16,
        _mm256_add_epi32,
        _mm256_unpacklo_epi8,
        _mm256_unpackhi_epi8,
        _mm256_sad_epu8,
        _mm256_madd_epi16,
        _mm256_slli_epi32,
        6,
        narrow256
    );
}
use avx2_impl::avx2;

mod avx2_vnni_impl {
    use super::*;
    // Used on CPUs with AVX-VNNI but no AVX-512, e.g. Alder Lake.
    adler32_vnni!(
        avx2_vnni,
        "avx2,avxvnni",
        32,
        __m256i,
        _mm256_loadu_si256,
        _mm256_setzero_si256,
        _mm256_add_epi8,
        _mm256_add_epi32,
        _mm256_dpbusd_avx_epi32,
        _mm256_set1_epi8,
        _mm256_set1_epi32,
        _mm256_mullo_epi32,
        _mm256_slli_epi32,
        [5, 6, 7],
        // Short inputs are faster without VNNI. This also leaves 32 bytes
        // before the end of every chunk for the last vector.
        below 32: avx2,
        // There is no masked load: load the 32 bytes that end with the last
        // n, and zero the others. Their weights are then the unshifted ones.
        |_c, n, upto, _mults, base| {
            let last: &[u8; 32] = upto[upto.len() - 32..].try_into().unwrap();
            // SAFETY: last and the mask are 32 bytes.
            let (last, mask) = unsafe {
                (
                    _mm256_loadu_si256(last.as_ptr().cast()),
                    _mm256_loadu_si256(tail_mask::<32>(n).as_ptr().cast()),
                )
            };
            (_mm256_and_si256(last, mask), base)
        },
        |v1, v2, s1, s2| reduce256(v1, v2, true, s1, s2)
    );
}
use avx2_vnni_impl::avx2_vnni;

mod avx512_vl256_vnni_impl {
    use super::*;
    // 256-bit vectors with AVX-512 masking; for Ice Lake and Tiger Lake,
    // which downclock too eagerly with ZMM registers.
    adler32_vnni!(
        avx512_vl256_vnni,
        "avx512bw,avx512vl,avx512vnni,avx2",
        32,
        __m256i,
        _mm256_loadu_si256,
        _mm256_setzero_si256,
        _mm256_add_epi8,
        _mm256_add_epi32,
        _mm256_dpbusd_epi32,
        _mm256_set1_epi8,
        _mm256_set1_epi32,
        _mm256_mullo_epi32,
        _mm256_slli_epi32,
        [5, 6, 7],
        |c, n, _upto, mults, _base| {
            // SAFETY: the masked load reads only the first n bytes.
            let data = unsafe { _mm256_maskz_loadu_epi8(u32::MAX >> (32 - n), c.as_ptr().cast()) };
            (data, mults)
        },
        |v1, v2, s1, s2| reduce256(v1, v2, true, s1, s2)
    );
}
use avx512_vl256_vnni_impl::avx512_vl256_vnni;

mod avx512_vl512_vnni_impl {
    use super::*;
    adler32_vnni!(
        avx512_vl512_vnni,
        "avx512bw,avx512vnni,avx512f,avx2",
        64,
        __m512i,
        _mm512_loadu_si512,
        _mm512_setzero_si512,
        _mm512_add_epi8,
        _mm512_add_epi32,
        _mm512_dpbusd_epi32,
        _mm512_set1_epi8,
        _mm512_set1_epi32,
        _mm512_mullo_epi32,
        _mm512_slli_epi32,
        [6, 7, 8],
        |c, n, _upto, mults, _base| {
            // SAFETY: the masked load reads only the first n bytes.
            let data = unsafe { _mm512_maskz_loadu_epi8(u64::MAX >> (64 - n), c.as_ptr().cast()) };
            (data, mults)
        },
        |v1, v2, s1, s2| reduce512(v1, v2, s1, s2)
    );
}
use avx512_vl512_vnni_impl::avx512_vl512_vnni;
