//! zlib (RFC 1950) wrapper around raw DEFLATE.

use crate::checksum::adler32;
use crate::deflate;
use crate::error::{need, CompressError, Result};
use crate::inflate;

const CM_DEFLATE: u8 = 8;
const FCHECK_MASK: u16 = 0x1f;
const FDICT: u8 = 0x20;

fn read_u32_be(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_be_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

/// Decompresses a zlib stream into `out`, returning the number of bytes
/// written.
pub fn decompress(input: &[u8], out: &mut [u8]) -> Result<usize> {
    need(input.len(), 0, 2)?;
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0f != CM_DEFLATE {
        return Err(CompressError::UnsupportedFeature(
            "zlib compression method (only deflate is supported)",
        ));
    }
    if cmf >> 4 > 7 {
        return Err(CompressError::InvalidData("zlib window size too large"));
    }
    if (u16::from(cmf) << 8 | u16::from(flg)) % FCHECK_MASK != 0 {
        return Err(CompressError::InvalidData("zlib header check failed"));
    }
    if flg & FDICT != 0 {
        return Err(CompressError::UnsupportedFeature("zlib preset dictionary"));
    }

    let result = inflate::inflate_with_consumed(&input[2..], out)?;
    let checksum_offset = 2 + result.consumed;
    let expected = read_u32_be(input, checksum_offset)?;
    if adler32(&out[..result.written]) != expected {
        return Err(CompressError::Checksum);
    }
    Ok(result.written)
}

/// Compresses `input` into a zlib stream, returning the number of bytes
/// written.
pub fn compress(input: &[u8], out: &mut [u8]) -> Result<usize> {
    compress_with(&mut deflate::Deflater::new(), input, out)
}

/// Compresses with a reusable compressor state.
pub fn compress_with(
    deflater: &mut deflate::Deflater,
    input: &[u8],
    out: &mut [u8],
) -> Result<usize> {
    if out.len() < 2 {
        return Err(CompressError::BufferTooSmall {
            required: 2,
            available: out.len(),
        });
    }
    // CM = deflate, CINFO = 7 (32 KiB window), FLEVEL = 0, FCHECK makes the
    // header a multiple of 31.
    out[0] = 0x78;
    out[1] = 0x01;
    let written = deflate::compress_with(deflater, input, &mut out[2..])?;
    let end = 2 + written;
    let checksum = adler32(input);
    let total = end + 4;
    if total > out.len() {
        return Err(CompressError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }
    out[end..end + 4].copy_from_slice(&checksum.to_be_bytes());
    Ok(total)
}
