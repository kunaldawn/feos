//! MPEG-1 against ffmpeg's decoding. See `tests/data/generate.py`.

use feos_video::{detect, info, ContainerFormat, Codec, VideoDecoder};

const FRAME_BYTES: usize = 64 * 48 * 4;

fn matches_reference(input: &[u8], reference: &[u8]) {
    assert_eq!(detect(input), Some(ContainerFormat::Mpeg1));
    let stated = info(input).unwrap();
    assert_eq!((stated.width, stated.height), (64, 48));
    assert_eq!(stated.codec, Codec::Mpeg1);
    assert!(stated.can_decode());
    let expected: Vec<&[u8]> = reference.chunks_exact(FRAME_BYTES).collect();
    assert_eq!(stated.frame_count as usize, expected.len());
    let mut decoder = VideoDecoder::open(input).unwrap();
    assert_eq!(decoder.frame_count() as usize, expected.len());
    let mut out = vec![0u8; FRAME_BYTES];
    for (index, frame) in expected.iter().enumerate() {
        let shown = decoder.decode(index as u32, &mut out).unwrap();
        assert_eq!(shown.index as usize, index);
        assert_within(&out, frame, index);
    }
    // Backwards, and then forwards again from there.
    for &index in &[3usize, 1, expected.len() - 1, 0, 5] {
        decoder.decode(index as u32, &mut out).unwrap();
        assert_within(&out, expected[index], index);
    }
}

fn assert_within(actual: &[u8], expected: &[u8], index: usize) {
    let total: u64 = actual
        .iter()
        .zip(expected)
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    let average = total as f64 / actual.len() as f64;
    // ffmpeg's IDCT and chroma scaling differ a little from these.
    assert!(average < 4.0, "frame {index}: average difference {average}");
}

#[test]
fn elementary_stream_matches_ffmpeg() {
    matches_reference(
        include_bytes!("data/mpeg1.m1v"),
        include_bytes!("data/mpeg1_m1v.ref"),
    );
}

#[test]
fn program_stream_matches_ffmpeg() {
    matches_reference(
        include_bytes!("data/mpeg1.mpg"),
        include_bytes!("data/mpeg1_mpg.ref"),
    );
}

#[test]
fn damage_gives_errors_or_pictures_and_never_panics() {
    let file: &[u8] = include_bytes!("data/mpeg1.m1v");
    let mut out = vec![0u8; FRAME_BYTES];
    for cut in [13, 40, 200, 1000, file.len() / 2] {
        if let Ok(mut decoder) = VideoDecoder::open(&file[..cut.min(file.len())]) {
            for index in 0..decoder.frame_count() {
                let _ = decoder.decode(index, &mut out);
            }
        }
    }
    let mut seed = 0x1234_5678u32;
    for _ in 0..200 {
        let mut bent = file.to_vec();
        for _ in 0..16 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let at = (seed >> 8) as usize % bent.len();
            bent[at] ^= (seed & 0xff) as u8;
        }
        if let Ok(mut decoder) = VideoDecoder::open(&bent[..]) {
            let mut frame = vec![0u8; decoder.info().rgba_len().unwrap().min(1 << 22)];
            for index in 0..decoder.frame_count().min(6) {
                let _ = decoder.decode(index, &mut frame);
            }
        }
    }
}
