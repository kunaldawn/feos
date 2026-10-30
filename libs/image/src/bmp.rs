//! Windows BMP decoding (1/4/8/16/24/32-bit) and 24-bit encoding.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

struct Header {
    width: usize,
    height: usize,
    top_down: bool,
    bpp: u16,
    compression: u32,
    palette_offset: usize,
    palette_len: usize,
    palette_stride: usize,
    data_offset: usize,
    stride: usize,
    masks: [u32; 4],
}

fn le_u16(input: &[u8], offset: usize) -> Result<u16> {
    need(input.len(), offset, 2)?;
    Ok(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn le_u32(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn parse(input: &[u8]) -> Result<Header> {
    if input.len() < 14 || &input[..2] != b"BM" {
        return Err(ImageError::UnknownFormat);
    }
    let data_offset = le_u32(input, 10)? as usize;
    let dib_size = le_u32(input, 14)? as usize;
    let (width, height, bpp, compression, colors_used, masks) = if dib_size == 12 {
        let width = le_u16(input, 18)? as usize;
        let height = le_u16(input, 20)? as usize;
        let bpp = le_u16(input, 24)?;
        (width, height, bpp, 0, 0, [0u32; 4])
    } else if dib_size >= 40 {
        let width = le_u32(input, 18)? as i32;
        let height = le_u32(input, 22)? as i32;
        if width <= 0 || height == 0 {
            return Err(ImageError::InvalidData("bad BMP dimensions"));
        }
        let bpp = le_u16(input, 28)?;
        let compression = le_u32(input, 30)?;
        let colors_used = le_u32(input, 46)?;
        let mut masks = [0u32; 4];
        if compression == 3 {
            if dib_size >= 52 {
                masks[0] = le_u32(input, 54)?;
                masks[1] = le_u32(input, 58)?;
                masks[2] = le_u32(input, 62)?;
                if dib_size >= 56 {
                    masks[3] = le_u32(input, 66)?;
                }
            } else {
                masks[0] = le_u32(input, 14 + 40)?;
                masks[1] = le_u32(input, 14 + 44)?;
                masks[2] = le_u32(input, 14 + 48)?;
            }
        }
        (
            width as usize,
            height.unsigned_abs() as usize,
            bpp,
            compression,
            colors_used,
            masks,
        )
    } else {
        return Err(ImageError::UnsupportedFeature("BMP DIB header"));
    };

    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(ImageError::UnsupportedFeature("BMP bit depth"));
    }
    if !matches!(compression, 0 | 3) {
        return Err(ImageError::UnsupportedFeature("BMP compression"));
    }
    if width == 0 || height == 0 {
        return Err(ImageError::InvalidData("bad BMP dimensions"));
    }
    if width > 1 << 24 || height > 1 << 24 {
        return Err(ImageError::InvalidData("BMP dimensions too large"));
    }

    let palette_len = if bpp <= 8 {
        let max = 1usize << bpp;
        let used = colors_used as usize;
        if used == 0 || used > max {
            max
        } else {
            used
        }
    } else {
        0
    };
    let palette_stride = if dib_size == 12 { 3 } else { 4 };
    let palette_offset = 14 + dib_size;
    let stride = ((width * bpp as usize + 31) / 32) * 4;

    Ok(Header {
        width,
        height,
        top_down: dib_size >= 40 && (le_u32(input, 22)? as i32) < 0,
        bpp,
        compression,
        palette_offset,
        palette_len,
        palette_stride,
        data_offset,
        stride,
        masks,
    })
}

fn raster_len(header: &Header) -> Result<usize> {
    let end = header
        .data_offset
        .checked_add(
            header
                .stride
                .checked_mul(header.height)
                .ok_or(ImageError::Overflow)?,
        )
        .ok_or(ImageError::Overflow)?;
    Ok(end)
}

/// Returns the dimensions of a BMP image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let header = parse(input)?;
    Ok(ImageInfo {
        width: header.width as u32,
        height: header.height as u32,
        format: ImageFormat::Bmp,
    })
}

/// BMP decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

fn extract(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 255;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let raw = (value & mask) >> shift;
    if max == 0 {
        0
    } else {
        ((raw as u64 * 255 + max as u64 / 2) / max as u64) as u8
    }
}

/// Decodes a BMP into RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8]) -> Result<ImageInfo> {
    let header = parse(input)?;
    let info = ImageInfo {
        width: header.width as u32,
        height: header.height as u32,
        format: ImageFormat::Bmp,
    };
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    need(input.len(), 0, raster_len(&header)?)?;

    if header.bpp <= 8 {
        need(
            input.len(),
            header.palette_offset,
            header.palette_len * header.palette_stride,
        )?;
    }

    let default_masks = if header.bpp == 16 {
        [0x7c00, 0x03e0, 0x001f, 0]
    } else {
        [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000]
    };
    let masks = if header.compression == 3 {
        header.masks
    } else {
        default_masks
    };

    for row in 0..header.height {
        let source_row = if header.top_down {
            row
        } else {
            header.height - 1 - row
        };
        let row_start = header.data_offset + source_row * header.stride;
        for x in 0..header.width {
            let (r, g, b, a) = match header.bpp {
                1 | 4 | 8 => {
                    let index = match header.bpp {
                        8 => input[row_start + x] as usize,
                        4 => {
                            let byte = input[row_start + x / 2];
                            ((if x % 2 == 0 { byte >> 4 } else { byte & 0x0f }) & 0x0f) as usize
                        }
                        _ => {
                            let byte = input[row_start + x / 8];
                            ((byte >> (7 - x % 8)) & 1) as usize
                        }
                    };
                    if index >= header.palette_len {
                        return Err(ImageError::InvalidData("BMP palette index out of range"));
                    }
                    let base = header.palette_offset + index * header.palette_stride;
                    let b = input[base];
                    let g = input[base + 1];
                    let r = input[base + 2];
                    (r, g, b, 255)
                }
                16 => {
                    let value = le_u16(input, row_start + x * 2)? as u32;
                    (
                        extract(value, masks[0]),
                        extract(value, masks[1]),
                        extract(value, masks[2]),
                        255,
                    )
                }
                24 => {
                    let base = row_start + x * 3;
                    (input[base + 2], input[base + 1], input[base], 255)
                }
                _ => {
                    let base = row_start + x * 4;
                    let alpha = if header.compression == 3 && masks[3] != 0 {
                        extract(le_u32(input, base)?, masks[3])
                    } else {
                        255
                    };
                    (input[base + 2], input[base + 1], input[base], alpha)
                }
            };
            let index = (row * header.width + x) * 4;
            out[index] = r;
            out[index + 1] = g;
            out[index + 2] = b;
            out[index + 3] = a;
        }
    }
    Ok(info)
}

