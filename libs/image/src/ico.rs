//! Windows icons (`.ico`).
//!
//! A file holds one or more images, each either a PNG or a DIB. A DIB here is a
//! BMP with no file header, twice its real height, and two pictures stacked: the
//! colours bottom-up, then a one-bit AND mask where 1 means transparent.
//!
//! # Alpha
//!
//! 32-bit icons hold straight (not premultiplied) BGRA, so an edge pixel with
//! alpha 96 keeps its full colour. Soft dark edges in such icons are real: they
//! are the artwork's drop shadow, black at low alpha. Fully transparent pixels
//! are stored as black, which is why anything that discards alpha, or that scales
//! the colours without weighting them by alpha, draws black fringes. This decoder
//! hands out straight RGBA, so compositors must blend with
//! `c * a + background * (1 - a)`, and must scale premultiplied.
//!
//! The AND mask applies to images below 32 bits. A 32-bit image whose alpha is
//! zero throughout, which old writers produced, is treated as having no alpha
//! and uses its mask as well.

use crate::error::{ImageError, Result};
use crate::{png, ImageFormat, ImageInfo};

const HEADER: usize = 6;
const ENTRY: usize = 16;
const INFO_HEADER: usize = 40;
const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One entry of the directory, with its payload.
#[derive(Clone, Copy)]
pub(crate) struct Entry<'a> {
    /// The depth for an icon. A cursor keeps its hot spot's y here.
    pub(crate) bits: u16,
    /// The planes for an icon. A cursor keeps its hot spot's x here.
    pub(crate) planes: u16,
    pub(crate) data: &'a [u8],
}

fn read_u16(input: &[u8], at: usize) -> Result<u16> {
    let bytes = input.get(at..at + 2).ok_or(ImageError::UnexpectedEnd)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(input: &[u8], at: usize) -> Result<u32> {
    let bytes = input.get(at..at + 4).ok_or(ImageError::UnexpectedEnd)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The directory type of an icon file.
pub(crate) const TYPE_ICON: u8 = 1;
/// The directory type of a cursor file, whose entries hold a hot spot where an
/// icon's hold planes and depth.
pub(crate) const TYPE_CURSOR: u8 = 2;

/// Whether `input` looks like an icon. The signature is short, so this checks
/// what the directory says too.
pub fn is_magic(input: &[u8]) -> bool {
    if !has_directory(input, TYPE_ICON) {
        return false;
    }
    let planes = u16::from_le_bytes([input[HEADER + 4], input[HEADER + 5]]);
    planes <= 1
}

/// Whether `input` starts with a directory of `kind` holding at least one entry.
pub(crate) fn has_directory(input: &[u8], kind: u8) -> bool {
    if input.len() < HEADER + ENTRY || input[..4] != [0, 0, kind, 0] {
        return false;
    }
    let count = u16::from_le_bytes([input[4], input[5]]) as usize;
    count != 0 && input[HEADER + 3] == 0
}

pub(crate) fn entry(input: &[u8], index: usize) -> Result<Entry<'_>> {
    let at = HEADER + index * ENTRY;
    input.get(at..at + ENTRY).ok_or(ImageError::UnexpectedEnd)?;
    let size = read_u32(input, at + 8)? as usize;
    let offset = read_u32(input, at + 12)? as usize;
    let end = offset.checked_add(size).ok_or(ImageError::Overflow)?;
    let data = input.get(offset..end).ok_or(ImageError::UnexpectedEnd)?;
    Ok(Entry {
        bits: read_u16(input, at + 6)?,
        planes: read_u16(input, at + 4)?,
        data,
    })
}

/// The dimensions and depth of an entry, from its payload rather than the
/// directory, which writers often get wrong.
pub(crate) fn measure(entry: &Entry<'_>) -> Result<(u32, u32, u16)> {
    if entry.data.starts_with(PNG_SIGNATURE) {
        let info = png::info(entry.data)?;
        return Ok((info.width, info.height, 32));
    }
    let header = read_u32(entry.data, 0)? as usize;
    if header < INFO_HEADER {
        return Err(ImageError::InvalidData("icon DIB header"));
    }
    let width = read_u32(entry.data, 4)? as i32;
    let height = read_u32(entry.data, 8)? as i32;
    let bits = read_u16(entry.data, 14)?;
    if width <= 0 || height <= 0 || height % 2 != 0 || width > 4096 || height > 8192 {
        return Err(ImageError::InvalidData("icon DIB dimensions"));
    }
    Ok((width as u32, height as u32 / 2, bits))
}

/// The image worth showing: the most pixels, and then the most colours.
fn best(input: &[u8]) -> Result<Entry<'_>> {
    if !is_magic(input) {
        return Err(ImageError::UnknownFormat);
    }
    best_entry(input)
}

