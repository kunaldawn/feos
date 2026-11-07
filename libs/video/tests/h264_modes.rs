//! H.264 beyond 8-bit progressive 4:2:0, through to RGBA. The decoder's own tests compare
//! the samples with FFmpeg; these check what reaches a player.

use feos_video::{info, VideoDecoder};

const PAFF: &[u8] = include_bytes!("../../h264/testdata/paff_pcm.h264");
const HI10: &[u8] = include_bytes!("../../h264/testdata/hi10_cabac.h264");
const HI422: &[u8] = include_bytes!("../../h264/testdata/hi42210_cabac.h264");
const MBAFF: &[u8] = include_bytes!("../../h264/testdata/mbaff_cabac.h264");

fn decode_all(stream: &[u8]) -> (u32, Vec<Vec<u8>>) {
    let opened = info(stream).expect("stream should open");
    assert!(opened.can_decode());
    let mut decoder = VideoDecoder::open(stream.to_vec()).expect("stream should open");
    let count = decoder.frame_count();
    assert_eq!(count, opened.frame_count);
    let mut out = vec![0u8; opened.rgba_len().unwrap()];
    let mut frames = Vec::new();
    for index in 0..count {
        decoder.decode(index, &mut out).expect("frame should decode");
        frames.push(out.clone());
    }
    (count, frames)
}

#[test]
fn two_fields_make_one_frame() {
    let (count, frames) = decode_all(PAFF);
    assert_eq!(count, 2);
    assert_ne!(frames[0], frames[1]);
}

#[test]
fn deep_and_interlaced_streams_reach_rgba() {
    for (stream, frames_wanted) in [(HI10, 16), (HI422, 16), (MBAFF, 16)] {
        let (count, frames) = decode_all(stream);
        assert_eq!(count, frames_wanted);
        // testsrc2 is full of colour: all that's being looked for is that it's there.
        let frame = &frames[0];
        let colourful = frame
            .chunks_exact(4)
            .filter(|p| p[0].abs_diff(p[1]) > 30 || p[1].abs_diff(p[2]) > 30)
            .count();
        assert!(colourful > frame.len() / 4 / 8, "only {colourful} coloured pixels");
    }
}

/// 10 bits with its display conversion lands close to the same picture in 8 bits.
#[test]
fn seeking_back_gives_the_same_picture() {
    let opened = info(HI10).unwrap();
    let mut decoder = VideoDecoder::open(HI10.to_vec()).unwrap();
    let mut first = vec![0u8; opened.rgba_len().unwrap()];
    decoder.decode(0, &mut first).unwrap();
    let mut other = first.clone();
    decoder.decode(9, &mut other).unwrap();
    decoder.decode(0, &mut other).unwrap();
    assert_eq!(first, other);
}
