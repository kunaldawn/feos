use feos_h264::Decoder;
use feos_h264::nal::parse_annex_b;

#[test]
fn decodes_mini_corpus_against_ffmpeg_yuv() {
    let bitstream = include_bytes!("../testdata/mini.h264");
    let reference = include_bytes!("../testdata/mini.yuv");
    let nals = parse_annex_b(bitstream);
    let mut decoder = Decoder::new();
    let mut output = Vec::new();

    for nal in &nals {
        if let Some(frame) = decoder.decode_nal(nal).expect("mini corpus decode failed") {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }
    }
    if let Some(frame) = decoder.flush() {
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
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
fn deterministic_fuzz_inputs_do_not_panic() {
    // This is intentionally dependency-free so it remains runnable in the
    // crate's normal test profile and in the no_std consumer's CI harness.
    let mut state = 0x7a26_4264u32;
    for len in 0..512usize {
        let mut bytes = vec![0u8; len + 4];
        bytes[..4].copy_from_slice(&[0, 0, 1, 0x41]);
        for byte in &mut bytes[4..] {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        let nals = parse_annex_b(&bytes);
        let mut decoder = Decoder::new();
        for nal in &nals {
            let _ = decoder.decode_nal(nal);
        }
        let _ = decoder.flush();
    }
}
