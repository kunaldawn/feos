//! Intra and inter prediction, and adding the residual on.

use super::{Frame, Mv, B_DC_PRED, B_HD_PRED, B_HE_PRED, B_HU_PRED, B_LD_PRED, B_PRED, B_RD_PRED, B_TM_PRED, B_VE_PRED, B_VL_PRED, B_VR_PRED, H_PRED, TM_PRED, V_PRED};

const SINPI8SQRT2: i32 = 35468;
const COSPI8SQRT2MINUS1: i32 = 20091;

/// The inverse Walsh-Hadamard transform, of a macroblock's Y2 block.
pub(super) fn inverse_walsh(input: &[i16; 16]) -> [i16; 16] {
    let mut temp = [0i32; 16];
    for i in 0..4 {
        let (i0, i4, i8, i12) = (input[i] as i32, input[4 + i] as i32, input[8 + i] as i32, input[12 + i] as i32);
        let (a1, b1, c1, d1) = (i0 + i12, i4 + i8, i4 - i8, i0 - i12);
        temp[i] = (a1 + b1) as i16 as i32;
        temp[4 + i] = (c1 + d1) as i16 as i32;
        temp[8 + i] = (a1 - b1) as i16 as i32;
        temp[12 + i] = (d1 - c1) as i16 as i32;
    }
    let mut out = [0i16; 16];
    for i in 0..4 {
        let row = &temp[i * 4..i * 4 + 4];
        let (a1, b1, c1, d1) = (row[0] + row[3], row[1] + row[2], row[1] - row[2], row[0] - row[3]);
        out[i * 4] = ((a1 + b1 + 3) >> 3) as i16;
        out[i * 4 + 1] = ((c1 + d1 + 3) >> 3) as i16;
        out[i * 4 + 2] = ((a1 - b1 + 3) >> 3) as i16;
        out[i * 4 + 3] = ((d1 - c1 + 3) >> 3) as i16;
    }
    out
}

