//! The deblocking filter (spec 8.7) over a finished picture.

use crate::deblock::{ALPHA_TABLE, BETA_TABLE, TC0_TABLE};

use super::dpb::{BOTTOM, FRAME, MbMotion};
use super::mb::*;
use super::pixel::Pixel;

/// What the filter works from.
pub(crate) struct Deblock<'a, P> {
    pub planes: &'a mut [alloc::vec::Vec<P>; 3],
    pub motion: &'a [MbMotion],
    pub mbs: &'a [MbInfo],
    pub w: usize,
    pub frame_h: usize,
    pub structure: u8,
    pub mbaff: bool,
    pub mch: usize,
    pub depth: u32,
}

/// One filter strength setting for an edge segment.
#[derive(Clone, Copy)]
struct Thresholds {
    alpha: i32,
    beta: i32,
    index_a: usize,
}

#[inline(always)]
fn thresholds(qp_avg: i32, offset_a: i32, offset_b: i32, depth: u32) -> Thresholds {
    let index_a = (qp_avg + offset_a).clamp(0, 51) as usize;
    let index_b = (qp_avg + offset_b).clamp(0, 51) as usize;
    Thresholds {
        alpha: ALPHA_TABLE[index_a] << (depth - 8),
        beta: BETA_TABLE[index_b] << (depth - 8),
        index_a,
    }
}

