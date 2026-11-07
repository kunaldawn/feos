//! Reconstruction of a macroblock: intra prediction in coding order with its residual,
//! and the residual on top of inter prediction.

use super::intra::{self, Avail};
use super::mb::*;
use super::pixel::Pixel;
use super::xform::{idct4_add, idct8_add};

impl<P: Pixel> Sd<'_, P> {
    /// Whether the samples left of rows `from..from + 4` of a `max_h`-row macroblock
    /// may be used for intra prediction.
    fn left_usable(&self, from: i32, count: i32, max_h: i32) -> bool {
        if self.nb_a.is_none() {
            return false;
        }
        if !self.pps.constrained_intra_pred_flag {
            return true;
        }
        (from..from + count).all(|y| {
            self.left(y, max_h)
                .is_some_and(|(m, _)| self.mbs[m].intra())
        })
    }

    pub fn recon_intra(&mut self) {
        let idx = self.idx;
        let kind = self.mbs[idx].kind;
        let usable = |nb: Option<usize>, s: &Self| nb.is_some_and(|m| s.intra_usable(m));
        let top = usable(self.nb_b, self);
        let tr = usable(self.nb_c, self);
        let tl = usable(self.nb_d, self);
        let left = [
            self.left_usable(0, 4, 16),
            self.left_usable(4, 4, 16),
            self.left_usable(8, 4, 16),
            self.left_usable(12, 4, 16),
        ];
        let (ly, ls) = (self.ly, self.ls);
        let depth = self.depth;
        let max = self.max;
        match kind {
            I16 => {
                let all = left.iter().all(|&l| l);
                intra::pred16x16(&mut self.planes[0], ly, ls, self.i16_mode, all, top, tl, depth);
                self.add_luma_4x4();
            }
            I4 => {
                for blk in 0..16 {
                    let (bx, by) = (BLK_X[blk] as usize, BLK_Y[blk] as usize);
                    let top_right = if by == 0 {
                        if bx < 3 { top } else { tr }
                    } else if bx == 3 {
                        false
                    } else {
                        BLK_ORDER[(by - 1) * 4 + bx + 1] < BLK_ORDER[by * 4 + bx]
                    };
                    let top_left = match (bx, by) {
                        (0, 0) => tl,
                        (0, _) => self.left_usable(by as i32 * 4 - 1, 1, 16),
                        (_, 0) => top,
                        _ => true,
                    };
                    let av = Avail {
                        left: bx > 0 || left[by],
                        top: by > 0 || top,
                        top_right,
                        top_left,
                    };
                    let at = ly + by * 4 * ls + bx * 4;
                    let mode = self.mbs[idx].modes[by * 4 + bx] as u8;
                    intra::pred4x4(&mut self.planes[0], at, ls, mode, av, depth);
                    if self.luma_any & (1 << blk) != 0 {
                        idct4_add(&mut self.planes[0], at, ls, &self.coef[blk * 16..blk * 16 + 16], max);
                        self.coef[blk * 16..blk * 16 + 16].fill(0);
                    }
                }
                self.luma_any = 0;
            }
            _ => {
                for b8 in 0..4 {
                    let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                    let av = Avail {
                        left: bx > 0 || (left[by] && left[by + 1]),
                        top: by > 0 || top,
                        top_right: match b8 {
                            0 => top,
                            1 => tr,
                            2 => true,
                            _ => false,
                        },
                        top_left: match b8 {
                            0 => tl,
                            1 => top,
                            2 => self.left_usable(7, 1, 16),
                            _ => true,
                        },
                    };
                    let at = ly + by * 4 * ls + bx * 4;
                    let mode = self.mbs[idx].modes[by * 4 + bx] as u8;
                    intra::pred8x8l(&mut self.planes[0], at, ls, mode, av, depth);
                    if self.luma_any & (0xf << (b8 * 4)) != 0 {
                        idct8_add(&mut self.planes[0], at, ls, &self.coef[b8 * 64..b8 * 64 + 64], max);
                        self.coef[b8 * 64..b8 * 64 + 64].fill(0);
                    }
                }
                self.luma_any = 0;
            }
        }
        if self.cfi != 0 {
            let mch = self.mch;
            let rows = mch / 4;
            let mut cleft = [false; 4];
            for (r, slot) in cleft.iter_mut().enumerate().take(rows) {
                *slot = self.left_usable(r as i32 * 4, 4, mch as i32);
            }
            let mode = self.mbs[idx].chroma_mode;
            let (lc, cs) = (self.lc, self.cs);
            for plane in 1..3 {
                intra::pred_chroma(&mut self.planes[plane], lc, cs, mode, cleft, top, tl, mch, depth);
            }
            self.add_chroma();
        }
    }

    fn add_luma_4x4(&mut self) {
        let max = self.max;
        let (ly, ls) = (self.ly, self.ls);
        let mut any = self.luma_any;
        while any != 0 {
            let blk = any.trailing_zeros() as usize;
            any &= any - 1;
            let at = ly + BLK_Y[blk] as usize * 4 * ls + BLK_X[blk] as usize * 4;
            idct4_add(&mut self.planes[0], at, ls, &self.coef[blk * 16..blk * 16 + 16], max);
            self.coef[blk * 16..blk * 16 + 16].fill(0);
        }
        self.luma_any = 0;
    }

    fn add_chroma(&mut self) {
        let max = self.max;
        let (lc, cs) = (self.lc, self.cs);
        for plane in 0..2 {
            let mut any = self.chroma_any[plane];
            while any != 0 {
                let b = any.trailing_zeros() as usize;
                any &= any - 1;
                let at = lc + (b >> 1) * 4 * cs + (b & 1) * 4;
                idct4_add(&mut self.planes[plane + 1], at, cs, &self.ccoef[plane][b * 16..b * 16 + 16], max);
                self.ccoef[plane][b * 16..b * 16 + 16].fill(0);
            }
            self.chroma_any[plane] = 0;
        }
    }

    pub fn add_residual_inter(&mut self) {
        if self.mbs[self.idx].t8 {
            let max = self.max;
            let (ly, ls) = (self.ly, self.ls);
            for b8 in 0..4 {
                if self.luma_any & (0xf << (b8 * 4)) != 0 {
                    let at = ly + (b8 >> 1) * 8 * ls + (b8 & 1) * 8;
                    idct8_add(&mut self.planes[0], at, ls, &self.coef[b8 * 64..b8 * 64 + 64], max);
                    self.coef[b8 * 64..b8 * 64 + 64].fill(0);
                }
            }
            self.luma_any = 0;
        } else {
            self.add_luma_4x4();
        }
        self.add_chroma();
    }
}
