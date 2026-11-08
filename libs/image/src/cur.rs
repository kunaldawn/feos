//! Windows cursors (`.cur`).
//!
//! A cursor file is an icon file (see [`crate::ico`]) with type 2 in its header,
//! and each directory entry holds the image's hot spot where an icon's holds its
//! planes and depth. The payloads are the same: a DIB with an AND mask, or a PNG.
//!
//! Old cursors also use the AND mask to invert the screen: a masked pixel whose
//! colour isn't black flips what's under it. That's how the text beam stays
//! visible over both black and white. [`decode`] returns such pixels as
//! transparent and marks them in a separate bit mask, so a caller that wants the
//! real effect can draw it.

use crate::error::{ImageError, Result};
use crate::ico::{self, TYPE_CURSOR};
use crate::png;

/// What a cursor image is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The pixel that points, from the left.
    pub hotspot_x: u32,
    /// The pixel that points, from the top.
    pub hotspot_y: u32,
}

impl CursorInfo {
    /// Bytes [`decode`] writes as RGBA.
    pub fn rgba_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Bytes of the invert mask [`decode`] fills, one bit a pixel.
    pub fn invert_len(&self) -> usize {
        (self.width as usize * self.height as usize).div_ceil(8)
    }
}

/// Whether `input` looks like a cursor file.
pub fn is_magic(input: &[u8]) -> bool {
    ico::has_directory(input, TYPE_CURSOR)
}

fn chosen(input: &[u8]) -> Result<(ico::Entry<'_>, CursorInfo)> {
    if !is_magic(input) {
        return Err(ImageError::UnknownFormat);
    }
    let entry = ico::best_entry(input)?;
    let (width, height, _) = ico::measure(&entry)?;
    // A hot spot outside the image is a broken file; keep it on the image.
    let info = CursorInfo {
        width,
        height,
        hotspot_x: u32::from(entry.planes).min(width.saturating_sub(1)),
        hotspot_y: u32::from(entry.bits).min(height.saturating_sub(1)),
    };
    Ok((entry, info))
}

/// Reads the size and hot spot of the image [`decode`] would choose: the one
/// with the most pixels, then the most colours.
pub fn info(input: &[u8]) -> Result<CursorInfo> {
    Ok(chosen(input)?.1)
}

/// Working memory [`decode`] needs: what a PNG payload needs, and otherwise none.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    let (entry, _) = chosen(input)?;
    if ico::is_png(entry.data) {
        png::scratch_len(entry.data)
    } else {
        Ok(0)
    }
}

/// Decodes the cursor into straight RGBA8888 in `out`.
///
/// `invert`, unless empty, receives a bit for each pixel that inverts the
/// screen (row by row, the first pixel in the low bit of the first byte); it
/// must then hold at least [`CursorInfo::invert_len`] bytes. A PNG payload never
/// inverts. `scratch` is only used by PNG payloads (see [`scratch_len`]).
pub fn decode(
    input: &[u8],
    out: &mut [u8],
    invert: &mut [u8],
    scratch: &mut [u8],
) -> Result<CursorInfo> {
    let (entry, info) = chosen(input)?;
    if ico::is_png(entry.data) {
        if !invert.is_empty() {
            let len = info.invert_len();
            let available = invert.len();
            invert
                .get_mut(..len)
                .ok_or(ImageError::BufferTooSmall {
                    required: len,
                    available,
                })?
                .fill(0);
        }
        png::decode(entry.data, out, scratch)?;
        return Ok(info);
    }
    ico::decode_dib(
        entry.data,
        info.width as usize,
        info.height as usize,
        out,
        invert,
    )?;
    Ok(info)
}

/// Whether bit `index` of an invert mask from [`decode`] is set.
pub fn inverts(mask: &[u8], index: usize) -> bool {
    mask.get(index / 8).is_some_and(|byte| byte >> (index % 8) & 1 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// A cursor file holding one 2x2 1-bit DIB with a hot spot at (1, 0).
    /// Row 0 (the top) is black then white; row 1 is masked: black (clear) then
    /// white (invert).
    fn tiny() -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&4i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        // Colours, bottom-up, each row padded to four bytes.
        dib.extend_from_slice(&[0b0100_0000, 0, 0, 0, 0b0100_0000, 0, 0, 0]);
        // Mask, bottom-up: the bottom row is masked.
        dib.extend_from_slice(&[0b1100_0000, 0, 0, 0, 0, 0, 0, 0]);
        let mut file = vec![0, 0, 2, 0, 1, 0, 2, 2, 2, 0, 1, 0, 0, 0];
        file.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        file.extend_from_slice(&22u32.to_le_bytes());
        file.extend_from_slice(&dib);
        file
    }

    #[test]
    fn a_cursor_decodes_with_its_hot_spot_and_inverted_pixels() {
        let file = tiny();
        assert!(is_magic(&file));
        assert!(!ico::is_magic(&file));
        let mut rgba = [0u8; 16];
        let mut invert = [0u8; 1];
        let info = decode(&file, &mut rgba, &mut invert, &mut []).unwrap();
        assert_eq!((info.width, info.height, info.hotspot_x, info.hotspot_y), (2, 2, 1, 0));
        assert_eq!(&rgba[..8], &[0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(rgba[11], 0, "masked black is clear");
        assert_eq!(rgba[15], 0, "masked white is clear too");
        assert!(!inverts(&invert, 2));
        assert!(inverts(&invert, 3));
        assert!(!inverts(&invert, 0));
    }

    #[test]
    fn small_buffers_are_errors() {
        let file = tiny();
        let mut rgba = [0u8; 15];
        assert!(decode(&file, &mut rgba, &mut [], &mut []).is_err());
        let mut rgba = [0u8; 16];
        let mut invert: [u8; 0] = [];
        assert!(decode(&file, &mut rgba, &mut invert, &mut []).is_ok());
    }

    #[test]
    fn an_icon_is_not_a_cursor() {
        let mut file = tiny();
        file[2] = 1;
        assert!(!is_magic(&file));
        assert!(info(&file).is_err());
    }

    #[test]
    fn every_truncation_fails_cleanly() {
        let file = tiny();
        for end in 0..file.len() {
            let mut rgba = [0u8; 16];
            let mut invert = [0u8; 1];
            let _ = decode(&file[..end], &mut rgba, &mut invert, &mut []);
        }
    }
}
