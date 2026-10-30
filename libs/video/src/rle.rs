//! Microsoft RLE8/RLE4 frame decoding (the AVI `RLE ` and `RLE4` codecs).

use crate::error::{require_rgba, Result, VideoError};

fn palette_color(palette: &[u8], index: u8) -> [u8; 4] {
    let i = index as usize * 4;
    if i + 3 < palette.len() {
        [palette[i + 2], palette[i + 1], palette[i], 255]
    } else {
        [index, index, index, 255]
    }
}

#[allow(clippy::too_many_arguments)]
fn set_pixel(out: &mut [u8], width: usize, height: usize, x: usize, y: usize, color: [u8; 4]) {
    if x >= width || y >= height {
        return;
    }
    let row = height - 1 - y;
    let o = (row * width + x) * 4;
    out[o..o + 4].copy_from_slice(&color);
}

/// Decodes an RLE8 sample into RGBA. `palette` contains BGRA quads.
pub(crate) fn decode_rle8(
    data: &[u8],
    width: u32,
    height: u32,
    palette: &[u8],
    out: &mut [u8],
) -> Result<()> {
    require_rgba(out, width, height)?;
    let w = width as usize;
    let h = height as usize;
    let mut pos = 0usize;
    let mut x = 0usize;
    let mut y = 0usize;
    while pos + 1 < data.len() {
        let count = data[pos] as usize;
        let value = data[pos + 1];
        pos += 2;
        if count != 0 {
            for _ in 0..count {
                set_pixel(out, w, h, x, y, palette_color(palette, value));
                x += 1;
            }
        } else {
            match value {
                0 => {
                    x = 0;
                    y += 1;
                }
                1 => break,
                2 => {
                    if pos + 1 >= data.len() {
                        return Err(VideoError::UnexpectedEnd);
                    }
                    x += data[pos] as usize;
                    y += data[pos + 1] as usize;
                    pos += 2;
                }
                n => {
                    let n = n as usize;
                    if pos + n > data.len() {
                        return Err(VideoError::UnexpectedEnd);
                    }
                    for &index in &data[pos..pos + n] {
                        set_pixel(out, w, h, x, y, palette_color(palette, index));
                        x += 1;
                    }
                    pos += n;
                    if n & 1 != 0 {
                        pos += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Decodes an RLE4 sample into RGBA. `palette` contains BGRA quads.
pub(crate) fn decode_rle4(
    data: &[u8],
    width: u32,
    height: u32,
    palette: &[u8],
    out: &mut [u8],
) -> Result<()> {
    require_rgba(out, width, height)?;
    let w = width as usize;
    let h = height as usize;
    let mut pos = 0usize;
    let mut x = 0usize;
    let mut y = 0usize;
    while pos + 1 < data.len() {
        let count = data[pos] as usize;
        let value = data[pos + 1];
        pos += 2;
        if count != 0 {
            for i in 0..count {
                let index = if i & 1 == 0 { value >> 4 } else { value & 0x0f };
                set_pixel(out, w, h, x, y, palette_color(palette, index));
                x += 1;
            }
        } else {
            match value {
                0 => {
                    x = 0;
                    y += 1;
                }
                1 => break,
                2 => {
                    if pos + 1 >= data.len() {
                        return Err(VideoError::UnexpectedEnd);
                    }
                    x += data[pos] as usize;
                    y += data[pos + 1] as usize;
                    pos += 2;
                }
                n => {
                    let n = n as usize;
                    let bytes = (n + 1) / 2;
                    if pos + bytes > data.len() {
                        return Err(VideoError::UnexpectedEnd);
                    }
                    for i in 0..n {
                        let byte = data[pos + i / 2];
                        let index = if i & 1 == 0 { byte >> 4 } else { byte & 0x0f };
                        set_pixel(out, w, h, x, y, palette_color(palette, index));
                        x += 1;
                    }
                    pos += bytes;
                    if bytes & 1 != 0 {
                        pos += 1;
                    }
                }
            }
        }
    }
    Ok(())
}
