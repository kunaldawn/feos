//! Robustness tests. Decoding runs under the conditions of the kernel: a 64 MiB
//! heap where exhaustion is fatal, and a small stack. Every mutated input must
//! end in frames or errors, so a panic or an abort fails the run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use feos_h264::nal::parse_annex_b;
use feos_h264::{Decoder, OrderedDecoder, annex_b_units};

const HEAP_LIMIT: usize = 64 * 1024 * 1024;
/// The kernel's task stack is 128 KiB. Unoptimized frames are several times
/// larger, so dev builds get more room; release runs at the real size.
const STACK_SIZE: usize = if cfg!(debug_assertions) {
    512 * 1024
} else {
    128 * 1024
};

/// Fails allocations beyond the kernel heap's size, the way the kernel does.
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

/// Xorshift PRNG, so a failure reproduces from its seed.
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

/// Byte offsets just past each start code: the NAL headers.
fn nal_starts(stream: &[u8]) -> Vec<usize> {
    stream
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect()
}

/// Returns a damaged copy of `seed`. Half of the edits land in the headers at
/// the start of a NAL, so parameter sets and slice headers get exercised
/// instead of only macroblock data.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut data = seed.to_vec();
    let starts = nal_starts(seed);
    for _ in 0..1 + rng.below(6) {
        if data.is_empty() {
            break;
        }
        let at = if rng.below(2) == 0 && !starts.is_empty() {
            (starts[rng.below(starts.len())] + rng.below(12)).min(data.len() - 1)
        } else {
            rng.below(data.len())
        };
        match rng.below(6) {
            0 | 1 => data[at] ^= 1 << rng.below(8),
            2 => data[at] = rng.next() as u8,
            3 => data[at] = if rng.below(2) == 0 { 0xff } else { 0 },
            4 => {
                let fill = rng.next() as u8;
                data.splice(at..at, std::iter::repeat_n(fill, 1 + rng.below(32)));
            }
            _ => {
                if rng.below(4) == 0 {
                    data.truncate(at);
                } else {
                    let end = (at + rng.below(32)).min(data.len());
                    data.drain(at..end);
                }
            }
        }
    }
    data
}

/// Decodes `stream` to the end, returning the number of frames produced.
fn decode(stream: &[u8]) -> usize {
    let mut decoder = OrderedDecoder::new();
    let mut frames = 0;
    for nal in annex_b_units(stream) {
        if let Ok(output) = decoder.decode_nal(&nal) {
            for frame in output {
                // Touching every visible row catches a bad crop or stride.
                for row in 0..frame.height as usize {
                    assert_eq!(frame.y_row(row).len(), frame.width as usize);
                }
                frames += 1;
            }
        }
    }
    frames + decoder.flush().len()
}

/// Runs `body` as the kernel would, returning its result.
fn on_small_stack<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn decodes_mini_corpus_against_ffmpeg_yuv() {
    let bitstream = include_bytes!("../testdata/mini.h264");
    let reference = include_bytes!("../testdata/mini.yuv");
    let nals = parse_annex_b(bitstream);
    let mut decoder = Decoder::new();
    let mut output = Vec::new();

    for nal in &nals {
        if let Some(frame) = decoder.decode_nal(nal).expect("mini corpus decode failed") {
            frame.append_i420(&mut output);
        }
    }
    if let Some(frame) = decoder.flush() {
        frame.append_i420(&mut output);
    }

    assert_eq!(output.len(), reference.len());
    assert_eq!(output, reference);
}

#[test]
fn malformed_input_is_rejected_without_panicking() {
    let inputs: [&[u8]; 5] = [
        &[],
        &[0, 0, 1, 0xff],
        &[0, 0, 1, 0x67, 0x00],
        &[0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff],
        &[0, 0, 1, 0x41, 0x80, 0x00],
    ];
    for input in inputs {
        let nals = parse_annex_b(input);
        let mut decoder = Decoder::new();
        for nal in &nals {
            let _ = decoder.decode_nal(nal);
        }
        let _ = decoder.flush();
    }
}

