//! Codec identifiers and the container-specific mappings to them.

use crate::pixel::PixelFormat;

/// A video codec understood by the demuxers.
///
/// `feos-video` reconstructs pixels for GIF, Motion JPEG, uncompressed/raw
/// frames, AVI RLE and H.264. The remaining variants are recognised so a
/// caller can inspect a file and choose a different backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Codec {
    /// Animated GIF (LZW).
    Gif,
    /// Motion JPEG (one JPEG per sample).
    Mjpeg,
    /// Uncompressed RGB in R,G,B order.
    RawRgb,
    /// Uncompressed RGB in B,G,R order.
    RawBgr,
    /// Uncompressed BGRA.
    RawBgra,
    /// Uncompressed RGBA.
    RawRgba,
    /// Uncompressed 8-bit luma.
    RawGray,
    /// Planar YUV 4:2:0.
    RawYuv420,
    /// Planar YUV 4:2:2.
    RawYuv422,
    /// Planar YUV 4:4:4.
    RawYuv444,
    /// Planar YVU 4:2:0 (YV12).
    RawYv12,
    /// Planar YVU 4:2:2 (YV16).
    RawYv16,
    /// Planar YVU 4:4:4 (YV24).
    RawYv24,
    /// 8-bit palette indices with a BGRA palette in the container.
    RawPal8,
    /// 16-bit RGB 5:5:5.
    RawBgr555,
    /// 16-bit RGB 5:6:5.
    RawBgr565,
    /// Packed YUY2.
    RawYuy2,
    /// Packed UYVY.
    RawUyvy,
    /// Microsoft RLE8.
    Rle8,
    /// Microsoft RLE4.
    Rle4,
    /// H.264 / MPEG-4 Part 10 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// VP8.
    Vp8,
    /// VP9.
    Vp9,
    /// AV1.
    Av1,
    /// MPEG-1 Part 2.
    Mpeg1,
    /// MPEG-2 Part 2.
    Mpeg2,
    /// MPEG-4 Part 2 (DivX/Xvid-style).
    Mpeg4,
    /// Theora.
    Theora,
    /// Apple ProRes.
    ProRes,
    /// SMPTE VC-1.
    Vc1,
    /// On2 VP6.
    Vp6,
    /// On2 VP6 with alpha.
    Vp6a,
    /// On2 VP6 Flash variant.
    Vp6f,
    /// Sorenson H.263 (FLV1).
    SorensonH263,
    /// ITU-T H.263.
    H263,
    /// Cinepak.
    Cinepak,
    /// A fourcc we do not know, stored as a big-endian integer.
    Unknown(u32),
}

