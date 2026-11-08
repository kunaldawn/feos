//! Per-macroblock state, and where a macroblock's neighbours are (spec 6.4.12). Storage
//! is indexed by the macroblock's position in the frame, so a field's macroblocks are
//! on every other row and those of an MBAFF pair are one above the other.

use alloc::vec::Vec;

use crate::bitstream::BitstreamReader;
use crate::error::DecodeError;
use crate::pps::Pps;
use crate::slice::SliceHeader;
use crate::sps::Sps;

use super::cabac::{CONTEXTS, Cabac};
use super::dpb::{MbMotion, RefEntry};
use super::pixel::Pixel;

pub(crate) const I4: u8 = 0;
pub(crate) const I8: u8 = 1;
pub(crate) const I16: u8 = 2;
pub(crate) const PCM: u8 = 3;
pub(crate) const INTER: u8 = 4;

/// What later macroblocks and deblocking need of a macroblock.
#[derive(Clone, Copy)]
pub(crate) struct MbInfo {
    /// The slice it was decoded in; 0 if it hasn't been.
    pub slice: u32,
    pub kind: u8,
    pub field: bool,
    pub t8: bool,
    pub skip: bool,
    /// B_Skip or B_Direct_16x16.
    pub direct16: bool,
    /// 8x8 blocks predicted by direct prediction.
    pub direct8: u8,
    /// Luma bits, and the chroma value above them.
    pub cbp: u8,
    /// coded_block_flag of luma DC, Cb DC, Cr DC.
    pub cbf_dc: u8,
    pub qp: i8,
    pub qpc: [i8; 2],
    pub chroma_mode: u8,
    /// Luma blocks with coefficients, in raster order, for deblocking.
    pub mask: u16,
    pub filter_idc: u8,
    pub alpha: i8,
    pub beta: i8,
    /// total_coeff per block: 16 luma in raster order, then 8 each of Cb and Cr.
    pub nz: [u8; 32],
    pub modes: [i8; 16],
    pub mvd: [[[u8; 2]; 16]; 2],
}

impl Default for MbInfo {
    fn default() -> Self {
        Self {
            slice: 0,
            kind: INTER,
            field: false,
            t8: false,
            skip: false,
            direct16: false,
            direct8: 0,
            cbp: 0,
            cbf_dc: 0,
            qp: 0,
            qpc: [0; 2],
            chroma_mode: 0,
            mask: 0,
            filter_idc: 0,
            alpha: 0,
            beta: 0,
            nz: [0; 32],
            modes: [2; 16],
            mvd: [[[0; 2]; 16]; 2],
        }
    }
}

impl MbInfo {
    #[inline(always)]
    pub fn intra(&self) -> bool {
        self.kind != INTER
    }
}

/// Level scales for every list and qP % 6, in raster order.
pub(crate) struct Scales {
    pub s4: [[[i32; 16]; 6]; 6],
    pub s8: [[[i32; 64]; 6]; 2],
}

