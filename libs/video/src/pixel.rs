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
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;
    let bytes = match format {
        PixelFormat::Rgba8888 | PixelFormat::Bgra8888 => {
            w.checked_mul(h).and_then(|n| n.checked_mul(4))
        }
        PixelFormat::Rgb888 | PixelFormat::Bgr888 => {
            w.checked_mul(h).and_then(|n| n.checked_mul(3))
        }
        PixelFormat::Gray8 => w.checked_mul(h),
        PixelFormat::Yuv420p => w
            .checked_mul(h)
            .and_then(|n| n.checked_add(cw.checked_mul(ch)?.checked_mul(2)?)),
        PixelFormat::Yuv422p => w
            .checked_mul(h)
            .and_then(|n| n.checked_add(cw.checked_mul(h)?.checked_mul(2)?)),
        PixelFormat::Yuv444p => w.checked_mul(h).and_then(|n| n.checked_mul(3)),
        PixelFormat::Yuy2 | PixelFormat::Uyvy => w.checked_mul(h).and_then(|n| n.checked_mul(2)),
    };
    bytes.ok_or(VideoError::Overflow)
}

/// Clamps and converts a limited-range BT.601 YUV triple to RGB.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    // Integer approximation of the limited-range BT.601 matrix (the same
    // coefficients used by FFmpeg's swscale).
    let r = (76309 * c + 104597 * e + 32768) >> 16;
    let g = (76309 * c - 25675 * d - 53279 * e + 32768) >> 16;
    let b = (76309 * c + 132201 * d + 32768) >> 16;
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
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
    convert(src, format, width, height, out, false)
}

/// Converts one bottom-up frame (as stored by uncompressed AVI) to RGBA.
pub fn to_rgba_flipped(
    src: &[u8],
    format: PixelFormat,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<()> {
    convert(src, format, width, height, out, true)
}

fn convert(
    src: &[u8],
    format: PixelFormat,
    width: u32,
    height: u32,
    out: &mut [u8],
    flip: bool,
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
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;

    match format {
        PixelFormat::Rgba8888
        | PixelFormat::Bgra8888
        | PixelFormat::Rgb888
        | PixelFormat::Bgr888
        | PixelFormat::Gray8
        | PixelFormat::Yuy2
        | PixelFormat::Uyvy => {
            let stride = match format {
                PixelFormat::Rgba8888 | PixelFormat::Bgra8888 => w * 4,
                PixelFormat::Rgb888 | PixelFormat::Bgr888 => w * 3,
                PixelFormat::Gray8 => w,
                _ => w * 2,
            };
            for y in 0..h {
                let sy = if flip { h - 1 - y } else { y };
                let srow = &src[sy * stride..(sy + 1) * stride];
                let orow = &mut out[y * w * 4..(y + 1) * w * 4];
                match format {
                    PixelFormat::Rgba8888 => {
                        for (i, px) in srow.chunks_exact(4).enumerate() {
                            put(orow, i * 4, px[0], px[1], px[2], px[3]);
                        }
                    }
                    PixelFormat::Bgra8888 => {
                        for (i, px) in srow.chunks_exact(4).enumerate() {
                            put(orow, i * 4, px[2], px[1], px[0], px[3]);
                        }
                    }
                    PixelFormat::Rgb888 => {
                        for (i, px) in srow.chunks_exact(3).enumerate() {
                            put(orow, i * 4, px[0], px[1], px[2], 255);
                        }
                    }
                    PixelFormat::Bgr888 => {
                        for (i, px) in srow.chunks_exact(3).enumerate() {
                            put(orow, i * 4, px[2], px[1], px[0], 255);
                        }
                    }
                    PixelFormat::Gray8 => {
                        for (i, &v) in srow.iter().enumerate() {
                            put(orow, i * 4, v, v, v, 255);
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
                    _ => unreachable!(),
                }
            }
        }
        PixelFormat::Yuv420p | PixelFormat::Yuv422p | PixelFormat::Yuv444p => {
            let plane = w * h;
            let (u, v) = match format {
                PixelFormat::Yuv420p => (
                    &src[plane..plane + cw * ch],
                    &src[plane + cw * ch..plane + 2 * cw * ch],
                ),
                PixelFormat::Yuv422p => (
                    &src[plane..plane + cw * h],
                    &src[plane + cw * h..plane + 2 * cw * h],
                ),
                _ => (&src[plane..plane * 2], &src[plane * 2..plane * 3]),
            };
            for y in 0..h {
                let sy = if flip { h - 1 - y } else { y };
                let orow = &mut out[y * w * 4..(y + 1) * w * 4];
                for x in 0..w {
                    let (uu, vv) = match format {
                        PixelFormat::Yuv420p => {
                            (u[(sy / 2) * cw + x / 2], v[(sy / 2) * cw + x / 2])
                        }
                        PixelFormat::Yuv422p => (u[sy * cw + x / 2], v[sy * cw + x / 2]),
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
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;
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
