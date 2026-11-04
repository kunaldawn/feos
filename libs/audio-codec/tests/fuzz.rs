//! Deterministic byte-mutation test of `info` and `decode`.
//!
//! Every fixture in `tests/data` is mutated with bit flips, byte overwrites,
//! truncations, stomped length fields and insertions, from a fixed seed, so a
//! failure reproduces exactly. The target may return any error, but may not
//! panic, overflow its stack, or abort:
//!
//! - The test runs in a thread with a [`STACK`]-sized stack, so unbounded
//!   recursion and oversized frames become a crash.
//! - The global allocator refuses to go above [`HEAP_LIMIT`] live bytes, the
//!   size of the kernel heap. An infallible allocation that gets refused calls
//!   `handle_alloc_error` and aborts the test binary, which fails the test.
//! - Arithmetic overflow panics in the default test profile.
//!
//! This is the file's only `#[test]`, so the allocation counter sees nothing but
//! the code under test. On a panic, the case is printed.
//!
//! For a longer soak, set `FEOS_FUZZ_ROUNDS=n` to multiply the iteration count
//! and `FEOS_FUZZ_ONLY=text` to run the fixtures whose names contain `text`. Run
//! with `--nocapture` to see, per fixture, how many cases decoded and the slowest.
//! `FEOS_FUZZ_STACK=bytes` overrides the stack size, to measure what is needed.
//!
//! Ogg pages and FLAC frame headers carry checksums, which would otherwise stop
//! nearly every mutated file at the demuxer. Most cases have their checksums
//! repaired after mutation, so the mutation reaches the codec behind them; the
//! rest keep them broken, to exercise that path as well.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use feos_audio_codec::{decode, info, scratch_len};

/// The kernel heap's size.
const HEAP_LIMIT: usize = 64 << 20;
/// Thread stack for the target.
const STACK: usize = 128 << 10;
/// The largest output buffer offered, in samples. A file can claim any length.
const MAX_OUT: usize = 1 << 20;
/// Mutated files per kilobyte of fixture, on top of [`BASE_CASES`].
const CASES_PER_KIB: u64 = 3;
const BASE_CASES: u64 = 100;
/// Each fixture is also cut at each length up to this.
const HEADER_CUTS: usize = 128;

