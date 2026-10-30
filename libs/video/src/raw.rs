//! Uncompressed AVI frame decoding.

use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::pixel;

/// Decodes an uncompressed sample into RGBA.
pub(crate) fn decode(
    sample: &[u8],
    codec: Codec,
    width: u32,
    height: u32,
    bottom_up: bool,
    out: &mut [u8],
) -> Result<()> {
    let format = codec
        .pixel_format()
        .ok_or(VideoError::UnsupportedCodec(codec.name()))?;
    require_rgba(out, width, height)?;
    if bottom_up {
        pixel::to_rgba_flipped(sample, format, width, height, out)
    } else {
        pixel::to_rgba(sample, format, width, height, out)
    }
}
