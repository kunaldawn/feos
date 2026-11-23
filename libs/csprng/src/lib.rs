//! The kernel's random number generator (stage 1e.1 of `docs/feos-abi-v2.md`, `GETRANDOM`).
//!
//! Output is ChaCha20 (RFC 8439) keyed by a 256-bit key, with **fast key erasure**: each
//! request first runs one block under the generator's key, keeps half of it as the
//! generator's next key and hands the other half to the request as the key of its own
//! [`Stream`]. The old key is gone before any output is made, so whoever captures the
//! generator's state later can't run it backwards to what earlier requests were given,
//! and the request's bytes are made outside whatever lock guards the generator.
//!
//! Input goes into a BLAKE2s (RFC 7693) pool, never straight into the key. Input from a
//! trusted source is credited with the bits it claims ([`Csprng::add_trusted`]); anything
//! else ([`Csprng::mix`]: interrupt timings, counters) is mixed in and never counted. The
//! pool is folded into the key (the key hashed with it) when the credit first reaches
//! [`SEED_BITS`], which is what seeds the generator, and then on the reseed schedule:
//! every [`RESEED_NS`] of time or [`RESEED_BYTES`] of output, whichever comes first.
//!
//! Before seeding, credited input stays in the pool until there's all of it: folding in a
//! few bits at a time would let someone watching the output guess each few in turn. Input
//! that was never counted is folded at the next request while there's no credited input
//! waiting, so output before seeding (`GRND_INSECURE`) differs from boot to boot by as
//! much as the timings do.
//!
//! Nothing allocates, and key material is wiped (with volatile writes) where it's let go.

#![cfg_attr(not(test), no_std)]

/// Trusted bits it takes to seed the generator.
pub const SEED_BITS: u32 = 256;

/// Most time between reseeds once seeded, in nanoseconds: five minutes.
pub const RESEED_NS: u64 = 300 * 1_000_000_000;

/// Most output between reseeds once seeded: a gibibyte.
pub const RESEED_BYTES: u64 = 1 << 30;

