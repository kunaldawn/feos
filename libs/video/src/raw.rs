//! Uncompressed frame decoding.

use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::pixel;

/// Decodes a tightly packed uncompressed sample into RGBA.
pub(crate) fn decode(
    sample: &[u8],
    codec: Codec,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<()> {
    let format = codec
        .pixel_format()
        .ok_or(VideoError::UnsupportedCodec(codec.name()))?;
    require_rgba(out, width, height)?;
    pixel::to_rgba(sample, format, width, height, out)
}

/// Decodes an uncompressed AVI sample, which is laid out as a Windows DIB.
pub(crate) fn decode_dib(
    sample: &[u8],
    codec: Codec,
    palette: &[u8],
    width: u32,
    height: u32,
    bottom_up: bool,
    out: &mut [u8],
) -> Result<()> {
    let format = codec
        .pixel_format()
        .ok_or(VideoError::UnsupportedCodec(codec.name()))?;
    require_rgba(out, width, height)?;
    pixel::dib_to_rgba(sample, format, palette, width, height, bottom_up, out)
}