#[inline]
fn clamp255(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// The inverse DCT, added to what's at `at` in `plane`.
#[inline]
fn add_residual(plane: &mut [u8], at: usize, stride: usize, coeffs: &[i16]) {
    if coeffs.iter().all(|&c| c == 0) {
        return;
    }
    let mut temp = [0i32; 16];
    for i in 0..4 {
        let (i0, i4, i8, i12) = (coeffs[i] as i32, coeffs[4 + i] as i32, coeffs[8 + i] as i32, coeffs[12 + i] as i32);
        let a1 = i0 + i8;
        let b1 = i0 - i8;
        let c1 = ((i4 * SINPI8SQRT2) >> 16) - (i12 + ((i12 * COSPI8SQRT2MINUS1) >> 16));
        let d1 = (i4 + ((i4 * COSPI8SQRT2MINUS1) >> 16)) + ((i12 * SINPI8SQRT2) >> 16);
        temp[i] = (a1 + d1) as i16 as i32;
        temp[12 + i] = (a1 - d1) as i16 as i32;
        temp[4 + i] = (b1 + c1) as i16 as i32;
        temp[8 + i] = (b1 - c1) as i16 as i32;
    }
    for i in 0..4 {
        let row = &temp[i * 4..i * 4 + 4];
        let a1 = row[0] + row[2];
        let b1 = row[0] - row[2];
        let c1 = ((row[1] * SINPI8SQRT2) >> 16) - (row[3] + ((row[3] * COSPI8SQRT2MINUS1) >> 16));
        let d1 = (row[1] + ((row[1] * COSPI8SQRT2MINUS1) >> 16)) + ((row[3] * SINPI8SQRT2) >> 16);
        let out = &mut plane[at + i * stride..at + i * stride + 4];
        out[0] = clamp255(out[0] as i32 + ((a1 + d1 + 4) >> 3));
        out[3] = clamp255(out[3] as i32 + ((a1 - d1 + 4) >> 3));
        out[1] = clamp255(out[1] as i32 + ((b1 + c1 + 4) >> 3));
        out[2] = clamp255(out[2] as i32 + ((b1 - c1 + 4) >> 3));
    }
}

/// Predicts a whole square, for 16x16 luma or 8x8 chroma.
fn predict_square(plane: &mut [u8], stride: usize, row: usize, col: usize, size: usize, mode: u8) {
    let at = row * size * stride + col * size;
    let mut above = [127u8; 16];
    let mut left = [129u8; 16];
    if row > 0 {
        above[..size].copy_from_slice(&plane[at - stride..at - stride + size]);
    }
    if col > 0 {
        for (i, value) in left.iter_mut().enumerate().take(size) {
            *value = plane[at + i * stride - 1];
        }
    }
    let corner = if row == 0 {
        127
    } else if col == 0 {
        129
    } else {
        plane[at - stride - 1]
    };
    match mode {
        V_PRED => {
            for i in 0..size {
                plane[at + i * stride..at + i * stride + size].copy_from_slice(&above[..size]);
            }
        }
        H_PRED => {
            for i in 0..size {
                plane[at + i * stride..at + i * stride + size].fill(left[i]);
            }
        }
        TM_PRED => {
            for i in 0..size {
                for j in 0..size {
                    plane[at + i * stride + j] =
                        clamp255(left[i] as i32 + above[j] as i32 - corner as i32);
                }
            }
        }
        _ => {
            let shift = size.trailing_zeros();
            let mut sum = 0u32;
            let mut edges = 0;
            if row > 0 {
                sum += above[..size].iter().map(|&v| v as u32).sum::<u32>();
                edges += 1;
            }
            if col > 0 {
                sum += left[..size].iter().map(|&v| v as u32).sum::<u32>();
                edges += 1;
            }
            let value = match edges {
                0 => 128,
                1 => (sum + (1 << (shift - 1))) >> shift,
                _ => (sum + (1 << shift)) >> (shift + 1),
            } as u8;
            for i in 0..size {
                plane[at + i * stride..at + i * stride + size].fill(value);
            }
        }
    }
}

/// Stride of the working copy for subblock prediction: one to the left, sixteen, and
/// four above and to the right.
const WORK: usize = 21;

/// Predicts one 4x4 subblock in the working copy, at `(x, y)` from its first real pixel.
fn predict_subblock(work: &mut [u8; WORK * 17], x: usize, y: usize, mode: u8) {
    let at = (y + 1) * WORK + x + 1;
    let e = |i: usize| work[at - WORK + i] as i32; // above, and on to the right
    let l = |i: usize| work[at + i * WORK - 1] as i32;
    let p = work[at - WORK - 1] as i32;
    let (a0, a1, a2, a3, a4, a5, a6, a7) = (e(0), e(1), e(2), e(3), e(4), e(5), e(6), e(7));
    let (l0, l1, l2, l3) = (l(0), l(1), l(2), l(3));
    let avg2 = |a: i32, b: i32| ((a + b + 1) >> 1) as u8;
    let avg3 = |a: i32, b: i32, c: i32| ((a + 2 * b + c + 2) >> 2) as u8;
    let mut out = [[0u8; 4]; 4];
    match mode {
        B_DC_PRED => {
            let value = ((a0 + a1 + a2 + a3 + l0 + l1 + l2 + l3 + 4) >> 3) as u8;
            out = [[value; 4]; 4];
        }
        B_TM_PRED => {
            let above = [a0, a1, a2, a3];
            let left = [l0, l1, l2, l3];
            for r in 0..4 {
                for c in 0..4 {
                    out[r][c] = clamp255(left[r] + above[c] - p);
                }
            }
        }
        B_VE_PRED => {
            let row = [avg3(p, a0, a1), avg3(a0, a1, a2), avg3(a1, a2, a3), avg3(a2, a3, a4)];
            out = [row; 4];
        }
        B_HE_PRED => {
            out[0] = [avg3(p, l0, l1); 4];
            out[1] = [avg3(l0, l1, l2); 4];
            out[2] = [avg3(l1, l2, l3); 4];
            out[3] = [avg3(l2, l3, l3); 4];
        }
        B_LD_PRED => {
            let d = [
                avg3(a0, a1, a2),
                avg3(a1, a2, a3),
                avg3(a2, a3, a4),
                avg3(a3, a4, a5),
                avg3(a4, a5, a6),
                avg3(a5, a6, a7),
                avg3(a6, a7, a7),
            ];
            for (r, row) in out.iter_mut().enumerate() {
                row.copy_from_slice(&d[r..r + 4]);
            }
        }
        B_RD_PRED => {
            let d = [
                avg3(l3, l2, l1),
                avg3(l2, l1, l0),
                avg3(l1, l0, p),
                avg3(l0, p, a0),
                avg3(p, a0, a1),
                avg3(a0, a1, a2),
                avg3(a1, a2, a3),
            ];
            for r in 0..4 {
                for c in 0..4 {
                    out[r][c] = d[3 + c - r];
                }
            }
        }
        B_VR_PRED => {
            out[3][0] = avg3(l2, l1, l0);
            out[2][0] = avg3(l1, l0, p);
            out[1][0] = avg3(l0, p, a0);
            out[3][1] = out[1][0];
            out[0][0] = avg2(p, a0);
            out[2][1] = out[0][0];
            out[1][1] = avg3(p, a0, a1);
            out[3][2] = out[1][1];
            out[0][1] = avg2(a0, a1);
            out[2][2] = out[0][1];
            out[1][2] = avg3(a0, a1, a2);
            out[3][3] = out[1][2];
            out[0][2] = avg2(a1, a2);
            out[2][3] = out[0][2];
            out[1][3] = avg3(a1, a2, a3);
            out[0][3] = avg2(a2, a3);
        }
        B_VL_PRED => {
            out[0][0] = avg2(a0, a1);
            out[1][0] = avg3(a0, a1, a2);
            out[0][1] = avg2(a1, a2);
            out[2][0] = out[0][1];
            out[1][1] = avg3(a1, a2, a3);
            out[3][0] = out[1][1];
            out[0][2] = avg2(a2, a3);
            out[2][1] = out[0][2];
            out[1][2] = avg3(a2, a3, a4);
            out[3][1] = out[1][2];
            out[0][3] = avg2(a3, a4);
            out[2][2] = out[0][3];
            out[1][3] = avg3(a3, a4, a5);
            out[3][2] = out[1][3];
            // These two don't follow the pattern, and that's how the format is.
            out[2][3] = avg3(a4, a5, a6);
            out[3][3] = avg3(a5, a6, a7);
        }
        B_HD_PRED => {
            out[3][0] = avg2(l3, l2);
            out[3][1] = avg3(l3, l2, l1);
            out[2][0] = avg2(l2, l1);
            out[3][2] = out[2][0];
            out[2][1] = avg3(l2, l1, l0);
            out[3][3] = out[2][1];
            out[2][2] = avg2(l1, l0);
            out[1][0] = out[2][2];
            out[2][3] = avg3(l1, l0, p);
            out[1][1] = out[2][3];
            out[1][2] = avg2(l0, p);
            out[0][0] = out[1][2];
            out[1][3] = avg3(l0, p, a0);
            out[0][1] = out[1][3];
            out[0][2] = avg3(p, a0, a1);
            out[0][3] = avg3(a0, a1, a2);
        }
        B_HU_PRED => {
            out[0][0] = avg2(l0, l1);
            out[0][1] = avg3(l0, l1, l2);
            out[0][2] = avg2(l1, l2);
            out[1][0] = out[0][2];
            out[0][3] = avg3(l1, l2, l3);
            out[1][1] = out[0][3];
            out[1][2] = avg2(l2, l3);
            out[2][0] = out[1][2];
            out[1][3] = avg3(l2, l3, l3);
            out[2][1] = out[1][3];
            let last = l3 as u8;
            out[2][2] = last;
            out[2][3] = last;
            out[3] = [last; 4];
        }
        _ => {}
    }
    for (r, row) in out.iter().enumerate() {
        work[at + r * WORK..at + r * WORK + 4].copy_from_slice(row);
    }
}

/// Predicts and reconstructs an intra macroblock.
#[allow(clippy::too_many_arguments)]
pub(super) fn intra(
    frame: &mut Frame,
    cols: usize,
    _rows: usize,
    row: usize,
    col: usize,
    y_mode: u8,
    uv_mode: u8,
    modes: &[u8; 16],
    coeffs: &[i16; 25 * 16],
) {
    let stride = cols * 16;
    let at = row * 16 * stride + col * 16;
    if y_mode == B_PRED {
        let mut work = [0u8; WORK * 17];
        // The row above, with its corner and the four to its right.
        if row == 0 {
            work[..WORK].fill(127);
        } else {
            let above = at - stride;
            work[0] = if col == 0 { 129 } else { frame.y[above - 1] };
            work[1..17].copy_from_slice(&frame.y[above..above + 16]);
            if col + 1 < cols {
                work[17..21].copy_from_slice(&frame.y[above + 16..above + 20]);
            } else {
                // Off the frame's right edge, the above row's last pixel carries on.
                let last = frame.y[above + 15];
                work[17..21].fill(last);
            }
        }
        for i in 0..16 {
            work[(i + 1) * WORK] = if col == 0 { 129 } else { frame.y[at + i * stride - 1] };
        }
        // Subblocks on the right use the row above the macroblock for all their rows.
        for r in [4, 8, 12] {
            let (top, rest) = work.split_at_mut(WORK);
            rest[(r - 1) * WORK + 17..(r - 1) * WORK + 21].copy_from_slice(&top[17..21]);
        }
        for b in 0..16 {
            let (x, y) = ((b & 3) * 4, (b >> 2) * 4);
            predict_subblock(&mut work, x, y, modes[b]);
            add_residual(&mut work, (y + 1) * WORK + x + 1, WORK, &coeffs[b * 16..b * 16 + 16]);
        }
        for r in 0..16 {
            frame.y[at + r * stride..at + r * stride + 16]
                .copy_from_slice(&work[(r + 1) * WORK + 1..(r + 1) * WORK + 17]);
        }
    } else {
        predict_square(&mut frame.y, stride, row, col, 16, y_mode);
        for b in 0..16 {
            let block = at + (b >> 2) * 4 * stride + (b & 3) * 4;
            add_residual(&mut frame.y, block, stride, &coeffs[b * 16..b * 16 + 16]);
        }
    }
    let stride = cols * 8;
    let at = row * 8 * stride + col * 8;
    predict_square(&mut frame.u, stride, row, col, 8, uv_mode);
    predict_square(&mut frame.v, stride, row, col, 8, uv_mode);
    for b in 0..4 {
        let block = at + (b >> 1) * 4 * stride + (b & 1) * 4;
        add_residual(&mut frame.u, block, stride, &coeffs[(16 + b) * 16..(17 + b) * 16]);
        add_residual(&mut frame.v, block, stride, &coeffs[(20 + b) * 16..(21 + b) * 16]);
    }
}

/// The picture being referred to, and how to read it.
pub(super) struct Reference<'a> {
    pub frame: &'a Frame,
    pub cols: usize,
    pub rows: usize,
    pub filters: &'static [[i16; 6]; 8],
    pub full_pixel: bool,
}

