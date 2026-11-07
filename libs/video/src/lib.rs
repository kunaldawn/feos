//! `feos-video` is a dependency-free video container and codec library for the
//! feos kernel.
//!
//! The crate follows the same rules as `feos-image` and `feos-audio-codec`:
//! containers are parsed in place and decoded frames are written into a
//! caller-provided RGBA8888 buffer. H.264 decoding is delegated to the vendored
//! `feos-h264` crate. [`info`] reads a file's properties without building
//! anything. [`VideoDecoder`] keeps a sample table and codec state, so decoding
//! the next frame costs one frame; all its allocation is fallible and limited
//! by [`DecoderLimits`]. [`decode_frame`] is a stateless wrapper around it.
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
//! | MPEG-1 elementary and program streams | yes (video only) | yes |
//!
//! Other codecs (HEVC, VP8, VP9, AV1, MPEG-2/4, MPEG-1 inside other containers, Theora, ProRes, VC-1, VP6,
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

extern crate alloc;

pub mod audio;
mod avi;
mod bitstream;
pub mod codec;
mod decoder;
mod error;
mod flv;
mod gif;
mod h264;
mod index;
mod ivf;
mod matroska;
mod mjpeg;
mod mp4;
mod mpeg1;
mod mpeg1_tables;
pub mod pixel;
mod raw;
mod rle;
pub mod vp8;

pub use audio::{audio_track, audio_track_from_source, AudioCodec, AudioPacket, AudioTrack};
pub use feos_source::Source;
pub use codec::Codec;
pub use decoder::VideoDecoder;
pub use error::{Result, VideoError};
pub use index::DecoderLimits;
pub use pixel::{ColourMatrix, PixelFormat};

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
    /// MPEG-1 video: an elementary or program stream.
    Mpeg1,
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
            Self::Mpeg1 => "mpeg1",
        }
    }
}

/// How a decoded frame should be shown. Decoded frames are never scaled or
/// rotated by this crate; that's up to whoever draws them.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DisplayMetadata {
    /// Sample aspect ratio as width:height, or `(0, 0)` for unspecified.
    pub sar: (u32, u32),
    /// Clockwise rotation to apply, one of 0, 90, 180 or 270.
    pub rotation: u16,
}

/// A stream's colour signalling, using H.273 code points as shared by H.264
/// VUI, Matroska and MP4 `colr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamColour {
    /// `matrix_coefficients`; 2 means unspecified.
    pub matrix: u8,
    /// `colour_primaries`; 2 means unspecified.
    pub primaries: u8,
    /// `transfer_characteristics`; 2 means unspecified.
    pub transfer: u8,
    /// Whether samples use the full 0-255 range.
    pub full_range: bool,
}

impl Default for StreamColour {
    fn default() -> Self {
        Self {
            matrix: 2,
            primaries: 2,
            transfer: 2,
            full_range: false,
        }
    }
}

/// Merges what an H.264 SPS says into container metadata. The container's
/// display settings win, as they're what a muxer chose to show. The codec knows
/// best how its own samples were encoded.
pub(crate) fn merge_sps_metadata(
    display: &mut DisplayMetadata,
    colour: &mut StreamColour,
    sps: &feos_h264::SpsInfo,
) {
    if display.sar == (0, 0) {
        let (w, h) = sps.vui.sample_aspect_ratio;
        display.sar = (w as u32, h as u32);
    }
    if sps.vui.matrix_coefficients != 2 {
        colour.matrix = sps.vui.matrix_coefficients;
    }
    if sps.vui.colour_primaries != 2 {
        colour.primaries = sps.vui.colour_primaries;
    }
    if sps.vui.transfer_characteristics != 2 {
        colour.transfer = sps.vui.transfer_characteristics;
    }
    colour.full_range |= sps.vui.full_range;
}

/// Basic properties of an encoded video.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VideoInfo {
    /// Width of decoded frames in pixels. Use
    /// [`display_size`](Self::display_size) for the shape to show them at.
    pub width: u32,
    /// Height of decoded frames in pixels.
    pub height: u32,
    /// Container or elementary-stream format.
    pub format: ContainerFormat,
    /// Video codec inside the container.
    pub codec: Codec,
    /// Number of coded frames, or 0 when the container does not say.
    pub frame_count: u32,
    /// Frame rate as numerator/denominator, when known. The average, for a
    /// variable-rate MP4. [`VideoDecoder`] has each frame's own time.
    pub frame_rate: Option<(u32, u32)>,
    /// Aspect ratio and rotation to show frames with.
    pub display: DisplayMetadata,
    /// Colour signalling.
    pub colour: StreamColour,
}

