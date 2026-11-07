//! Motion vector prediction (spec 8.4.1), direct prediction (8.4.1.2) and the inter
//! prediction of a macroblock (8.4.2).

use crate::error::DecodeError;

use super::dpb::{BOTTOM, FRAME};
use super::mb::*;
use super::mc::{self, View};
use super::pixel::Pixel;

/// How a co-located motion vector's vertical scale relates to this macroblock's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scale {
    OneToOne,
    FrmToFld,
    FldToFrm,
}

struct Col {
    mv: [i16; 2],
    ref_idx: i8,
    key: i32,
    scale: Scale,
}

#[inline(always)]
fn median(a: i16, b: i16, c: i16) -> i16 {
    a.max(b).min(a.min(b).max(c))
}

#[inline(always)]
fn poc_index(parity: u8) -> usize {
    (parity == BOTTOM) as usize
}

impl<P: Pixel> Sd<'_, P> {
    /// Motion at a block of macroblock `mb`, in this macroblock's frame or field terms.
    #[inline]
    fn motion_at(&self, list: usize, mb: usize, bx: usize, by: usize) -> (i8, [i16; 2]) {
        let m = &self.motion[mb];
        let mut r = m.ref_idx[list][(by / 2) * 2 + bx / 2];
        let mut mv = m.mv[list][by * 4 + bx];
        if r < 0 {
            return (-1, [0, 0]);
        }
        if self.mbaff && mb != self.idx {
            let other = self.mbs[mb].field;
            if self.field && !other {
                mv[1] /= 2;
                r *= 2;
            } else if !self.field && other {
                mv[1] = mv[1].wrapping_mul(2);
                r >>= 1;
            }
        }
        (r, mv)
    }

    /// The neighbour's reference index and vector, or `None` if it's not available.
    /// x and y are in blocks relative to this macroblock; `y` of -1 is above, and D
    /// is asked for by the sample row in `left_row`.
    #[inline]
    fn nb_left(&self, list: usize, y_sample: i32) -> Option<(i8, [i16; 2])> {
        let (mb, row) = self.left(y_sample, 16)?;
        Some(self.motion_at(list, mb, 3, row as usize / 4))
    }

    fn neighbours(
        &self,
        list: usize,
        bx: usize,
        by: usize,
        bw: usize,
    ) -> (
        Option<(i8, [i16; 2])>,
        Option<(i8, [i16; 2])>,
        Option<(i8, [i16; 2])>,
    ) {
        let idx = self.idx;
        let a = if bx > 0 {
            Some(self.motion_at(list, idx, bx - 1, by))
        } else {
            self.nb_left(list, by as i32 * 4)
        };
        let b = if by > 0 {
            Some(self.motion_at(list, idx, bx, by - 1))
        } else {
            self.nb_b.map(|m| self.motion_at(list, m, bx, 3))
        };
        // C, or D where C isn't there.
        let xc = bx + bw;
        let c = if by == 0 {
            if xc < 4 {
                self.nb_b.map(|m| self.motion_at(list, m, xc, 3))
            } else {
                self.nb_c.map(|m| self.motion_at(list, m, 0, 3))
            }
        } else if xc < 4
            && (BLK_ORDER[(by - 1) * 4 + xc] as usize) < (BLK_ORDER[by * 4 + bx] as usize)
        {
            Some(self.motion_at(list, idx, xc, by - 1))
        } else {
            None
        };
        let c = c.or_else(|| {
            if bx > 0 && by > 0 {
                Some(self.motion_at(list, idx, bx - 1, by - 1))
            } else if by > 0 {
                self.nb_left(list, by as i32 * 4 - 1)
            } else if bx > 0 {
                self.nb_b.map(|m| self.motion_at(list, m, bx - 1, 3))
            } else {
                self.nb_d
                    .map(|m| self.motion_at(list, m, 3, self.d_row as usize / 4))
            }
        });
        (a, b, c)
    }

    /// 8.4.1.3. `shape` is 1 and 2 for the parts of 16x8, 3 and 4 for those of 8x16,
    /// otherwise 0.
    pub fn predict_mv(
        &self,
        list: usize,
        bx: usize,
        by: usize,
        bw: usize,
        r: i8,
        shape: u8,
    ) -> [i16; 2] {
        let (a, b, c) = self.neighbours(list, bx, by, bw);
        let (ra, ma) = a.unwrap_or((-1, [0, 0]));
        let (rb, mb) = b.unwrap_or((-1, [0, 0]));
        let (rc, mc) = c.unwrap_or((-1, [0, 0]));
        match shape {
            1 if rb == r => return mb,
            2 if ra == r => return ma,
            3 if ra == r => return ma,
            4 if rc == r => return mc,
            _ => {}
        }
        if b.is_none() && c.is_none() && a.is_some() {
            return ma;
        }
        match (ra == r, rb == r, rc == r) {
            (true, false, false) => ma,
            (false, true, false) => mb,
            (false, false, true) => mc,
            _ => [median(ma[0], mb[0], mc[0]), median(ma[1], mb[1], mc[1])],
        }
    }

    /// P_Skip's motion (8.4.1.1).
    pub fn pred_p_skip(&mut self) -> Result<(), DecodeError> {
        let idx = self.idx;
        let a = self.nb_left(0, 0);
        let b = self.nb_b.map(|m| self.motion_at(0, m, 0, 3));
        let zero = match (a, b) {
            (Some((ra, ma)), Some((rb, mb))) => (ra == 0 && ma == [0, 0]) || (rb == 0 && mb == [0, 0]),
            _ => true,
        };
        let mv = if zero {
            [0, 0]
        } else {
            self.predict_mv(0, 0, 0, 4, 0, 0)
        };
        self.motion[idx].ref_idx[0] = [0; 4];
        self.motion[idx].mv[0] = [mv; 16];
        Ok(())
    }

    /// Fills in ref keys once a macroblock's motion is known.
    pub fn finish_motion(&mut self) {
        let idx = self.idx;
        let field = self.field;
        self.motion[idx].field = field;
        for list in 0..2 {
            for b8 in 0..4 {
                let r = self.motion[idx].ref_idx[list][b8];
                let key = if r < 0 { -1 } else { self.ref_key(list, r) };
                self.motion[idx].ref_key[list][b8] = key;
            }
        }
    }

    fn ref_key(&self, list: usize, r: i8) -> i32 {
        let (entry, parity) = self.ref_parity(list, r);
        match self.lists[list].get(entry) {
            Some(e) => e.uid * 4 + parity as i32,
            None => -1,
        }
    }

    /// The list entry a reference index means here, and which of its fields, if any.
    #[inline]
    fn ref_parity(&self, list: usize, r: i8) -> (usize, u8) {
        if self.mbaff && self.field {
            let parity = if r & 1 == 0 {
                self.parity
            } else {
                FRAME ^ self.parity
            };
            (r as usize >> 1, parity)
        } else {
            let entry = r as usize;
            let parity = self.lists[list].get(entry).map_or(FRAME, |e| e.parity);
            (entry, parity)
        }
    }

    /// POC of what a reference index means.
    fn ref_poc(&self, list: usize, r: i8) -> i32 {
        let (entry, parity) = self.ref_parity(list, r);
        match self.lists[list].get(entry) {
            Some(e) => match parity {
                FRAME => e.own_poc,
                p => e.poc[poc_index(p)],
            },
            None => 0,
        }
    }

    fn ref_long(&self, list: usize, r: i8) -> bool {
        let (entry, _) = self.ref_parity(list, r);
        self.lists[list].get(entry).is_some_and(|e| e.long)
    }

    /// POC of this macroblock's picture or field.
    fn mb_poc(&self) -> i32 {
        match self.parity {
            FRAME => self.cur_poc[0].min(self.cur_poc[1]),
            p => self.cur_poc[poc_index(p)],
        }
    }

    /// 8.4.1.2.1: the co-located block for sub-block `sub` of 8x8 block `b8`.
    fn colocated(&self, b8: usize, sub: usize) -> Col {
        let w = self.w;
        let entry = &self.lists[1][0];
        let motion: &[super::dpb::MbMotion] = match &entry.data {
            Some(data) => &data.motion,
            None => self.motion,
        };
        let inference = self.sps.direct_8x8_inference_flag;
        let (x4, y4) = if inference {
            ((b8 & 1) * 3, (b8 >> 1) * 3)
        } else {
            ((b8 & 1) * 2 + (sub & 1), (b8 >> 1) * 2 + (sub >> 1))
        };
        let y_col = y4 * 4;
        let x = self.mbx;
        let (mb, y_m, scale);
        if self.structure != FRAME {
            // This is a field. Rows of field macroblocks and the frame's pairs line up.
            let row = self.mby;
            let pair_top = x + row * 2 * w;
            let col_bottom = (entry.parity == BOTTOM) as usize;
            if entry.coded_fields {
                mb = pair_top + col_bottom * w;
                y_m = y_col;
                scale = Scale::OneToOne;
            } else if entry.mbaff && motion[pair_top].field {
                mb = pair_top + (self.structure == BOTTOM) as usize * w;
                y_m = y_col;
                scale = Scale::OneToOne;
            } else {
                mb = pair_top + (y_col / 8) * w;
                y_m = (2 * y_col) % 16;
                scale = Scale::FrmToFld;
            }
        } else if !self.mbaff {
            if entry.coded_fields {
                let row = self.mby;
                let par = self.closer_field(entry) as usize;
                mb = x + (row / 2 * 2 + par) * w;
                y_m = 8 * (row % 2) + 4 * (y_col / 8);
                scale = Scale::FldToFrm;
            } else {
                mb = self.idx;
                y_m = y_col;
                scale = Scale::OneToOne;
            }
        } else {
            let pair_top = x + (self.addr / 2 / w) * 2 * w;
            if entry.coded_fields {
                if self.field {
                    mb = pair_top + (self.lower as usize) * w;
                    y_m = y_col;
                    scale = Scale::OneToOne;
                } else {
                    let par = self.closer_field(entry) as usize;
                    mb = pair_top + par * w;
                    y_m = 8 * (self.addr & 1) + 4 * (y_col / 8);
                    scale = Scale::FldToFrm;
                }
            } else {
                let col_field = motion[pair_top].field;
                if !self.field {
                    if !col_field {
                        mb = self.idx;
                        y_m = y_col;
                        scale = Scale::OneToOne;
                    } else {
                        let par = self.closer_field(entry) as usize;
                        mb = pair_top + par * w;
                        y_m = 8 * (self.addr & 1) + 4 * (y_col / 8);
                        scale = Scale::FldToFrm;
                    }
                } else if col_field {
                    mb = self.idx;
                    y_m = y_col;
                    scale = Scale::OneToOne;
                } else {
                    mb = pair_top + (y_col / 8) * w;
                    y_m = (2 * y_col) % 16;
                    scale = Scale::FrmToFld;
                }
            }
        }
        let m = &motion[mb];
        let blk = (y_m / 4) * 4 + x4;
        let b8c = (y_m / 8) * 2 + x4 / 2;
        if m.intra {
            return Col {
                mv: [0, 0],
                ref_idx: -1,
                key: -1,
                scale,
            };
        }
        let list = if m.ref_idx[0][b8c] >= 0 { 0 } else { 1 };
        Col {
            mv: m.mv[list][blk],
            ref_idx: m.ref_idx[list][b8c],
            key: m.ref_key[list][b8c],
            scale,
        }
    }

    /// For a frame macroblock: true if the bottom field of a reference is the closer
    /// (Table 8-6).
    fn closer_field(&self, entry: &super::dpb::RefEntry<P>) -> bool {
        let cur = self.cur_poc[0].min(self.cur_poc[1]);
        let top = (entry.poc[0] as i64 - cur as i64).abs();
        let bottom = (entry.poc[1] as i64 - cur as i64).abs();
        top >= bottom
    }

    /// Direct prediction of the 8x8 blocks in `which`.
    pub fn pred_direct(&mut self, which: u8) -> Result<(), DecodeError> {
        if self.lists[1].is_empty() || self.lists[0].is_empty() {
            return Err(DecodeError::InvalidSyntax("direct prediction without references"));
        }
        if self.h.direct_spatial_mv_pred_flag {
            self.direct_spatial(which);
        } else {
            self.direct_temporal(which);
        }
        Ok(())
    }

    fn direct_spatial(&mut self, which: u8) {
        let idx = self.idx;
        if !self.direct_ready {
            self.direct_ready = true;
            let mut refs = [-1i8; 2];
            for list in 0..2 {
                let (a, b, c) = self.neighbours(list, 0, 0, 4);
                for n in [a, b, c].into_iter().flatten() {
                    let r = n.0;
                    if r >= 0 && (refs[list] < 0 || r < refs[list]) {
                        refs[list] = r;
                    }
                }
            }
            if refs[0] < 0 && refs[1] < 0 {
                self.direct_ref = [0, 0];
                self.direct_mv = [[0, 0]; 2];
            } else {
                self.direct_ref = refs;
                for list in 0..2 {
                    self.direct_mv[list] = if refs[list] >= 0 {
                        self.predict_mv(list, 0, 0, 4, refs[list], 0)
                    } else {
                        [0, 0]
                    };
                }
            }
        }
        let refs = self.direct_ref;
        let mvs = self.direct_mv;
        let inference = self.sps.direct_8x8_inference_flag;
        let list1_long = self.ref_long(1, 0);
        for b8 in 0..4 {
            if which & (1 << b8) == 0 {
                continue;
            }
            for list in 0..2 {
                self.motion[idx].ref_idx[list][b8] = refs[list];
            }
            let (x0, y0) = ((b8 & 1) * 2, (b8 >> 1) * 2);
            for sub in 0..4 {
                let col_zero = if refs[0] == 0 || refs[1] == 0 {
                    let col = self.colocated(b8, if inference { 0 } else { sub });
                    !list1_long
                        && col.ref_idx == 0
                        && (-1..=1).contains(&col.mv[0])
                        && (-1..=1).contains(&col.mv[1])
                } else {
                    false
                };
                let (bx, by) = (x0 + (sub & 1), y0 + (sub >> 1));
                for list in 0..2 {
                    let mv = if refs[list] < 0 || (refs[list] == 0 && col_zero) {
                        [0, 0]
                    } else {
                        mvs[list]
                    };
                    self.motion[idx].mv[list][by * 4 + bx] = mv;
                }
            }
        }
    }

    fn direct_temporal(&mut self, which: u8) {
        let idx = self.idx;
        let inference = self.sps.direct_8x8_inference_flag;
        for b8 in 0..4 {
            if which & (1 << b8) == 0 {
                continue;
            }
            let (x0, y0) = ((b8 & 1) * 2, (b8 >> 1) * 2);
            for sub in 0..4 {
                let col = self.colocated(b8, if inference { 0 } else { sub });
                let ref0 = if col.ref_idx < 0 {
                    0
                } else {
                    self.map_col_to_list0(&col)
                };
                let mut mv_col = col.mv;
                match col.scale {
                    Scale::FrmToFld => mv_col[1] /= 2,
                    Scale::FldToFrm => mv_col[1] = mv_col[1].wrapping_mul(2),
                    Scale::OneToOne => {}
                }
                let poc0 = self.ref_poc(0, ref0);
                let poc1 = self.ref_poc(1, 0);
                let cur = self.mb_poc();
                let td = (poc1 - poc0).clamp(-128, 127);
                let (mv0, mv1) = if self.ref_long(0, ref0) || td == 0 {
                    (mv_col, [0, 0])
                } else {
                    let tb = (cur - poc0).clamp(-128, 127);
                    let tx = (16384 + (td / 2).abs()) / td;
                    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                    let scale = |v: i16| ((dsf * v as i32 + 128) >> 8) as i16;
                    let m0 = [scale(mv_col[0]), scale(mv_col[1])];
                    (m0, [m0[0].wrapping_sub(mv_col[0]), m0[1].wrapping_sub(mv_col[1])])
                };
                let (bx, by) = (x0 + (sub & 1), y0 + (sub >> 1));
                let m = &mut self.motion[idx];
                m.mv[0][by * 4 + bx] = mv0;
                m.mv[1][by * 4 + bx] = mv1;
                m.ref_idx[0][b8] = ref0;
                m.ref_idx[1][b8] = 0;
            }
        }
    }

    /// The list 0 index that means the picture the co-located block referred to.
    fn map_col_to_list0(&self, col: &Col) -> i8 {
        let uid = col.key >> 2;
        let col_parity = (col.key & 3) as u8;
        let list = &self.lists[0];
        if self.mbaff && self.field {
            // Indices count fields: even is this macroblock's parity.
            let k = list.iter().position(|e| e.uid == uid).unwrap_or(0) as i8;
            let odd = col.scale == Scale::OneToOne && col_parity != FRAME && col_parity != self.parity;
            return k * 2 + odd as i8;
        }
        if self.structure == FRAME {
            return list.iter().position(|e| e.uid == uid).unwrap_or(0) as i8;
        }
        let want = if col.scale == Scale::OneToOne && col_parity != FRAME {
            col_parity
        } else {
            self.structure
        };
        list.iter()
            .position(|e| e.uid == uid && e.parity == want)
            .unwrap_or(0) as i8
    }

    /// Inter prediction of the whole macroblock from its stored motion.
    pub fn inter_pred(&mut self) {
        let m = &self.motion[self.idx];
        let same = |list: usize, a: usize, b: usize| m.mv[list][a] == m.mv[list][b];
        let uniform8 = |b8: usize| -> bool {
            let at = (b8 >> 1) * 8 + (b8 & 1) * 2;
            (0..2).all(|l| same(l, at, at + 1) && same(l, at, at + 4) && same(l, at, at + 5))
        };
        let subs = [uniform8(0), uniform8(1), uniform8(2), uniform8(3)];
        let whole = subs.iter().all(|&s| s)
            && (0..2).all(|l| {
                m.ref_idx[l] == [m.ref_idx[l][0]; 4]
                    && same(l, 0, 2)
                    && same(l, 0, 8)
                    && same(l, 0, 10)
            });
        if whole {
            self.mc_block(0, 0, 4, 4);
            return;
        }
        for b8 in 0..4 {
            let (x0, y0) = ((b8 & 1) * 2, (b8 >> 1) * 2);
            if subs[b8] {
                self.mc_block(x0, y0, 2, 2);
            } else {
                for k in 0..4 {
                    self.mc_block(x0 + (k & 1), y0 + (k >> 1), 1, 1);
                }
            }
        }
    }

    /// Predicts one block, in units of 4 luma samples, and stores it in the picture.
    fn mc_block(&mut self, bx: usize, by: usize, bw: usize, bh: usize) {
        let idx = self.idx;
        let b8 = (by / 2) * 2 + bx / 2;
        let blk = by * 4 + bx;
        let m = &self.motion[idx];
        let refs = [m.ref_idx[0][b8], m.ref_idx[1][b8]];
        let mvs = [m.mv[0][blk], m.mv[1][blk]];
        let w = self.w;
        let frame_h = self.frame_h as i32;
        let chroma = self.cfi != 0;
        let (lw, lh) = (bw * 4, bh * 4);
        let (cw, ch) = (bw * 2, if self.mch == 16 { bh * 4 } else { bh * 2 });
        let px = (self.mbx * 16 + bx * 4) as i32;
        let py = (self.mby * 16 + by * 4) as i32;
        let cx = (self.mbx * 8 + bx * 2) as i32;
        let cy = (self.mby * self.mch + by * ch / bh.max(1)) as i32;
        let Some(preds) = self.preds.take() else {
            return;
        };
        let mut used = [false; 2];
        for list in 0..2 {
            let r = refs[list];
            if r < 0 {
                continue;
            }
            let (entry, parity) = self.ref_parity(list, r);
            let Some(e) = self.lists[list].get(entry) else {
                continue;
            };
            used[list] = true;
            let (data0, data1, data2): (&[P], &[P], &[P]) = match &e.data {
                Some(d) => (&d.planes[0], &d.planes[1], &d.planes[2]),
                None => (&self.planes[0], &self.planes[1], &self.planes[2]),
            };
            let is_field = parity != FRAME;
            let mul = if is_field { 2 } else { 1 };
            let bottom = parity == BOTTOM;
            let lv = View {
                data: data0,
                base: if bottom { w * 16 } else { 0 },
                stride: w * 16 * mul,
                width: w as i32 * 16,
                height: frame_h * 16 / mul as i32,
            };
            let mv = mvs[list];
            let (mx, my) = (mv[0] as i32, mv[1] as i32);
            let [out0, out1, out2] = &mut preds[list];
            mc::luma(
                &lv,
                px + (mx >> 2),
                py + (my >> 2),
                mx & 3,
                my & 3,
                lw,
                lh,
                out0,
                self.max,
            );
            if chroma {
                let plane_h = frame_h * self.mch as i32 / mul as i32;
                let view = |data| View {
                    data,
                    base: if bottom { w * 8 } else { 0 },
                    stride: w * 8 * mul,
                    width: w as i32 * 8,
                    height: plane_h,
                };
                let (yi, fy) = if self.mch == 16 {
                    (cy + (my >> 2), (my & 3) << 1)
                } else {
                    // Fields of opposite parity sit a quarter of a chroma sample apart
                    // (Table 8-9).
                    let mut mcy = my;
                    if is_field && parity != self.parity {
                        mcy += if self.parity == BOTTOM { 2 } else { -2 };
                    }
                    (cy + (mcy >> 3), mcy & 7)
                };
                mc::chroma(&view(data1), cx + (mx >> 3), yi, mx & 7, fy, cw, ch, out1);
                mc::chroma(&view(data2), cx + (mx >> 3), yi, mx & 7, fy, cw, ch, out2);
            }
        }
        // Weighting.
        let [p0, p1] = &mut *preds;
        let explicit = if self.is_b {
            self.pps.weighted_bipred_idc == 1
        } else {
            self.pps.weighted_pred_flag
        };
        let implicit = self.is_b && self.pps.weighted_bipred_idc == 2;
        let planes = if chroma { 3 } else { 1 };
        let wp_shift = self.depth as i32 - 8;
        let wr = |_: usize, r: i8| -> usize {
            if self.mbaff && self.field {
                r as usize >> 1
            } else {
                r as usize
            }
        };
        for plane in 0..planes {
            let (w_, h_) = if plane == 0 { (lw, lh) } else { (cw, ch) };
            if used[0] && used[1] {
                if explicit {
                    if let Some(t) = &self.h.weight_table {
                        let (a, b) = (
                            t.l0.get(wr(0, refs[0])),
                            t.l1.get(wr(1, refs[1])),
                        );
                        if let (Some(a), Some(b)) = (a, b) {
                            let (log, w0, w1, o0, o1) = if plane == 0 {
                                (
                                    t.luma_log2_weight_denom,
                                    a.luma_weight,
                                    b.luma_weight,
                                    a.luma_offset,
                                    b.luma_offset,
                                )
                            } else {
                                (
                                    t.chroma_log2_weight_denom,
                                    a.chroma_weight[plane - 1],
                                    b.chroma_weight[plane - 1],
                                    a.chroma_offset[plane - 1],
                                    b.chroma_offset[plane - 1],
                                )
                            };
                            let offset = ((o0 << wp_shift) + (o1 << wp_shift) + 1) >> 1;
                            mc::weight_bi(&mut p0[plane], &p1[plane], w_, h_, log, w0, w1, offset, self.max);
                            continue;
                        }
                    }
                    mc::average(&mut p0[plane], &p1[plane], w_, h_);
                } else if implicit {
                    let (w0, w1) = self.implicit_weights(refs);
                    if w0 == 32 && w1 == 32 {
                        mc::average(&mut p0[plane], &p1[plane], w_, h_);
                    } else {
                        mc::weight_bi(&mut p0[plane], &p1[plane], w_, h_, 5, w0, w1, 0, self.max);
                    }
                } else {
                    mc::average(&mut p0[plane], &p1[plane], w_, h_);
                }
            } else if explicit {
                let list = if used[0] { 0 } else { 1 };
                if let Some(t) = &self.h.weight_table {
                    let weights = if list == 0 { &t.l0 } else { &t.l1 };
                    if let Some(a) = weights.get(wr(list, refs[list])) {
                        let (log, weight, offset) = if plane == 0 {
                            (t.luma_log2_weight_denom, a.luma_weight, a.luma_offset)
                        } else {
                            (
                                t.chroma_log2_weight_denom,
                                a.chroma_weight[plane - 1],
                                a.chroma_offset[plane - 1],
                            )
                        };
                        let target = if list == 0 { &mut p0[plane] } else { &mut p1[plane] };
                        if weight != (1 << log) || offset != 0 {
                            mc::weight_uni(target, w_, h_, log, weight, offset << wp_shift, self.max);
                        }
                    }
                }
            }
        }
        // Store.
        let from = if used[0] { &*p0 } else { &*p1 };
        let at = self.ly + by * 4 * self.ls + bx * 4;
        for y in 0..lh {
            let row = at + y * self.ls;
            self.planes[0][row..row + lw].copy_from_slice(&from[0][y * 16..y * 16 + lw]);
        }
        if chroma {
            let cy_mb = by * ch / bh.max(1);
            let at = self.lc + cy_mb * self.cs + bx * 2;
            for plane in 1..3 {
                for y in 0..ch {
                    let row = at + y * self.cs;
                    self.planes[plane][row..row + cw]
                        .copy_from_slice(&from[plane][y * 16..y * 16 + cw]);
                }
            }
        }
        self.preds = Some(preds);
    }

    /// 8.4.2.3.1: implicit bi-predictive weights.
    fn implicit_weights(&self, refs: [i8; 2]) -> (i32, i32) {
        if self.ref_long(0, refs[0]) || self.ref_long(1, refs[1]) {
            return (32, 32);
        }
        let poc0 = self.ref_poc(0, refs[0]);
        let poc1 = self.ref_poc(1, refs[1]);
        let td = (poc1 - poc0).clamp(-128, 127);
        if td == 0 {
            return (32, 32);
        }
        let tb = (self.mb_poc() - poc0).clamp(-128, 127);
        let tx = (16384 + (td / 2).abs()) / td;
        let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023) >> 2;
        if !(-64..=128).contains(&dsf) {
            (32, 32)
        } else {
            (64 - dsf, dsf)
        }
    }
}

