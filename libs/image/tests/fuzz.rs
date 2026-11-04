//! Deterministic byte-mutation test for every decoder entry point.
//!
//! Each fixture in `tests/data` seeds a seeded series of mutations. The decoders
//! run under an allocator that refuses anything over the kernel's 64 MiB heap,
//! on a thread with a kernel-sized stack, so a panic, an abort, an allocation
//! failure the decoder does not handle, and a stack overflow all fail the test.
//! This is the file's only `#[test]`, so the allocator counts only this work.
//!
//! 128 KiB is the size of a kernel process stack (`kernel/src/sched.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use feos_image::{decode, detect, info, jpeg, scratch_len, ImageFormat};

const HEAP_LIMIT: usize = 64 * 1024 * 1024;
const STACK_SIZE: usize = 128 * 1024;
const SEED: u64 = 0x5eed_f0e5_1a6e_0001;
/// Mutated copies per fixture. Keeps the test at seconds in a dev build.
const ROUNDS: usize = 300;

struct LimitedAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

impl LimitedAllocator {
    fn reserve(&self, size: usize) -> bool {
        let mut live = LIVE.load(Ordering::Relaxed);
        loop {
            let Some(next) = live.checked_add(size).filter(|&n| n <= HEAP_LIMIT) else {
                return false;
            };
            match LIVE.compare_exchange_weak(live, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    PEAK.fetch_max(next, Ordering::Relaxed);
                    return true;
                }
                Err(current) => live = current,
            }
        }
    }
}

// SAFETY: forwards to `System`, and only refuses requests by returning null.
unsafe impl GlobalAlloc for LimitedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let block = System.alloc(layout);
        if block.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let block = System.alloc_zeroed(layout);
        if block.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        System.dealloc(block, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: LimitedAllocator = LimitedAllocator;

/// xorshift64*: small, dependency-free and the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }
}

fn interesting_u16(rng: &mut Rng) -> u16 {
    const VALUES: [u16; 12] = [0, 1, 2, 7, 8, 9, 64, 255, 256, 1000, 0x7fff, 0xffff];
    VALUES[rng.below(VALUES.len())]
}

/// Offsets of fields that bound work or memory, for each format.
fn stomp_length_or_dimension(rng: &mut Rng, data: &mut [u8]) {
    let len = data.len();
    if len < 2 {
        return;
    }
    let value = interesting_u16(rng);
    let put_be16 = |at: usize, data: &mut [u8]| {
        if at + 2 <= len {
            data[at..at + 2].copy_from_slice(&value.to_be_bytes());
        }
    };
    match detect(data) {
        Some(ImageFormat::Jpeg) => {
            // Walk segments, then stomp a random one's length or its contents.
            let mut segments = [0usize; 32];
            let mut count = 0;
            let mut pos = 2;
            while pos + 4 <= len && data[pos] == 0xff && count < segments.len() {
                segments[count] = pos;
                count += 1;
                if data[pos + 1] == 0xda {
                    break;
                }
                pos += 2 + u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            }
            if count > 0 {
                let at = segments[rng.below(count)];
                let field = if rng.below(2) == 0 {
                    2
                } else {
                    5 + 2 * rng.below(3)
                };
                put_be16(at + field, data);
            }
        }
        Some(ImageFormat::Png) => {
            // IHDR width or height; the checksum is repaired afterwards.
            let at = 16 + 4 * rng.below(2) + 2 * rng.below(2);
            put_be16(at, data);
        }
        Some(ImageFormat::Bmp) => {
            const FIELDS: [usize; 8] = [10, 14, 18, 22, 28, 30, 46, 54];
            let at = FIELDS[rng.below(FIELDS.len())];
            if at + 2 <= len {
                data[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        Some(ImageFormat::Qoi) => put_be16(4 + 2 * rng.below(4), data),
        Some(ImageFormat::Ppm) | None => {
            // Rewrite an ASCII number in the header with another.
            let header = &data[..len.min(32)];
            if let Some(start) = header.iter().position(|b| b.is_ascii_digit()) {
                let mut at = start + 1;
                for _ in 0..rng.below(3) {
                    while at < header.len() && !header[at].is_ascii_digit() {
                        at += 1;
                    }
                }
                if at < header.len() {
                    data[at] = b'0' + rng.below(10) as u8;
                }
            }
        }
    }
}

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut data = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        if data.is_empty() {
            break;
        }
        let at = rng.below(data.len());
        match rng.below(6) {
            0 => data[at] ^= 1 << rng.below(8),
            1 => data[at] = rng.next() as u8,
            2 => {
                // JPEG and PNG structure both turn on these byte values.
                data[at] = [0x00, 0xff, 0xd9, 0xda, 0xc4, 0xdd, 0x7f, 0x80][rng.below(8)];
            }
            3 => {
                let end = (at + 1 + rng.below(64)).min(data.len());
                data.truncate(if rng.below(2) == 0 { at } else { end });
            }
            4 => stomp_length_or_dimension(rng, &mut data),
            _ => {
                let insert = (0..1 + rng.below(16)).map(|_| rng.next() as u8);
                data.splice(at..at, insert.collect::<Vec<u8>>());
            }
        }
    }
    data
}

const CRC_POLY: u32 = 0xedb8_8320;

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                CRC_POLY ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Rewrites every chunk's CRC, so a mutated PNG reaches the decoder proper.
fn repair_png_checksums(data: &mut [u8]) {
    let mut pos = 8;
    while pos + 12 <= data.len() {
        let length = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let Some(end) = (pos + 8)
            .checked_add(length as usize)
            .filter(|&e| e + 4 <= data.len())
        else {
            return;
        };
        let crc = crc32(&[&data[pos + 4..end]]);
        data[end..end + 4].copy_from_slice(&crc.to_be_bytes());
        pos = end + 4;
    }
}

/// Mutates the inflated pixel data of a PNG and stores it back, so that the
/// filter and expansion code sees bad data with a correct zlib checksum.
fn mutate_png_pixels(rng: &mut Rng, seed: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 8;
    let mut compressed = Vec::new();
    let mut out = seed.get(..33)?.to_vec();
    while pos + 12 <= seed.len() {
        let length = u32::from_be_bytes(seed[pos..pos + 4].try_into().ok()?) as usize;
        if &seed[pos + 4..pos + 8] == b"IDAT" {
            compressed.extend_from_slice(seed.get(pos + 8..pos + 8 + length)?);
        } else if pos > 8 && &seed[pos + 4..pos + 8] != b"IEND" {
            out.extend_from_slice(seed.get(pos..pos + 12 + length)?);
        }
        pos += 12 + length;
    }

    let mut raw = vec![0u8; 1 << 16];
    let written = feos_compress::zlib::decompress(&compressed, &mut raw).ok()?;
    raw.truncate(written);
    if raw.is_empty() {
        return None;
    }
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(raw.len());
        raw[at] = rng.next() as u8;
    }
    match rng.below(4) {
        0 => raw.truncate(rng.below(raw.len())),
        1 => raw.extend_from_slice(&[0, 1, 2, 3, 4]),
        _ => {}
    }

    // One stored zlib block; the fixtures are all far under 64 KiB raw.
    let length = raw.len() as u16;
    let mut idat = vec![0x78, 0x01, 0x01];
    idat.extend_from_slice(&length.to_le_bytes());
    idat.extend_from_slice(&(!length).to_le_bytes());
    idat.extend_from_slice(&raw);
    idat.extend_from_slice(&feos_compress::adler32(&raw).to_be_bytes());

    out.extend_from_slice(&(idat.len() as u32).to_be_bytes());
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&idat);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"IEND");
    out.extend_from_slice(&[0; 4]);
    repair_png_checksums(&mut out);
    Some(out)
}

