//! Windows BMP decoding and 24-bit encoding.
//!
//! The decoder reads 1/4/8/16/24/32-bit files, bottom-up and top-down, with
//! `BI_BITFIELDS`/`BI_ALPHABITFIELDS` masks and `BI_RLE8`/`BI_RLE4` compression.
//! Pixels an RLE stream skips take palette entry 0.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

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
        if matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS) {
            // Inside a V2+ header, or straight after a 40-byte one.
            masks[0] = le_u32(input, 54)?;
            masks[1] = le_u32(input, 58)?;
            masks[2] = le_u32(input, 62)?;
            if dib_size >= 56 || compression == BI_ALPHABITFIELDS {
                masks[3] = le_u32(input, 66)?;
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
    let valid = match compression {
        BI_RGB => true,
        BI_RLE8 => bpp == 8,
        BI_RLE4 => bpp == 4,
        BI_BITFIELDS | BI_ALPHABITFIELDS => matches!(bpp, 16 | 32),
        _ => return Err(ImageError::UnsupportedFeature("BMP compression")),
    };
    if !valid {
        return Err(ImageError::InvalidData(
            "BMP compression and depth disagree",
        ));
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
    let palette_offset = dib_size.checked_add(14).ok_or(ImageError::Overflow)?;
    let stride = (width * bpp as usize).div_ceil(32) * 4;

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

/// Scales the field `mask` selects to 8 bits. An absent field reads as zero.
fn extract(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
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
    if header.bpp <= 8 {
        let palette_bytes = header
            .palette_len
            .checked_mul(header.palette_stride)
            .ok_or(ImageError::Overflow)?;
        need(input.len(), header.palette_offset, palette_bytes)?;
    }
    if matches!(header.compression, BI_RLE8 | BI_RLE4) {
        decode_rle(input, &header, out)?;
        return Ok(info);
    }
    need(input.len(), 0, raster_len(&header)?)?;

    let default_masks = if header.bpp == 16 {
        [0x7c00, 0x03e0, 0x001f, 0]
    } else {
        [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0]
    };
    let masks = if matches!(header.compression, BI_BITFIELDS | BI_ALPHABITFIELDS) {
        header.masks
    } else {
        default_masks
    };
    let alpha = |value: u32| {
        if masks[3] != 0 {
            extract(value, masks[3])
        } else {
            255
        }
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
                    let (r, g, b) = palette_color(input, &header, index)?;
                    (r, g, b, 255)
                }
                16 => {
                    let value = le_u16(input, row_start + x * 2)? as u32;
                    (
                        extract(value, masks[0]),
                        extract(value, masks[1]),
                        extract(value, masks[2]),
                        alpha(value),
                    )
                }
                24 => {
                    let base = row_start + x * 3;
                    (input[base + 2], input[base + 1], input[base], 255)
                }
                _ => {
                    let value = le_u32(input, row_start + x * 4)?;
                    (
                        extract(value, masks[0]),
                        extract(value, masks[1]),
                        extract(value, masks[2]),
                        alpha(value),
                    )
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

fn palette_color(input: &[u8], header: &Header, index: usize) -> Result<(u8, u8, u8)> {
    if index >= header.palette_len {
        return Err(ImageError::InvalidData("BMP palette index out of range"));
    }
    let base = header.palette_offset + index * header.palette_stride;
    Ok((input[base + 2], input[base + 1], input[base]))
}

/// Decodes `BI_RLE8` and `BI_RLE4` data.
///
/// Each command is two bytes: a run count and a value, or a zero and an escape
/// (end of line, end of bitmap, delta, or a literal run). Literal runs are
/// padded to 16 bits. Drawing past the right or bottom edge is an error.
fn decode_rle(input: &[u8], header: &Header, out: &mut [u8]) -> Result<()> {
    let (background_r, background_g, background_b) = palette_color(input, header, 0)?;
    // Doubling copies, because a small file can name a very large image.
    let len = header.width * header.height * 4;
    out[..4].copy_from_slice(&[background_r, background_g, background_b, 255]);
    let mut filled = 4;
    while filled < len {
        let count = filled.min(len - filled);
        out.copy_within(0..count, filled);
        filled += count;
    }

    let four_bit = header.compression == BI_RLE4;
    let mut pos = header.data_offset;
    // Rows count in file order, so row 0 is the bottom of a bottom-up image.
    let mut x = 0usize;
    let mut row = 0usize;
    let put = |x: usize, row: usize, index: usize, out: &mut [u8]| -> Result<()> {
        if x >= header.width || row >= header.height {
            return Err(ImageError::InvalidData("BMP RLE data leaves the image"));
        }
        let y = if header.top_down {
            row
        } else {
            header.height - 1 - row
        };
        let (r, g, b) = palette_color(input, header, index)?;
        let at = (y * header.width + x) * 4;
        out[at] = r;
        out[at + 1] = g;
        out[at + 2] = b;
        Ok(())
    };
    let byte = |pos: usize| input.get(pos).copied().ok_or(ImageError::UnexpectedEnd);

    loop {
        let count = byte(pos)? as usize;
        let value = byte(pos + 1)?;
        pos += 2;
        if count != 0 {
            for i in 0..count {
                let index = match (four_bit, i % 2) {
                    (false, _) => value,
                    (true, 0) => value >> 4,
                    (true, _) => value & 0x0f,
                };
                put(x + i, row, index as usize, out)?;
            }
            x += count;
            continue;
        }
        match value {
            0 => {
                x = 0;
                row += 1;
            }
            1 => return Ok(()),
            2 => {
                x += byte(pos)? as usize;
                row += byte(pos + 1)? as usize;
                pos += 2;
            }
            literal => {
                let literal = literal as usize;
                for i in 0..literal {
                    let index = if four_bit {
                        let packed = byte(pos + i / 2)?;
                        if i % 2 == 0 {
                            packed >> 4
                        } else {
                            packed & 0x0f
                        }
                    } else {
                        byte(pos + i)?
                    };
                    put(x + i, row, index as usize, out)?;
                }
                x += literal;
                let bytes = if four_bit {
                    literal.div_ceil(2)
                } else {
                    literal
                };
                pos += bytes + bytes % 2;
            }
        }
    }
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
    let stride = (width as usize * 3).div_ceil(4) * 4;
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