pub(crate) const ZIG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
pub(crate) const FIELD4: [u8; 16] = [0, 4, 1, 8, 12, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];
#[rustfmt::skip]
pub(crate) const ZIG8: [u8; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10, 17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// The 8x8 field scan, as row * 8 + column.
#[rustfmt::skip]
pub(crate) const FIELD8: [u8; 64] = [
     0,  8, 16,  1,  9, 24, 32, 17,  2, 25, 40, 48, 56, 33, 10,  3,
    18, 41, 49, 57, 26, 11,  4, 19, 34, 42, 50, 58, 27, 12,  5, 20,
    35, 43, 51, 59, 28, 13,  6, 21, 36, 44, 52, 60, 29, 14, 22, 37,
    45, 53, 61, 30,  7, 15, 38, 46, 54, 62, 23, 31, 39, 47, 55, 63,
];

/// Column and row of each luma block in coding order.
pub(crate) const BLK_X: [u8; 16] = [0, 1, 0, 1, 2, 3, 2, 3, 0, 1, 0, 1, 2, 3, 2, 3];
pub(crate) const BLK_Y: [u8; 16] = [0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3];
/// Coding order of the block at column x, row y: `BLK_ORDER[y * 4 + x]`.
pub(crate) const BLK_ORDER: [u8; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

impl Scales {
    pub fn new() -> Self {
        Self {
            s4: [[[0; 16]; 6]; 6],
            s8: [[[0; 64]; 6]; 2],
        }
    }

    /// Fills the tables from a PPS's lists (8.5.9).
    pub fn build(&mut self, pps: &Pps) {
        const V4: [[i32; 3]; 6] = [
            [10, 16, 13],
            [11, 18, 14],
            [13, 20, 16],
            [14, 23, 18],
            [16, 25, 20],
            [18, 29, 23],
        ];
        const V8: [[i32; 6]; 6] = [
            [20, 18, 32, 19, 25, 24],
            [22, 19, 35, 21, 28, 26],
            [26, 23, 42, 24, 33, 31],
            [28, 25, 45, 26, 35, 33],
            [32, 28, 51, 30, 40, 38],
            [36, 32, 58, 34, 46, 43],
        ];
        for list in 0..6 {
            for q in 0..6 {
                for k in 0..16 {
                    let at = ZIG4[k] as usize;
                    let (i, j) = (at >> 2, at & 3);
                    let cat = if i % 2 == 0 && j % 2 == 0 {
                        0
                    } else if i % 2 == 1 && j % 2 == 1 {
                        1
                    } else {
                        2
                    };
                    self.s4[list][q][at] = pps.scaling_list_4x4[list][k] as i32 * V4[q][cat];
                }
            }
        }
        for list in 0..2 {
            for q in 0..6 {
                for at in 0..64 {
                    let (i, j) = (at >> 3, at & 7);
                    let cat = if i % 4 == 0 && j % 4 == 0 {
                        0
                    } else if i % 2 == 1 && j % 2 == 1 {
                        1
                    } else if i % 4 == 2 && j % 4 == 2 {
                        2
                    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
                        3
                    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
                        4
                    } else {
                        5
                    };
                    // 8x8 lists are kept in raster order.
                    self.s8[list][q][at] = pps.scaling_list_8x8[list][at] as i32 * V8[q][cat];
                }
            }
        }
    }
}

/// Predictions from each list, for each plane.
pub(crate) type Preds<P> = [[[P; 256]; 3]; 2];

/// Everything a slice decodes with. `planes`, `motion` and `mbs` are those of the frame.
pub(crate) struct Sd<'a, P: Pixel> {
    pub planes: &'a mut [Vec<P>; 3],
    pub motion: &'a mut [MbMotion],
    pub mbs: &'a mut [MbInfo],
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub h: &'a SliceHeader,
    pub scales: &'a Scales,
    pub lists: [Vec<RefEntry<P>>; 2],

    pub w: usize,
    /// Height of the frame in macroblocks.
    pub frame_h: usize,
    /// Macroblocks in this picture, frame or field.
    pub pic_mbs: usize,
    /// `FRAME`, `TOP` or `BOTTOM`.
    pub structure: u8,
    pub mbaff: bool,
    pub cfi: u32,
    /// Chroma plane height per macroblock: 8 or 16, or 0 without chroma.
    pub mch: usize,
    pub depth: u32,
    pub max: i32,
    pub qp_off: i32,
    pub slice_id: u32,
    pub is_b: bool,
    pub is_i: bool,
    pub cur_poc: [i32; 2],
    pub rbsp: &'a [u8],

    pub br: BitstreamReader<'a>,
    pub cab: Option<Cabac<'a>>,
    pub ctx: [u8; CONTEXTS],

    pub qp_prev: i32,
    pub last_dqp: bool,

    // The current macroblock.
    pub addr: usize,
    pub idx: usize,
    pub mbx: usize,
    /// Row of its first line within its frame or field, in macroblocks; in MBAFF that of
    /// the pair times two, plus one for a lower frame macroblock.
    pub mby: usize,
    /// A field macroblock, of a field picture or of an MBAFF pair.
    pub field: bool,
    /// The lower of an MBAFF pair.
    pub lower: bool,
    /// `FRAME` for a frame macroblock, or its field's parity.
    pub parity: u8,
    /// In MBAFF the left pair's upper macroblock, otherwise the left macroblock.
    pub nb_a: Option<usize>,
    pub nb_b: Option<usize>,
    pub nb_c: Option<usize>,
    pub nb_d: Option<usize>,
    pub d_row: i32,
    /// Sample offsets and strides of the macroblock: luma, then chroma.
    pub ly: usize,
    pub ls: usize,
    pub lc: usize,
    pub cs: usize,

    // Parsed residual.
    pub coef: [i32; 256],
    pub ccoef: [[i32; 128]; 2],
    /// Luma blocks with coefficients, by coding order.
    pub luma_any: u16,
    /// Chroma blocks with coefficients, in raster order, per plane.
    pub chroma_any: [u8; 2],
    pub lev_pos: [u8; 64],
    pub lev_val: [i32; 64],
    pub cavlc_count: usize,
    /// Intra16x16 prediction mode.
    pub i16_mode: u8,
    /// Spatial direct's reference indices and motion vectors, once worked out.
    pub direct_ready: bool,
    pub direct_ref: [i8; 2],
    pub direct_mv: [[i16; 2]; 2],
    /// Working space of inter prediction, lent to it while it runs.
    pub preds: Option<&'a mut Preds<P>>,
}

impl<P: Pixel> Sd<'_, P> {
    /// Positions the decoder on macroblock `addr`, with `field` the pair's coding in
    /// MBAFF, and finds its neighbours.
    pub fn locate(&mut self, addr: usize, field_mb: bool) {
        let w = self.w;
        self.addr = addr;
        let s = w * 16;
        let c = w * 8;
        self.nb_a = None;
        self.nb_b = None;
        self.nb_c = None;
        self.nb_d = None;
        self.d_row = 15;
        if self.mbaff {
            let pair = addr / 2;
            let (x, row) = (pair % w, pair / w);
            let lower = addr & 1 != 0;
            let top = x + row * 2 * w;
            self.mbx = x;
            self.lower = lower;
            self.field = field_mb;
            self.idx = top + if lower { w } else { 0 };
            self.parity = if field_mb {
                if lower { super::dpb::BOTTOM } else { super::dpb::TOP }
            } else {
                super::dpb::FRAME
            };
            let id = self.slice_id;
            let mbs = &*self.mbs;
            let pair_at = |cond: bool, at: usize| -> Option<usize> {
                (cond && mbs[at].slice == id).then_some(at)
            };
            let pa = if x > 0 { pair_at(true, top - 1) } else { None };
            let pb = if row > 0 { pair_at(true, top - 2 * w) } else { None };
            let pc = if row > 0 && x + 1 < w {
                pair_at(true, top - 2 * w + 1)
            } else {
                None
            };
            let pd = if row > 0 && x > 0 {
                pair_at(true, top - 2 * w - 1)
            } else {
                None
            };
            self.nb_a = pa;
            if !field_mb {
                if !lower {
                    self.nb_b = pb.map(|m| m + w);
                    self.nb_c = pc.map(|m| m + w);
                    self.nb_d = pd.map(|m| m + w);
                } else {
                    self.nb_b = Some(top);
                    self.nb_c = None;
                    // The line above this one, which in a field pair is in its bottom
                    // field's macroblock.
                    self.nb_d = pa.map(|m| if mbs[m].field { m + w } else { m });
                    if let Some(m) = pa {
                        self.d_row = if mbs[m].field { 7 } else { 15 };
                    }
                }
                self.mby = row * 2 + lower as usize;
                self.ly = (row * 32 + if lower { 16 } else { 0 }) * s + x * 16;
                self.ls = s;
                let ch = self.mch;
                self.lc = (row * 2 * ch + if lower { ch } else { 0 }) * c + x * 8;
                self.cs = c;
            } else {
                if !lower {
                    self.nb_b = pb.map(|m| if mbs[m].field { m } else { m + w });
                    self.nb_c = pc.map(|m| if mbs[m].field { m } else { m + w });
                    self.nb_d = pd.map(|m| if mbs[m].field { m } else { m + w });
                } else {
                    self.nb_b = pb.map(|m| m + w);
                    self.nb_c = pc.map(|m| m + w);
                    self.nb_d = pd.map(|m| m + w);
                }
                self.mby = row;
                self.ly = (row * 32 + lower as usize) * s + x * 16;
                self.ls = s * 2;
                let ch = self.mch;
                self.lc = (row * 2 * ch + lower as usize) * c + x * 8;
                self.cs = c * 2;
            }
        } else {
            let (x, row) = (addr % w, addr / w);
            self.mbx = x;
            self.mby = row;
            self.lower = false;
            let id = self.slice_id;
            let is_field = self.structure != super::dpb::FRAME;
            self.field = is_field;
            self.parity = self.structure;
            let (rowstep, base) = if is_field {
                (2 * w, if self.structure == super::dpb::BOTTOM { w } else { 0 })
            } else {
                (w, 0)
            };
            self.idx = x + row * rowstep + base;
            let idx = self.idx;
            let mbs = &*self.mbs;
            if x > 0 && mbs[idx - 1].slice == id {
                self.nb_a = Some(idx - 1);
            }
            if row > 0 {
                let up = idx - rowstep;
                if mbs[up].slice == id {
                    self.nb_b = Some(up);
                }
                if x + 1 < w && mbs[up + 1].slice == id {
                    self.nb_c = Some(up + 1);
                }
                if x > 0 && mbs[up - 1].slice == id {
                    self.nb_d = Some(up - 1);
                }
            }
            let ch = self.mch;
            if is_field {
                let odd = (self.structure == super::dpb::BOTTOM) as usize;
                self.ly = (row * 32 + odd) * s + x * 16;
                self.ls = s * 2;
                self.lc = (row * 2 * ch + odd) * c + x * 8;
                self.cs = c * 2;
            } else {
                self.ly = row * 16 * s + x * 16;
                self.ls = s;
                self.lc = row * ch * c + x * 8;
                self.cs = c;
            }
        }
    }

    /// The macroblock and row holding the sample left of row `y`, where a macroblock
    /// has `max_h` rows (Table 6-4 for MBAFF).
    #[inline]
    pub fn left(&self, y: i32, max_h: i32) -> Option<(usize, i32)> {
        let a = self.nb_a?;
        if !self.mbaff {
            return Some((a, y));
        }
        let w = self.w;
        let a_field = self.mbs[a].field;
        Some(if !self.field {
            if !a_field {
                (if self.lower { a + w } else { a }, y)
            } else {
                let row = if self.lower { y + max_h } else { y };
                (if y & 1 == 0 { a } else { a + w }, row >> 1)
            }
        } else if a_field {
            (if self.lower { a + w } else { a }, y)
        } else {
            let odd = self.lower as i32;
            if y < max_h / 2 {
                (a, (y << 1) + odd)
            } else {
                (a + w, (y << 1) + odd - max_h)
            }
        })
    }

    /// The macroblock left of this one for what's decided per macroblock.
    #[inline]
    pub fn mb_a(&self) -> Option<usize> {
        self.left(0, 16).map(|(m, _)| m)
    }

    /// Whether a neighbouring macroblock's samples are usable for intra prediction.
    #[inline]
    pub fn intra_usable(&self, mb: usize) -> bool {
        !self.pps.constrained_intra_pred_flag || self.mbs[mb].intra()
    }

    /// Error out on data the slice can't have.
    pub fn check_overrun(&self) -> Result<(), DecodeError> {
        if let Some(cab) = &self.cab
            && cab.overrun() {
                return Err(DecodeError::InvalidSyntax("CABAC data ran out"));
            }
        Ok(())
    }
}
