//! Motion compensation (spec 8.4.2): quarter-sample luma, eighth-sample chroma and
//! weighted prediction. A reference is a window of a plane: a frame, or one field of a
//! frame seen with a doubled stride.

use super::pixel::Pixel;

/// One plane of a reference picture, as a frame or a field.
#[derive(Clone, Copy)]
pub(crate) struct View<'a, P> {
    pub data: &'a [P],
    /// Offset of the first line: the bottom field starts one frame line in.
    pub base: usize,
    pub stride: usize,
    pub width: i32,
    pub height: i32,
}

impl<P: Pixel> View<'_, P> {
    #[inline(always)]
    fn at(&self, x: i32, y: i32) -> P {
        let x = x.clamp(0, self.width - 1) as usize;
        let y = y.clamp(0, self.height - 1) as usize;
        self.data[self.base + y * self.stride + x]
    }
}

const WINDOW: usize = 24;

#[inline(always)]
fn tap(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// The horizontal half-sample positions of the block starting at `org`.
#[inline(always)]
fn half_h<P: Pixel>(src: &[P], org: usize, stride: usize, w: usize, h: usize, out: &mut [P; 256], max: i32) {
    for j in 0..h {
        let r = org + j * stride - 2;
        let s = &src[r..r + w + 5];
        let line = &mut out[j * 16..j * 16 + w];
        for (i, o) in line.iter_mut().enumerate() {
            let val = tap(
                s[i].get(),
                s[i + 1].get(),
                s[i + 2].get(),
                s[i + 3].get(),
                s[i + 4].get(),
                s[i + 5].get(),
            );
            *o = P::new(((val + 16) >> 5).clamp(0, max));
        }
    }
}

/// The vertical half-sample positions of the block starting at `org`.
#[inline(always)]
fn half_v<P: Pixel>(src: &[P], org: usize, stride: usize, w: usize, h: usize, out: &mut [P; 256], max: i32) {
    for j in 0..h {
        let r = org + j * stride;
        let rows = [
            &src[r - 2 * stride..r - 2 * stride + w],
            &src[r - stride..r - stride + w],
            &src[r..r + w],
            &src[r + stride..r + stride + w],
            &src[r + 2 * stride..r + 2 * stride + w],
            &src[r + 3 * stride..r + 3 * stride + w],
        ];
        let line = &mut out[j * 16..j * 16 + w];
        for (i, o) in line.iter_mut().enumerate() {
            let val = tap(
                rows[0][i].get(),
                rows[1][i].get(),
                rows[2][i].get(),
                rows[3][i].get(),
                rows[4][i].get(),
                rows[5][i].get(),
            );
            *o = P::new(((val + 16) >> 5).clamp(0, max));
        }
    }
}

/// The positions halfway both ways.
#[inline(always)]
fn half_hv<P: Pixel>(src: &[P], org: usize, stride: usize, w: usize, h: usize, out: &mut [P; 256], max: i32) {
    let mut mid = [0i32; 21 * 16];
    for j in 0..h + 5 {
        let r = org + j * stride - 2 * stride - 2;
        let s = &src[r..r + w + 5];
        for i in 0..w {
            mid[j * 16 + i] = tap(
                s[i].get(),
                s[i + 1].get(),
                s[i + 2].get(),
                s[i + 3].get(),
                s[i + 4].get(),
                s[i + 5].get(),
            );
        }
    }
    for j in 0..h {
        for i in 0..w {
            let at = j * 16 + i;
            let val = tap(
                mid[at],
                mid[at + 16],
                mid[at + 32],
                mid[at + 48],
                mid[at + 64],
                mid[at + 80],
            );
            out[at] = P::new(((val + 512) >> 10).clamp(0, max));
        }
    }
}

#[inline(always)]
fn average_into<P: Pixel>(out: &mut [P; 256], other: &[P; 256], w: usize, h: usize) {
    for j in 0..h {
        let line = &mut out[j * 16..j * 16 + w];
        let from = &other[j * 16..j * 16 + w];
        for (o, f) in line.iter_mut().zip(from) {
            *o = P::new((o.get() + f.get() + 1) >> 1);
        }
    }
}

#[inline(always)]
fn average_src<P: Pixel>(out: &mut [P; 256], src: &[P], org: usize, stride: usize, w: usize, h: usize) {
    for j in 0..h {
        let line = &mut out[j * 16..j * 16 + w];
        let from = &src[org + j * stride..org + j * stride + w];
        for (o, f) in line.iter_mut().zip(from) {
            *o = P::new((o.get() + f.get() + 1) >> 1);
        }
    }
}

/// Quarter-sample interpolation (8.4.2.2.1) from a window where the filter's reach is valid.
fn luma_kernel<P: Pixel>(src: &[P], org: usize, stride: usize, fx: i32, fy: i32, w: usize, h: usize, out: &mut [P; 256], max: i32) {
    let right = org + 1;
    let below = org + stride;
    match (fx, fy) {
        (0, 0) => {
            for j in 0..h {
                let row = org + j * stride;
                out[j * 16..j * 16 + w].copy_from_slice(&src[row..row + w]);
            }
        }
        (_, 0) => {
            half_h(src, org, stride, w, h, out, max);
            if fx != 2 {
                average_src(out, src, if fx == 3 { right } else { org }, stride, w, h);
            }
        }
        (0, _) => {
            half_v(src, org, stride, w, h, out, max);
            if fy != 2 {
                average_src(out, src, if fy == 3 { below } else { org }, stride, w, h);
            }
        }
        (2, 2) => half_hv(src, org, stride, w, h, out, max),
        (2, _) => {
            half_hv(src, org, stride, w, h, out, max);
            let mut other = [P::default(); 256];
            half_h(src, if fy == 3 { below } else { org }, stride, w, h, &mut other, max);
            average_into(out, &other, w, h);
        }
        (_, 2) => {
            half_hv(src, org, stride, w, h, out, max);
            let mut other = [P::default(); 256];
            half_v(src, if fx == 3 { right } else { org }, stride, w, h, &mut other, max);
            average_into(out, &other, w, h);
        }
        _ => {
            half_h(src, if fy == 3 { below } else { org }, stride, w, h, out, max);
            let mut other = [P::default(); 256];
            half_v(src, if fx == 3 { right } else { org }, stride, w, h, &mut other, max);
            average_into(out, &other, w, h);
        }
    }
}

/// Quarter-sample luma prediction of a `w`x`h` block at integer position (`x`, `y`)
/// plus fraction (`fx`, `fy`). `out` has a stride of 16.
pub(crate) fn luma<P: Pixel>(
    v: &View<P>,
    x: i32,
    y: i32,
    fx: i32,
    fy: i32,
    w: usize,
    h: usize,
    out: &mut [P; 256],
    max: i32,
) {
    // The window includes the six-tap filter's reach. Outside the picture, samples
    // come from the nearest edge.
    if x >= 2 && y >= 2 && x + w as i32 + 3 <= v.width && y + h as i32 + 3 <= v.height {
        let org = v.base + y as usize * v.stride + x as usize;
        luma_kernel(v.data, org, v.stride, fx, fy, w, h, out, max);
    } else {
        let mut edge = [P::default(); WINDOW * WINDOW];
        for j in 0..h + 5 {
            for i in 0..w + 5 {
                edge[j * WINDOW + i] = v.at(x + i as i32 - 2, y + j as i32 - 2);
            }
        }
        luma_kernel(&edge, 2 * WINDOW + 2, WINDOW, fx, fy, w, h, out, max);
    }
}

/// Bilinear chroma prediction. (`x`, `y`) is the integer position, (`fx`, `fy`) in
/// eighths. `out` has a stride of 16.
pub(crate) fn chroma<P: Pixel>(
    v: &View<P>,
    x: i32,
    y: i32,
    fx: i32,
    fy: i32,
    w: usize,
    h: usize,
    out: &mut [P; 256],
) {
    let inside = x >= 0 && y >= 0 && x + (w as i32) < v.width && y + (h as i32) < v.height;
    let (wa, wb, wc, wd) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    if inside {
        let org = v.base + y as usize * v.stride + x as usize;
        for j in 0..h {
            let r0 = org + j * v.stride;
            let top = &v.data[r0..r0 + w + 1];
            let bot = &v.data[r0 + v.stride..r0 + v.stride + w + 1];
            for i in 0..w {
                let val = wa * top[i].get()
                    + wb * top[i + 1].get()
                    + wc * bot[i].get()
                    + wd * bot[i + 1].get();
                out[j * 16 + i] = P::new((val + 32) >> 6);
            }
        }
    } else {
        for j in 0..h as i32 {
            for i in 0..w as i32 {
                let val = wa * v.at(x + i, y + j).get()
                    + wb * v.at(x + i + 1, y + j).get()
                    + wc * v.at(x + i, y + j + 1).get()
                    + wd * v.at(x + i + 1, y + j + 1).get();
                out[j as usize * 16 + i as usize] = P::new((val + 32) >> 6);
            }
        }
    }
}

/// Default prediction from two lists, into the first.
pub(crate) fn average<P: Pixel>(a: &mut [P; 256], b: &[P; 256], w: usize, h: usize) {
    for j in 0..h {
        for i in 0..w {
            a[j * 16 + i] = P::new((a[j * 16 + i].get() + b[j * 16 + i].get() + 1) >> 1);
        }
    }
}

/// Explicit weighting of one list, in place.
pub(crate) fn weight_uni<P: Pixel>(
    a: &mut [P; 256],
    w: usize,
    h: usize,
    log_wd: u32,
    weight: i32,
    offset: i32,
    max: i32,
) {
    for j in 0..h {
        for i in 0..w {
            let s = a[j * 16 + i].get() * weight;
            let val = if log_wd >= 1 {
                ((s + (1 << (log_wd - 1))) >> log_wd) + offset
            } else {
                s + offset
            };
            a[j * 16 + i] = P::new(val.clamp(0, max));
        }
    }
}

/// Weighting of two lists, into the first. Offsets are the sum's halves, as in
/// (o0 + o1 + 1) >> 1.
pub(crate) fn weight_bi<P: Pixel>(
    a: &mut [P; 256],
    b: &[P; 256],
    w: usize,
    h: usize,
    log_wd: u32,
    w0: i32,
    w1: i32,
    offset: i32,
    max: i32,
) {
    for j in 0..h {
        for i in 0..w {
            let s = a[j * 16 + i].get() * w0 + b[j * 16 + i].get() * w1;
            let val = ((s + (1 << log_wd)) >> (log_wd + 1)) + offset;
            a[j * 16 + i] = P::new(val.clamp(0, max));
        }
    }
}