/// The best entry of a directory already known to be one.
pub(crate) fn best_entry(input: &[u8]) -> Result<Entry<'_>> {
    let count = read_u16(input, 4)? as usize;
    let mut chosen: Option<(u64, u16, Entry<'_>)> = None;
    for index in 0..count {
        let Ok(found) = entry(input, index) else {
            continue;
        };
        let Ok((width, height, bits)) = measure(&found) else {
            continue;
        };
        let area = width as u64 * height as u64;
        if chosen.as_ref().is_none_or(|(a, b, _)| (area, bits) > (*a, *b)) {
            chosen = Some((area, bits, found));
        }
    }
    chosen
        .map(|(_, _, found)| found)
        .ok_or(ImageError::InvalidData("icon has no usable image"))
}

/// Reads the dimensions of the image [`decode`] would choose.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let (width, height, _) = measure(&best(input)?)?;
    Ok(ImageInfo {
        width,
        height,
        format: ImageFormat::Ico,
    })
}

/// Working memory for [`decode`]: what a PNG payload needs, and otherwise none.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    let chosen = best(input)?;
    if chosen.data.starts_with(PNG_SIGNATURE) {
        png::scratch_len(chosen.data)
    } else {
        Ok(0)
    }
}

/// Decodes the largest, deepest image into straight RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8], scratch: &mut [u8]) -> Result<ImageInfo> {
    let chosen = best(input)?;
    let info = info(input)?;
    if chosen.data.starts_with(PNG_SIGNATURE) {
        png::decode(chosen.data, out, scratch)?;
        return Ok(info);
    }
    decode_dib(chosen.data, info.width as usize, info.height as usize, out, &mut [])?;
    Ok(info)
}

/// Whether a payload is a PNG rather than a DIB.
pub(crate) fn is_png(data: &[u8]) -> bool {
    data.starts_with(PNG_SIGNATURE)
}

fn stride(width: usize, bits: usize) -> Result<usize> {
    let row_bits = width.checked_mul(bits).ok_or(ImageError::Overflow)?;
    Ok(row_bits.div_ceil(32) * 4)
}

