//! Decoded application icons.

use feos_image::ImageError;
use feos_sync::SpinLock;

use crate::{BundleError, Result};

/// Maximum icon width and height accepted from a bundle.
pub const MAX_ICON: usize = 48;

const PIXEL_BYTES: usize = MAX_ICON * MAX_ICON * 4;

/// Scratch space shared by all icon decodes.
static SCRATCH: SpinLock<[u8; crate::ICON_SCRATCH]> = SpinLock::new([0; crate::ICON_SCRATCH]);

/// A decoded RGBA8888 icon.
///
/// The icon is stored inline so it can live in a fixed application catalog
/// without an allocator. Pixels beyond `width`/`height` are zero.
#[derive(Clone, Copy)]
pub struct Icon {
    width: u32,
    height: u32,
    pixels: [u8; PIXEL_BYTES],
}

impl Icon {
    /// An empty icon.
    pub const EMPTY: Self = Self {
        width: 0,
        height: 0,
        pixels: [0; PIXEL_BYTES],
    };

    /// Decodes an encoded image into an icon.
    pub fn decode(encoded: &[u8]) -> Result<Self> {
        let info = feos_image::info(encoded)?;
        if info.width == 0 || info.height == 0 {
            return Err(BundleError::Image(ImageError::InvalidData(
                "icon has zero dimensions",
            )));
        }
        if info.width as usize > MAX_ICON || info.height as usize > MAX_ICON {
            return Err(BundleError::IconTooLarge);
        }

        let needed = info.rgba_len()?;
        if needed > PIXEL_BYTES {
            return Err(BundleError::IconTooLarge);
        }
        let scratch_needed = feos_image::scratch_len(encoded)?;
        if scratch_needed > crate::ICON_SCRATCH {
            return Err(BundleError::BufferTooSmall {
                required: scratch_needed,
                available: crate::ICON_SCRATCH,
            });
        }

        let mut rgba = [0u8; PIXEL_BYTES];
        let mut scratch = SCRATCH.lock();
        let info =
            feos_image::decode(encoded, &mut rgba[..needed], &mut scratch[..scratch_needed])?;

        let mut icon = Self::EMPTY;
        icon.width = info.width;
        icon.height = info.height;
        icon.pixels[..needed].copy_from_slice(&rgba[..needed]);
        Ok(icon)
    }

    /// Icon width in pixels (zero for an empty icon).
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Icon height in pixels (zero for an empty icon).
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Returns `true` when no image was decoded.
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Returns the RGBA pixel at `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = (y as usize * self.width as usize + x as usize) * 4;
        Some([
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
            self.pixels[index + 3],
        ])
    }
}

impl Default for Icon {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_icon_has_no_pixels() {
        let icon = Icon::EMPTY;
        assert!(icon.is_empty());
        assert_eq!(icon.width(), 0);
        assert_eq!(icon.pixel(0, 0), None);
    }

    #[test]
    fn decodes_a_qoi_icon() {
        let mut encoded = [0u8; 64];
        let len = feos_image::qoi::encode_rgba(
            2,
            2,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
            &mut encoded,
        )
        .unwrap();
        let icon = Icon::decode(&encoded[..len]).unwrap();
        assert_eq!(icon.width(), 2);
        assert_eq!(icon.height(), 2);
        assert_eq!(icon.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(icon.pixel(3, 0), None);
    }

    #[test]
    fn rejects_oversized_icons() {
        let mut encoded = [0u8; 128 * 128 * 4 + 128];
        let pixels = [0u8; 64 * 64 * 4];
        let len = feos_image::qoi::encode_rgba(64, 64, &pixels, &mut encoded).unwrap();
        assert!(matches!(
            Icon::decode(&encoded[..len]),
            Err(BundleError::IconTooLarge)
        ));
    }
}