impl VideoInfo {
    /// Size to show a frame at, after the sample aspect ratio and rotation.
    pub fn display_size(&self) -> (u32, u32) {
        let (mut width, mut height) = (self.width, self.height);
        let (sar_w, sar_h) = self.display.sar;
        if sar_w != 0 && sar_h != 0 {
            if sar_w >= sar_h {
                let scaled = width as u64 * sar_w as u64 / sar_h as u64;
                width = scaled.min(u32::MAX as u64) as u32;
            } else {
                let scaled = height as u64 * sar_h as u64 / sar_w as u64;
                height = scaled.min(u32::MAX as u64) as u32;
            }
        }
        if self.display.rotation % 180 == 90 {
            (height, width)
        } else {
            (width, height)
        }
    }

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
    pub fn can_decode(&self) -> bool {
        // MPEG-1 only comes from its own streams, not from inside other containers.
        self.codec.can_decode() || self.format == ContainerFormat::Mpeg1
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
    /// Presentation time in milliseconds. Zero from [`decode_frame`] on GIF.
    pub pts_ms: u64,
    /// The frame's display-order index. Past the requested index if that frame
    /// was lost to a damaged sample.
    pub index: u32,
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
            || &input[4..8] == b"free"
            || &input[4..8] == b"wide"
            || &input[4..8] == b"skip"
            || &input[4..8] == b"pnot"
            || &input[4..8] == b"styp"
            || &input[4..8] == b"sidx"
            || &input[4..8] == b"moof")
    {
        Some(ContainerFormat::Mp4)
    } else if input.len() >= 4 && input[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        Some(ContainerFormat::Matroska)
    } else if input.len() >= 3 && &input[..3] == b"FLV" {
        Some(ContainerFormat::Flv)
    } else if input.len() >= 4 && &input[..4] == b"DKIF" {
        Some(ContainerFormat::Ivf)
    } else if mpeg1::is_magic(input) {
        Some(ContainerFormat::Mpeg1)
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

/// Reads the properties of an encoded video. This allocates nothing beyond an
/// H.264 parameter set, and doesn't decode any frames.
pub fn info(input: &[u8]) -> Result<VideoInfo> {
    let format = detect(input).ok_or(VideoError::UnknownFormat)?;
    let plain = |width, height, codec, frame_count, frame_rate| VideoInfo {
        width,
        height,
        format,
        codec,
        frame_count,
        frame_rate,
        display: DisplayMetadata::default(),
        colour: StreamColour::default(),
    };
    match format {
        ContainerFormat::Gif => {
            let gif = gif::info(input)?;
            Ok(plain(gif.width, gif.height, Codec::Gif, gif.frame_count, None))
        }
        ContainerFormat::Avi => {
            let avi = avi::info(input)?;
            Ok(VideoInfo {
                display: avi.display,
                colour: avi.colour,
                ..plain(avi.width, avi.height, avi.codec, avi.frame_count, avi.frame_rate)
            })
        }
        ContainerFormat::Mp4 => {
            let mp4 = mp4::info(input)?;
            Ok(VideoInfo {
                display: mp4.display,
                colour: mp4.colour,
                ..plain(mp4.width, mp4.height, mp4.codec, mp4.frame_count, mp4.frame_rate)
            })
        }
        ContainerFormat::Matroska => {
            let mkv = matroska::info(input)?;
            Ok(VideoInfo {
                display: mkv.display,
                colour: mkv.colour,
                ..plain(mkv.width, mkv.height, mkv.codec, mkv.frame_count, mkv.frame_rate)
            })
        }
        ContainerFormat::Flv => {
            let flv = flv::info(input)?;
            Ok(VideoInfo {
                display: flv.display,
                colour: flv.colour,
                ..plain(flv.width, flv.height, flv.codec, flv.frame_count, flv.frame_rate)
            })
        }
        ContainerFormat::Mpeg1 => {
            let found = mpeg1::info(input)?;
            Ok(plain(
                found.width,
                found.height,
                Codec::Mpeg1,
                found.frames,
                Some(found.frame_rate),
            ))
        }
        ContainerFormat::Ivf => {
            let ivf = ivf::info(input)?;
            Ok(plain(ivf.width, ivf.height, ivf.codec, ivf.frame_count, ivf.frame_rate))
        }
        ContainerFormat::H264 => {
            let sps = h264::info_from_annex_b(input)?;
            let mut display = DisplayMetadata::default();
            let mut colour = StreamColour::default();
            merge_sps_metadata(&mut display, &mut colour, &sps);
            Ok(VideoInfo {
                display,
                colour,
                ..plain(
                    sps.width,
                    sps.height,
                    Codec::H264,
                    h264::annex_b_frame_count(input),
                    sps.frame_rate.and_then(|(n, d)| reduce_fraction(n, d)),
                )
            })
        }
        ContainerFormat::MotionJpeg => {
            let mjpeg = mjpeg::info(input)?;
            Ok(plain(mjpeg.width, mjpeg.height, Codec::Mjpeg, mjpeg.frame_count, None))
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
///
/// This opens the video each time, which for inter-coded video restarts from the
/// closest key frame. To decode more than one frame, use [`VideoDecoder`].
pub fn decode_frame(
    input: &[u8],
    index: u32,
    out: &mut [u8],
    scratch: &mut [u8],
) -> Result<DecodedFrame> {
    if detect(input) == Some(ContainerFormat::Gif) {
        let gif = gif::info(input)?;
        if index >= gif.frame_count {
            return Err(VideoError::FrameOutOfRange {
                index,
                count: gif.frame_count,
            });
        }
        let delay_ms = gif::decode_frame(input, index, out, scratch)?;
        return Ok(DecodedFrame {
            width: gif.width,
            height: gif.height,
            delay_ms,
            pts_ms: 0,
            index,
        });
    }
    VideoDecoder::open(input)?.decode(index, out)
}
