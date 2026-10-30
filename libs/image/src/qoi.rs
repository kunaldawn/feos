//! Quite OK Image (QOI) decoding and encoding.

use crate::error::{ImageError, Result};
use crate::{ImageFormat, ImageInfo};

const MAGIC: [u8; 4] = *b"qoif";
const HEADER_LEN: usize = 14;
const END_MARKER: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

fn dimensions(input: &[u8]) -> Result<(usize, usize)> {
    if input.len() < HEADER_LEN || input[..4] != MAGIC {
        return Err(ImageError::UnknownFormat);
    }
    let width = u32::from_be_bytes([input[4], input[5], input[6], input[7]]) as usize;
    let height = u32::from_be_bytes([input[8], input[9], input[10], input[11]]) as usize;
    if width == 0 || height == 0 {
        return Err(ImageError::InvalidData("QOI image has no pixels"));
    }
    width.checked_mul(height).ok_or(ImageError::Overflow)?;
    Ok((width, height))
}

/// Returns the dimensions of a QOI image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let (width, height) = dimensions(input)?;
    Ok(ImageInfo {
        width: width as u32,
        height: height as u32,
        format: ImageFormat::Qoi,
    })
}

/// QOI decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

fn hash(pixel: [u8; 4]) -> usize {
    (pixel[0] as usize * 3 + pixel[1] as usize * 5 + pixel[2] as usize * 7 + pixel[3] as usize * 11)
        % 64
}

/// Decodes a QOI image into RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8]) -> Result<ImageInfo> {
    let (width, height) = dimensions(input)?;
    let info = ImageInfo {
        width: width as u32,
        height: height as u32,
        format: ImageFormat::Qoi,
    };
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }

    let mut index = [[0u8; 4]; 64];
    let mut pixel = [0u8, 0, 0, 255];
    let mut pos = HEADER_LEN;
    let pixels = width * height;
    let mut emitted = 0usize;
    let mut run = 0usize;

    while emitted < pixels {
        if run > 0 {
            run -= 1;
        } else {
            let op = *input.get(pos).ok_or(ImageError::UnexpectedEnd)?;
            pos += 1;
            match op {
                0xfe => {
                    if pos + 3 > input.len() {
                        return Err(ImageError::UnexpectedEnd);
                    }
                    pixel[0] = input[pos];
                    pixel[1] = input[pos + 1];
                    pixel[2] = input[pos + 2];
                    pos += 3;
                }
                0xff => {
                    if pos + 4 > input.len() {
                        return Err(ImageError::UnexpectedEnd);
                    }
                    pixel.copy_from_slice(&input[pos..pos + 4]);
                    pos += 4;
                }
                0x00..=0x3f => {
                    pixel = index[op as usize];
                }
                0x40..=0x7f => {
                    pixel[0] = pixel[0].wrapping_add(((op >> 4) & 3).wrapping_sub(2));
                    pixel[1] = pixel[1].wrapping_add(((op >> 2) & 3).wrapping_sub(2));
                    pixel[2] = pixel[2].wrapping_add((op & 3).wrapping_sub(2));
                }
                0x80..=0xbf => {
                    let second = *input.get(pos).ok_or(ImageError::UnexpectedEnd)?;
                    pos += 1;
                    let dg = (op & 0x3f).wrapping_sub(32);
                    pixel[0] = pixel[0]
                        .wrapping_add(dg.wrapping_sub(8).wrapping_add((second >> 4) & 0x0f));
                    pixel[1] = pixel[1].wrapping_add(dg);
                    pixel[2] =
                        pixel[2].wrapping_add(dg.wrapping_sub(8).wrapping_add(second & 0x0f));
                }
                _ => {
                    run = (op & 0x3f) as usize;
                }
            }
            index[hash(pixel)] = pixel;
        }

        out[emitted * 4..emitted * 4 + 4].copy_from_slice(&pixel);
        emitted += 1;
    }
    Ok(info)
}

/// Encodes an RGBA8888 image as QOI.
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
    let required = HEADER_LEN
        .checked_add(pixel_len / 4 * 5)
        .and_then(|n| n.checked_add(END_MARKER.len()))
        .ok_or(ImageError::Overflow)?;
    if out.len() < required {
        return Err(ImageError::BufferTooSmall {
            required,
            available: out.len(),
        });
    }

    out[..4].copy_from_slice(&MAGIC);
    out[4..8].copy_from_slice(&width.to_be_bytes());
    out[8..12].copy_from_slice(&height.to_be_bytes());
    out[12] = 4;
    out[13] = 0;

    let mut pos = HEADER_LEN;
    let mut previous = [0u8, 0, 0, 255];
    for pixel in pixels[..pixel_len].chunks_exact(4) {
        if pixel[3] == previous[3] {
            out[pos] = 0xfe;
            out[pos + 1] = pixel[0];
            out[pos + 2] = pixel[1];
            out[pos + 3] = pixel[2];
            pos += 4;
        } else {
            out[pos] = 0xff;
            out[pos + 1..pos + 5].copy_from_slice(pixel);
            pos += 5;
        }
        previous.copy_from_slice(pixel);
    }
    out[pos..pos + END_MARKER.len()].copy_from_slice(&END_MARKER);
    Ok(pos + END_MARKER.len())
}