#[test]
fn mutated_streams_decode_without_panicking_or_exhausting_the_heap() {
    const SEEDS: &[&[u8]] = &[
        include_bytes!("../testdata/mini.h264"),
        include_bytes!("../testdata/baseline_cavlc.h264"),
        include_bytes!("../testdata/main_cabac_pyramid.h264"),
        include_bytes!("../testdata/main_cavlc_b.h264"),
        include_bytes!("../testdata/high_cqm_jvt.h264"),
        include_bytes!("../testdata/high_cavlc_8x8dct.h264"),
        include_bytes!("../testdata/crop_rect_left_top.h264"),
        include_bytes!("../testdata/sliced_deblock_idc2.h264"),
        include_bytes!("../testdata/hi10_cabac.h264"),
        include_bytes!("../testdata/hi10_cavlc.h264"),
        include_bytes!("../testdata/hi422_cabac.h264"),
        include_bytes!("../testdata/hi422_cavlc.h264"),
        include_bytes!("../testdata/hi42210_cabac.h264"),
        include_bytes!("../testdata/mbaff_cabac.h264"),
        include_bytes!("../testdata/mbaff_temporal.h264"),
        include_bytes!("../testdata/mbaff_cavlc.h264"),
        include_bytes!("../testdata/mbaff_hi422.h264"),
        include_bytes!("../testdata/paff_pcm.h264"),
    ];
    // FEOS_FUZZ_ROUNDS and FEOS_FUZZ_SEED widen the search for one-off runs.
    let rounds: u64 = option_env!("FEOS_FUZZ_ROUNDS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) { 60 } else { 250 });
    let base: u64 = option_env!("FEOS_FUZZ_SEED")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x5eed_0000_0000_0001);

    let (frames, intact) = on_small_stack(move || {
        let mut frames = 0usize;
        let mut intact = 0usize;
        for (index, seed) in SEEDS.iter().enumerate() {
            intact += decode(seed);
            for round in 0..rounds {
                let mut rng = Rng(base ^ ((index as u64) << 32) ^ (round << 8));
                let input = mutate(seed, &mut rng);
                frames += decode(&input);
            }
        }
        (frames, intact)
    });
    // Most mutations leave a stream that still produces pictures. If this
    // drops, the test has stopped reaching the macroblock layer.
    assert!(intact > 0);
    assert!(
        frames as u64 > rounds * SEEDS.len() as u64,
        "only {frames} frames survived mutation"
    );
}

