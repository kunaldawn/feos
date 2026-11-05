//! Decoded application icons.

use feos_image::ImageError;
use feos_sync::SpinLock;

use crate::{BundleError, Result};

/// Maximum icon width and height accepted from a bundle.
pub const MAX_ICON: usize = 48;

const PIXEL_BYTES: usize = MAX_ICON * MAX_ICON * 4;
/// The other sizes a bundle can supply, drawn in place of scaling.
const MEDIUM_ICON: usize = 32;
const SMALL_ICON: usize = 16;

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
    /// An exact 32 by 32 version, when `has_medium`.
    medium: [u8; MEDIUM_ICON * MEDIUM_ICON * 4],
    has_medium: bool,
    /// An exact 16 by 16 version, when `has_small`.
    small: [u8; SMALL_ICON * SMALL_ICON * 4],
    has_small: bool,
}

/// One size of an icon, in RGBA rows.
#[derive(Clone, Copy)]
pub struct IconImage<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` RGBA pixels.
    pub pixels: &'a [u8],
}

impl Icon {
    /// An empty icon.
    pub const EMPTY: Self = Self {
        width: 0,
        height: 0,
        pixels: [0; PIXEL_BYTES],
        medium: [0; MEDIUM_ICON * MEDIUM_ICON * 4],
        has_medium: false,
        small: [0; SMALL_ICON * SMALL_ICON * 4],
        has_small: false,
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

    /// Adds an exact 16 or 32 pixel square version, if that's what `encoded` is.
    /// Anything else, or a file that won't decode, is ignored: the icon has its
    /// main image, and scaling that is a fine fallback.
    pub fn add_size(&mut self, encoded: &[u8]) {
        let Ok(info) = feos_image::info(encoded) else {
            return;
        };
        let Ok(scratch_needed) = feos_image::scratch_len(encoded) else {
            return;
        };
        if scratch_needed > crate::ICON_SCRATCH {
            return;
        }
        let mut scratch = SCRATCH.lock();
        let scratch = &mut scratch[..scratch_needed];
        match (info.width as usize, info.height as usize) {
            (MEDIUM_ICON, MEDIUM_ICON) => {
                self.has_medium = feos_image::decode(encoded, &mut self.medium, scratch).is_ok();
            }
            (SMALL_ICON, SMALL_ICON) => {
                self.has_small = feos_image::decode(encoded, &mut self.small, scratch).is_ok();
            }
            _ => {}
        }
    }

    /// The version to draw at `size` pixels: the smallest not smaller than it.
    pub fn image_for(&self, size: u32) -> IconImage<'_> {
        if self.has_small && size <= SMALL_ICON as u32 {
            IconImage {
                width: SMALL_ICON as u32,
                height: SMALL_ICON as u32,
                pixels: &self.small,
            }
        } else if self.has_medium && size <= MEDIUM_ICON as u32 {
            IconImage {
                width: MEDIUM_ICON as u32,
                height: MEDIUM_ICON as u32,
                pixels: &self.medium,
            }
        } else {
            IconImage {
                width: self.width,
                height: self.height,
                pixels: &self.pixels,
            }
        }
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
    fn exact_sizes_are_chosen_over_scaling() {
        let big = include_bytes!("../../../assets/icons/folders/folder-closed-48.ico");
        let medium = include_bytes!("../../../assets/icons/folders/folder-closed-32.ico");
        let small = include_bytes!("../../../assets/icons/folders/folder-closed-16.ico");
        let mut icon = Icon::decode(big).unwrap();
        assert_eq!(icon.image_for(16).width, 48, "scaling, with nothing else");
        icon.add_size(small);
        icon.add_size(medium);
        assert_eq!(icon.image_for(16).width, 16);
        assert_eq!(icon.image_for(20).width, 32);
        assert_eq!(icon.image_for(32).width, 32);
        assert_eq!(icon.image_for(48).width, 48);
        assert_eq!(icon.image_for(64).width, 48);
        assert_eq!(icon.image_for(16).pixels.len(), 16 * 16 * 4);
        // A size that isn't one of the two is ignored.
        let mut other = Icon::EMPTY;
        other.add_size(big);
        assert_eq!(other.image_for(16).width, 0);
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
