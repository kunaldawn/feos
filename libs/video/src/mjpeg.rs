//! Motion JPEG: one JPEG image per video sample.

use alloc::vec::Vec;

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

/// Largest single image in a file on disk.
const MAX_STREAM_IMAGE: usize = 16 << 20;

/// The same as [`build_index`] for a file that stays on disk. It reads the whole file once. Gives
/// the index, and the first image, which is all [`info`] needs to see.
pub(crate) fn walk(file: &dyn feos_source::Source, len: usize, limits: &DecoderLimits) -> Result<(Index, Vec<u8>)> {
    let mut index = Index::default();
    let mut first = Vec::new();
    let mut size = 2usize << 20;
    let mut chunk = feos_source::zeroed(size).ok_or(VideoError::OutOfMemory)?;
    let mut pos = 0usize;
    while pos < len {
        let read = file.read_full(pos as u64, &mut chunk);
        if read == 0 {
            break;
        }
        let bytes = &chunk[..read];
        let mut at = 0usize;
        while let Some(start) = find_soi(bytes, at) {
            let Some(image_len) = feos_image::jpeg::image_end(&bytes[start..]) else {
                at = start;
                break;
            };
            index.push(limits, len, pos + start, image_len, 0, FLAG_KEY)?;
            if first.is_empty() {
                first
                    .try_reserve_exact(image_len)
                    .map_err(|_| VideoError::OutOfMemory)?;
                first.extend_from_slice(&bytes[start..start + image_len]);
            }
            at = start + image_len.max(2);
        }
        if pos + read >= len {
            // What's left is a trailing partial image, which doesn't count.
            break;
        }
        if at == 0 {
            // The image doesn't fit here: look again with more room.
            if size >= MAX_STREAM_IMAGE {
                return Err(VideoError::LimitExceeded("an image is too large"));
            }
            size *= 2;
            drop(core::mem::take(&mut chunk));
            chunk = feos_source::zeroed(size).ok_or(VideoError::OutOfMemory)?;
            continue;
        }
        pos += at;
    }
    Ok((index, first))
}

/// Decodes one JPEG, returning its dimensions.
pub(crate) fn decode_sample(sample: &[u8], out: &mut [u8]) -> Result<(u32, u32)> {
    let image = feos_image::jpeg::info(sample)?;
    require_rgba(out, image.width, image.height)?;
    feos_image::jpeg::decode(sample, out)?;
    Ok((image.width, image.height))
}
