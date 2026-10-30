//! `feos-video` is a dependency-free video container and codec library for the
//! feos kernel.
//!
//! The crate follows the same rules as `feos-image` and `feos-audio-codec`:
//! containers are parsed in place, decoded frames are written into a
//! caller-provided RGBA8888 buffer, and no allocator is used. H.264 decoding is
//! delegated to the vendored `feos-h264` crate, which uses `alloc` for its
//! reference-picture buffer.
//!
//! | Container | Demux | Decode |
//! |-----------|-------|--------|
//! | GIF (animated) | yes | yes |
//! | AVI (RIFF) | yes | MJPEG, uncompressed RGB/YUV, RLE8/RLE4, H.264 |
//! | MP4 / MOV (ISO BMFF) | yes | MJPEG, uncompressed, H.264 |
//! | Matroska / WebM | yes | MJPEG, uncompressed, H.264 |
//! | FLV | yes | H.264 |
//! | IVF | yes | metadata (VP8/VP9/AV1 payload) |
//! | Annex-B H.264 | yes | yes |
//! | Motion JPEG | yes | yes |
//!
//! Other codecs (HEVC, VP8, VP9, AV1, MPEG-1/2/4, Theora, ProRes, VC-1, VP6,
//! H.263, Cinepak) are recognised and described by [`info`]. Their samples are
//! located by the demuxers, but pixel reconstruction is not implemented yet
//! and [`decode_frame`] returns [`VideoError::UnsupportedCodec`].
//!
//! ```
//! use feos_video::{decode_frame, detect, ContainerFormat};
//!
//! // A 1x1 animated GIF with a single red pixel.
//! let gif: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\
//!                    \xff\x00\x00\x00\x00\x00\
//!                    \x21\xf9\x04\x00\x00\x00\x00\x00\
//!                    \x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\
//!                    \x02\x02\x44\x01\x00\x3b";
//! assert_eq!(detect(gif), Some(ContainerFormat::Gif));
//!
//! let info = feos_video::info(gif).unwrap();
//! assert_eq!((info.width, info.height), (1, 1));
//!
//! let mut rgba = [0u8; 4];
//! let mut scratch = [0u8; 20];
//! let frame = decode_frame(gif, 0, &mut rgba, &mut scratch).unwrap();
//! assert_eq!(frame.width, 1);
//! assert_eq!(rgba, [255, 0, 0, 255]);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
// Container parsers keep a few accessors and codec hooks that are used by
// tests or by future decoders.
#![allow(dead_code)]

mod avi;
mod bitstream;
pub mod codec;
mod error;
mod flv;
mod gif;
mod h264;
mod ivf;
mod matroska;
mod mjpeg;
mod mp4;
pub mod pixel;
mod raw;
mod rle;

pub use codec::Codec;
pub use error::{Result, VideoError};
pub use pixel::PixelFormat;

/// A video container or elementary stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContainerFormat {
    /// Animated GIF.
    Gif,
    /// RIFF AVI.
    Avi,
    /// ISO Base Media (MP4/MOV).
    Mp4,
    /// Matroska or WebM.
    Matroska,
    /// Flash Video.
    Flv,
    /// IVF elementary stream.
    Ivf,
    /// Raw Annex-B H.264.
    H264,
    /// Concatenated JPEG images (Motion JPEG).
    MotionJpeg,
}

impl ContainerFormat {
    /// A short, lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Avi => "avi",
            Self::Mp4 => "mp4",
            Self::Matroska => "matroska",
            Self::Flv => "flv",
            Self::Ivf => "ivf",
            Self::H264 => "h264",
            Self::MotionJpeg => "mjpeg",
        }
    }
}

/// Basic properties of an encoded video.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VideoInfo {
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Container or elementary-stream format.
    pub format: ContainerFormat,
    /// Video codec inside the container.
    pub codec: Codec,
    /// Number of coded frames, or 0 when the container does not say.
    pub frame_count: u32,
    /// Frame rate as numerator/denominator, when known.
    pub frame_rate: Option<(u32, u32)>,
}