/// Encodes an RGBA8888 image as a 24-bit BMP.
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
    let stride = ((width as usize * 3 + 3) / 4) * 4;
    let data_size = stride
        .checked_mul(height as usize)
        .ok_or(ImageError::Overflow)?;
    let total = 14 + 40 + data_size;
    if out.len() < total {
        return Err(ImageError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }

    out[..2].copy_from_slice(b"BM");
    out[2..6].copy_from_slice(&(total as u32).to_le_bytes());
    out[6..10].fill(0);
    out[10..14].copy_from_slice(&54u32.to_le_bytes());

    out[14..18].copy_from_slice(&40u32.to_le_bytes());
    out[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    out[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    out[26..28].copy_from_slice(&1u16.to_le_bytes());
    out[28..30].copy_from_slice(&24u16.to_le_bytes());
    out[30..34].copy_from_slice(&0u32.to_le_bytes());
    out[34..38].copy_from_slice(&(data_size as u32).to_le_bytes());
    out[38..42].copy_from_slice(&2835u32.to_le_bytes());
    out[42..46].copy_from_slice(&2835u32.to_le_bytes());
    out[46..50].copy_from_slice(&0u32.to_le_bytes());
    out[50..54].copy_from_slice(&0u32.to_le_bytes());

    for y in 0..height as usize {
        let source_row = height as usize - 1 - y;
        let start = 54 + y * stride;
        for x in 0..width as usize {
            let src = (source_row * width as usize + x) * 4;
            let dst = start + x * 3;
            out[dst] = pixels[src + 2];
            out[dst + 1] = pixels[src + 1];
            out[dst + 2] = pixels[src];
        }
        for byte in &mut out[start + width as usize * 3..start + stride] {
            *byte = 0;
        }
    }
    Ok(total)
}
