//! IVF elementary-stream demuxing (VP8/VP9/AV1).
//!
//! IVF is a tiny container used by the WebM reference encoders. Pixel decoding
//! is delegated to a codec backend; this module supplies dimensions, codec and
//! frame access.

use crate::bitstream::{fourcc_at, u16_le_at, u32_le_at};
use crate::codec::Codec;
use crate::error::{need, Result, VideoError};

#[derive(Clone, Copy, Debug)]
pub(crate) struct IvfInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
}

fn header(input: &[u8]) -> Result<(IvfInfo, usize)> {
    need(input.len(), 0, 32)?;
    if &input[..4] != b"DKIF" {
        return Err(VideoError::UnknownFormat);
    }
    let header_size = u16_le_at(input, 6)? as usize;
    if header_size < 32 || header_size > input.len() {
        return Err(VideoError::InvalidData("IVF header size"));
    }
    let codec = Codec::from_ivf(fourcc_at(input, 8)?);
    let width = u16_le_at(input, 12)? as u32;
    let height = u16_le_at(input, 14)? as u32;
    let denominator = u32_le_at(input, 16)?;
    let numerator = u32_le_at(input, 20)?;
    let frame_count = u32_le_at(input, 24)?;
    let rate = crate::reduce_fraction(denominator, numerator);
    Ok((
        IvfInfo {
            width,
            height,
            codec,
            frame_count,
            frame_rate: rate,
        },
        header_size,
    ))
}

fn count_frames(input: &[u8], mut pos: usize) -> u32 {
    let mut count = 0u32;
    while pos + 12 <= input.len() {
        let size = u32_le_at(input, pos).unwrap_or(0) as usize;
        let next = pos + 12 + size;
        if next > input.len() {
            break;
        }
        count = count.saturating_add(1);
        pos = next;
    }
    count
}

pub(crate) fn info(input: &[u8]) -> Result<IvfInfo> {
    let (mut info, header_size) = header(input)?;
    if info.frame_count == 0 {
        info.frame_count = count_frames(input, header_size);
    }
    Ok(info)
}

/// Returns the `index`th IVF frame payload.
pub(crate) fn frame(input: &[u8], index: u32) -> Result<&[u8]> {
    let (info, header_size) = header(input)?;
    let mut pos = header_size;
    let mut current = 0u32;
    while pos + 12 <= input.len() {
        let size = u32_le_at(input, pos)? as usize;
        let start = pos + 12;
        let end = start.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > input.len() {
            return Err(VideoError::UnexpectedEnd);
        }
        if current == index {
            return Ok(&input[start..end]);
        }
        current = current.saturating_add(1);
        pos = end;
    }
    Err(VideoError::FrameOutOfRange {
        index,
        count: info.frame_count.min(current),
    })
}
