//! Motion JPEG: one JPEG image per video sample.

use crate::error::{require_rgba, Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};

#[derive(Clone, Copy, Debug)]
pub(crate) struct MjpegInfo {
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
}

fn find_soi(input: &[u8], from: usize) -> Option<usize> {
    let bytes = input.get(from..)?;
    bytes
        .windows(2)
        .position(|pair| pair == [0xff, 0xd8])
        .map(|at| from + at)
}

/// Calls `f` with the byte range of each complete JPEG in a concatenated
/// stream, until it returns `false`. A trailing partial image doesn't count.
fn for_each_image(input: &[u8], mut f: impl FnMut(usize, usize) -> Result<bool>) -> Result<()> {
    let mut pos = 0usize;
    while let Some(start) = find_soi(input, pos) {
        let Some(len) = feos_image::jpeg::image_end(&input[start..]) else {
            break;
        };
        // image_end walks segment lengths, so a thumbnail's EOI inside an EXIF
        // segment doesn't end the frame.
        if !f(start, len)? {
            break;
        }
        pos = start + len.max(2);
    }
    Ok(())
}

pub(crate) fn info(input: &[u8]) -> Result<MjpegInfo> {
    let mut first = None;
    let mut count = 0u32;
    for_each_image(input, |start, len| {
        if first.is_none() {
            first = Some((start, len));
        }
        count = count.saturating_add(1);
        Ok(true)
    })?;
    let (start, len) = first.ok_or(VideoError::UnexpectedEnd)?;
    let image = feos_image::jpeg::info(&input[start..start + len])?;
    Ok(MjpegInfo {
        width: image.width,
        height: image.height,
        frame_count: count,
    })
}

/// Builds the table of images once, so finding a frame needs no search.
pub(crate) fn build_index(input: &[u8], limits: &DecoderLimits) -> Result<Index> {
    let mut index = Index::default();
    for_each_image(input, |start, len| {
        index.push(limits, input.len(), start, len, 0, FLAG_KEY)?;
        Ok(true)
    })?;
    Ok(index)
}

/// Decodes one JPEG, returning its dimensions.
pub(crate) fn decode_sample(sample: &[u8], out: &mut [u8]) -> Result<(u32, u32)> {
    let image = feos_image::jpeg::info(sample)?;
    require_rgba(out, image.width, image.height)?;
    feos_image::jpeg::decode(sample, out)?;
    Ok((image.width, image.height))
}
