//! Netpbm decoding (P1 to P6) and P6 encoding.
//!
//! P1/P4 are bitmaps, P2/P5 graymaps and P3/P6 pixmaps; the first of each pair
//! is ASCII. Header comments are accepted everywhere the format allows them.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    AsciiBits,
    AsciiSamples,
    RawBits,
    RawSamples,
}

struct Header {
    width: usize,
    height: usize,
    maxval: u32,
    channels: usize,
    encoding: Encoding,
    data_start: usize,
}

/// Whether `input` starts with a Netpbm magic number.
///
/// "P1" to "P4" are short enough to begin ordinary text, so those also need the
/// whitespace or comment the format requires next.
pub(crate) fn is_magic(input: &[u8]) -> bool {
    match input {
        [b'P', b'5' | b'6', ..] => true,
        [b'P', b'1'..=b'4', next, ..] => is_space(*next) || *next == b'#',
        _ => false,
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

fn skip_space(input: &[u8], mut pos: usize) -> usize {
    loop {
        while pos < input.len() && is_space(input[pos]) {
            pos += 1;
        }
        if pos < input.len() && input[pos] == b'#' {
            while pos < input.len() && input[pos] != b'\n' {
                pos += 1;
            }
        } else {
            return pos;
        }
    }
}

fn token(input: &[u8], pos: &mut usize) -> Result<u32> {
    *pos = skip_space(input, *pos);
    let start = *pos;
    let mut value = 0u32;
    while *pos < input.len() && input[*pos].is_ascii_digit() {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((input[*pos] - b'0') as u32))
            .ok_or(ImageError::Overflow)?;
        *pos += 1;
    }
    if *pos == start {
        return if start >= input.len() {
            Err(ImageError::UnexpectedEnd)
        } else {
            Err(ImageError::InvalidData("expected a PPM number"))
        };
    }
    Ok(value)
}

fn parse(input: &[u8]) -> Result<Header> {
    need(input.len(), 0, 2)?;
    if input[0] != b'P' {
        return Err(ImageError::UnknownFormat);
    }
    let (encoding, channels) = match input[1] {
        b'1' => (Encoding::AsciiBits, 1),
        b'2' => (Encoding::AsciiSamples, 1),
        b'3' => (Encoding::AsciiSamples, 3),
        b'4' => (Encoding::RawBits, 1),
        b'5' => (Encoding::RawSamples, 1),
        b'6' => (Encoding::RawSamples, 3),
        _ => return Err(ImageError::UnknownFormat),
    };
    let mut pos = 2;
    let width = token(input, &mut pos)? as usize;
    let height = token(input, &mut pos)? as usize;
    let maxval = if matches!(encoding, Encoding::AsciiBits | Encoding::RawBits) {
        1
    } else {
        token(input, &mut pos)?
    };
    if width == 0 || height == 0 || maxval == 0 || maxval > 65535 {
        return Err(ImageError::InvalidData("bad PPM header"));
    }
    if matches!(encoding, Encoding::RawBits | Encoding::RawSamples) {
        // Exactly one whitespace character separates the header from the raster.
        need(input.len(), pos, 1)?;
        if input[pos] == b'\r' && input.get(pos + 1) == Some(&b'\n') {
            pos += 2;
        } else if is_space(input[pos]) {
            pos += 1;
        } else {
            return Err(ImageError::InvalidData("missing PPM raster separator"));
        }
    }
    Ok(Header {
        width,
        height,
        maxval,
        channels,
        encoding,
        data_start: pos,
    })
}

/// Returns the dimensions of a PPM image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let header = parse(input)?;
    Ok(ImageInfo {
        width: header.width as u32,
        height: header.height as u32,
        format: ImageFormat::Ppm,
    })
}

/// PPM decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Scales a sample to 8 bits. Values over `max` are malformed, and saturate.
fn scale(value: u32, max: u32) -> u8 {
    if max == 255 {
        value.min(255) as u8
    } else {
        ((value.min(max) * 255 + max / 2) / max) as u8
    }
}

fn store(pixel: &mut [u8], channels: usize, samples: [u8; 3]) {
    let (r, g, b) = if channels == 3 {
        (samples[0], samples[1], samples[2])
    } else {
        (samples[0], samples[0], samples[0])
    };
    pixel.copy_from_slice(&[r, g, b, 255]);
}

fn decode_raw_samples(input: &[u8], header: &Header, pixels: &mut [u8]) -> Result<()> {
    let bytes_per_sample = if header.maxval < 256 { 1 } else { 2 };
    let raster = header
        .width
        .checked_mul(header.height)
        .and_then(|pixels| pixels.checked_mul(header.channels))
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or(ImageError::Overflow)?;
    need(input.len(), header.data_start, raster)?;
    let data = &input[header.data_start..header.data_start + raster];
    let stride = header.channels * bytes_per_sample;
    for (pixel, encoded) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(data.chunks_exact(stride)) {
        let mut samples = [0u8; 3];
        for (sample, bytes) in samples
            .iter_mut()
            .zip(encoded.chunks_exact(bytes_per_sample))
        {
            let value = if bytes_per_sample == 2 {
                u16::from_be_bytes([bytes[0], bytes[1]]) as u32
            } else {
                bytes[0] as u32
            };
            *sample = scale(value, header.maxval);
        }
        store(pixel, header.channels, samples);
    }
    Ok(())
}