/// Predicts a block from `reference` into `out`. The picture is treated as if its edge
/// pixels carried on for ever, and vectors reach as far as they like.
#[allow(clippy::too_many_arguments)]
fn predict_block(
    out: &mut [u8],
    out_at: usize,
    out_stride: usize,
    reference: &[u8],
    stride: usize,
    height: usize,
    x: i32,
    y: i32,
    width_block: usize,
    height_block: usize,
    mv: Mv,
    filters: &[[i16; 6]; 8],
) {
    let (mx, my) = ((mv.x & 7) as usize, (mv.y & 7) as usize);
    let sx = x + (mv.x >> 3) as i32;
    let sy = y + (mv.y >> 3) as i32;
    // Two to the left and above and three past, for the filter.
    let (w, h) = (width_block + 5, height_block + 5);
    let plane_w = stride as i32;
    let plane_h = height as i32;
    let mut patch = [0u8; 21 * 21];
    let inside = sx >= 2 && sy >= 2 && sx + width_block as i32 + 3 <= plane_w && sy + height_block as i32 + 3 <= plane_h;
    let (source, source_stride, origin): (&[u8], usize, usize) = if inside {
        (reference, stride, sy as usize * stride + sx as usize)
    } else {
        for r in 0..h {
            let ry = (sy - 2 + r as i32).clamp(0, plane_h - 1) as usize;
            for c in 0..w {
                let rx = (sx - 2 + c as i32).clamp(0, plane_w - 1) as usize;
                patch[r * 21 + c] = reference[ry * stride + rx];
            }
        }
        (&patch, 21, 2 * 21 + 2)
    };
    if mx == 0 && my == 0 {
        for r in 0..height_block {
            let from = origin + r * source_stride;
            out[out_at + r * out_stride..out_at + r * out_stride + width_block]
                .copy_from_slice(&source[from..from + width_block]);
        }
        return;
    }
    // First across, into a block two rows taller above and three below, then down.
    let mut temp = [0u8; 16 * 21];
    let f = &filters[mx];
    for r in 0..h {
        let from = origin - 2 * source_stride + r * source_stride;
        let row = &mut temp[r * 16..r * 16 + width_block];
        if mx == 0 {
            row.copy_from_slice(&source[from..from + width_block]);
        } else {
            for (c, value) in row.iter_mut().enumerate() {
                let s = |offset: usize| source[from + c + offset - 2] as i32;
                let sum = s(0) * f[0] as i32
                    + s(1) * f[1] as i32
                    + s(2) * f[2] as i32
                    + s(3) * f[3] as i32
                    + s(4) * f[4] as i32
                    + s(5) * f[5] as i32;
                *value = clamp255((sum + 64) >> 7);
            }
        }
    }
    let f = &filters[my];
    for r in 0..height_block {
        let row = &mut out[out_at + r * out_stride..out_at + r * out_stride + width_block];
        for (c, value) in row.iter_mut().enumerate() {
            if my == 0 {
                *value = temp[(r + 2) * 16 + c];
            } else {
                let s = |offset: usize| temp[(r + offset) * 16 + c] as i32;
                let sum = s(0) * f[0] as i32
                    + s(1) * f[1] as i32
                    + s(2) * f[2] as i32
                    + s(3) * f[3] as i32
                    + s(4) * f[4] as i32
                    + s(5) * f[5] as i32;
                *value = clamp255((sum + 64) >> 7);
            }
        }
    }
}