/// Decodes a DIB payload into straight RGBA.
///
/// Where the AND mask is set over a colour other than black, the image asks for
/// the screen under it to be inverted, which RGBA can't say. Those pixels come
/// out transparent (their colour is left in place), and their bits are set in `invert` (row by row, one bit a
/// pixel, the first pixel in the low bit) when it's not empty.
pub(crate) fn decode_dib(
    data: &[u8],
    width: usize,
    height: usize,
    out: &mut [u8],
    invert: &mut [u8],
) -> Result<()> {
    let required = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageError::Overflow)?;
    if out.len() < required {
        return Err(ImageError::BufferTooSmall {
            required,
            available: out.len(),
        });
    }
    let invert_len = (width * height).div_ceil(8);
    if !invert.is_empty() {
        if invert.len() < invert_len {
            return Err(ImageError::BufferTooSmall {
                required: invert_len,
                available: invert.len(),
            });
        }
        invert[..invert_len].fill(0);
    }
    let header = read_u32(data, 0)? as usize;
    let bits = read_u16(data, 14)? as usize;
    let compression = read_u32(data, 16)?;
    if compression != 0 {
        return Err(ImageError::UnsupportedFeature("compressed icon DIB"));
    }
    let palette_len = match bits {
        1 | 4 | 8 => {
            let used = read_u32(data, 32)? as usize;
            if used == 0 || used > 1 << bits {
                1 << bits
            } else {
                used
            }
        }
        24 | 32 => 0,
        _ => return Err(ImageError::UnsupportedFeature("icon bit depth")),
    };
    let palette_start = header;
    let colours = palette_start
        .checked_add(palette_len * 4)
        .ok_or(ImageError::Overflow)?;
    let palette = data
        .get(palette_start..colours)
        .ok_or(ImageError::UnexpectedEnd)?;
    let colour_stride = stride(width, bits)?;
    let colour_len = colour_stride
        .checked_mul(height)
        .ok_or(ImageError::Overflow)?;
    let mask_start = colours.checked_add(colour_len).ok_or(ImageError::Overflow)?;
    let pixels = data
        .get(colours..mask_start)
        .ok_or(ImageError::UnexpectedEnd)?;
    let mask_stride = stride(width, 1)?;
    // Some writers leave a 32-bit image's mask out, as it has alpha instead.
    let mask = data
        .get(mask_start..mask_start.saturating_add(mask_stride * height))
        .ok_or(ImageError::UnexpectedEnd);
    let mask = match (bits, mask) {
        (32, Err(_)) => None,
        (_, mask) => Some(mask?),
    };
    // Rows are stored bottom-up.
    let mut any_alpha = false;
    for y in 0..height {
        let row = &pixels[(height - 1 - y) * colour_stride..][..colour_stride];
        let target = &mut out[y * width * 4..(y + 1) * width * 4];
        for (x, pixel) in target.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (b, g, r, a) = match bits {
                32 => {
                    let at = x * 4;
                    (row[at], row[at + 1], row[at + 2], row[at + 3])
                }
                24 => {
                    let at = x * 3;
                    (row[at], row[at + 1], row[at + 2], 255)
                }
                _ => {
                    let index = match bits {
                        8 => row[x] as usize,
                        4 => (row[x / 2] >> if x % 2 == 0 { 4 } else { 0 } & 0x0f) as usize,
                        _ => (row[x / 8] >> (7 - x % 8) & 1) as usize,
                    };
                    let entry = palette
                        .get(index * 4..index * 4 + 4)
                        .ok_or(ImageError::InvalidData("icon palette index"))?;
                    (entry[0], entry[1], entry[2], 255)
                }
            };
            any_alpha |= bits == 32 && a != 0;
            pixel.copy_from_slice(&[r, g, b, a]);
        }
    }
    if bits == 32 && any_alpha {
        return Ok(());
    }
    if let Some(mask) = mask {
        for y in 0..height {
            let row = &mask[(height - 1 - y) * mask_stride..][..mask_stride];
            let target = &mut out[y * width * 4..(y + 1) * width * 4];
            for (x, pixel) in target.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let transparent = row[x / 8] >> (7 - x % 8) & 1 != 0;
                if transparent && pixel[..3] != [0, 0, 0]
                    && !invert.is_empty() {
                        let at = y * width + x;
                        invert[at / 8] |= 1 << (at % 8);
                    }
                pixel[3] = if transparent { 0 } else { 255 };
            }
        }
    } else {
        // Nothing says what's transparent, so nothing is.
        for pixel in out[..required].as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode as decode_any, detect};
    use std::vec;
    use std::vec::Vec;

    /// An icon file holding one 2x2 DIB of the given depth. `colours` holds the
    /// rows bottom-up and already padded; `mask` likewise.
    fn dib_icon(bits: u16, palette: &[u8], colours: &[u8], mask: &[u8]) -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&4i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&bits.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        dib.extend_from_slice(palette);
        dib.extend_from_slice(colours);
        dib.extend_from_slice(mask);
        let mut file = vec![0, 0, 1, 0, 1, 0, 2, 2, 0, 0, 1, 0];
        file.extend_from_slice(&bits.to_le_bytes());
        file.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        file.extend_from_slice(&22u32.to_le_bytes());
        file.extend_from_slice(&dib);
        file
    }

    fn rgba(file: &[u8]) -> Vec<u8> {
        assert_eq!(detect(file), Some(ImageFormat::Ico));
        let info = crate::info(file).unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        let mut out = vec![0u8; 16];
        decode_any(file, &mut out, &mut []).unwrap();
        out
    }

    // Top row: mask 0 then 1. Bottom row: mask 1 then 0.
    const MASK: [u8; 8] = [0b0100_0000, 0, 0, 0, 0b1000_0000, 0, 0, 0];

    #[test]
    fn one_bit_uses_its_palette_and_mask() {
        let palette = [0, 0, 255, 0, 0, 255, 0, 0];
        // Bottom row 1,0; top row 0,1.
        let colours = [0b1000_0000, 0, 0, 0, 0b0100_0000, 0, 0, 0];
        let out = rgba(&dib_icon(1, &palette, &colours, &MASK));
        // Bottom-up: the first stored row is the bottom one.
        assert_eq!(&out[0..4], &[255, 0, 0, 0]);
        assert_eq!(&out[4..8], &[0, 255, 0, 255]);
        assert_eq!(&out[8..12], &[0, 255, 0, 255]);
        assert_eq!(&out[12..16], &[255, 0, 0, 0]);
    }

    #[test]
    fn four_bit_reads_high_nibble_first() {
        let mut palette = [0u8; 64];
        palette[4..8].copy_from_slice(&[1, 2, 3, 0]);
        palette[8..12].copy_from_slice(&[4, 5, 6, 0]);
        let colours = [0x12, 0, 0, 0, 0x21, 0, 0, 0];
        let out = rgba(&dib_icon(4, &palette, &colours, &[0; 8]));
        assert_eq!(&out[0..4], &[6, 5, 4, 255]);
        assert_eq!(&out[4..8], &[3, 2, 1, 255]);
        assert_eq!(&out[8..12], &[3, 2, 1, 255]);
        assert_eq!(&out[12..16], &[6, 5, 4, 255]);
    }

    #[test]
    fn eight_bit_and_twenty_four_bit_apply_the_mask() {
        let mut palette = vec![0u8; 1024];
        palette[4 * 7..4 * 7 + 3].copy_from_slice(&[10, 20, 30]);
        let colours = [7, 7, 0, 0, 7, 7, 0, 0];
        let out = rgba(&dib_icon(8, &palette, &colours, &MASK));
        assert_eq!(&out[0..4], &[30, 20, 10, 0]);
        assert_eq!(&out[4..8], &[30, 20, 10, 255]);

        let colours = [1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0];
        let out = rgba(&dib_icon(24, &[], &colours, &MASK));
        assert_eq!(&out[0..4], &[9, 8, 7, 0]);
        assert_eq!(&out[4..8], &[12, 11, 10, 255]);
        assert_eq!(&out[8..12], &[3, 2, 1, 255]);
        assert_eq!(&out[12..16], &[6, 5, 4, 0]);
    }

    #[test]
    fn thirty_two_bit_alpha_is_straight_and_beats_the_mask() {
        let colours = [
            10, 20, 30, 128, 40, 50, 60, 0, // bottom row
            70, 80, 90, 255, 100, 110, 120, 1, // top row
        ];
        // The mask would make every pixel transparent.
        let out = rgba(&dib_icon(32, &[], &colours, &[0xff; 8]));
        assert_eq!(&out[0..4], &[90, 80, 70, 255]);
        assert_eq!(&out[4..8], &[120, 110, 100, 1], "colour isn't premultiplied");
        assert_eq!(&out[8..12], &[30, 20, 10, 128]);
        assert_eq!(&out[12..16], &[60, 50, 40, 0]);
    }

    #[test]
    fn thirty_two_bit_with_no_alpha_falls_back_on_the_mask() {
        let colours = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        let out = rgba(&dib_icon(32, &[], &colours, &MASK));
        assert_eq!(out[3], 0);
        assert_eq!(out[7], 255);
        assert_eq!(out[11], 255);
        assert_eq!(out[15], 0);
    }

    #[test]
    fn a_png_payload_decodes_as_png() {
        let mut encoded = vec![0u8; 4096];
        let pixels = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let len = png::encode_rgba(2, 2, &pixels, &mut encoded).unwrap();
        let mut file = vec![0, 0, 1, 0, 1, 0, 2, 2, 0, 0, 1, 0, 32, 0];
        file.extend_from_slice(&(len as u32).to_le_bytes());
        file.extend_from_slice(&22u32.to_le_bytes());
        file.extend_from_slice(&encoded[..len]);
        let mut out = [0u8; 16];
        let mut scratch = vec![0u8; crate::scratch_len(&file).unwrap()];
        decode_any(&file, &mut out, &mut scratch).unwrap();
        assert_eq!(out, pixels);
    }

    #[test]
    fn the_largest_deepest_image_is_chosen() {
        let small = dib_icon(24, &[], &[0; 16], &[0; 8]);
        // Two directory entries: a 1x1 at the small one's data, patched below.
        let mut one = small.clone();
        one[22 + 4] = 1;
        one[22 + 8] = 2;
        let body = &one[22..];
        let mut file = vec![0, 0, 1, 0, 2, 0];
        for (w, bits, offset) in [(1u8, 24u16, 38u32), (2, 24, 38 + body.len() as u32)] {
            file.extend_from_slice(&[w, w, 0, 0, 1, 0]);
            file.extend_from_slice(&bits.to_le_bytes());
            file.extend_from_slice(&(body.len() as u32).to_le_bytes());
            file.extend_from_slice(&offset.to_le_bytes());
        }
        file.extend_from_slice(body);
        file.extend_from_slice(&small[22..]);
        assert_eq!(info(&file).unwrap().width, 2);
    }

    #[test]
    fn a_real_32_bit_icon_has_soft_edges_in_straight_alpha() {
        let file = include_bytes!("../../../assets/icons/system/shutdown-padded-48.ico");
        let info = crate::info(file).unwrap();
        assert_eq!((info.width, info.height), (48, 48));
        let mut out = vec![0u8; info.rgba_len().unwrap()];
        decode_any(file, &mut out, &mut []).unwrap();
        let pixels: Vec<&[u8]> = out.chunks_exact(4).collect();
        assert_eq!(pixels[0][3], 0, "the corner is clear");
        // A premultiplied pixel can't have a channel above its alpha.
        assert!(pixels
            .iter()
            .any(|p| p[3] > 0 && p[3] < 255 && p[..3].iter().any(|&c| c > p[3])));
    }

    #[test]
    fn real_icons_at_every_depth_decode() {
        for file in [
            &include_bytes!("../../../assets/icons/apps/application-window-32.ico")[..],
            &include_bytes!("../../../assets/icons/system/shutdown-padded-16.ico")[..],
        ] {
            let info = crate::info(file).unwrap();
            let mut out = vec![0u8; info.rgba_len().unwrap()];
            decode_any(file, &mut out, &mut []).unwrap();
            assert!(out.chunks_exact(4).any(|p| p[3] != 0));
        }
    }

    #[test]
    fn damage_is_an_error_and_not_a_panic() {
        let good = dib_icon(8, &[0; 1024], &[0; 8], &[0; 8]);
        for cut in 0..good.len() {
            let mut out = [0u8; 16];
            let _ = decode(&good[..cut], &mut out, &mut []);
            let _ = info(&good[..cut]);
        }
        for at in 0..good.len() {
            let mut bent = good.clone();
            bent[at] ^= 0xff;
            let mut out = vec![0u8; 64];
            let _ = decode(&bent, &mut out, &mut []);
        }
        // Too small an output is reported.
        let mut out = [0u8; 4];
        assert!(matches!(
            decode(&good, &mut out, &mut []),
            Err(ImageError::BufferTooSmall { .. })
        ));
    }
}