struct LimitedAlloc;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: this forwards to `System`, which upholds the `GlobalAlloc` contract,
// and refusing an allocation by returning null is allowed.
unsafe impl GlobalAlloc for LimitedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        if live > HEAP_LIMIT {
            LIVE.fetch_sub(size, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        PEAK.fetch_max(live, Ordering::Relaxed);
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            LIVE.fetch_sub(size, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: LimitedAlloc = LimitedAlloc;

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A value in `0..n`; `n` must not be 0.
    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 16) as usize % n
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

/// Byte values that sit on decoder boundaries.
const INTERESTING: [u8; 8] = [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff, 0x0f, 0xf0];

/// Mutations aimed at the file's start, where headers live.
fn header_pos(rng: &mut Rng, len: usize) -> usize {
    if rng.chance(2) {
        rng.below(len.min(256))
    } else {
        rng.below(len)
    }
}

fn mutate(rng: &mut Rng, file: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(4) {
        if file.is_empty() {
            file.push(rng.byte());
        }
        let pos = header_pos(rng, file.len());
        match rng.below(8) {
            // Bit flip.
            0 | 1 => file[pos] ^= 1 << rng.below(8),
            // Byte overwrite.
            2 => file[pos] = rng.byte(),
            3 => file[pos] = INTERESTING[rng.below(INTERESTING.len())],
            // Truncation.
            4 => {
                let keep = if rng.chance(2) {
                    rng.below(file.len())
                } else {
                    file.len() - rng.below(file.len().min(64))
                };
                file.truncate(keep);
            }
            // A length field stomped with an extreme value, either endianness.
            5 => {
                let width = [1, 2, 3, 4, 8][rng.below(5)];
                let end = (pos + width).min(file.len());
                let value = match rng.below(4) {
                    0 => 0,
                    1 => u64::MAX,
                    2 => 0x7fff_ffff_ffff_ffff,
                    _ => rng.next(),
                };
                file[pos..end].copy_from_slice(&value.to_le_bytes()[..end - pos]);
            }
            // Insertion.
            6 => {
                let count = 1 + rng.below(32);
                let fill: Vec<u8> = (0..count).map(|_| rng.byte()).collect();
                file.splice(pos..pos, fill);
            }
            // Replay of a piece of the file somewhere else.
            _ => {
                let count = 1 + rng.below(file.len().min(512));
                let from = rng.below(file.len() - count + 1);
                let to = rng.below(file.len() - count + 1);
                file.copy_within(from..from + count, to);
            }
        }
    }
}

fn ogg_crc(data: &[u8], crc: u32) -> u32 {
    let mut crc = crc;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc >> 31 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Rewrites the checksum of every well-formed Ogg page.
fn repair_ogg(file: &mut [u8]) {
    let mut pos = 0;
    while pos + 27 <= file.len() {
        if &file[pos..pos + 4] != b"OggS" {
            pos += 1;
            continue;
        }
        let segments = file[pos + 26] as usize;
        let Some(table) = file.get(pos + 27..pos + 27 + segments) else {
            return;
        };
        let body: usize = table.iter().map(|&b| b as usize).sum();
        let end = pos + 27 + segments + body;
        if end > file.len() {
            return;
        }
        file[pos + 22..pos + 26].fill(0);
        let crc = ogg_crc(&file[pos..end], 0);
        file[pos + 22..pos + 26].copy_from_slice(&crc.to_le_bytes());
        pos = end;
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 7 } else { crc << 1 };
        }
    }
    crc
}

/// Rewrites the CRC-8 of everything shaped like a FLAC frame header. The frame's
/// CRC-16 is checked only after its subframes decode, so needs no repair.
fn repair_flac(file: &mut [u8]) {
    for pos in 0..file.len().saturating_sub(16) {
        if file[pos] != 0xff || file[pos + 1] & 0xfe != 0xf8 {
            continue;
        }
        let lead = file[pos + 4];
        let extra = match lead {
            0x00..=0x7f => 0,
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf7 => 3,
            0xf8..=0xfb => 4,
            0xfc..=0xfd => 5,
            0xfe => 6,
            _ => continue,
        };
        let block = match file[pos + 2] >> 4 {
            6 => 1,
            7 => 2,
            _ => 0,
        };
        let rate = match file[pos + 2] & 15 {
            12 => 1,
            13 | 14 => 2,
            _ => 0,
        };
        let crc_at = pos + 5 + extra + block + rate;
        if crc_at < file.len() {
            file[crc_at] = crc8(&file[pos..crc_at]);
        }
    }
}

/// Runs one input through the public API. Only a crash can fail this. Returns
/// whether the file decoded without an error.
fn exercise(file: &[u8], out: &mut [i16]) -> bool {
    let claimed = info(file);
    // Size the output the way a careful caller does: from the claim, capped.
    let samples = claimed
        .ok()
        .and_then(|props| props.sample_count().ok())
        .unwrap_or(MAX_OUT)
        .min(MAX_OUT);
    let _ = scratch_len(file);
    let result = decode(file, &mut out[..samples], &mut []);
    // What it reports having written must fit in what it was given.
    if let (Ok(decoded), Ok(props)) = (result, claimed) {
        assert!(decoded.frames * decoded.info.channels as usize <= samples);
        assert_eq!(decoded.info.format, props.format);
    }
    result.is_ok()
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("fixture directory") {
        let path = entry.expect("fixture entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // Reference outputs and documentation are not inputs.
        if name.ends_with(".s16") || name.ends_with(".md") {
            continue;
        }
        files.push((name, std::fs::read(&path).expect("fixture")));
    }
    // Directory order differs between machines; the test must not.
    files.sort();
    assert!(files.len() >= 10, "fixtures are missing");
    files
}

/// Runs every case and returns how many there were.
fn run(rounds: u64) -> u64 {
    let mut out = vec![0i16; MAX_OUT];
    let mut total = 0;
    let only = std::env::var("FEOS_FUZZ_ONLY").unwrap_or_default();
    for (index, (name, fixture)) in fixtures().iter().enumerate() {
        if !name.contains(&only) {
            continue;
        }
        // Unmodified, every fixture must at least not crash.
        CURRENT.with(|case| *case.borrow_mut() = format!("{name}, unmodified"));
        exercise(fixture, &mut out);

        // A cut at every length through the headers, not left to chance.
        for len in 0..fixture.len().min(HEADER_CUTS) {
            CURRENT.with(|case| *case.borrow_mut() = format!("{name}, cut at {len}"));
            exercise(&fixture[..len], &mut out);
        }

        let ogg = fixture.starts_with(b"OggS");
        let flac = fixture.starts_with(b"fLaC");
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ ((index as u64 + 1) << 32));
        let cases = rounds * (BASE_CASES + CASES_PER_KIB * fixture.len() as u64 / 1024);
        let mut worst = (std::time::Duration::ZERO, 0);
        let mut decoded = 0;
        for case in 0..cases {
            CURRENT.with(|current| *current.borrow_mut() = format!("{name}, case {case}"));
            let mut file = fixture.clone();
            mutate(&mut rng, &mut file);
            if !rng.chance(8) {
                if ogg {
                    repair_ogg(&mut file);
                } else if flac {
                    repair_flac(&mut file);
                }
            }
            let start = std::time::Instant::now();
            decoded += exercise(&file, &mut out) as u64;
            if start.elapsed() > worst.0 {
                worst = (start.elapsed(), case);
            }
            total += 1;
        }
        println!(
            "{name}: {cases} cases, {decoded} decoded, slowest {:?} (case {})",
            worst.0, worst.1
        );
    }
    total
}

thread_local! {
    static CURRENT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

#[test]
fn mutated_files_never_crash_the_decoders() {
    let rounds = std::env::var("FEOS_FUZZ_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        CURRENT.with(|case| eprintln!("panic in {}", case.borrow()));
        hook(info);
    }));
    let stack = std::env::var("FEOS_FUZZ_STACK")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(STACK);
    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || run(rounds))
        .expect("spawn fuzz thread");
    let cases = worker.join().expect("a decoder panicked");
    println!(
        "{cases} cases, peak heap {} KiB",
        PEAK.load(Ordering::Relaxed) / 1024
    );
    assert!(cases > 0);
}
