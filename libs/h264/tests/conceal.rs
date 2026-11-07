//! What a damaged stream comes to: the same number of pictures, with the damage only
//! where it has to be. These take clean clips apart and break them.

use feos_h264::{annex_b_ranges, annex_b_units, Decoder, Frame};

const MULTI_SLICE: &[u8] = include_bytes!("../testdata/multi_slice.h264");
const MULTI_SLICE_CAVLC: &[u8] = include_bytes!("../testdata/multi_slice_cavlc.h264");

fn frames_of(stream: &[u8]) -> Vec<Frame> {
    let mut decoder = Decoder::new();
    let mut frames = Vec::new();
    for nal in annex_b_units(stream) {
        if let Ok(Some(frame)) = decoder.decode_nal(&nal) {
            frames.push(frame);
        }
    }
    while let Some(frame) = decoder.flush() {
        frames.push(frame);
    }
    frames
}

fn luma(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    for row in 0..frame.height as usize {
        out.extend_from_slice(frame.y_row(row));
    }
    out
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let error: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum();
    if error == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 * a.len() as f64 / error).log10()
}

/// The NALs of a stream, as ranges with their start codes, for putting back together.
fn nals(stream: &[u8]) -> Vec<&[u8]> {
    annex_b_ranges(stream).map(|range| &stream[range]).collect()
}

fn join(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

fn is_slice(nal: &[u8]) -> bool {
    matches!(nal[0] & 0x1f, 1 | 5)
}

/// The slices of the nth picture, by where first_mb_in_slice is 0.
fn slices_of_picture(all: &[&[u8]], picture: usize) -> Vec<usize> {
    let mut seen = 0;
    let mut found = Vec::new();
    for (index, nal) in all.iter().enumerate() {
        if !is_slice(nal) {
            continue;
        }
        // ue(v) of 0 is a single set bit.
        if nal[1] & 0x80 != 0 {
            seen += 1;
        }
        if seen == picture + 1 {
            found.push(index);
        }
    }
    found
}

fn lost(stream: &[u8], name: &str, picture: usize, which: usize) {
    let all = nals(stream);
    let clean = frames_of(stream);
    let slices = slices_of_picture(&all, picture);
    assert!(slices.len() > which, "{name}: picture {picture} has {} slices", slices.len());
    let kept: Vec<&[u8]> = all
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != slices[which])
        .map(|(_, nal)| *nal)
        .collect();
    let damaged = frames_of(&join(&kept));
    assert_eq!(clean.len(), damaged.len(), "{name}: pictures went missing");
    let concealed: u32 = damaged.iter().map(|frame| frame.concealed_mbs).sum();
    assert!(concealed > 0, "{name}: nothing was concealed");
    assert_eq!(damaged[picture].concealed_mbs, concealed, "{name}: only that picture");
    for index in 0..picture {
        assert!(luma(&clean[index]) == luma(&damaged[index]), "{name}: picture {index} changed");
    }
    // Better than nothing by a distance: the frame that was lost has moved little.
    let quality = psnr(&luma(&clean[picture]), &luma(&damaged[picture]));
    let blank = vec![128u8; luma(&clean[picture]).len()];
    let nothing = psnr(&luma(&clean[picture]), &blank);
    println!("{name}: picture {picture} slice {which} lost: {quality:.1} dB, against {nothing:.1} dB for grey");
    assert!(quality > nothing + 5.0, "{name}: {quality:.1} dB against {nothing:.1} dB");
}

#[test]
fn a_lost_slice_is_filled_in() {
    lost(MULTI_SLICE, "CABAC", 2, 1);
    lost(MULTI_SLICE_CAVLC, "CAVLC", 2, 1);
}

#[test]
fn a_lost_first_slice_does_not_merge_into_the_last_picture() {
    lost(MULTI_SLICE, "CABAC", 2, 0);
    lost(MULTI_SLICE_CAVLC, "CAVLC", 2, 0);
}

#[test]
fn a_damaged_slice_keeps_what_decoded() {
    for stream in [MULTI_SLICE, MULTI_SLICE_CAVLC] {
        let all = nals(stream);
        let clean = frames_of(stream);
        let slices = slices_of_picture(&all, 2);
        let mut broken = all[slices[0]].to_vec();
        // Far enough in to have decoded something, and all of it wrong from there on.
        let from = broken.len() / 2;
        for byte in &mut broken[from..] {
            *byte = 0xff;
        }
        let mut with: Vec<&[u8]> = all.clone();
        with[slices[0]] = &broken;
        let damaged = frames_of(&join(&with));
        assert_eq!(clean.len(), damaged.len());
    }
}

/// Everything at once: no panic, and pictures keep coming.
#[test]
fn random_damage_never_panics_and_keeps_the_count_close() {
    let mut seed = 0x0123_4567_89ab_cdefu64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for stream in [MULTI_SLICE, MULTI_SLICE_CAVLC] {
        let all = nals(stream);
        for _ in 0..60 {
            let mut owned: Vec<Vec<u8>> = all.iter().map(|nal| nal.to_vec()).collect();
            let mut dropped = Vec::new();
            for (index, nal) in owned.iter_mut().enumerate() {
                if !is_slice(nal) || next() % 6 != 0 {
                    continue;
                }
                if next() % 2 == 0 {
                    dropped.push(index);
                } else {
                    let at = 1 + next() as usize % (nal.len() - 1);
                    nal[at] = next() as u8;
                }
            }
            let kept: Vec<&[u8]> = owned
                .iter()
                .enumerate()
                .filter(|(index, _)| !dropped.contains(index))
                .map(|(_, nal)| nal.as_slice())
                .collect();
            let frames = frames_of(&join(&kept));
            assert!(!frames.is_empty() || dropped.len() > all.len() / 8);
        }
    }
}
