//! gzip (RFC 1952) container support.

use crate::checksum::crc32;
use crate::deflate;
use crate::error::{need, CompressError, Result};
use crate::inflate;

const MAGIC: [u8; 2] = [0x1f, 0x8b];
const METHOD_DEFLATE: u8 = 8;

const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const RESERVED: u8 = 0xe0;

fn read_u16_le(input: &[u8], offset: usize) -> Result<u16> {
    need(input.len(), offset, 2)?;
    Ok(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn read_u32_le(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn skip_cstring(input: &[u8], mut pos: usize) -> Result<usize> {
    while pos < input.len() {
        let byte = input[pos];
        pos += 1;
        if byte == 0 {
            return Ok(pos);
        }
    }
    Err(CompressError::UnexpectedEnd)
}

/// Parses a member header and returns the offset of its deflate data.
fn header_len(input: &[u8]) -> Result<usize> {
    need(input.len(), 0, 10)?;
    if input[0..2] != MAGIC {
        return Err(CompressError::InvalidData("bad gzip magic"));
    }
    if input[2] != METHOD_DEFLATE {
        return Err(CompressError::UnsupportedFeature("gzip compression method"));
    }
    let flags = input[3];
    if flags & RESERVED != 0 {
        return Err(CompressError::InvalidData("reserved gzip flags set"));
    }

    let mut pos = 10;

    if flags & FEXTRA != 0 {
        let extra = read_u16_le(input, pos)? as usize;
        pos = pos
            .checked_add(2)
            .and_then(|value| value.checked_add(extra))
            .ok_or(CompressError::Overflow)?;
        need(input.len(), pos, 0)?;
    }
    if flags & FNAME != 0 {
        pos = skip_cstring(input, pos)?;
    }
    if flags & FCOMMENT != 0 {
        pos = skip_cstring(input, pos)?;
    }
    if flags & FHCRC != 0 {
        let expected = read_u16_le(input, pos)?;
        let actual = (crc32(&input[..pos]) & 0xffff) as u16;
        if actual != expected {
            return Err(CompressError::Checksum);
        }
        pos += 2;
    }
    Ok(pos)
}

/// Decompresses one or more concatenated gzip members into `out`, returning the
/// number of bytes written.
pub fn decompress(input: &[u8], out: &mut [u8]) -> Result<usize> {
    if input.is_empty() {
        return Err(CompressError::UnexpectedEnd);
    }

    let mut written = 0usize;
    let mut pos = 0usize;
    while pos < input.len() {
        let header = pos + header_len(&input[pos..])?;
        let result = inflate::inflate_with_consumed(&input[header..], &mut out[written..])?;
        let trailer = header + result.consumed;
        let expected_crc = read_u32_le(input, trailer)?;
        let expected_size = read_u32_le(input, trailer + 4)?;

        let member = &out[written..written + result.written];
        if crc32(member) != expected_crc || member.len() as u32 != expected_size {
            return Err(CompressError::Checksum);
        }

        written += result.written;
        pos = trailer + 8;
    }
    Ok(written)
}

/// Compresses `input` into a single gzip member, returning the number of bytes
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
    const HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    if out.len() < HEADER.len() {
        return Err(CompressError::BufferTooSmall {
            required: HEADER.len(),
            available: out.len(),
        });
    }
    out[..HEADER.len()].copy_from_slice(&HEADER);

    let written = deflate::compress_with(deflater, input, &mut out[HEADER.len()..])?;
    let trailer = HEADER.len() + written;
    let total = trailer + 8;
    if total > out.len() {
        return Err(CompressError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }
    out[trailer..trailer + 4].copy_from_slice(&crc32(input).to_le_bytes());
    out[trailer + 4..trailer + 8].copy_from_slice(&(input.len() as u32).to_le_bytes());
    Ok(total)
}