/// "expand 32-byte k".
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Overwrites `words` with zeroes in a way the compiler can't leave out as a dead store.
fn wipe(words: &mut [u32]) {
    for word in words.iter_mut() {
        // SAFETY: a valid, aligned reference to a `u32`.
        unsafe { core::ptr::write_volatile(word, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// As [`wipe`], for bytes.
fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

#[inline(always)]
fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// The ChaCha20 block function (RFC 8439 section 2.3): sixteen words of key stream for
/// `key`, block `counter` and `nonce`.
pub fn chacha20_block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u32; 16] {
    let mut initial = [0u32; 16];
    initial[..4].copy_from_slice(&SIGMA);
    initial[4..12].copy_from_slice(key);
    initial[12] = counter;
    initial[13..].copy_from_slice(nonce);
    let mut state = initial;
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }
    for (word, start) in state.iter_mut().zip(initial.iter()) {
        *word = word.wrapping_add(*start);
    }
    wipe(&mut initial);
    state
}

/// A 32-byte key as ChaCha20 reads it: eight little-endian words.
pub fn key_words(bytes: &[u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
        *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    words
}

/// A block's words as the bytes of key stream they are.
fn block_bytes(block: &[u32; 16], out: &mut [u8; 64]) {
    for (chunk, word) in out.chunks_exact_mut(4).zip(block.iter()) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
}

/// BLAKE2s's initialisation vector, SHA-256's.
const IV: [u32; 8] = [0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab, 0x5be0_cd19];

/// The message schedule of BLAKE2s's ten rounds.
const SCHEDULE: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

#[inline(always)]
fn mix_g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

fn compress(h: &mut [u32; 8], block: &[u8; 64], length: u64, last: bool) {
    let mut m = [0u32; 16];
    for (word, chunk) in m.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let mut v = [0u32; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= length as u32;
    v[13] ^= (length >> 32) as u32;
    if last {
        v[14] = !v[14];
    }
    for s in &SCHEDULE {
        mix_g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        mix_g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        mix_g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        mix_g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        mix_g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        mix_g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        mix_g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        mix_g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for (index, word) in h.iter_mut().enumerate() {
        *word ^= v[index] ^ v[index + 8];
    }
    wipe(&mut m);
    wipe(&mut v);
}

/// BLAKE2s with a 32-byte digest and no key: the pool input is hashed into.
#[cfg_attr(test, derive(Clone))]
pub struct Blake2s {
    h: [u32; 8],
    buffer: [u8; 64],
    used: usize,
    /// Bytes compressed so far.
    length: u64,
}

impl Blake2s {
    pub const fn new() -> Self {
        let mut h = IV;
        // Parameter block: a 32-byte digest, no key, fanout and depth 1.
        h[0] ^= 0x0101_0020;
        Self { h, buffer: [0; 64], used: 0, length: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // The last block is kept back until more comes, since only the final one is
            // compressed as last.
            if self.used == self.buffer.len() {
                self.length = self.length.wrapping_add(self.buffer.len() as u64);
                compress(&mut self.h, &self.buffer, self.length, false);
                self.used = 0;
            }
            let take = (self.buffer.len() - self.used).min(data.len());
            self.buffer[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
        }
    }

    /// The digest of everything given since the last, and a fresh start.
    pub fn finalize(&mut self) -> [u8; 32] {
        self.length = self.length.wrapping_add(self.used as u64);
        self.buffer[self.used..].fill(0);
        compress(&mut self.h, &self.buffer, self.length, true);
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        self.wipe();
        *self = Self::new();
        out
    }

    fn wipe(&mut self) {
        wipe(&mut self.h);
        wipe_bytes(&mut self.buffer);
    }
}

impl Default for Blake2s {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Blake2s {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// The key stream one request is given: ChaCha20 under a key the generator made for it
/// alone. At most 2^32 blocks per nonce word, and the nonce counts on past that.
pub struct Stream {
    key: [u32; 8],
    counter: u32,
    nonce: [u32; 3],
    block: [u8; 64],
    used: usize,
}

impl Stream {
    fn new(key: [u32; 8]) -> Self {
        Self { key, counter: 0, nonce: [0; 3], block: [0; 64], used: 64 }
    }

    /// Fills `out` with the next bytes of the stream.
    pub fn fill(&mut self, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            if self.used == self.block.len() {
                let mut words = chacha20_block(&self.key, self.counter, &self.nonce);
                block_bytes(&words, &mut self.block);
                wipe(&mut words);
                self.counter = self.counter.wrapping_add(1);
                if self.counter == 0 {
                    self.nonce[0] = self.nonce[0].wrapping_add(1);
                }
                self.used = 0;
            }
            let take = (self.block.len() - self.used).min(out.len() - done);
            out[done..done + take].copy_from_slice(&self.block[self.used..self.used + take]);
            // What's been given is gone from here too.
            wipe_bytes(&mut self.block[self.used..self.used + take]);
            self.used += take;
            done += take;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        wipe(&mut self.key);
        wipe_bytes(&mut self.block);
    }
}

/// The generator: a key, the pool, and what the pool has been credited with.
#[cfg_attr(test, derive(Clone))]
pub struct Csprng {
    key: [u32; 8],
    pool: Blake2s,
    /// The pool holds input not yet folded into the key.
    pool_dirty: bool,
    /// Trusted bits in the pool since the last fold, up to [`SEED_BITS`]'s worth counted.
    pending_bits: u32,
    seeded: bool,
    reseeds: u64,
    last_reseed_ns: u64,
    /// Output since the last reseed.
    since_reseed: u64,
}

impl Csprng {
    pub const fn new() -> Self {
        Self {
            key: [0; 8],
            pool: Blake2s::new(),
            pool_dirty: false,
            pending_bits: 0,
            seeded: false,
            reseeds: 0,
            last_reseed_ns: 0,
            since_reseed: 0,
        }
    }

    /// Whether it has had [`SEED_BITS`] of trusted input folded in.
    pub fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// Times the pool has been folded in as a seed or reseed.
    pub fn reseeds(&self) -> u64 {
        self.reseeds
    }

    /// Trusted bits waiting in the pool.
    pub fn pending_bits(&self) -> u32 {
        self.pending_bits
    }

    /// Mixes `bytes` into the pool, counted as no entropy at all.
    pub fn mix(&mut self, bytes: &[u8]) {
        // Each input's length goes in before it, so inputs can't run into each other.
        self.pool.update(&(bytes.len() as u64).to_le_bytes());
        self.pool.update(bytes);
        self.pool_dirty = true;
    }

    /// Mixes `bytes` from a trusted source into the pool, credited with `bits` (never
    /// more than the bytes hold).
    pub fn add_trusted(&mut self, bytes: &[u8], bits: u32) {
        self.mix(bytes);
        let most = u32::try_from(bytes.len().saturating_mul(8)).unwrap_or(u32::MAX);
        self.pending_bits = self.pending_bits.saturating_add(bits.min(most));
    }

    /// Whether [`reseed`](Self::reseed) would fold the pool in now: the first time when
    /// the credit is there, then on the schedule.
    pub fn wants_reseed(&self, now_ns: u64) -> bool {
        if self.seeded {
            now_ns.saturating_sub(self.last_reseed_ns) >= RESEED_NS || self.since_reseed >= RESEED_BYTES
        } else {
            self.pending_bits >= SEED_BITS
        }
    }

    /// Folds the pool into the key, if it's seeded or this seeds it: the caller has mixed
    /// in what fresh input it could find. Returns whether this is what seeded it.
    pub fn reseed(&mut self, now_ns: u64) -> bool {
        let seeds = !self.seeded && self.pending_bits >= SEED_BITS;
        if !self.seeded && !seeds {
            return false;
        }
        self.fold();
        self.seeded = true;
        self.pending_bits = 0;
        self.reseeds = self.reseeds.wrapping_add(1);
        self.last_reseed_ns = now_ns;
        self.since_reseed = 0;
        seeds
    }

    /// The key becomes the hash of the pool and itself.
    fn fold(&mut self) {
        let mut bytes = [0u8; 32];
        for (chunk, word) in bytes.chunks_exact_mut(4).zip(self.key.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        self.pool.update(&bytes);
        let mut digest = self.pool.finalize();
        wipe(&mut self.key);
        self.key = key_words(&digest);
        wipe_bytes(&mut digest);
        wipe_bytes(&mut bytes);
        self.pool_dirty = false;
    }

    /// A stream for one request of `length` bytes. One block under the key gives the
    /// generator its next key and the stream its own, so the key that made the stream is
    /// gone before the stream makes anything.
    pub fn request(&mut self, length: usize) -> Stream {
        if !self.seeded && self.pool_dirty && self.pending_bits == 0 {
            self.fold();
        }
        let mut block = chacha20_block(&self.key, 0, &[0; 3]);
        let mut next = [0u32; 8];
        let mut own = [0u32; 8];
        next.copy_from_slice(&block[..8]);
        own.copy_from_slice(&block[8..]);
        self.key = next;
        wipe(&mut block);
        wipe(&mut next);
        self.since_reseed = self.since_reseed.saturating_add(length as u64);
        let stream = Stream::new(own);
        wipe(&mut own);
        stream
    }
}

impl Default for Csprng {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
    }

    fn block_of(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
        let mut out = [0u8; 64];
        block_bytes(&chacha20_block(key, counter, nonce), &mut out);
        out
    }

    fn rfc_key() -> [u32; 8] {
        let bytes: [u8; 32] = core::array::from_fn(|index| index as u8);
        key_words(&bytes)
    }

    #[test]
    fn rfc8439_block_function() {
        // Section 2.3.2.
        let nonce = [0x0900_0000, 0x4a00_0000, 0];
        let expected = hex(
            "10f1e7e4 d13b5915 500fdd1f a32071c4 c7d1f4c7 33c06803 0422aa9a c3d46c4e
             d2826446 079faa09 14c2d705 d98b02a2 b5129cd1 de164eb9 cbd083e8 a2503c4e",
        );
        assert_eq!(expected.len(), 64);
        assert_eq!(block_of(&rfc_key(), 1, &nonce).as_slice(), expected.as_slice());
        // Its state words, as section 2.3.2 also lists them.
        let words = chacha20_block(&rfc_key(), 1, &nonce);
        assert_eq!(&words[..4], &[0xe4e7_f110, 0x1559_3bd1, 0x1fdd_0f50, 0xc471_20a3]);
        assert_eq!(&words[12..], &[0xd19c_12b5, 0xb94e_16de, 0xe883_d0cb, 0x4e3c_50a2]);
    }

    #[test]
    fn rfc8439_zero_key_block() {
        // Appendix A.1, test vector 1: the all-zero key, nonce and counter.
        let expected = hex(
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586",
        );
        assert_eq!(block_of(&[0; 8], 0, &[0; 3]).as_slice(), expected.as_slice());
    }

    #[test]
    fn rfc8439_encryption() {
        // Section 2.4.2: the key stream from block 1 on, XORed with the plaintext.
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let nonce = [0, 0x4a00_0000, 0];
        let mut stream = Stream { key: rfc_key(), counter: 1, nonce, block: [0; 64], used: 64 };
        let mut keystream = vec![0u8; plaintext.len()];
        stream.fill(&mut keystream);
        let cipher: Vec<u8> = plaintext.iter().zip(&keystream).map(|(p, k)| p ^ k).collect();
        let expected = hex("6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b");
        assert_eq!(&cipher[..32], expected.as_slice());
        assert_eq!(cipher.len(), 114);
    }

    #[test]
    fn stream_pieces_match_one_fill() {
        let make = || Stream::new(rfc_key());
        let mut whole = vec![0u8; 1000];
        make().fill(&mut whole);
        let mut pieces = vec![0u8; 1000];
        let mut stream = make();
        let mut at = 0;
        for size in [1, 63, 64, 65, 7, 300, 500] {
            let end = (at + size).min(pieces.len());
            stream.fill(&mut pieces[at..end]);
            at = end;
        }
        assert_eq!(whole, pieces);
        // And they're ChaCha20's blocks from counter 0.
        assert_eq!(&whole[..64], block_of(&rfc_key(), 0, &[0; 3]).as_slice());
        assert_eq!(&whole[64..128], block_of(&rfc_key(), 1, &[0; 3]).as_slice());
    }

    #[test]
    fn blake2s_vectors() {
        // RFC 7693 appendix B, and the empty message.
        let mut hash = Blake2s::new();
        hash.update(b"abc");
        assert_eq!(hash.finalize().as_slice(), hex("508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982").as_slice());
        assert_eq!(Blake2s::new().finalize().as_slice(), hex("69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9").as_slice());
        // Finalizing starts afresh.
        hash.update(b"abc");
        assert_eq!(hash.finalize().as_slice(), hex("508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982").as_slice());
    }

    #[test]
    fn blake2s_pieces_match_one_update() {
        let data: Vec<u8> = (0..1000u32).map(|value| (value * 7) as u8).collect();
        for length in [0, 1, 63, 64, 65, 128, 129, 1000] {
            let mut whole = Blake2s::new();
            whole.update(&data[..length]);
            let mut pieces = Blake2s::new();
            for chunk in data[..length].chunks(17) {
                pieces.update(chunk);
            }
            assert_eq!(whole.finalize(), pieces.finalize(), "{length}");
        }
    }

    fn seeded() -> Csprng {
        let mut generator = Csprng::new();
        generator.add_trusted(&[0x5a; 32], 256);
        assert!(generator.wants_reseed(0));
        assert!(generator.reseed(0));
        generator
    }

    fn output(generator: &mut Csprng, length: usize) -> Vec<u8> {
        let mut out = vec![0u8; length];
        generator.request(length).fill(&mut out);
        out
    }

    #[test]
    fn seeding_takes_256_trusted_bits() {
        let mut generator = Csprng::new();
        generator.mix(&[1; 4096]);
        assert!(!generator.wants_reseed(0));
        assert!(!generator.reseed(0), "uncounted input never seeds");
        generator.add_trusted(&[2; 16], 128);
        assert!(!generator.wants_reseed(0));
        // Credited with no more than the bytes hold.
        generator.add_trusted(&[3; 8], 1000);
        assert_eq!(generator.pending_bits(), 192);
        assert!(!generator.reseed(0) && !generator.is_seeded());
        generator.add_trusted(&[4; 8], 64);
        assert!(generator.wants_reseed(0));
        assert!(generator.reseed(5));
        assert!(generator.is_seeded());
        assert_eq!((generator.pending_bits(), generator.reseeds()), (0, 1));
        // Seeding happens once.
        assert!(!generator.reseed(6));
    }

    #[test]
    fn reseed_schedule() {
        let mut generator = seeded();
        assert!(!generator.wants_reseed(RESEED_NS - 1));
        assert!(generator.wants_reseed(RESEED_NS));
        generator.reseed(RESEED_NS);
        assert!(!generator.wants_reseed(RESEED_NS + 1));
        // Output counts too.
        for _ in 0..1024 {
            generator.request(1 << 20);
        }
        assert!(generator.wants_reseed(RESEED_NS + 1));
        generator.reseed(RESEED_NS + 1);
        assert!(!generator.wants_reseed(RESEED_NS + 2));
    }

    #[test]
    fn reseed_changes_output() {
        let mut one = seeded();
        let mut two = one.clone();
        // The same state gives the same output.
        assert_eq!(output(&mut one.clone(), 64), output(&mut two.clone(), 64));
        two.mix(b"one more interrupt");
        two.reseed(1);
        one.reseed(1);
        assert_ne!(output(&mut one, 64), output(&mut two, 64));
        // A reseed with nothing new still moves the key on.
        let mut three = seeded();
        let before = three.clone();
        three.reseed(2);
        assert_ne!(output(&mut three, 64), output(&mut before.clone(), 64));
    }

    #[test]
    fn requests_differ() {
        let mut generator = seeded();
        let outputs: Vec<Vec<u8>> = (0..100).map(|_| output(&mut generator, 32)).collect();
        for (index, first) in outputs.iter().enumerate() {
            assert!(outputs[index + 1..].iter().all(|other| other != first));
        }
    }

    #[test]
    fn key_erasure() {
        let mut generator = seeded();
        let old_key = generator.key;
        let given = output(&mut generator, 256);
        let captured = generator.clone();
        // The key that made the request is gone from the state.
        assert_ne!(captured.key, old_key);
        assert!(given.chunks(32).all(|chunk| chunk != key_bytes(&captured.key)));
        // Nothing the captured state can make going forward is what was given: not its
        // next request, and not its key used as a stream key, at any block.
        let mut next = captured.clone();
        assert_ne!(output(&mut next, 256), given);
        let mut stream = Stream::new(captured.key);
        let mut long = vec![0u8; 64 * 1024];
        stream.fill(&mut long);
        assert!(long.windows(32).all(|window| window != &given[..32]));
        // Running the block function forward from the old key is the only way back to
        // the request's key, and it gives exactly what was handed out.
        let block = chacha20_block(&old_key, 0, &[0; 3]);
        assert_eq!(&block[..8], &captured.key);
        let mut own = [0u32; 8];
        own.copy_from_slice(&block[8..]);
        let mut replay = vec![0u8; 256];
        Stream::new(own).fill(&mut replay);
        assert_eq!(replay, given);
    }

    fn key_bytes(key: &[u32; 8]) -> Vec<u8> {
        key.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[test]
    fn unseeded_output_folds_uncounted_input() {
        // With nothing counted waiting, timings change the output before seeding.
        let mut one = Csprng::new();
        let mut two = Csprng::new();
        one.mix(&1u64.to_le_bytes());
        two.mix(&2u64.to_le_bytes());
        assert_ne!(output(&mut one, 32), output(&mut two, 32));
        // Counted bits short of a seed stay in the pool: output doesn't depend on them,
        // so it can't give them away a few at a time.
        let mut three = Csprng::new();
        let mut four = Csprng::new();
        three.add_trusted(&[7; 8], 64);
        four.add_trusted(&[8; 8], 64);
        assert_eq!(output(&mut three, 32), output(&mut four, 32));
        assert!(!three.is_seeded());
    }

    #[test]
    fn monobit_is_balanced() {
        let mut generator = seeded();
        let data = output(&mut generator, 1 << 20);
        let ones: u64 = data.iter().map(|byte| u64::from(byte.count_ones())).sum();
        let bits = (data.len() * 8) as f64;
        let sigma = (bits / 4.0).sqrt();
        assert!((ones as f64 - bits / 2.0).abs() < 4.0 * sigma, "{ones} of {bits}");
    }
}