/// A chroma vector, from four luma ones added up.
fn chroma_split(sum_x: i32, sum_y: i32, full_pixel: bool) -> Mv {
    let round = |value: i32| (if value < 0 { value - 4 } else { value + 4 }) / 8;
    let (mut x, mut y) = (round(sum_x), round(sum_y));
    if full_pixel {
        x &= !7;
        y &= !7;
    }
    Mv { y: y as i16, x: x as i16 }
}

/// Predicts and reconstructs an inter macroblock.
#[allow(clippy::too_many_arguments)]
pub(super) fn inter(
    frame: &mut Frame,
    reference: &Reference<'_>,
    row: usize,
    col: usize,
    split: bool,
    mv: Mv,
    mvs: &[Mv; 16],
    coeffs: &[i16; 25 * 16],
) {
    let stride = reference.cols * 16;
    let height = reference.rows * 16;
    let (x, y) = ((col * 16) as i32, (row * 16) as i32);
    let at = row * 16 * stride + col * 16;
    let filters = reference.filters;
    if split {
        for b in 0..16 {
            let (bx, by) = ((b & 3) * 4, (b >> 2) * 4);
            predict_block(
                &mut frame.y,
                at + by * stride + bx,
                stride,
                &reference.frame.y,
                stride,
                height,
                x + bx as i32,
                y + by as i32,
                4,
                4,
                mvs[b],
                filters,
            );
        }
    } else {
        predict_block(&mut frame.y, at, stride, &reference.frame.y, stride, height, x, y, 16, 16, mv, filters);
    }
    for b in 0..16 {
        let block = at + (b >> 2) * 4 * stride + (b & 3) * 4;
        add_residual(&mut frame.y, block, stride, &coeffs[b * 16..b * 16 + 16]);
    }

    let stride = reference.cols * 8;
    let height = reference.rows * 8;
    let (x, y) = ((col * 8) as i32, (row * 8) as i32);
    let at = row * 8 * stride + col * 8;
    if split {
        for b in 0..4 {
            let (bx, by) = (b & 1, b >> 1);
            let mut sum_x = 0;
            let mut sum_y = 0;
            for i in [0, 1, 4, 5] {
                let luma = mvs[by * 8 + bx * 2 + i];
                sum_x += luma.x as i32;
                sum_y += luma.y as i32;
            }
            let chroma = chroma_split(sum_x, sum_y, reference.full_pixel);
            let (ox, oy) = (bx * 4, by * 4);
            for (out, from) in [
                (&mut frame.u, &reference.frame.u),
                (&mut frame.v, &reference.frame.v),
            ] {
                predict_block(
                    out,
                    at + oy * stride + ox,
                    stride,
                    from,
                    stride,
                    height,
                    x + ox as i32,
                    y + oy as i32,
                    4,
                    4,
                    chroma,
                    filters,
                );
            }
        }
    } else {
        let half = |value: i16| {
            let value = value as i32;
            (value + if value < 0 { -1 } else { 1 }) / 2
        };
        let (mut cx, mut cy) = (half(mv.x), half(mv.y));
        if reference.full_pixel {
            cx &= !7;
            cy &= !7;
        }
        let chroma = Mv { y: cy as i16, x: cx as i16 };
        predict_block(&mut frame.u, at, stride, &reference.frame.u, stride, height, x, y, 8, 8, chroma, filters);
        predict_block(&mut frame.v, at, stride, &reference.frame.v, stride, height, x, y, 8, 8, chroma, filters);
    }
    for b in 0..4 {
        let block = at + (b >> 1) * 4 * stride + (b & 1) * 4;
        add_residual(&mut frame.u, block, stride, &coeffs[(16 + b) * 16..(17 + b) * 16]);
        add_residual(&mut frame.v, block, stride, &coeffs[(20 + b) * 16..(21 + b) * 16]);
    }
}
