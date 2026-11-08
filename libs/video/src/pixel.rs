//! Pixel formats and conversions into the RGBA8888 canvas used by feos.

use crate::error::{Result, VideoError};

/// A decoded frame layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    /// Packed 8-bit RGBA, alpha last.
    Rgba8888,
    /// Packed 8-bit BGRA, alpha last.
    Bgra8888,
    /// Packed 8-bit RGB.
    Rgb888,
    /// Packed 8-bit BGR (the layout used by uncompressed AVI).
    Bgr888,
    /// 8-bit luma only.
    Gray8,
    /// Planar YUV 4:2:0 (I420/YV12-style planes).
    Yuv420p,
    /// Planar YUV 4:2:2.
    Yuv422p,
    /// Planar YUV 4:4:4.
    Yuv444p,
    /// Planar 4:2:0 with the V plane first (YV12).
    Yvu420p,
    /// Planar 4:2:2 with the V plane first (YV16).
    Yvu422p,
    /// Planar 4:4:4 with the V plane first (YV24).
    Yvu444p,
    /// 8-bit indices into a BGRA palette.
    Pal8,
    /// 16-bit little-endian RGB 5:5:5, as used by DIBs.
    Bgr555,
    /// 16-bit little-endian RGB 5:6:5, as used by DIBs.
    Bgr565,
    /// Packed YUY2 (Y0 U Y1 V).
    Yuy2,
    /// Packed UYVY (U Y0 V Y1).
    Uyvy,
}

impl PixelFormat {
    /// A short, lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rgba8888 => "rgba8888",
            Self::Bgra8888 => "bgra8888",
            Self::Rgb888 => "rgb888",
            Self::Bgr888 => "bgr888",
            Self::Gray8 => "gray8",
            Self::Yuv420p => "yuv420p",
            Self::Yuv422p => "yuv422p",
            Self::Yuv444p => "yuv444p",
            Self::Yvu420p => "yvu420p",
            Self::Yvu422p => "yvu422p",
            Self::Yvu444p => "yvu444p",
            Self::Pal8 => "pal8",
            Self::Bgr555 => "bgr555",
            Self::Bgr565 => "bgr565",
            Self::Yuy2 => "yuy2",
            Self::Uyvy => "uyvy",
        }
    }
}

/// Number of bytes in an RGBA8888 frame.
pub fn rgba_len(width: u32, height: u32) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(VideoError::Overflow)
}

/// Number of bytes needed by one frame in `format`.
pub fn frame_len(format: PixelFormat, width: u32, height: u32) -> Result<usize> {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let bytes = match format {
        PixelFormat::Rgba8888 | PixelFormat::Bgra8888 => {
            w.checked_mul(h).and_then(|n| n.checked_mul(4))
        }
        PixelFormat::Rgb888 | PixelFormat::Bgr888 => {
            w.checked_mul(h).and_then(|n| n.checked_mul(3))
        }
        PixelFormat::Gray8 | PixelFormat::Pal8 => w.checked_mul(h),
        PixelFormat::Bgr555 | PixelFormat::Bgr565 => w.checked_mul(h).and_then(|n| n.checked_mul(2)),
        PixelFormat::Yuv420p | PixelFormat::Yvu420p => w
            .checked_mul(h)
            .and_then(|n| n.checked_add(cw.checked_mul(ch)?.checked_mul(2)?)),
        PixelFormat::Yuv422p | PixelFormat::Yvu422p => w
            .checked_mul(h)
            .and_then(|n| n.checked_add(cw.checked_mul(h)?.checked_mul(2)?)),
        PixelFormat::Yuv444p | PixelFormat::Yvu444p => {
            w.checked_mul(h).and_then(|n| n.checked_mul(3))
        }
        PixelFormat::Yuy2 | PixelFormat::Uyvy => w.checked_mul(h).and_then(|n| n.checked_mul(2)),
    };
    bytes.ok_or(VideoError::Overflow)
}

/// A YUV to RGB matrix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColourMatrix {
    /// BT.601, used for standard-definition video and JPEG.
    Bt601,
    /// BT.709, used for high-definition video.
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020,
}

/// 16.16 fixed-point coefficients for one matrix and range.
#[derive(Clone, Copy, Debug)]
pub struct YuvCoefficients {
    y_offset: i32,
    y: i32,
    r_v: i32,
    g_u: i32,
    g_v: i32,
    b_u: i32,
}