fn decode_ascii_samples(input: &[u8], header: &Header, pixels: &mut [u8]) -> Result<()> {
    let mut pos = header.data_start;
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let mut samples = [0u8; 3];
        for sample in samples.iter_mut().take(header.channels) {
            *sample = scale(token(input, &mut pos)?, header.maxval);
        }
        store(pixel, header.channels, samples);
    }
    Ok(())
}

/// In a PBM, 1 is black. P1 digits need no whitespace between them.
fn decode_ascii_bits(input: &[u8], header: &Header, pixels: &mut [u8]) -> Result<()> {
    let mut pos = header.data_start;
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pos = skip_space(input, pos);
        let value = match input.get(pos) {
            Some(b'0') => 255,
            Some(b'1') => 0,
            Some(_) => return Err(ImageError::InvalidData("expected a PBM bit")),
            None => return Err(ImageError::UnexpectedEnd),
        };
        pos += 1;
        pixel.copy_from_slice(&[value, value, value, 255]);
    }
    Ok(())
}

/// P4 rows are packed most significant bit first and padded to a byte.
fn decode_raw_bits(input: &[u8], header: &Header, pixels: &mut [u8]) -> Result<()> {
    let stride = header.width.div_ceil(8);
    let raster = stride
        .checked_mul(header.height)
        .ok_or(ImageError::Overflow)?;
    need(input.len(), header.data_start, raster)?;
    let data = &input[header.data_start..header.data_start + raster];
    for (row, bytes) in pixels
        .chunks_exact_mut(header.width * 4)
        .zip(data.chunks_exact(stride))
    {
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let value = if bytes[x / 8] & (0x80 >> (x % 8)) != 0 {
                0
            } else {
                255
            };
            pixel.copy_from_slice(&[value, value, value, 255]);
        }
    }
    Ok(())
}

/// Decodes a P1 to P6 image into RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8]) -> Result<ImageInfo> {
    let header = parse(input)?;
    let info = ImageInfo {
        width: header.width as u32,
        height: header.height as u32,
        format: ImageFormat::Ppm,
    };
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    let pixels = &mut out[..out_len];
    match header.encoding {
        Encoding::AsciiBits => decode_ascii_bits(input, &header, pixels)?,
        Encoding::AsciiSamples => decode_ascii_samples(input, &header, pixels)?,
        Encoding::RawBits => decode_raw_bits(input, &header, pixels)?,
        Encoding::RawSamples => decode_raw_samples(input, &header, pixels)?,
    }
    Ok(info)
}

/// Encodes an RGBA8888 image as a binary P6 PPM.
pub fn encode_rgba(width: u32, height: u32, pixels: &[u8], out: &mut [u8]) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(ImageError::InvalidData("image has no pixels"));
    }
    let pixel_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(ImageError::Overflow)?;
    if pixels.len() < pixel_len {
        return Err(ImageError::BufferTooSmall {
            required: pixel_len,
            available: pixels.len(),
        });
    }

    let header = format_args_len(width, height);
    let total = header
        .checked_add(pixel_len / 4 * 3)
        .ok_or(ImageError::Overflow)?;
    if out.len() < total {
        return Err(ImageError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }
    let mut text = [0u8; 48];
    let mut len = 0usize;
    text[..3].copy_from_slice(b"P6\n");
    len += 3;
    len += write_number(&mut text[len..], width);
    text[len] = b' ';
    len += 1;
    len += write_number(&mut text[len..], height);
    text[len..len + 5].copy_from_slice(b"\n255\n");
    len += 5;
    out[..len].copy_from_slice(&text[..len]);

    let mut pos = len;
    for pixel in pixels
        .as_chunks::<4>().0.iter()
        .take(width as usize * height as usize)
    {
        out[pos] = pixel[0];
        out[pos + 1] = pixel[1];
        out[pos + 2] = pixel[2];
        pos += 3;
    }
    Ok(pos)
}

fn write_number(out: &mut [u8], value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut v = value;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    while v > 0 {
        digits[count] = b'0' + (v % 10) as u8;
        v /= 10;
        count += 1;
    }
    for i in 0..count {
        out[i] = digits[count - 1 - i];
    }
    count
}

fn format_args_len(width: u32, height: u32) -> usize {
    let digits = |mut value: u32| {
        let mut n = 1;
        while value >= 10 {
            value /= 10;
            n += 1;
        }
        n
    };
    3 + digits(width) + 1 + digits(height) + 5
}
