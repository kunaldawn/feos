//! `feos-image` is a dependency-free image codec library for the feos kernel.
//!
//! Decoders write straight into a caller-provided RGBA8888 buffer and never
//! allocate. Some formats need additional working memory; that space is also
//! supplied by the caller through the `scratch` argument of
//! [`decode`](decode()).
//!
//! Supported formats:
//!
//! | Format | Decode | Encode |
//! |--------|--------|--------|
//! | PNG    | yes (non-interlaced and Adam7; 1/2/4/8/16-bit; palette, gray, RGB, RGBA) | RGBA, uncompressed zlib |
//! | JPEG   | baseline and extended sequential, 8-bit | no |
//! | BMP    | 1/4/8/16/24/32-bit uncompressed and bitfields | 24-bit |
//! | PPM    | P5/P6 binary, 8/16-bit | P6 |
//! | QOI    | yes | yes |
//!
//! ```
//! use feos_image::{decode, detect, ImageFormat};
//!
//! let mut qoi = [0u8; 64];
//! let len = feos_image::qoi::encode_rgba(1, 1, &[255, 0, 0, 255], &mut qoi).unwrap();
//! let qoi = &qoi[..len];
//! assert_eq!(detect(qoi), Some(ImageFormat::Qoi));
//!
//! let mut rgba = [0u8; 4];
//! let info = decode(qoi, &mut rgba, &mut []).unwrap();
//! assert_eq!(info.width, 1);
//! assert_eq!(rgba, [255, 0, 0, 255]);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod bmp;
mod error;
pub mod jpeg;
pub mod png;
pub mod ppm;
pub mod qoi;

pub use error::{ImageError, Result};

/// Image container formats understood by this crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageFormat {
    /// Portable Network Graphics.
    Png,
    /// JPEG / JFIF.
    Jpeg,
    /// Windows bitmap.
    Bmp,
    /// Portable pixmap (P5/P6).
    Ppm,
    /// Quite OK Image format.
    Qoi,
}

impl ImageFormat {
    /// A short, lowercase name for the format.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Bmp => "bmp",
            Self::Ppm => "ppm",
            Self::Qoi => "qoi",
        }
    }
}

/// Basic properties of an encoded image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImageInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Container format.
    pub format: ImageFormat,
}

impl ImageInfo {
    /// Number of bytes needed for an RGBA8888 output buffer.
    pub fn rgba_len(&self) -> Result<usize> {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ImageError::Overflow)
    }
}

/// Detects an image format from its signature.
pub fn detect(input: &[u8]) -> Option<ImageFormat> {
    if input.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some(ImageFormat::Png)
    } else if input.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if input.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if input.starts_with(b"qoif") {
        Some(ImageFormat::Qoi)
    } else if input.len() >= 2 && input[0] == b'P' && matches!(input[1], b'5' | b'6') {
        Some(ImageFormat::Ppm)
    } else {
        None
    }
}

/// Reads the dimensions and format of an encoded image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    match detect(input).ok_or(ImageError::UnknownFormat)? {
        ImageFormat::Png => png::info(input),
        ImageFormat::Jpeg => jpeg::info(input),
        ImageFormat::Bmp => bmp::info(input),
        ImageFormat::Ppm => ppm::info(input),
        ImageFormat::Qoi => qoi::info(input),
    }
}

/// Working-memory requirement of [`decode`] for this image.
///
/// Pass a buffer of at least this size as `scratch`. Formats that need no
/// scratch return zero.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    match detect(input).ok_or(ImageError::UnknownFormat)? {
        ImageFormat::Png => png::scratch_len(input),
        ImageFormat::Jpeg => jpeg::scratch_len(input),
        ImageFormat::Bmp => bmp::scratch_len(input),
        ImageFormat::Ppm => ppm::scratch_len(input),
        ImageFormat::Qoi => qoi::scratch_len(input),
    }
}

/// Decodes any supported image into an RGBA8888 buffer.
///
/// `out` must be at least [`ImageInfo::rgba_len`] bytes. `scratch` must be at
/// least [`scratch_len`] bytes for the same input.
pub fn decode<'a>(input: &[u8], out: &'a mut [u8], scratch: &mut [u8]) -> Result<ImageInfo> {
    match detect(input).ok_or(ImageError::UnknownFormat)? {
        ImageFormat::Png => png::decode(input, out, scratch),
        ImageFormat::Jpeg => jpeg::decode(input, out),
        ImageFormat::Bmp => bmp::decode(input, out),
        ImageFormat::Ppm => ppm::decode(input, out),
        ImageFormat::Qoi => qoi::decode(input, out),
    }
}
