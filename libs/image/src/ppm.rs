//! Netpbm PPM decoding (P5/P6) and P6 encoding.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

struct Header {
    width: usize,
    height: usize,
    maxval: u32,
    channels: usize,
    data_start: usize,
}

fn skip_space(input: &[u8], mut pos: usize) -> usize {
    loop {
        while pos < input.len() && matches!(input[pos], b' ' | b'\t' | b'\r' | b'\n') {
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
        return Err(ImageError::InvalidData("expected a PPM number"));
    }
    Ok(value)
}

fn parse(input: &[u8]) -> Result<Header> {
    need(input.len(), 0, 2)?;
    if input[0] != b'P' || !matches!(input[1], b'5' | b'6') {
        return Err(ImageError::UnknownFormat);
    }
    let channels = if input[1] == b'6' { 3 } else { 1 };
    let mut pos = 2;
    let width = token(input, &mut pos)? as usize;
    let height = token(input, &mut pos)? as usize;
    let maxval = token(input, &mut pos)?;
    if width == 0 || height == 0 || maxval == 0 || maxval > 65535 {
        return Err(ImageError::InvalidData("bad PPM header"));
    }
    // Exactly one whitespace character separates the header from the raster.
    need(input.len(), pos, 1)?;
    if input[pos] == b'\r' && input.get(pos + 1) == Some(&b'\n') {
        pos += 2;
    } else if matches!(input[pos], b' ' | b'\t' | b'\r' | b'\n') {
        pos += 1;
    } else {
        return Err(ImageError::InvalidData("missing PPM raster separator"));
    }
    Ok(Header {
        width,
        height,
        maxval,
        channels,
        data_start: pos,
    })
}

fn raster_len(header: &Header) -> Result<usize> {
    let bytes_per_sample = if header.maxval < 256 { 1 } else { 2 };
    header
        .width
        .checked_mul(header.height)
        .and_then(|pixels| pixels.checked_mul(header.channels))
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or(ImageError::Overflow)
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

/// Decodes a P5/P6 image into RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8]) -> Result<ImageInfo> {
    let header = parse(input)?;
    let info = info(input)?;
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    let raster = raster_len(&header)?;
    need(input.len(), header.data_start, raster)?;
    let data = &input[header.data_start..header.data_start + raster];
    let two_bytes = header.maxval >= 256;
    let max = header.maxval as u32;

    let sample = |index: usize| -> u8 {
        let value = if two_bytes {
            u16::from_be_bytes([data[index * 2], data[index * 2 + 1]]) as u32
        } else {
            data[index] as u32
        };
        if max == 255 {
            value as u8
        } else {
            ((value * 255 + max / 2) / max) as u8
        }
    };

    let pixels = header.width * header.height;
    for i in 0..pixels {
        let base = i * header.channels;
        let (r, g, b) = if header.channels == 3 {
            (sample(base), sample(base + 1), sample(base + 2))
        } else {
            let v = sample(base);
            (v, v, v)
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = 255;
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
        .chunks_exact(4)
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