#[derive(Default)]
struct Totals {
    inputs: usize,
    decoded: usize,
    errors: usize,
    skipped_for_budget: usize,
}

/// Does what a careful caller does, under the shared memory budget.
fn exercise(input: &[u8], totals: &mut Totals) {
    totals.inputs += 1;
    let _ = detect(input);
    let _ = jpeg::image_end(input);
    let Ok(image) = info(input) else {
        totals.errors += 1;
        return;
    };
    let (Ok(out_len), Ok(scratch)) = (image.rgba_len(), scratch_len(input)) else {
        totals.errors += 1;
        return;
    };
    let available = HEAP_LIMIT.saturating_sub(LIVE.load(Ordering::Relaxed));
    if out_len.saturating_add(scratch) > available {
        totals.skipped_for_budget += 1;
        return;
    }
    // Checked against the budget above, with only this thread allocating, so
    // `vec!` cannot fail here, and its zeroed pages cost nothing when the decoder rejects the input.
    let mut out = vec![0u8; out_len];
    let mut scratch_buffer = vec![0u8; scratch];
    match decode(input, &mut out, &mut scratch_buffer) {
        Ok(decoded) => {
            assert_eq!((decoded.width, decoded.height), (image.width, image.height));
            totals.decoded += 1;
        }
        Err(_) => totals.errors += 1,
    }
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let data = fs::read(&path).unwrap();
            (name, data)
        })
        // References and the README are not images.
        .filter(|(_, data)| detect(data).is_some())
        .collect();
    files.sort();
    files
}

fn run(files: &[(String, Vec<u8>)]) -> Totals {
    let mut totals = Totals::default();
    let mut rng = Rng(SEED);
    for (_, seed) in files {
        exercise(seed, &mut totals);
        let is_png = detect(seed) == Some(ImageFormat::Png);
        for round in 0..ROUNDS {
            // PNG checksums would stop nearly every mutation at the first
            // chunk, so most rounds repair them, leaving a few to test the CRC
            // path itself.
            if is_png && round % 3 == 0 {
                if let Some(mutated) = mutate_png_pixels(&mut rng, seed) {
                    exercise(&mutated, &mut totals);
                }
                continue;
            }
            let mut mutated = mutate(&mut rng, seed);
            if is_png && round % 8 != 1 {
                repair_png_checksums(&mut mutated);
            }
            exercise(&mutated, &mut totals);
        }
    }
    totals
}

#[test]
fn mutated_images_never_panic() {
    let files = fixtures();
    assert!(files.len() >= 40, "only {} fixtures found", files.len());

    let worker = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn({
            let files = files.clone();
            move || run(&files)
        })
        .unwrap();
    let totals = worker.join().expect("a decoder panicked on mutated input");

    println!(
        "{} fixtures, {} inputs: {} decoded, {} rejected, {} over budget; peak heap {} KiB",
        files.len(),
        totals.inputs,
        totals.decoded,
        totals.errors,
        totals.skipped_for_budget,
        PEAK.load(Ordering::Relaxed) / 1024,
    );
    // A mutator that breaks everything at the first header would prove nothing.
    assert!(
        totals.decoded > totals.inputs / 20,
        "too few inputs decoded"
    );
    assert!(
        totals.errors > totals.inputs / 20,
        "too few inputs rejected"
    );
}
