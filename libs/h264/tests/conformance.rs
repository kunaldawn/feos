//! Decodes small libx264 clips and compares them with FFmpeg's decode, whose
//! H.264 output is bit-exact. `testdata/generate.py` documents each clip and
//! writes the expected hashes into `testdata/conformance.txt`.

use feos_h264::{DecodeError, OrderedDecoder, annex_b_units};

const TABLE: &str = include_str!("../testdata/conformance.txt");

macro_rules! clips {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../testdata/", $name, ".h264")) as &[u8])),*]
    };
}

const CLIPS: &[(&str, &[u8])] = clips![
    "baseline_cavlc",
    "main_cabac_b",
    "main_cabac_pyramid",
    "main_cavlc_b",
    "main_spatial_direct",
    "high_8x8dct",
    "high_cqm_jvt",
    "high_chroma_offset",
    "multi_slice",
    "multi_slice_cavlc",
    "sliced_deblock_idc2",
    "odd_size_crop",
    "crop_rect_left_top",
    "intra_only",
    "idr_every_4",
    "open_gop",
    "high_cavlc_8x8dct",
    "high_cavlc_intra_8x8",
];

fn fnv64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325, |value, &byte| {
        (value ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

fn decode_all(stream: &[u8]) -> (Vec<u8>, u32, u32, usize) {
    let mut decoder = OrderedDecoder::new();
    let mut i420 = Vec::new();
    let (mut width, mut height, mut frames) = (0, 0, 0);
    let mut sink = |frame: feos_h264::Frame| {
        width = frame.width;
        height = frame.height;
        frames += 1;
        frame.append_i420(&mut i420);
    };
    for nal in annex_b_units(stream) {
        for frame in decoder.decode_nal(&nal).expect("clip should decode") {
            sink(frame);
        }
    }
    for frame in decoder.flush() {
        sink(frame);
    }
    (i420, width, height, frames)
}

#[test]
fn clips_match_ffmpeg_output() {
    let mut failures = Vec::new();
    for line in TABLE.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [name, width, height, frames, hash] = fields[..] else {
            panic!("bad table line: {line}");
        };
        let (_, stream) = CLIPS
            .iter()
            .find(|(clip, _)| *clip == name)
            .unwrap_or_else(|| panic!("{name} is not in CLIPS"));
        let (i420, w, h, count) = decode_all(stream);
        let got = (w.to_string(), h.to_string(), count.to_string());
        let want = (width.to_string(), height.to_string(), frames.to_string());
        let matches = u64::from_str_radix(hash, 16).unwrap() == fnv64(&i420);
        if got != want || !matches {
            failures.push(format!("{name}: {got:?} vs {want:?}, hash match {matches}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

macro_rules! must_be_unsupported {
    ($test:ident, $file:literal) => {
        #[test]
        fn $test() {
            let stream = include_bytes!(concat!("../testdata/", $file));
            let mut decoder = OrderedDecoder::new();
            let mut result = Ok(());
            for nal in annex_b_units(stream) {
                if let Err(error) = decoder.decode_nal(&nal) {
                    result = Err(error);
                    break;
                }
            }
            assert!(
                matches!(result, Err(DecodeError::Unsupported(_))),
                "{:?}",
                result
            );
        }
    };
}

must_be_unsupported!(rejects_high10, "unsupported_high10.h264");
must_be_unsupported!(rejects_high422, "unsupported_high422.h264");
must_be_unsupported!(rejects_high444, "unsupported_high444.h264");
must_be_unsupported!(rejects_monochrome, "unsupported_mono.h264");
must_be_unsupported!(rejects_mbaff_cabac, "unsupported_mbaff_cabac.h264");
must_be_unsupported!(rejects_mbaff_cavlc, "unsupported_mbaff_cavlc.h264");