impl YuvCoefficients {
    /// Coefficients for a matrix, with limited (16-235) or full (0-255) range.
    pub const fn new(matrix: ColourMatrix, full_range: bool) -> Self {
        // Limited range scales luma by 255/219 and chroma by 255/224. These are
        // the integer coefficients FFmpeg's swscale uses for BT.601.
        match (matrix, full_range) {
            (ColourMatrix::Bt601, false) => Self::make(16, 76309, 104597, 25675, 53279, 132201),
            (ColourMatrix::Bt709, false) => Self::make(16, 76309, 117489, 13975, 34925, 138438),
            (ColourMatrix::Bt2020, false) => Self::make(16, 76309, 110013, 12276, 42626, 140363),
            (ColourMatrix::Bt601, true) => Self::make(0, 65536, 91881, 22554, 46802, 116130),
            (ColourMatrix::Bt709, true) => Self::make(0, 65536, 103206, 12277, 30679, 121609),
            (ColourMatrix::Bt2020, true) => Self::make(0, 65536, 96639, 10784, 37444, 123299),
        }
    }

    const fn make(y_offset: i32, y: i32, r_v: i32, g_u: i32, g_v: i32, b_u: i32) -> Self {
        Self {
            y_offset,
            y,
            r_v,
            g_u,
            g_v,
            b_u,
        }
    }

    /// Converts one sample.
    #[inline]
    pub const fn rgb(&self, y: u8, u: u8, v: u8) -> (u8, u8, u8) {
        let c = (y as i32 - self.y_offset) * self.y;
        let d = u as i32 - 128;
        let e = v as i32 - 128;
        (
            clamp_u8((c + self.r_v * e + 32768) >> 16),
            clamp_u8((c - self.g_u * d - self.g_v * e + 32768) >> 16),
            clamp_u8((c + self.b_u * d + 32768) >> 16),
        )
    }
}

#[inline]
const fn clamp_u8(value: i32) -> u8 {
    if value < 0 {
        0
    } else if value > 255 {
        255
    } else {
        value as u8
    }
}

/// The matrix a stream's own signalling selects, or the conventional choice
/// for its size when it's unspecified. `matrix_coefficients` uses the H.273
/// numbering shared by H.264 VUI, Matroska and MP4 `colr`.
pub fn matrix_for(matrix_coefficients: u8, height: u32) -> ColourMatrix {
    match matrix_coefficients {
        1 => ColourMatrix::Bt709,
        5 | 6 => ColourMatrix::Bt601,
        9 | 10 => ColourMatrix::Bt2020,
        _ if height >= 720 => ColourMatrix::Bt709,
        _ => ColourMatrix::Bt601,
    }
}

const BT601_LIMITED: YuvCoefficients = YuvCoefficients::new(ColourMatrix::Bt601, false);

/// Clamps and converts a limited-range BT.601 YUV triple to RGB.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    BT601_LIMITED.rgb(y, u, v)
}

#[inline]
fn put(out: &mut [u8], offset: usize, r: u8, g: u8, b: u8, a: u8) {
    out[offset] = r;
    out[offset + 1] = g;
    out[offset + 2] = b;
    out[offset + 3] = a;
}