impl VideoInfo {
    /// Number of bytes needed for one RGBA8888 frame.
    pub fn rgba_len(&self) -> Result<usize> {
        pixel::rgba_len(self.width, self.height)
    }

    /// Duration in milliseconds, when the frame count and rate are known.
    pub fn duration_ms(&self) -> Option<u64> {
        let (num, den) = self.frame_rate?;
        if num == 0 || den == 0 || self.frame_count == 0 {
            return None;
        }
        Some(
            (self.frame_count as u64)
                .saturating_mul(1000)
                .saturating_mul(den as u64)
                / num as u64,
        )
    }

    /// Whether this build can decode the codec into pixels.
    pub const fn can_decode(&self) -> bool {
        self.codec.can_decode()
    }
}

/// A decoded frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Display delay in milliseconds (0 when unknown).
    pub delay_ms: u32,
}

/// Detects a video format from its signature.
pub fn detect(input: &[u8]) -> Option<ContainerFormat> {
    if input.len() >= 6 && (&input[..6] == b"GIF87a" || &input[..6] == b"GIF89a") {
        Some(ContainerFormat::Gif)
    } else if input.len() >= 12 && &input[..4] == b"RIFF" && &input[8..12] == b"AVI " {
        Some(ContainerFormat::Avi)
    } else if input.len() >= 8
        && (&input[4..8] == b"ftyp"
            || &input[4..8] == b"moov"
            || &input[4..8] == b"mdat"
            || &input[4..8] == b"free")
    {
        Some(ContainerFormat::Mp4)
    } else if input.len() >= 4 && input[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        Some(ContainerFormat::Matroska)
    } else if input.len() >= 3 && &input[..3] == b"FLV" {
        Some(ContainerFormat::Flv)
    } else if input.len() >= 4 && &input[..4] == b"DKIF" {
        Some(ContainerFormat::Ivf)
    } else if is_annex_b(input) {
        Some(ContainerFormat::H264)
    } else if input.len() >= 3 && input[0] == 0xff && input[1] == 0xd8 && input[2] == 0xff {
        Some(ContainerFormat::MotionJpeg)
    } else {
        None
    }
}

fn reduce_fraction(numerator: u32, denominator: u32) -> Option<(u32, u32)> {
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let mut a = numerator;
    let mut b = denominator;
    while b != 0 {
        let next = a % b;
        a = b;
        b = next;
    }
    Some((numerator / a, denominator / a))
}

fn is_annex_b(input: &[u8]) -> bool {
    if !(input.starts_with(&[0, 0, 1]) || input.starts_with(&[0, 0, 0, 1])) {
        return false;
    }
    let start = if input.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        3
    };
    input
        .get(start)
        .map(|byte| matches!(byte & 0x1f, 1 | 5 | 6 | 7 | 8 | 9))
        .unwrap_or(false)
}