impl Codec {
    /// A short, lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Mjpeg => "mjpeg",
            Self::RawRgb => "raw-rgb",
            Self::RawBgr => "raw-bgr",
            Self::RawBgra => "raw-bgra",
            Self::RawRgba => "raw-rgba",
            Self::RawGray => "raw-gray",
            Self::RawYuv420 => "raw-yuv420",
            Self::RawYuv422 => "raw-yuv422",
            Self::RawYuv444 => "raw-yuv444",
            Self::RawYv12 => "raw-yv12",
            Self::RawYv16 => "raw-yv16",
            Self::RawYv24 => "raw-yv24",
            Self::RawPal8 => "raw-pal8",
            Self::RawBgr555 => "raw-bgr555",
            Self::RawBgr565 => "raw-bgr565",
            Self::RawYuy2 => "raw-yuy2",
            Self::RawUyvy => "raw-uyvy",
            Self::Rle8 => "rle8",
            Self::Rle4 => "rle4",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
            Self::Mpeg1 => "mpeg1",
            Self::Mpeg2 => "mpeg2",
            Self::Mpeg4 => "mpeg4",
            Self::Theora => "theora",
            Self::ProRes => "prores",
            Self::Vc1 => "vc1",
            Self::Vp6 => "vp6",
            Self::Vp6a => "vp6a",
            Self::Vp6f => "vp6f",
            Self::SorensonH263 => "sorenson-h263",
            Self::H263 => "h263",
            Self::Cinepak => "cinepak",
            Self::Unknown(_) => "unknown",
        }
    }

    /// Whether this build can turn samples of this codec into RGBA pixels.
    pub const fn can_decode(self) -> bool {
        matches!(
            self,
            Self::Gif
                | Self::Mjpeg
                | Self::RawRgb
                | Self::RawBgr
                | Self::RawBgra
                | Self::RawRgba
                | Self::RawGray
                | Self::RawYuv420
                | Self::RawYuv422
                | Self::RawYuv444
                | Self::RawYv12
                | Self::RawYv16
                | Self::RawYv24
                | Self::RawPal8
                | Self::RawBgr555
                | Self::RawBgr565
                | Self::RawYuy2
                | Self::RawUyvy
                | Self::Rle8
                | Self::Rle4
                | Self::Vp8
                | Self::H264
        )
    }

    /// The pixel format of an uncompressed codec, if it has one.
    pub const fn pixel_format(self) -> Option<PixelFormat> {
        match self {
            Self::RawRgb => Some(PixelFormat::Rgb888),
            Self::RawBgr => Some(PixelFormat::Bgr888),
            Self::RawBgra => Some(PixelFormat::Bgra8888),
            Self::RawRgba => Some(PixelFormat::Rgba8888),
            Self::RawGray => Some(PixelFormat::Gray8),
            Self::RawYuv420 => Some(PixelFormat::Yuv420p),
            Self::RawYuv422 => Some(PixelFormat::Yuv422p),
            Self::RawYuv444 => Some(PixelFormat::Yuv444p),
            Self::RawYv12 => Some(PixelFormat::Yvu420p),
            Self::RawYv16 => Some(PixelFormat::Yvu422p),
            Self::RawYv24 => Some(PixelFormat::Yvu444p),
            Self::RawPal8 => Some(PixelFormat::Pal8),
            Self::RawBgr555 => Some(PixelFormat::Bgr555),
            Self::RawBgr565 => Some(PixelFormat::Bgr565),
            Self::RawYuy2 => Some(PixelFormat::Yuy2),
            Self::RawUyvy => Some(PixelFormat::Uyvy),
            _ => None,
        }
    }

    /// Maps an AVI `biCompression`/handler fourcc to a codec.
    pub fn from_avi(fourcc: [u8; 4]) -> Self {
        match &fourcc {
            b"MJPG" | b"mjpg" | b"JPEG" | b"jpeg" | b"AVRN" => Self::Mjpeg,
            b"H264" | b"h264" | b"X264" | b"x264" | b"AVC1" | b"avc1" | b"DAVC" => Self::H264,
            b"HEVC" | b"H265" | b"hvc1" | b"hev1" => Self::Hevc,
            b"VP80" | b"vp80" => Self::Vp8,
            b"VP90" | b"vp90" => Self::Vp9,
            b"AV01" | b"av01" => Self::Av1,
            b"MPG1" | b"mpg1" => Self::Mpeg1,
            b"MPG2" | b"mpg2" => Self::Mpeg2,
            b"MP4V" | b"mp4v" | b"XVID" | b"xvid" | b"DIVX" | b"divx" | b"DX50" | b"FMP4" => {
                Self::Mpeg4
            }
            b"THEO" | b"theo" => Self::Theora,
            b"apcn" | b"APCN" => Self::ProRes,
            b"WVC1" | b"wvc1" => Self::Vc1,
            b"VP60" | b"VP61" | b"VP62" => Self::Vp6,
            b"VP6A" => Self::Vp6a,
            b"VP6F" | b"FLV4" => Self::Vp6f,
            b"FLV1" | b"flv1" => Self::SorensonH263,
            b"H263" | b"h263" => Self::H263,
            b"CVID" | b"cvid" => Self::Cinepak,
            b"YUY2" | b"yuy2" => Self::RawYuy2,
            b"UYVY" | b"uyvy" => Self::RawUyvy,
            b"I420" | b"i420" | b"IYUV" => Self::RawYuv420,
            b"YV12" | b"yv12" => Self::RawYv12,
            b"Y42B" => Self::RawYuv422,
            b"YV16" | b"yv16" => Self::RawYv16,
            b"444P" => Self::RawYuv444,
            b"YV24" | b"yv24" => Self::RawYv24,
            b"RGB " | b"rgb " => Self::RawBgr,
            b"BGR " | b"bgr " => Self::RawBgr,
            b"RGBA" | b"rgba" => Self::RawRgba,
            b"BGRA" | b"bgra" => Self::RawBgra,
            b"GREY" | b"Y800" | b"Y8  " => Self::RawGray,
            b"RLE " | b"RLE8" | b"rle8" => Self::Rle8,
            b"RLE4" | b"rle4" => Self::Rle4,
            other => Self::Unknown(u32::from_be_bytes(*other)),
        }
    }

    /// Maps an ISO-BMFF sample-entry fourcc to a codec.
    pub fn from_mp4(fourcc: [u8; 4]) -> Self {
        match &fourcc {
            b"avc1" | b"avc3" | b"AVC1" | b"AVC3" => Self::H264,
            b"hvc1" | b"hev1" | b"hvc2" | b"hev2" => Self::Hevc,
            b"vp08" | b"VP08" => Self::Vp8,
            b"vp09" | b"VP09" => Self::Vp9,
            b"av01" | b"AV01" => Self::Av1,
            b"mp4v" | b"MP4V" => Self::Mpeg4,
            b"mp2v" | b"MP2V" => Self::Mpeg2,
            b"mpg2" | b"MPG2" => Self::Mpeg2,
            b"mjpa" | b"jpeg" | b"JPEG" | b"MJPG" => Self::Mjpeg,
            b"raw " | b"RAW " => Self::RawRgb,
            b"24BG" => Self::RawBgr,
            b"RGBA" | b"rgba" => Self::RawRgba,
            b"BGRA" | b"bgra" => Self::RawBgra,
            b"2vuy" => Self::RawUyvy,
            b"yuv2" | b"YUY2" => Self::RawYuy2,
            b"GREY" | b"Y800" => Self::RawGray,
            b"apcn" | b"apch" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => Self::ProRes,
            b"theo" | b"THEO" => Self::Theora,
            b"s263" | b"S263" | b"h263" | b"H263" => Self::H263,
            b"cvid" | b"CVID" => Self::Cinepak,
            _ => Self::Unknown(u32::from_be_bytes(fourcc)),
        }
    }

    /// Maps a Matroska `CodecID` string to a codec.
    pub fn from_matroska(id: &[u8]) -> Self {
        match id {
            b"V_MPEG4/ISO/AVC" | b"V_MPEG4/ISO/AVC/ES" => Self::H264,
            b"V_MPEGH/ISO/HEVC" => Self::Hevc,
            b"V_VP8" => Self::Vp8,
            b"V_VP9" => Self::Vp9,
            b"V_AV1" => Self::Av1,
            b"V_MPEG1" | b"V_MPEG/ISO/MPEG1" => Self::Mpeg1,
            b"V_MPEG2" | b"V_MPEG/ISO/MPEG2" => Self::Mpeg2,
            b"V_MPEG4/ISO/ASP" | b"V_MPEG4/ISO/SP" => Self::Mpeg4,
            b"V_THEORA" => Self::Theora,
            b"V_PRORES" => Self::ProRes,
            b"V_MJPEG" | b"V_JPEG" => Self::Mjpeg,
            b"V_MS/VFW/FOURCC" => Self::Unknown(0),
            // The pixel layout is in Video/ColourSpace: see `from_colour_space`.
            b"V_UNCOMPRESSED" => Self::Unknown(0),
            _ => Self::Unknown(0),
        }
    }

    /// Maps a Matroska `ColourSpace` fourcc for `V_UNCOMPRESSED` to a codec.
    pub fn from_colour_space(fourcc: [u8; 4]) -> Self {
        match &fourcc {
            b"RGB\x18" | b"RGB2" => Self::RawRgb,
            b"BGR\x18" | b"\x18BGR" => Self::RawBgr,
            b"RGBA" | b"RGB\x20" => Self::RawRgba,
            b"BGRA" | b"BGR\x20" => Self::RawBgra,
            b"Y800" | b"Y8  " | b"GREY" | b"Y1\0\x08" => Self::RawGray,
            b"Y41B" => Self::Unknown(u32::from_be_bytes(fourcc)),
            other => Self::from_avi(*other),
        }
    }

    /// Maps an FLV video codec id to a codec.
    pub fn from_flv(id: u8) -> Self {
        match id {
            2 => Self::SorensonH263,
            3 => Self::Unknown(0x5343_524e), // "SCRN"
            4 => Self::Vp6,
            5 => Self::Vp6a,
            6 => Self::Unknown(0x53435232), // "SCR2"
            7 => Self::H264,
            12 => Self::Hevc,
            _ => Self::Unknown(id as u32),
        }
    }

    /// Maps an IVF fourcc to a codec.
    pub fn from_ivf(fourcc: [u8; 4]) -> Self {
        match &fourcc {
            b"VP80" | b"vp80" => Self::Vp8,
            b"VP90" | b"vp90" => Self::Vp9,
            b"AV01" | b"av01" => Self::Av1,
            _ => Self::Unknown(u32::from_be_bytes(fourcc)),
        }
    }
}
