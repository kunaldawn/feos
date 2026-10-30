//! Motion JPEG: one JPEG image per video sample.

use crate::error::{require_rgba, Result, VideoError};

#[derive(Clone, Copy, Debug)]
pub(crate) struct MjpegInfo {
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
}

/// Finds the byte range of the `index`th JPEG in a concatenated stream.
pub(crate) fn find_frame(input: &[u8], index: u32) -> Result<&[u8]> {
    let mut pos = 0usize;
    let mut current = 0u32;
    loop {
        let start = find_soi(input, pos).ok_or(if current == 0 {
            VideoError::UnknownFormat
        } else {
            VideoError::FrameOutOfRange {
                index,
                count: current,
            }
        })?;
        let end = find_eoi(input, start + 2).ok_or(VideoError::UnexpectedEnd)?;
        if current == index {
            return Ok(&input[start..end]);
        }
        current += 1;
        pos = end;
    }
}

fn find_soi(input: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < input.len() {
        if input[i] == 0xff && input[i + 1] == 0xd8 {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_eoi(input: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < input.len() {
        if input[i] == 0xff {
            match input[i + 1] {
                0xd9 => return Some(i + 2),
                // Stuffed zero and restart markers are part of the entropy
                // stream and do not end the image.
                0x00 | 0xd0..=0xd7 => i += 2,
                _ => i += 2,
            }
        } else {
            i += 1;
        }
    }
    None
}

pub(crate) fn info(input: &[u8]) -> Result<MjpegInfo> {
    let first = find_frame(input, 0)?;
    let image = feos_image::jpeg::info(first)?;
    let mut count = 1u32;
    let mut pos = first.len();
    while find_soi(input, pos).is_some() {
        count = count.saturating_add(1);
        match find_frame(input, count - 1) {
            Ok(frame) => pos += frame.len(),
            Err(_) => break,
        }
    }
    Ok(MjpegInfo {
        width: image.width,
        height: image.height,
        frame_count: count,
    })
}

pub(crate) fn decode_sample(sample: &[u8], out: &mut [u8]) -> Result<()> {
    let image = feos_image::jpeg::info(sample)?;
    require_rgba(out, image.width, image.height)?;
    feos_image::jpeg::decode(sample, out)?;
    Ok(())
}