/// Reads the properties of an encoded video.
pub fn info(input: &[u8]) -> Result<VideoInfo> {
    match detect(input).ok_or(VideoError::UnknownFormat)? {
        ContainerFormat::Gif => {
            let gif = gif::info(input)?;
            Ok(VideoInfo {
                width: gif.width,
                height: gif.height,
                format: ContainerFormat::Gif,
                codec: Codec::Gif,
                frame_count: gif.frame_count,
                frame_rate: None,
            })
        }
        ContainerFormat::Avi => {
            let avi = avi::info(input)?;
            Ok(VideoInfo {
                width: avi.width,
                height: avi.height,
                format: ContainerFormat::Avi,
                codec: avi.codec,
                frame_count: avi.frame_count,
                frame_rate: avi.frame_rate,
            })
        }
        ContainerFormat::Mp4 => {
            let mp4 = mp4::info(input)?;
            Ok(VideoInfo {
                width: mp4.width,
                height: mp4.height,
                format: ContainerFormat::Mp4,
                codec: mp4.codec,
                frame_count: mp4.frame_count,
                frame_rate: mp4.frame_rate,
            })
        }
        ContainerFormat::Matroska => {
            let mkv = matroska::info(input)?;
            Ok(VideoInfo {
                width: mkv.width,
                height: mkv.height,
                format: ContainerFormat::Matroska,
                codec: mkv.codec,
                frame_count: mkv.frame_count,
                frame_rate: mkv.frame_rate,
            })
        }
        ContainerFormat::Flv => {
            let flv = flv::info(input)?;
            Ok(VideoInfo {
                width: flv.width,
                height: flv.height,
                format: ContainerFormat::Flv,
                codec: flv.codec,
                frame_count: flv.frame_count,
                frame_rate: flv.frame_rate,
            })
        }
        ContainerFormat::Ivf => {
            let ivf = ivf::info(input)?;
            Ok(VideoInfo {
                width: ivf.width,
                height: ivf.height,
                format: ContainerFormat::Ivf,
                codec: ivf.codec,
                frame_count: ivf.frame_count,
                frame_rate: ivf.frame_rate,
            })
        }
        ContainerFormat::H264 => {
            let h264 = h264::info_from_annex_b(input)?;
            Ok(VideoInfo {
                width: h264.width,
                height: h264.height,
                format: ContainerFormat::H264,
                codec: Codec::H264,
                frame_count: h264::annex_b_frame_count(input),
                frame_rate: None,
            })
        }
        ContainerFormat::MotionJpeg => {
            let mjpeg = mjpeg::info(input)?;
            Ok(VideoInfo {
                width: mjpeg.width,
                height: mjpeg.height,
                format: ContainerFormat::MotionJpeg,
                codec: Codec::Mjpeg,
                frame_count: mjpeg.frame_count,
                frame_rate: None,
            })
        }
    }
}

/// Working-memory requirement of [`decode_frame`].
///
/// Most containers need no scratch space. Animated GIF needs `width * height *
/// 5` bytes: one index plane plus one saved canvas for disposal method 3.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    match detect(input).ok_or(VideoError::UnknownFormat)? {
        ContainerFormat::Gif => gif::scratch_len(input),
        _ => Ok(0),
    }
}

/// Decodes the frame at `index` into an RGBA8888 buffer.
///
/// `out` must be at least [`VideoInfo::rgba_len`] bytes. `scratch` must be at
/// least [`scratch_len`] bytes.
pub fn decode_frame(
    input: &[u8],
    index: u32,
    out: &mut [u8],
    scratch: &mut [u8],
) -> Result<DecodedFrame> {
    let info = info(input)?;
    if info.frame_count != 0 && index >= info.frame_count {
        return Err(VideoError::FrameOutOfRange {
            index,
            count: info.frame_count,
        });
    }
    let delay_ms = match info.format {
        ContainerFormat::Gif => gif::decode_frame(input, index, out, scratch)?,
        ContainerFormat::Avi => avi::decode_frame(input, index, out)?,
        ContainerFormat::Mp4 => mp4::decode_frame(input, index, out)?,
        ContainerFormat::Matroska => matroska::decode_frame(input, index, out)?,
        ContainerFormat::Flv => flv::decode_frame(input, index, out)?,
        ContainerFormat::Ivf => {
            let _sample = ivf::frame(input, index)?;
            return Err(VideoError::UnsupportedCodec(info.codec.name()));
        }
        ContainerFormat::H264 => h264::decode_annex_b(input, index, out)?,
        ContainerFormat::MotionJpeg => {
            let frame = mjpeg::find_frame(input, index)?;
            mjpeg::decode_sample(frame, out)?;
            0
        }
    };
    Ok(DecodedFrame {
        width: info.width,
        height: info.height,
        delay_ms,
    })
}