/// Converts one frame in `format` into an RGBA8888 buffer.
pub fn to_rgba(
    src: &[u8],
    format: PixelFormat,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<()> {
    convert(src, format, width, height, out, false, false, &[])
}

/// Converts one bottom-up frame (as stored by uncompressed AVI) to RGBA.
pub fn to_rgba_flipped(
    src: &[u8],
    format: PixelFormat,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<()> {
    convert(src, format, width, height, out, true, false, &[])
}

/// Converts one frame stored as a Windows DIB, as uncompressed AVI does: packed
/// rows padded to four bytes, optionally bottom-up, 32-bit pixels with no
/// meaningful alpha, and 8-bit pixels indexing a BGRA `palette`. Planar and
/// packed YUV have no padding and are always stored top-down.
pub fn dib_to_rgba(
    src: &[u8],
    format: PixelFormat,
    palette: &[u8],
    width: u32,
    height: u32,
    bottom_up: bool,
    out: &mut [u8],
) -> Result<()> {
    let is_rgb = matches!(
        format,
        PixelFormat::Rgb888
            | PixelFormat::Bgr888
            | PixelFormat::Bgra8888
            | PixelFormat::Rgba8888
            | PixelFormat::Gray8
            | PixelFormat::Pal8
            | PixelFormat::Bgr555
            | PixelFormat::Bgr565
    );
    convert(
        src,
        format,
        width,
        height,
        out,
        bottom_up && is_rgb,
        is_rgb,
        palette,
    )?;
    if matches!(format, PixelFormat::Bgra8888 | PixelFormat::Rgba8888) {
        for pixel in out[..rgba_len(width, height)?].as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Ok(())
}

fn convert(
    src: &[u8],
    format: PixelFormat,
    width: u32,
    height: u32,
    out: &mut [u8],
    flip: bool,
    dib: bool,
    palette: &[u8],
) -> Result<()> {
    let needed = frame_len(format, width, height)?;
    if src.len() < needed {
        return Err(VideoError::BufferTooSmall {
            required: needed,
            available: src.len(),
        });
    }
    let out_len = rgba_len(width, height)?;
    if out.len() < out_len {
        return Err(VideoError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);

    match format {
        PixelFormat::Rgba8888
        | PixelFormat::Bgra8888
        | PixelFormat::Rgb888
        | PixelFormat::Bgr888
        | PixelFormat::Gray8
        | PixelFormat::Pal8
        | PixelFormat::Bgr555
        | PixelFormat::Bgr565
        | PixelFormat::Yuy2
        | PixelFormat::Uyvy => {
            let stride = match format {
                PixelFormat::Rgba8888 | PixelFormat::Bgra8888 => w * 4,
                PixelFormat::Rgb888 | PixelFormat::Bgr888 => w * 3,
                PixelFormat::Gray8 | PixelFormat::Pal8 => w,
                _ => w * 2,
            };
            // DIB rows are padded to four bytes. Accept a frame of tightly
            // packed rows too: some muxers, FFmpeg's among them, write those.
            // The last padded row needn't carry its padding.
            let padded = (stride + 3) & !3;
            let padded_size = (padded * h).saturating_sub(padded - stride);
            let pitch = if dib && src.len() >= padded_size {
                padded
            } else {
                stride
            };
            for y in 0..h {
                let sy = if flip { h - 1 - y } else { y };
                let srow = &src[sy * pitch..sy * pitch + stride];
                let orow = &mut out[y * w * 4..(y + 1) * w * 4];
                match format {
                    PixelFormat::Rgba8888 => {
                        for (i, px) in srow.as_chunks::<4>().0.iter().enumerate() {
                            put(orow, i * 4, px[0], px[1], px[2], px[3]);
                        }
                    }
                    PixelFormat::Bgra8888 => {
                        for (i, px) in srow.as_chunks::<4>().0.iter().enumerate() {
                            put(orow, i * 4, px[2], px[1], px[0], px[3]);
                        }
                    }
                    PixelFormat::Rgb888 => {
                        for (i, px) in srow.as_chunks::<3>().0.iter().enumerate() {
                            put(orow, i * 4, px[0], px[1], px[2], 255);
                        }
                    }
                    PixelFormat::Bgr888 => {
                        for (i, px) in srow.as_chunks::<3>().0.iter().enumerate() {
                            put(orow, i * 4, px[2], px[1], px[0], 255);
                        }
                    }
                    PixelFormat::Gray8 => {
                        for (i, &v) in srow.iter().enumerate() {
                            put(orow, i * 4, v, v, v, 255);
                        }
                    }
                    PixelFormat::Pal8 => {
                        for (i, &index) in srow.iter().enumerate() {
                            let (r, g, b) = palette
                                .get(index as usize * 4..index as usize * 4 + 3)
                                .map_or((index, index, index), |c| (c[2], c[1], c[0]));
                            put(orow, i * 4, r, g, b, 255);
                        }
                    }
                    PixelFormat::Bgr555 => {
                        for (i, px) in srow.as_chunks::<2>().0.iter().enumerate() {
                            let v = u16::from_le_bytes([px[0], px[1]]);
                            let (r, g, b) = ((v >> 10) & 31, (v >> 5) & 31, v & 31);
                            put(
                                orow,
                                i * 4,
                                ((r << 3) | (r >> 2)) as u8,
                                ((g << 3) | (g >> 2)) as u8,
                                ((b << 3) | (b >> 2)) as u8,
                                255,
                            );
                        }
                    }
                    PixelFormat::Bgr565 => {
                        for (i, px) in srow.as_chunks::<2>().0.iter().enumerate() {
                            let v = u16::from_le_bytes([px[0], px[1]]);
                            let (r, g, b) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                            put(
                                orow,
                                i * 4,
                                ((r << 3) | (r >> 2)) as u8,
                                ((g << 2) | (g >> 4)) as u8,
                                ((b << 3) | (b >> 2)) as u8,
                                255,
                            );
                        }
                    }
                    PixelFormat::Yuy2 => {
                        for x in (0..w).step_by(2) {
                            let i = x * 2;
                            let y0 = srow[i];
                            let u = srow[i + 1];
                            let y1 = srow[(i + 2).min(stride - 1)];
                            let v = srow[(i + 3).min(stride - 1)];
                            let (r, g, b) = yuv_to_rgb(y0, u, v);
                            put(orow, x * 4, r, g, b, 255);
                            if x + 1 < w {
                                let (r, g, b) = yuv_to_rgb(y1, u, v);
                                put(orow, (x + 1) * 4, r, g, b, 255);
                            }
                        }
                    }
                    PixelFormat::Uyvy => {
                        for x in (0..w).step_by(2) {
                            let i = x * 2;
                            let u = srow[i];
                            let y0 = srow[i + 1];
                            let v = srow[(i + 2).min(stride - 1)];
                            let y1 = srow[(i + 3).min(stride - 1)];
                            let (r, g, b) = yuv_to_rgb(y0, u, v);
                            put(orow, x * 4, r, g, b, 255);
                            if x + 1 < w {
                                let (r, g, b) = yuv_to_rgb(y1, u, v);
                                put(orow, (x + 1) * 4, r, g, b, 255);
                            }
                        }
                    }
                    // The outer arm lets only the formats matched here in.
                    #[allow(clippy::unreachable)]
                    _ => unreachable!(),
                }
            }
        }
        PixelFormat::Yuv420p
        | PixelFormat::Yuv422p
        | PixelFormat::Yuv444p
        | PixelFormat::Yvu420p
        | PixelFormat::Yvu422p
        | PixelFormat::Yvu444p => {
            let plane = w * h;
            let (first, second) = match format {
                PixelFormat::Yuv420p | PixelFormat::Yvu420p => (
                    &src[plane..plane + cw * ch],
                    &src[plane + cw * ch..plane + 2 * cw * ch],
                ),
                PixelFormat::Yuv422p | PixelFormat::Yvu422p => (
                    &src[plane..plane + cw * h],
                    &src[plane + cw * h..plane + 2 * cw * h],
                ),
                _ => (&src[plane..plane * 2], &src[plane * 2..plane * 3]),
            };
            let (u, v) = match format {
                PixelFormat::Yvu420p | PixelFormat::Yvu422p | PixelFormat::Yvu444p => {
                    (second, first)
                }
                _ => (first, second),
            };
            for y in 0..h {
                let sy = if flip { h - 1 - y } else { y };
                let orow = &mut out[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    let (uu, vv) = match format {
                        PixelFormat::Yuv420p | PixelFormat::Yvu420p => {
                            (u[(sy / 2) * cw + x / 2], v[(sy / 2) * cw + x / 2])
                        }
                        PixelFormat::Yuv422p | PixelFormat::Yvu422p => {
                            (u[sy * cw + x / 2], v[sy * cw + x / 2])
                        }
                        _ => (u[sy * w + x], v[sy * w + x]),
                    };
                    let (r, g, b) = yuv_to_rgb(src[sy * w + x], uu, vv);
                    put(orow, x * 4, r, g, b, 255);
                }
            }
        }
    }
    Ok(())
}

/// Converts a planar 4:2:0 YUV frame into RGBA.
pub fn yuv420_to_rgba(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<()> {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    if y.len() < w * h || u.len() < cw * ch || v.len() < cw * ch {
        return Err(VideoError::InvalidData("short YUV plane"));
    }
    let out_len = rgba_len(width, height)?;
    if out.len() < out_len {
        return Err(VideoError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    for row in 0..h {
        for col in 0..w {
            let (r, g, b) = yuv_to_rgb(
                y[row * w + col],
                u[(row / 2) * cw + col / 2],
                v[(row / 2) * cw + col / 2],
            );
            put(out, (row * w + col) * 4, r, g, b, 255);
        }
    }
    Ok(())
}

/// Converts one row pair's worth of 4:2:0 samples into an RGBA row.
#[inline]
pub(crate) fn yuv420_row(
    coefficients: &YuvCoefficients,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    out: &mut [u8],
) {
    for (x, (pixel, &luma)) in out.as_chunks_mut::<4>().0.iter_mut().zip(y).enumerate() {
        let chroma = x / 2;
        let cb = u.get(chroma).copied().unwrap_or(128);
        let cr = v.get(chroma).copied().unwrap_or(128);
        let (r, g, b) = coefficients.rgb(luma, cb, cr);
        pixel.copy_from_slice(&[r, g, b, 255]);
    }
}
