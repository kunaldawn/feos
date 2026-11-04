//! Deterministic byte-mutation test of the demuxers and decoders, under the
//! kernel's conditions: a 64 MiB heap where exhaustion is fatal, and a small
//! stack. A panic, an abort or an overflow fails the run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use feos_video::{decode_frame, info, scratch_len, VideoDecoder};

const HEAP_LIMIT: usize = 64 * 1024 * 1024;
/// The kernel's task stack is 128 KiB. Unoptimized frames are several times
/// larger, so dev builds get more room; release runs at the real size.
const STACK_SIZE: usize = if cfg!(debug_assertions) {
    512 * 1024
} else {
    128 * 1024
};
/// What a caller would give a frame, before skipping a video as too large.
const FRAME_BUDGET: usize = 16 * 1024 * 1024;

struct LimitedHeap;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator, only refusing some requests.
unsafe impl GlobalAlloc for LimitedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size() > HEAP_LIMIT {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static HEAP: LimitedHeap = LimitedHeap;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit.max(1) as u64) as usize
    }
}

/// Damages a copy of `seed`. Half of the edits are near the start, where the
/// container headers usually are, and many stomp a length-like 32-bit field.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut data = seed.to_vec();
    for _ in 0..1 + rng.below(5) {
        if data.is_empty() {
            break;
        }
        let at = if rng.below(2) == 0 {
            rng.below(data.len().min(600))
        } else {
            rng.below(data.len())
        };
        match rng.below(7) {
            0 | 1 => data[at] ^= 1 << rng.below(8),
            2 => data[at] = rng.next() as u8,
            3 => {
                let value = [0u32, 1, 0xffff_ffff, 0x7fff_ffff, 0xffff, 16384][rng.below(6)];
                let end = (at + 4).min(data.len());
                let bytes = value.to_be_bytes();
                data[at..end].copy_from_slice(&bytes[..end - at]);
            }
            4 => {
                let fill = rng.next() as u8;
                data.splice(at..at, std::iter::repeat_n(fill, 1 + rng.below(16)));
            }
            5 => {
                let end = (at + 1 + rng.below(16)).min(data.len());
                data.drain(at..end);
            }
            _ => {
                if rng.below(3) == 0 {
                    data.truncate(at);
                }
            }
        }
    }
    data
}

/// Exercises the API. Returns the number of frames that decoded.
fn exercise(input: &[u8], rng: &mut Rng) -> usize {
    let Ok(props) = info(input) else {
        return 0;
    };
    let Ok(len) = props.rgba_len() else {
        return 0;
    };
    if len > FRAME_BUDGET {
        return 0;
    }
    let mut out = vec![0u8; len];
    let mut decoded = 0;
    if rng.below(8) == 0 {
        // The wrapper, including GIF's caller-provided scratch.
        if let Ok(scratch) = scratch_len(input) {
            if scratch <= FRAME_BUDGET {
                let mut scratch = vec![0u8; scratch];
                let index = rng.below(4) as u32;
                decoded += usize::from(decode_frame(input, index, &mut out, &mut scratch).is_ok());
            }
        }
    }
    let Ok(mut decoder) = VideoDecoder::open(input) else {
        return decoded;
    };
    let count = decoder.frame_count();
    // A frame can grow past the header's size mid-stream.
    let mut targets: Vec<u32> = (0..count.min(6)).collect();
    if count > 0 {
        targets.push(rng.below(count as usize) as u32);
        targets.push(0);
        targets.push(count - 1);
    }
    for target in targets {
        match decoder.decode(target, &mut out) {
            Ok(frame) => {
                assert!(frame.width as usize * frame.height as usize * 4 <= out.len());
                decoded += 1;
            }
            Err(error) if error.is_fatal() => break,
            Err(_) => {}
        }
    }
    let _ = decoder.frame_time_ms(0);
    let _ = decoder.duration_ms();
    let _ = decoder.frame_at_time(1000);
    let _ = decoder.key_frame_at_or_before(u32::MAX);
    let _ = decoder.key_frame_after(0);
    decoded
}

const SEEDS: &[&[u8]] = &[
    include_bytes!("data/anim.gif"),
    include_bytes!("data/raw.avi"),
    include_bytes!("data/mjpeg.avi"),
    include_bytes!("data/h264.mp4"),
    include_bytes!("data/h264.mkv"),
    include_bytes!("data/h264.flv"),
    include_bytes!("data/h264.h264"),
    include_bytes!("data/vp8.webm"),
    include_bytes!("data/vp9.ivf"),
    include_bytes!("data/bframes.mp4"),
    include_bytes!("data/bframes.mkv"),
    include_bytes!("data/bframes.flv"),
    include_bytes!("data/bframes.avi"),
    include_bytes!("data/fragmented.mp4"),
    include_bytes!("data/rotated.mp4"),
    include_bytes!("data/rle8.avi"),
    include_bytes!("data/raw_pal8_w30.avi"),
    include_bytes!("data/raw_yv12_w30.avi"),
    include_bytes!("data/raw_i420.mkv"),
    include_bytes!("data/vfr.mp4"),
    include_bytes!("../../image/tests/data/test.jpg"),
];

#[test]
fn mutated_files_never_panic_or_exhaust_the_heap() {
    // FEOS_FUZZ_ROUNDS and FEOS_FUZZ_SEED widen the search for one-off runs.
    let rounds: u64 = option_env!("FEOS_FUZZ_ROUNDS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) { 120 } else { 400 });
    let base: u64 = option_env!("FEOS_FUZZ_SEED")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x5eed_0000_0000_0001);
    let decoded = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let mut decoded = 0;
            for (index, seed) in SEEDS.iter().enumerate() {
                for round in 0..rounds {
                    let mut rng =
                        Rng(base ^ ((index as u64) << 40) ^ (round << 8));
                    let input = mutate(seed, &mut rng);
                    decoded += exercise(&input, &mut rng);
                }
            }
            decoded
        })
        .unwrap()
        .join()
        .unwrap();
    // If this drops, mutation has stopped reaching the decoders.
    assert!(
        decoded as u64 > rounds * SEEDS.len() as u64,
        "only {decoded} frames decoded"
    );
}