/// Filters one line of samples across an edge: `q` is q0's offset and `step` the
/// distance between samples across the edge.
#[inline(always)]
fn filter_line<P: Pixel>(d: &mut [P], q: usize, step: usize, bs: u8, t: Thresholds, luma: bool, depth: u32) {
    let (p0, p1) = (d[q - step].get(), d[q - 2 * step].get());
    let (q0, q1) = (d[q].get(), d[q + step].get());
    if (p0 - q0).abs() >= t.alpha || (p1 - p0).abs() >= t.beta || (q1 - q0).abs() >= t.beta {
        return;
    }
    let max = (1 << depth) - 1;
    if bs < 4 {
        let tc0 = TC0_TABLE[t.index_a][bs as usize - 1] << (depth - 8);
        if luma {
            let (p2, q2) = (d[q - 3 * step].get(), d[q + 2 * step].get());
            let (ap, aq) = ((p2 - p0).abs(), (q2 - q0).abs());
            let tc = tc0 + (ap < t.beta) as i32 + (aq < t.beta) as i32;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d[q - step] = P::new((p0 + delta).clamp(0, max));
            d[q] = P::new((q0 - delta).clamp(0, max));
            if ap < t.beta {
                let v = p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0);
                d[q - 2 * step] = P::new(v);
            }
            if aq < t.beta {
                let v = q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0);
                d[q + step] = P::new(v);
            }
        } else {
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d[q - step] = P::new((p0 + delta).clamp(0, max));
            d[q] = P::new((q0 - delta).clamp(0, max));
        }
    } else if luma {
        let (p2, q2) = (d[q - 3 * step].get(), d[q + 2 * step].get());
        let small = (p0 - q0).abs() < (t.alpha >> 2) + 2;
        if small && (p2 - p0).abs() < t.beta {
            let p3 = d[q - 4 * step].get();
            d[q - step] = P::new((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
            d[q - 2 * step] = P::new((p2 + p1 + p0 + q0 + 2) >> 2);
            d[q - 3 * step] = P::new((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3);
        } else {
            d[q - step] = P::new((2 * p1 + p0 + q1 + 2) >> 2);
        }
        if small && (q2 - q0).abs() < t.beta {
            let q3 = d[q + 3 * step].get();
            d[q] = P::new((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3);
            d[q + step] = P::new((p0 + q0 + q1 + q2 + 2) >> 2);
            d[q + 2 * step] = P::new((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3);
        } else {
            d[q] = P::new((2 * q1 + q0 + p1 + 2) >> 2);
        }
    } else {
        d[q - step] = P::new((2 * p1 + p0 + q1 + 2) >> 2);
        d[q] = P::new((2 * q1 + q0 + p1 + 2) >> 2);
    }
}

impl<P: Pixel> Deblock<'_, P> {
    /// Runs the filter over the picture.
    pub fn run(&mut self) {
        let w = self.w;
        if self.mbaff {
            for row in 0..self.frame_h / 2 {
                for x in 0..w {
                    let top = x + row * 2 * w;
                    self.filter_mb_mbaff(top, x, row, false);
                    self.filter_mb_mbaff(top + w, x, row, true);
                }
            }
        } else if self.structure == FRAME {
            for row in 0..self.frame_h {
                for x in 0..w {
                    self.filter_mb(x + row * w, x, row, w, row * 16 * w * 16 + x * 16, w * 16, row * self.mch * w * 8 + x * 8, w * 8);
                }
            }
        } else {
            let odd = (self.structure == BOTTOM) as usize;
            for row in 0..self.frame_h / 2 {
                for x in 0..w {
                    self.filter_mb(
                        x + (row * 2 + odd) * w,
                        x,
                        row,
                        2 * w,
                        (row * 32 + odd) * w * 16 + x * 16,
                        w * 32,
                        (row * 2 * self.mch + odd) * w * 8 + x * 8,
                        w * 16,
                    );
                }
            }
        }
    }

    /// bS between two blocks of macroblocks not in mixed coding, for an edge that
    /// isn't intra or has no coefficients: the motion test.
    #[inline]
    fn motion_differs(&self, mp: usize, bp: usize, mq: usize, bq: usize, limit: i32) -> bool {
        let (p, q) = (&self.motion[mp], &self.motion[mq]);
        let b8 = |b: usize| (b >> 3) * 2 + ((b & 3) >> 1);
        let (bp8, bq8) = (b8(bp), b8(bq));
        let (kp0, kp1) = (p.ref_key[0][bp8], p.ref_key[1][bp8]);
        let (kq0, kq1) = (q.ref_key[0][bq8], q.ref_key[1][bq8]);
        if !((kp0 == kq0 && kp1 == kq1) || (kp0 == kq1 && kp1 == kq0)) {
            return true;
        }
        let far = |a: [i16; 2], b: [i16; 2]| {
            (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= limit
        };
        let (p0, p1) = (p.mv[0][bp], p.mv[1][bp]);
        let (q0, q1) = (q.mv[0][bq], q.mv[1][bq]);
        if kp0 != kp1 {
            if kp0 == kq0 {
                far(p0, q0) || far(p1, q1)
            } else {
                far(p0, q1) || far(p1, q0)
            }
        } else {
            (far(p0, q0) || far(p1, q1)) && (far(p0, q1) || far(p1, q0))
        }
    }

    /// bS of a segment between blocks at raster positions, with no mixed coding.
    #[inline]
    fn strength(&self, mp: usize, bp: usize, mq: usize, bq: usize, mb_edge: bool, vertical: bool, field: bool) -> u8 {
        let (np, nq) = (&self.mbs[mp], &self.mbs[mq]);
        if np.intra() || nq.intra() {
            return if mb_edge && (vertical || !field) { 4 } else { 3 };
        }
        if np.mask & (1 << bp) != 0 || nq.mask & (1 << bq) != 0 {
            return 2;
        }
        let limit = if field { 2 } else { 4 };
        self.motion_differs(mp, bp, mq, bq, limit) as u8
    }

    /// Filters a vertical edge's segments, given each one's bS. `lq` and `cq` are the
    /// offsets of the edge's first q0 sample in luma and chroma.
    #[allow(clippy::too_many_arguments)]
    fn vertical(&mut self, mp: usize, mq: usize, bs: [u8; 4], lq: usize, ls: usize, cq: usize, cs: usize, luma: bool, chroma: bool) {
        if bs == [0; 4] {
            return;
        }
        let (n, q) = (self.mbs[mp], self.mbs[mq]);
        let (oa, ob) = (q.alpha as i32 * 2, q.beta as i32 * 2);
        let depth = self.depth;
        if luma {
            let t = thresholds((n.qp as i32 + q.qp as i32 + 1) >> 1, oa, ob, depth);
            let plane = &mut self.planes[0];
            for (r, &b) in bs.iter().enumerate() {
                if b != 0 {
                    for y in 0..4 {
                        filter_line(plane, lq + (r * 4 + y) * ls, 1, b, t, true, depth);
                    }
                }
            }
        }
        if chroma {
            let per = self.mch / 4;
            for c in 0..2 {
                let t = thresholds((n.qpc[c] as i32 + q.qpc[c] as i32 + 1) >> 1, oa, ob, depth);
                let plane = &mut self.planes[c + 1];
                for (r, &b) in bs.iter().enumerate() {
                    if b != 0 {
                        for y in 0..per {
                            filter_line(plane, cq + (r * per + y) * cs, 1, b, t, false, depth);
                        }
                    }
                }
            }
        }
    }

    /// Filters a horizontal edge's segments; `ls` and `cs` also go across the edge.
    #[allow(clippy::too_many_arguments)]
    fn horizontal(&mut self, mp: usize, mq: usize, bs: [u8; 4], lq: usize, ls: usize, cq: usize, cs: usize, luma: bool, chroma: bool) {
        if bs == [0; 4] {
            return;
        }
        let (n, q) = (self.mbs[mp], self.mbs[mq]);
        let (oa, ob) = (q.alpha as i32 * 2, q.beta as i32 * 2);
        let depth = self.depth;
        if luma {
            let t = thresholds((n.qp as i32 + q.qp as i32 + 1) >> 1, oa, ob, depth);
            let plane = &mut self.planes[0];
            for (c, &b) in bs.iter().enumerate() {
                if b != 0 {
                    for k in 0..4 {
                        filter_line(plane, lq + c * 4 + k, ls, b, t, true, depth);
                    }
                }
            }
        }
        if chroma {
            for pl in 0..2 {
                let t = thresholds((n.qpc[pl] as i32 + q.qpc[pl] as i32 + 1) >> 1, oa, ob, depth);
                let plane = &mut self.planes[pl + 1];
                for (c, &b) in bs.iter().enumerate() {
                    if b != 0 {
                        for k in 0..2 {
                            filter_line(plane, cq + c * 2 + k, cs, b, t, false, depth);
                        }
                    }
                }
            }
        }
    }

    /// The vertical edges inside a macroblock.
    fn inner_vertical(&mut self, mq: usize, ly: usize, ls: usize, lc: usize, cs: usize, field: bool) {
        let t8 = self.mbs[mq].t8;
        let has_chroma = self.mch != 0;
        for e in 1..4 {
            let luma = e % 2 == 0 || !t8;
            let chroma = has_chroma && e % 2 == 0;
            if !luma && !chroma {
                continue;
            }
            let mut bs = [0u8; 4];
            for (r, b) in bs.iter_mut().enumerate() {
                let bq = r * 4 + e;
                *b = self.strength(mq, bq - 1, mq, bq, false, true, field);
            }
            self.vertical(mq, mq, bs, ly + e * 4, ls, lc + e * 2, cs, luma, chroma);
        }
    }

    /// The horizontal edges inside a macroblock.
    fn inner_horizontal(&mut self, mq: usize, ly: usize, ls: usize, lc: usize, cs: usize, field: bool) {
        let t8 = self.mbs[mq].t8;
        let mch = self.mch;
        for e in 1..4 {
            let luma = e % 2 == 0 || !t8;
            let chroma = match mch {
                8 => e % 2 == 0,
                16 => true,
                _ => false,
            };
            if !luma && !chroma {
                continue;
            }
            let mut bs = [0u8; 4];
            for (c, b) in bs.iter_mut().enumerate() {
                let bq = e * 4 + c;
                *b = self.strength(mq, bq - 4, mq, bq, false, false, field);
            }
            self.horizontal(mq, mq, bs, ly + e * 4 * ls, ls, lc + (e * mch / 4) * cs, cs, luma, chroma);
        }
    }

    /// Filters one macroblock of a frame or field picture.
    #[allow(clippy::too_many_arguments)]
    fn filter_mb(&mut self, mq: usize, x: usize, row: usize, up_step: usize, ly: usize, ls: usize, lc: usize, cs: usize) {
        let q = self.mbs[mq];
        if q.slice == 0 || q.filter_idc == 1 {
            return;
        }
        let field = self.structure != FRAME;
        let has_chroma = self.mch != 0;
        if let Some(mp) = (x > 0).then(|| mq - 1).filter(|&m| self.edge_ok(&q, m)) {
            let mut bs = [0u8; 4];
            for (r, b) in bs.iter_mut().enumerate() {
                *b = self.strength(mp, r * 4 + 3, mq, r * 4, true, true, field);
            }
            self.vertical(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
        }
        self.inner_vertical(mq, ly, ls, lc, cs, field);
        if let Some(mp) = (row > 0).then(|| mq - up_step).filter(|&m| self.edge_ok(&q, m)) {
            let mut bs = [0u8; 4];
            for (c, b) in bs.iter_mut().enumerate() {
                *b = self.strength(mp, 12 + c, mq, c, true, false, field);
            }
            self.horizontal(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
        }
        self.inner_horizontal(mq, ly, ls, lc, cs, field);
    }

    /// Whether an edge with macroblock `m` is filtered, by slice rules.
    #[inline(always)]
    fn edge_ok(&self, q: &MbInfo, m: usize) -> bool {
        let p = &self.mbs[m];
        p.slice != 0 && (q.filter_idc != 2 || p.slice == q.slice)
    }

    /// bS on an MBAFF edge between a frame and a field macroblock.
    #[inline]
    fn strength_mixed(&self, mp: usize, bp: usize, mq: usize, bq: usize, vertical: bool) -> u8 {
        let (np, nq) = (&self.mbs[mp], &self.mbs[mq]);
        if np.intra() || nq.intra() {
            return if vertical { 4 } else { 3 };
        }
        if np.mask & (1 << bp) != 0 || nq.mask & (1 << bq) != 0 {
            return 2;
        }
        1
    }

    /// The left neighbour of row `y` of a macroblock with `max_h` rows, when the left pair
    /// is coded differently (Table 6-4): its macroblock, and row within it.
    #[inline]
    fn left_mixed(&self, top_a: usize, field: bool, lower: bool, y: usize, max_h: usize) -> (usize, usize) {
        let w = self.w;
        if !field {
            // This is a frame macroblock beside a field pair.
            let row = if lower { y + max_h } else { y };
            (if y & 1 == 0 { top_a } else { top_a + w }, row >> 1)
        } else {
            let odd = lower as usize;
            if y < max_h / 2 {
                (top_a, y * 2 + odd)
            } else {
                (top_a + w, y * 2 + odd - max_h)
            }
        }
    }

    /// Filters one macroblock of an MBAFF frame.
    fn filter_mb_mbaff(&mut self, mq: usize, x: usize, row: usize, lower: bool) {
        let q = self.mbs[mq];
        if q.slice == 0 || q.filter_idc == 1 {
            return;
        }
        let w = self.w;
        let field = q.field;
        let (s, c) = (w * 16, w * 8);
        let mch = self.mch;
        let has_chroma = mch != 0;
        let (ly, ls, lc, cs) = if field {
            (
                (row * 32 + lower as usize) * s + x * 16,
                2 * s,
                (row * 2 * mch + lower as usize) * c + x * 8,
                2 * c,
            )
        } else {
            (
                (row * 32 + 16 * lower as usize) * s + x * 16,
                s,
                (row * 2 * mch + mch * lower as usize) * c + x * 8,
                c,
            )
        };
        let top = x + row * 2 * w;
        let depth = self.depth;
        let (oa, ob) = (q.alpha as i32 * 2, q.beta as i32 * 2);

        // Left edge.
        if x > 0 && self.edge_ok(&q, top - 1) {
            let top_a = top - 1;
            if self.mbs[top_a].field == field {
                let mp = top_a + lower as usize * w;
                let mut bs = [0u8; 4];
                for (r, b) in bs.iter_mut().enumerate() {
                    *b = self.strength(mp, r * 4 + 3, mq, r * 4, true, true, field);
                }
                self.vertical(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
            } else {
                for y in 0..16 {
                    let (mp, r) = self.left_mixed(top_a, field, lower, y, 16);
                    let b = self.strength_mixed(mp, (r / 4) * 4 + 3, mq, (y / 4) * 4, true);
                    if b != 0 {
                        let n = self.mbs[mp];
                        let t = thresholds((n.qp as i32 + q.qp as i32 + 1) >> 1, oa, ob, depth);
                        filter_line(&mut self.planes[0], ly + y * ls, 1, b, t, true, depth);
                    }
                }
                if has_chroma {
                    let luma_per = 16 / mch;
                    for y in 0..mch {
                        // bS from the luma row in the same macroblock on the left: beside a
                        // field pair that's one in the same field.
                        let mut luma_y = y * luma_per;
                        if !field && luma_per == 2 {
                            luma_y += y & 1;
                        }
                        let (lp, lr) = self.left_mixed(top_a, field, lower, luma_y, 16);
                        let b = self.strength_mixed(lp, (lr / 4) * 4 + 3, mq, (luma_y / 4) * 4, true);
                        if b != 0 {
                            let (mp, _) = self.left_mixed(top_a, field, lower, y, mch);
                            let n = self.mbs[mp];
                            for pl in 0..2 {
                                let qp = (n.qpc[pl] as i32 + q.qpc[pl] as i32 + 1) >> 1;
                                let t = thresholds(qp, oa, ob, depth);
                                filter_line(&mut self.planes[pl + 1], lc + y * cs, 1, b, t, false, depth);
                            }
                        }
                    }
                }
            }
        }
        self.inner_vertical(mq, ly, ls, lc, cs, field);

        // Top edge.
        if !field && lower {
            let mp = mq - w;
            let mut bs = [0u8; 4];
            for (cx, b) in bs.iter_mut().enumerate() {
                *b = self.strength(mp, 12 + cx, mq, cx, true, false, false);
            }
            self.horizontal(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
        } else if row > 0 && self.edge_ok(&q, top - 2 * w) {
            let above = top - 2 * w;
            let above_field = self.mbs[above].field;
            if !field && above_field {
                // A frame macroblock under a field pair: each field apart, over double
                // strides, reaching only into this macroblock's own lines of that field.
                for parity in 0..2 {
                    let mp = above + parity * w;
                    let mut bs = [0u8; 4];
                    for (cx, b) in bs.iter_mut().enumerate() {
                        *b = self.strength_mixed(mp, 12 + cx, mq, cx, false);
                    }
                    self.horizontal(mp, mq, bs, ly + parity * s, 2 * s, lc + parity * c, 2 * c, true, has_chroma);
                }
            } else if !field {
                let mp = mq - w;
                let mut bs = [0u8; 4];
                for (cx, b) in bs.iter_mut().enumerate() {
                    *b = self.strength(mp, 12 + cx, mq, cx, true, false, false);
                }
                self.horizontal(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
            } else {
                let mp = if above_field && !lower { above } else { above + w };
                let mut bs = [0u8; 4];
                for (cx, b) in bs.iter_mut().enumerate() {
                    *b = if above_field {
                        self.strength(mp, 12 + cx, mq, cx, true, false, true)
                    } else {
                        self.strength_mixed(mp, 12 + cx, mq, cx, false)
                    };
                }
                self.horizontal(mp, mq, bs, ly, ls, lc, cs, true, has_chroma);
            }
        }
        self.inner_horizontal(mq, ly, ls, lc, cs, field);
    }
}