/// The same with field-coded and other streams that aren't in the repository, if
/// `FEOS_H264_CONF` names a directory holding them. See `fate.rs`.
#[test]
fn mutated_conformance_streams_decode_without_panicking() {
    let Ok(dir) = std::env::var("FEOS_H264_CONF") else {
        println!("skipped: FEOS_H264_CONF not set");
        return;
    };
    const NAMES: &[&str] = &[
        "CVSE3_Sony_H.jsv",
        "CVSEFDFT3_Sony_E.jsv",
        "Sharp_MP_Field_1_B.jvt",
        "Sharp_MP_PAFF_1r2.jvt",
        "CAPAMA3_Sand_F.264",
        "CAFI1_SVA_C.264",
        "CVMA1_TOSHIBA_B.264",
        "CAMA3_Sand_E.264",
        "camp_mot_picaff0_full.26l",
        "cvmp_mot_fld0_full_B.26l",
        "MR9_BT_B.h264",
        "CAPCM1_Sand_E.264",
        "FRext/HPCAMAPALQ_BRCM_B.264",
        "FRext/Hi422FR1_SONY_A.jsv",
        "FRext/PPH422I1_Panasonic_A.264",
        "FRext/HPCVMOLQ_BRCM_B.264",
    ];
    // Enough for parameter sets and several frames: the sizes keep this to minutes.
    const LENGTH: usize = 192 * 1024;
    let rounds: u64 = std::env::var("FEOS_FUZZ_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12);
    on_small_stack(move || {
        for (index, name) in NAMES.iter().enumerate() {
            let Ok(mut seed) = std::fs::read(std::path::Path::new(&dir).join(name)) else {
                println!("{name}: not found");
                continue;
            };
            seed.truncate(LENGTH);
            for round in 0..rounds {
                let mut rng = Rng(0x5eed_0000_0000_0002 ^ ((index as u64) << 32) ^ (round << 8));
                let input = mutate(&seed, &mut rng);
                decode(&input);
            }
        }
    });
}

#[test]
fn pure_noise_after_real_parameter_sets_is_survivable() {
    // Real SPS and PPS, then random slice data, reaches the macroblock layer.
    let frames = on_small_stack(|| {
        let seed: &[u8] = include_bytes!("../testdata/baseline_cavlc.h264");
        let starts = nal_starts(seed);
        // Keep the slice header's first bytes so the noise lands in macroblocks.
        let idr = starts
            .iter()
            .copied()
            .find(|&at| seed[at] & 0x1f == 5)
            .unwrap();
        let header = &seed[..idr + 6];
        let mut rng = Rng(0x7a26_4264);
        let mut frames = 0;
        for len in 0..200usize {
            let mut stream = header.to_vec();
            for _ in 0..len * 4 {
                stream.push(rng.next() as u8);
            }
            frames += decode(&stream);
        }
        frames
    });
    assert!(frames > 0, "noise never reached the macroblock layer");
}

#[test]
fn oversized_lists_do_not_exhaust_the_heap() {
    // A megabyte of 0xff is a valid run of short Exp-Golomb codes in a slice
    // header's modification and marking loops.
    on_small_stack(|| {
        let seed: &[u8] = include_bytes!("../testdata/baseline_cavlc.h264");
        let starts = nal_starts(seed);
        let mut stream = seed[..starts[2]].to_vec();
        for nal_header in [0x65u8, 0x41] {
            stream.extend_from_slice(&[nal_header]);
            stream.extend(std::iter::repeat_n(0xffu8, 1 << 20));
            stream.extend_from_slice(&[0, 0, 1]);
        }
        decode(&stream);
    });
}

#[test]
fn start_code_floods_stay_cheap() {
    // MED-7: a file of nothing but start codes must not build a huge list.
    on_small_stack(|| {
        let stream: Vec<u8> = [0u8, 0, 1, 0x09].repeat(1 << 20);
        assert_eq!(parse_annex_b(&stream).len(), feos_h264::nal::MAX_ANNEX_B_NALS);
        assert_eq!(annex_b_units(&stream).count(), 1 << 20);
        decode(&stream);
    });
}

#[test]
fn trailing_zeros_stay_cheap() {
    // MED-29: megabytes of trailing zeros must not rescan per macroblock.
    on_small_stack(|| {
        let seed: &[u8] = include_bytes!("../testdata/baseline_cavlc.h264");
        let mut stream = seed.to_vec();
        stream.extend(std::iter::repeat_n(0u8, 8 << 20));
        assert_eq!(decode(&stream), 12);
    });
}

#[test]
fn huge_sps_is_refused_before_allocating_pictures() {
    use feos_h264::{DecodeError, Limits};
    // MED-4: a real 64x48 stream with limits below its size.
    let seed: &[u8] = include_bytes!("../testdata/baseline_cavlc.h264");
    for limits in [
        Limits {
            max_width: 32,
            ..Limits::default()
        },
        Limits {
            max_memory: 4096,
            ..Limits::default()
        },
    ] {
        let mut decoder = Decoder::with_limits(limits);
        let mut refused = false;
        for nal in annex_b_units(seed) {
            refused |= matches!(decoder.decode_nal(&nal), Err(DecodeError::OutOfMemory));
        }
        assert!(refused);
    }
}
