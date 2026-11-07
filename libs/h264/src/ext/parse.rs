//! Slice data and the macroblock layer (spec 7.3.4, 7.3.5) for CAVLC and CABAC, with
//! the context selection of 9.3.3.1.

use crate::cabac::{
    LAST_COEFF_FLAG_OFFSET_8X8, SIGNIFICANT_COEFF_FLAG_FIELD_OFFSET_8X8,
    SIGNIFICANT_COEFF_FLAG_OFFSET_8X8,
};
use crate::cavlc::parse_residual_block_cavlc;
use crate::error::DecodeError;
use crate::residual::{CBP_INTER_TABLE, CBP_INTRA_TABLE, QPC_TABLE};

use super::dpb::MbMotion;
use super::mb::*;
use super::pixel::Pixel;

const BAD: DecodeError = DecodeError::InvalidSyntax("bad macroblock data");

/// Shape of a B macroblock partitioning, and each partition's prediction: 1 is L0,
/// 2 is L1, 3 both.
const B_TYPES: [(u8, [u8; 2]); 22] = [
    (0, [0, 0]),
    (0, [1, 0]),
    (0, [2, 0]),
    (0, [3, 0]),
    (1, [1, 1]),
    (2, [1, 1]),
    (1, [2, 2]),
    (2, [2, 2]),
    (1, [1, 2]),
    (2, [1, 2]),
    (1, [2, 1]),
    (2, [2, 1]),
    (1, [1, 3]),
    (2, [1, 3]),
    (1, [2, 3]),
    (2, [2, 3]),
    (1, [3, 1]),
    (2, [3, 1]),
    (1, [3, 2]),
    (2, [3, 2]),
    (1, [3, 3]),
    (2, [3, 3]),
];

/// B sub-macroblock types: prediction and shape (0 8x8, 1 8x4, 2 4x8, 3 4x4).
const B_SUB: [(u8, u8); 13] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (1, 1),
    (1, 2),
    (2, 1),
    (2, 2),
    (3, 1),
    (3, 2),
    (1, 3),
    (2, 3),
    (3, 3),
];

impl<P: Pixel> Sd<'_, P> {
    #[inline(always)]
    fn bin(&mut self, ctx: usize) -> bool {
        // `cab` is always there on CABAC paths, and `ctx` below CONTEXTS by construction.
        match self.cab.as_mut() {
            Some(cab) => cab.decision(&mut self.ctx[ctx]),
            None => false,
        }
    }

    #[inline(always)]
    fn bypass(&mut self) -> bool {
        match self.cab.as_mut() {
            Some(cab) => cab.bypass(),
            None => false,
        }
    }

    fn terminate(&mut self) -> bool {
        match self.cab.as_mut() {
            Some(cab) => cab.terminate(),
            None => true,
        }
    }

    /// Decodes the whole slice, starting at `first`, which is in macroblocks.
    pub fn decode_slice_data(&mut self, first: usize) -> Result<(), DecodeError> {
        let mut addr = first;
        if addr >= self.pic_mbs {
            return Err(DecodeError::InvalidSyntax("first_mb_in_slice out of range"));
        }
        let cabac = self.pps.entropy_coding_mode_flag;
        let inter = !self.is_i;
        let mut run: i64 = -1;
        let mut prev_skipped = false;
        // Lookahead for a skipped upper macroblock of an MBAFF pair.
        let mut lower_skip: Option<bool> = None;
        let mut field_read = false;
        let mut pair_field = false;
        loop {
            if addr >= self.pic_mbs {
                return Ok(());
            }
            let upper = addr & 1 == 0;
            if self.mbaff && upper {
                pair_field = self.infer_field(addr);
                field_read = false;
                lower_skip = None;
                self.mark_pair(addr, pair_field);
            }
            self.locate(addr, pair_field);
            self.begin_mb();
            let mut skipped = false;
            if inter {
                if cabac {
                    skipped = match lower_skip.take() {
                        Some(known) if self.mbaff && !upper => known,
                        _ => self.read_skip_flag(),
                    };
                    if skipped && self.mbaff && upper {
                        // The pair's coding decides how this one is predicted, and
                        // it's only known from the lower macroblock's data.
                        self.mbs[self.idx].skip = true;
                        self.mbs[self.idx].kind = INTER;
                        self.locate(addr + 1, pair_field);
                        self.mbs[self.idx].slice = self.slice_id;
                        let next = self.read_skip_flag();
                        lower_skip = Some(next);
                        if !next {
                            pair_field = self.read_field_flag(addr);
                            field_read = true;
                        }
                        self.mark_pair(addr, pair_field);
                        self.locate(addr, pair_field);
                    }
                } else {
                    if run == -1 {
                        let value = self.br.read_ue().map_err(|_| BAD)? as i64;
                        if value > (self.pic_mbs - addr) as i64 {
                            return Err(BAD);
                        }
                        run = value;
                    }
                    if run > 0 {
                        run -= 1;
                        skipped = true;
                        if self.mbaff && upper && run == 0 && self.br.more_rbsp_data() {
                            // An upper skip with the lower coded: its flag comes next.
                            pair_field = self.br.peek_bits(1) != 0;
                            self.mark_pair(addr, pair_field);
                            self.locate(addr, pair_field);
                        }
                    } else {
                        run = -1;
                    }
                }
            }
            if skipped {
                self.decode_skip_mb()?;
                    prev_skipped = true;
                if cabac {
                    if !(self.mbaff && upper) && self.terminate() {
                        return Ok(());
                    }
                    self.check_overrun()?;
                } else if run == 0 {
                    if !self.br.more_rbsp_data() {
                        return Ok(());
                    }
                    run = -2;
                }
                addr += 1;
                continue;
            }
            if self.mbaff && (upper || (prev_skipped && !field_read)) {
                let flag = if cabac {
                    self.read_field_flag(addr)
                } else {
                    self.br.read_bit().map_err(|_| BAD)? != 0
                };
                if flag != pair_field {
                    pair_field = flag;
                    self.mark_pair(addr, pair_field);
                    self.locate(addr, pair_field);
                }
                field_read = true;
            }
            if run == -2 {
                run = -1;
            }
            self.macroblock_layer()?;
            prev_skipped = false;
            if cabac {
                if !(self.mbaff && upper) && self.terminate() {
                    return Ok(());
                }
                self.check_overrun()?;
            } else if !self.br.more_rbsp_data() {
                return Ok(());
            }
            addr += 1;
        }
    }

    /// 7.4.4: a pair's coding when nothing says, from the pair on the left, then above.
    fn infer_field(&self, addr: usize) -> bool {
        let w = self.w;
        let pair = addr / 2;
        let (x, row) = (pair % w, pair / w);
        let top = x + row * 2 * w;
        if x > 0 && self.mbs[top - 1].slice == self.slice_id {
            return self.mbs[top - 1].field;
        }
        if row > 0 && self.mbs[top - 2 * w].slice == self.slice_id {
            return self.mbs[top - 2 * w].field;
        }
        false
    }

    /// Records the pair's coding on both its macroblocks.
    fn mark_pair(&mut self, addr: usize, field: bool) {
        let w = self.w;
        let pair = addr / 2;
        let top = pair % w + (pair / w) * 2 * w;
        self.mbs[top].field = field;
        self.mbs[top + w].field = field;
        self.motion[top].field = field;
        self.motion[top + w].field = field;
    }

    /// Stamps the macroblock as being in this slice, clearing what an earlier picture
    /// or field left. Its pair's coding stays.
    fn begin_mb(&mut self) {
        let field = self.field;
        let qp = self.qp_prev;
        let chroma = self.pps.chroma_qp_index_offset;
        let chroma2 = self.pps.second_chroma_qp_index_offset;
        let qpc = [self.qpc_of(qp, chroma), self.qpc_of(qp, chroma2)];
        let (idc, alpha, beta) = (
            self.h.disable_deblocking_filter_idc as u8,
            self.h.slice_alpha_c0_offset_div2 as i8,
            self.h.slice_beta_offset_div2 as i8,
        );
        let mb = &mut self.mbs[self.idx];
        *mb = MbInfo {
            slice: self.slice_id,
            field,
            qp: qp as i8,
            qpc,
            filter_idc: idc,
            alpha,
            beta,
            ..MbInfo::default()
        };
        self.motion[self.idx] = MbMotion {
            intra: false,
            field,
            ..MbMotion::default()
        };
        self.direct_ready = false;
    }

    /// QPc, without the bit depth offset, for a QPy (8.5.8, 8.7.2.2).
    pub fn qpc_of(&self, qp: i32, offset: i32) -> i8 {
        let index = (qp + offset).clamp(-self.qp_off, 51);
        (if index < 0 {
            index
        } else {
            QPC_TABLE[index as usize]
        }) as i8
    }

    fn read_skip_flag(&mut self) -> bool {
        let a = self.mb_a().is_some_and(|m| !self.mbs[m].skip);
        let b = self.nb_b.is_some_and(|m| !self.mbs[m].skip);
        let base = if self.is_b { 24 } else { 11 };
        self.bin(base + a as usize + b as usize)
    }

    fn read_field_flag(&mut self, addr: usize) -> bool {
        let w = self.w;
        let pair = addr / 2;
        let (x, row) = (pair % w, pair / w);
        let top = x + row * 2 * w;
        let id = self.slice_id;
        let a = x > 0 && self.mbs[top - 1].slice == id && self.mbs[top - 1].field;
        let b = row > 0 && self.mbs[top - 2 * w].slice == id && self.mbs[top - 2 * w].field;
        self.bin(70 + a as usize + b as usize)
    }

    fn decode_skip_mb(&mut self) -> Result<(), DecodeError> {
        let idx = self.idx;
        self.mbs[idx].skip = true;
        self.mbs[idx].kind = INTER;
        if self.is_b {
            self.mbs[idx].direct16 = true;
            self.mbs[idx].direct8 = 15;
            self.pred_direct(15)?;
        } else {
            self.pred_p_skip()?;
        }
        self.finish_motion();
        self.inter_pred();
        self.last_dqp = false;
        Ok(())
    }

    fn read_mb_type_intra_cabac(&mut self, base: usize, prefix: bool) -> Result<u32, DecodeError> {
        // Slice-type I has its own contexts and a context for the first bin.
        let (b0, o1, o2, o3, o4, o5) = if prefix {
            (self.bin(base), 1, 2, 2, 3, 3)
        } else {
            let a = self.mb_a().is_some_and(|m| self.mbs[m].kind != I4 && self.mbs[m].kind != I8);
            let b = self.nb_b.is_some_and(|m| self.mbs[m].kind != I4 && self.mbs[m].kind != I8);
            (self.bin(base + a as usize + b as usize), 3, 4, 5, 6, 7)
        };
        if !b0 {
            return Ok(0);
        }
        if self.terminate() {
            return Ok(25);
        }
        let mut t = 1 + 12 * self.bin(base + o1) as u32;
        if self.bin(base + o2) {
            t += 4 + 4 * self.bin(base + o3) as u32;
        }
        t += 2 * self.bin(base + o4) as u32;
        t += self.bin(base + o5) as u32;
        Ok(t)
    }

    /// mb_type as coded for the slice type. Returns the type and whether it's intra.
    fn read_mb_type(&mut self) -> Result<(u32, bool), DecodeError> {
        if !self.pps.entropy_coding_mode_flag {
            let t = self.br.read_ue().map_err(|_| BAD)?;
            return Ok(if self.is_i {
                (t, true)
            } else if self.is_b {
                if t >= 23 { (t - 23, true) } else { (t, false) }
            } else if t >= 5 {
                (t - 5, true)
            } else {
                (t, false)
            });
        }
        if self.is_i {
            return Ok((self.read_mb_type_intra_cabac(3, false)?, true));
        }
        if !self.is_b {
            if self.bin(14) {
                return Ok((self.read_mb_type_intra_cabac(17, true)?, true));
            }
            let t = if self.bin(15) {
                if self.bin(17) { 1 } else { 2 }
            } else if self.bin(16) {
                3
            } else {
                0
            };
            return Ok((t, false));
        }
        let a = self.mb_a().is_some_and(|m| !self.mbs[m].direct16);
        let b = self.nb_b.is_some_and(|m| !self.mbs[m].direct16);
        if !self.bin(27 + a as usize + b as usize) {
            return Ok((0, false));
        }
        if !self.bin(27 + 3) {
            return Ok((1 + self.bin(27 + 5) as u32, false));
        }
        let mut bits = (self.bin(27 + 4) as u32) << 3;
        bits |= (self.bin(27 + 5) as u32) << 2;
        bits |= (self.bin(27 + 5) as u32) << 1;
        bits |= self.bin(27 + 5) as u32;
        Ok(match bits {
            0..=7 => (bits + 3, false),
            13 => (self.read_mb_type_intra_cabac(32, true)?, true),
            14 => (11, false),
            15 => (22, false),
            _ => {
                let bits = (bits << 1) | self.bin(27 + 5) as u32;
                (bits - 4, false)
            }
        })
    }

    fn read_sub_type_p(&mut self) -> Result<u32, DecodeError> {
        if !self.pps.entropy_coding_mode_flag {
            let t = self.br.read_ue().map_err(|_| BAD)?;
            if t > 3 {
                return Err(BAD);
            }
            return Ok(t);
        }
        Ok(if self.bin(21) {
            0
        } else if !self.bin(22) {
            1
        } else if self.bin(23) {
            2
        } else {
            3
        })
    }

    fn read_sub_type_b(&mut self) -> Result<u32, DecodeError> {
        if !self.pps.entropy_coding_mode_flag {
            let t = self.br.read_ue().map_err(|_| BAD)?;
            if t > 12 {
                return Err(BAD);
            }
            return Ok(t);
        }
        if !self.bin(36) {
            return Ok(0);
        }
        if !self.bin(37) {
            return Ok(1 + self.bin(39) as u32);
        }
        let mut t = 3;
        if self.bin(38) {
            if self.bin(39) {
                return Ok(11 + self.bin(39) as u32);
            }
            t += 4;
        }
        t += 2 * self.bin(39) as u32;
        t += self.bin(39) as u32;
        Ok(t)
    }

    fn macroblock_layer(&mut self) -> Result<(), DecodeError> {
        let (mb_type, intra) = self.read_mb_type()?;
        if intra {
            return self.intra_mb(mb_type);
        }
        let idx = self.idx;
        self.mbs[idx].kind = INTER;
        let mut no_sub8 = true;
        let is_8x8 = if self.is_b { mb_type == 22 } else { mb_type >= 3 };
        let cabac = self.pps.entropy_coding_mode_flag;
        if self.is_b && mb_type == 0 {
            self.mbs[idx].direct16 = true;
            self.mbs[idx].direct8 = 15;
            self.pred_direct(15)?;
            no_sub8 = self.sps.direct_8x8_inference_flag;
        } else if is_8x8 {
            let mut shapes = [0u8; 4];
            let mut preds = [1u8; 4];
            let mut direct = 0u8;
            for i in 0..4 {
                if self.is_b {
                    let t = self.read_sub_type_b()? as usize;
                    (preds[i], shapes[i]) = B_SUB[t];
                    if t == 0 {
                        direct |= 1 << i;
                        if !self.sps.direct_8x8_inference_flag {
                            no_sub8 = false;
                        }
                    } else if shapes[i] != 0 {
                        no_sub8 = false;
                    }
                } else {
                    shapes[i] = self.read_sub_type_p()? as u8;
                    if shapes[i] != 0 {
                        no_sub8 = false;
                    }
                }
            }
            self.mbs[idx].direct8 = direct;
            if direct != 0 {
                self.pred_direct(direct)?;
            }
            let zero_refs = !self.is_b && mb_type == 4;
            for list in 0..2 {
                for i in 0..4 {
                    if direct & (1 << i) != 0 || preds[i] & (1 << list) == 0 {
                        continue;
                    }
                    let r = if zero_refs {
                        0
                    } else {
                        self.read_ref_idx(list, (i & 1) * 2, (i >> 1) * 2)?
                    };
                    self.motion[idx].ref_idx[list][i] = r;
                }
            }
            for list in 0..2 {
                for i in 0..4 {
                    if direct & (1 << i) != 0 || preds[i] & (1 << list) == 0 {
                        continue;
                    }
                    let (x0, y0) = ((i & 1) * 2, (i >> 1) * 2);
                    let r = self.motion[idx].ref_idx[list][i];
                    match shapes[i] {
                        0 => self.read_mv(list, x0, y0, 2, 2, r, 0)?,
                        1 => {
                            self.read_mv(list, x0, y0, 2, 1, r, 0)?;
                            self.read_mv(list, x0, y0 + 1, 2, 1, r, 0)?;
                        }
                        2 => {
                            self.read_mv(list, x0, y0, 1, 2, r, 0)?;
                            self.read_mv(list, x0 + 1, y0, 1, 2, r, 0)?;
                        }
                        _ => {
                            for k in 0..4 {
                                self.read_mv(list, x0 + (k & 1), y0 + (k >> 1), 1, 1, r, 0)?;
                            }
                        }
                    }
                }
            }
        } else {
            let (shape, preds) = if self.is_b {
                B_TYPES[mb_type as usize]
            } else if mb_type == 0 {
                (0, [1, 0])
            } else {
                (mb_type as u8, [1, 1])
            };
            let parts = if shape == 0 { 1 } else { 2 };
            for list in 0..2 {
                for p in 0..parts {
                    if preds[p] & (1 << list) == 0 {
                        continue;
                    }
                    let (x, y) = match shape {
                        1 => (0, p * 2),
                        2 => (p * 2, 0),
                        _ => (0, 0),
                    };
                    let r = self.read_ref_idx(list, x, y)?;
                    let refs = &mut self.motion[idx].ref_idx[list];
                    match shape {
                        0 => refs.fill(r),
                        1 => {
                            refs[p * 2] = r;
                            refs[p * 2 + 1] = r;
                        }
                        _ => {
                            refs[p] = r;
                            refs[p + 2] = r;
                        }
                    }
                }
            }
            for list in 0..2 {
                for p in 0..parts {
                    if preds[p] & (1 << list) == 0 {
                        continue;
                    }
                    match shape {
                        0 => {
                            let r = self.motion[idx].ref_idx[list][0];
                            self.read_mv(list, 0, 0, 4, 4, r, 0)?;
                        }
                        1 => {
                            let r = self.motion[idx].ref_idx[list][p * 2];
                            self.read_mv(list, 0, p * 2, 4, 2, r, 1 + p as u8)?;
                        }
                        _ => {
                            let r = self.motion[idx].ref_idx[list][p];
                            self.read_mv(list, p * 2, 0, 2, 4, r, 3 + p as u8)?;
                        }
                    }
                }
            }
        }
        self.finish_motion();
        // coded_block_pattern, transform size, QP, residual.
        let cbp = self.read_cbp(false)?;
        self.mbs[idx].cbp = cbp;
        if cbp & 15 != 0 && self.pps.transform_8x8_mode_flag && no_sub8 {
            let t8 = if cabac {
                self.read_t8_flag()
            } else {
                self.br.read_bit().map_err(|_| BAD)? != 0
            };
            self.mbs[idx].t8 = t8;
        }
        if cbp != 0 {
            self.read_qp_delta()?;
            self.residual(false)?;
        } else {
            self.last_dqp = false;
        }
        self.inter_pred();
        self.add_residual_inter();
        Ok(())
    }

    fn read_t8_flag(&mut self) -> bool {
        let a = self.mb_a().is_some_and(|m| self.mbs[m].t8);
        let b = self.nb_b.is_some_and(|m| self.mbs[m].t8);
        self.bin(399 + a as usize + b as usize)
    }

    fn read_qp_delta(&mut self) -> Result<(), DecodeError> {
        let range = 52 + self.qp_off;
        let delta = if self.pps.entropy_coding_mode_flag {
            let mut ctx = 60 + self.last_dqp as usize;
            let mut k = 0;
            while self.bin(ctx) {
                ctx = if k == 0 { 62 } else { 63 };
                k += 1;
                if k > 2 * range {
                    return Err(BAD);
                }
            }
            if k & 1 != 0 { (k + 1) / 2 } else { -(k / 2) }
        } else {
            self.br.read_se().map_err(|_| BAD)?
        };
        if delta < -(range / 2) - 1 || delta > range / 2 {
            return Err(BAD);
        }
        self.last_dqp = delta != 0;
        self.qp_prev = (self.qp_prev + delta + 2 * range + self.qp_off).rem_euclid(range) - self.qp_off;
        self.set_qp();
        Ok(())
    }

    /// Stores the macroblock's QPs for deblocking.
    fn set_qp(&mut self) {
        let qp = self.qp_prev;
        let qpc = [
            self.qpc_of(qp, self.pps.chroma_qp_index_offset),
            self.qpc_of(qp, self.pps.second_chroma_qp_index_offset),
        ];
        let mb = &mut self.mbs[self.idx];
        mb.qp = qp as i8;
        mb.qpc = qpc;
    }

    fn read_cbp(&mut self, intra: bool) -> Result<u8, DecodeError> {
        if !self.pps.entropy_coding_mode_flag {
            let code = self.br.read_ue().map_err(|_| BAD)? as usize;
            return if self.cfi == 0 {
                // Table 9-4 for no chroma: a mapping of its own.
                const INTRA: [u8; 16] = [15, 0, 7, 11, 13, 14, 3, 5, 10, 12, 1, 2, 4, 8, 6, 9];
                const INTER: [u8; 16] = [0, 1, 2, 4, 8, 3, 5, 10, 12, 15, 7, 11, 13, 14, 6, 9];
                let table = if intra { &INTRA } else { &INTER };
                table.get(code).copied().ok_or(BAD)
            } else {
                let table = if intra { &CBP_INTRA_TABLE } else { &CBP_INTER_TABLE };
                table.get(code).copied().ok_or(BAD)
            };
        }
        // Luma, block by block, each seeing the bins before it.
        let mut luma = 0u8;
        for b8 in 0..4usize {
            let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
            // A bit counts as "coded" (no increment) if there's no macroblock, or it's
            // I_PCM, or the bit is set; only skips and clear bits add.
            let a = if bx > 0 {
                (luma & (1 << (b8 - 1))) == 0
            } else {
                match self.left(by as i32 * 4, 16) {
                    None => false,
                    Some((m, row)) => {
                        let n = &self.mbs[m];
                        n.kind != PCM && (n.skip || n.cbp & (1 << ((row as usize / 8) * 2 + 1)) == 0)
                    }
                }
            };
            let b = if by > 0 {
                (luma & (1 << (b8 - 2))) == 0
            } else {
                match self.nb_b {
                    None => false,
                    Some(m) => {
                        let n = &self.mbs[m];
                        n.kind != PCM && (n.skip || n.cbp & (1 << (2 + bx / 2)) == 0)
                    }
                }
            };
            if self.bin(73 + a as usize + 2 * b as usize) {
                luma |= 1 << b8;
            }
        }
        let mut chroma = 0u8;
        if self.cfi != 0 {
            let of = |n: &MbInfo| -> u8 {
                if n.kind == PCM {
                    2
                } else if n.skip {
                    0
                } else {
                    n.cbp >> 4
                }
            };
            let ca = self.mb_a().map_or(0, |m| of(&self.mbs[m]));
            let cb = self.nb_b.map_or(0, |m| of(&self.mbs[m]));
            if self.bin(77 + (ca != 0) as usize + 2 * (cb != 0) as usize) {
                chroma = 1 + self.bin(77 + 4 + (ca == 2) as usize + 2 * (cb == 2) as usize) as u8;
            }
        }
        Ok(luma | (chroma << 4))
    }

    /// Raw reference index of a block position outside or inside this macroblock.
    fn ref_cond(&self, list: usize, bx: i32, by: i32) -> bool {
        let (mb, x, y) = if bx < 0 {
            match self.left(by * 4, 16) {
                None => return false,
                Some((m, row)) => (m, 3, (row / 4) as usize),
            }
        } else if by < 0 {
            match self.nb_b {
                None => return false,
                Some(m) => (m, bx as usize, 3),
            }
        } else {
            (self.idx, bx as usize, by as usize)
        };
        let n = &self.mbs[mb];
        let b8 = (y / 2) * 2 + x / 2;
        if n.intra() || n.skip || n.direct8 & (1 << b8) != 0 {
            return false;
        }
        let r = self.motion[mb].ref_idx[list][b8];
        if self.mbaff && !self.field && n.field {
            r > 1
        } else {
            r > 0
        }
    }

    fn read_ref_idx(&mut self, list: usize, bx: usize, by: usize) -> Result<i8, DecodeError> {
        let active = if list == 0 {
            self.h.num_ref_idx_l0_active
        } else {
            self.h.num_ref_idx_l1_active
        } as i32;
        let limit = if self.mbaff && self.field { active * 2 } else { active };
        if limit == 1 {
            return Ok(0);
        }
        let r = if self.pps.entropy_coding_mode_flag {
            let a = self.ref_cond(list, bx as i32 - 1, by as i32);
            let b = self.ref_cond(list, bx as i32, by as i32 - 1);
            let mut ctx = 54 + a as usize + 2 * b as usize;
            let mut r = 0;
            while self.bin(ctx) {
                ctx = if r == 0 { 54 + 4 } else { 54 + 5 };
                r += 1;
                if r > 64 {
                    return Err(BAD);
                }
            }
            r
        } else if limit == 2 {
            1 - self.br.read_bit().map_err(|_| BAD)? as i32
        } else {
            let v = self.br.read_ue().map_err(|_| BAD)?;
            if v > 64 {
                return Err(BAD);
            }
            v as i32
        };
        if r >= limit {
            return Err(BAD);
        }
        Ok(r as i8)
    }

    /// Neighbouring |mvd| for the context of a component.
    fn mvd_at(&self, list: usize, bx: i32, by: i32, comp: usize) -> i32 {
        let (mb, x, y) = if bx < 0 {
            match self.left(by * 4, 16) {
                None => return 0,
                Some((m, row)) => (m, 3usize, (row / 4) as usize),
            }
        } else if by < 0 {
            match self.nb_b {
                None => return 0,
                Some(m) => (m, bx as usize, 3),
            }
        } else {
            (self.idx, bx as usize, by as usize)
        };
        let n = &self.mbs[mb];
        let v = n.mvd[list][y * 4 + x][comp] as i32;
        if comp == 1 && self.mbaff && n.field != self.field {
            if self.field { v / 2 } else { v * 2 }
        } else {
            v
        }
    }

    fn read_mvd(&mut self, list: usize, bx: usize, by: usize, comp: usize) -> Result<i32, DecodeError> {
        if !self.pps.entropy_coding_mode_flag {
            return self.br.read_se().map_err(|_| BAD);
        }
        let sum = self.mvd_at(list, bx as i32 - 1, by as i32, comp)
            + self.mvd_at(list, bx as i32, by as i32 - 1, comp);
        let base = if comp == 0 { 40 } else { 47 };
        let inc = if sum < 3 {
            0
        } else if sum > 32 {
            2
        } else {
            1
        };
        if !self.bin(base + inc) {
            return Ok(0);
        }
        let mut value = 1;
        let mut ctx = 3;
        while value < 9 && self.bin(base + ctx) {
            value += 1;
            if ctx < 6 {
                ctx += 1;
            }
        }
        if value >= 9 {
            let mut k = 3;
            while self.bypass() {
                value += 1 << k;
                k += 1;
                if k > 24 {
                    return Err(BAD);
                }
            }
            while k > 0 {
                k -= 1;
                value += (self.bypass() as i32) << k;
            }
        }
        Ok(if self.bypass() { -value } else { value })
    }

    /// Reads a partition's motion vector difference and stores the motion vector.
    #[allow(clippy::too_many_arguments)]
    fn read_mv(
        &mut self,
        list: usize,
        bx: usize,
        by: usize,
        bw: usize,
        bh: usize,
        r: i8,
        shape: u8,
    ) -> Result<(), DecodeError> {
        let pred = self.predict_mv(list, bx, by, bw, r, shape);
        let dx = self.read_mvd(list, bx, by, 0)?;
        let dy = self.read_mvd(list, bx, by, 1)?;
        let mv = [
            (pred[0] as i32 + dx).clamp(-32768, 32767) as i16,
            (pred[1] as i32 + dy).clamp(-32768, 32767) as i16,
        ];
        let idx = self.idx;
        let stored = [dx.unsigned_abs().min(127) as u8, dy.unsigned_abs().min(127) as u8];
        for y in by..by + bh {
            for x in bx..bx + bw {
                self.motion[idx].mv[list][y * 4 + x] = mv;
                self.mbs[idx].mvd[list][y * 4 + x] = stored;
            }
        }
        Ok(())
    }

    fn intra_mb(&mut self, t: u32) -> Result<(), DecodeError> {
        let idx = self.idx;
        self.motion[idx].intra = true;
        if t > 25 {
            return Err(BAD);
        }
        if t == 25 {
            return self.pcm_mb();
        }
        let cabac = self.pps.entropy_coding_mode_flag;
        let mut cbp;
        if t == 0 {
            let mut t8 = false;
            if self.pps.transform_8x8_mode_flag {
                t8 = if cabac {
                    self.read_t8_flag()
                } else {
                    self.br.read_bit().map_err(|_| BAD)? != 0
                };
            }
            self.mbs[idx].kind = if t8 { I8 } else { I4 };
            self.mbs[idx].t8 = t8;
            let count = if t8 { 4 } else { 16 };
            for i in 0..count {
                let blk = if t8 { i * 4 } else { i };
                let (bx, by) = (BLK_X[blk] as usize, BLK_Y[blk] as usize);
                let pred = self.pred_intra_mode(bx, by, t8);
                let mode = if cabac {
                    if self.bin(68) {
                        pred
                    } else {
                        let v = self.bin(69) as i8
                            | (self.bin(69) as i8) << 1
                            | (self.bin(69) as i8) << 2;
                        if v < pred { v } else { v + 1 }
                    }
                } else if self.br.read_bit().map_err(|_| BAD)? != 0 {
                    pred
                } else {
                    let v = self.br.read_bits(3).map_err(|_| BAD)? as i8;
                    if v < pred { v } else { v + 1 }
                };
                let m = &mut self.mbs[idx].modes;
                if t8 {
                    m[by * 4 + bx] = mode;
                    m[by * 4 + bx + 1] = mode;
                    m[by * 4 + 4 + bx] = mode;
                    m[by * 4 + 4 + bx + 1] = mode;
                } else {
                    m[by * 4 + bx] = mode;
                }
            }
            cbp = 0;
        } else {
            self.mbs[idx].kind = I16;
            self.i16_mode = ((t - 1) & 3) as u8;
            cbp = ((((t - 1) / 4) % 3) as u8) << 4;
            if t >= 13 {
                cbp |= 15;
            }
        }
        if self.cfi != 0 {
            let mode = if cabac {
                let of = |n: &MbInfo| n.intra() && n.kind != PCM && n.chroma_mode != 0;
                let a = self.mb_a().is_some_and(|m| of(&self.mbs[m]));
                let b = self.nb_b.is_some_and(|m| of(&self.mbs[m]));
                if !self.bin(64 + a as usize + b as usize) {
                    0
                } else if !self.bin(67) {
                    1
                } else if !self.bin(67) {
                    2
                } else {
                    3
                }
            } else {
                let v = self.br.read_ue().map_err(|_| BAD)?;
                if v > 3 {
                    return Err(BAD);
                }
                v as u8
            };
            self.mbs[idx].chroma_mode = mode;
        }
        if t == 0 {
            cbp = self.read_cbp(true)?;
        }
        self.mbs[idx].cbp = cbp;
        if cbp != 0 || t != 0 {
            self.read_qp_delta()?;
            self.residual(true)?;
        } else {
            self.last_dqp = false;
        }
        self.recon_intra();
        Ok(())
    }

    /// Predicted intra mode of a 4x4 or 8x8 block (8.3.1.1, 8.3.2.1).
    fn pred_intra_mode(&self, bx: usize, by: usize, t8: bool) -> i8 {
        // None: the neighbour forces DC. Otherwise its mode, 2 if it isn't NxN.
        let idx = self.idx;
        let a: Option<i8> = if bx > 0 {
            Some(self.mbs[idx].modes[by * 4 + bx - 1])
        } else {
            self.left(by as i32 * 4, 16).and_then(|(m, row)| {
                let n = &self.mbs[m];
                if !self.intra_usable(m) {
                    None
                } else if n.kind == I4 || n.kind == I8 {
                    let row = row as usize / 4;
                    // An I8 here reads a 4x4 neighbour's upper right of its 8x8 block,
                    // but a lower one for a frame macroblock's third block beside a
                    // field pair (8.3.2.1).
                    let row = if t8 && n.kind == I4 {
                        let lower = self.mbaff && !self.field && n.field && by == 2;
                        (row & !1) + lower as usize
                    } else {
                        row
                    };
                    Some(n.modes[row * 4 + 3])
                } else {
                    Some(2)
                }
            })
        };
        let b: Option<i8> = if by > 0 {
            Some(self.mbs[idx].modes[(by - 1) * 4 + bx])
        } else {
            self.nb_b.and_then(|m| {
                let n = &self.mbs[m];
                if !self.intra_usable(m) {
                    None
                } else if n.kind == I4 || n.kind == I8 {
                    Some(n.modes[12 + bx])
                } else {
                    Some(2)
                }
            })
        };
        match (a, b) {
            (Some(a), Some(b)) => a.min(b),
            _ => 2,
        }
    }

    fn pcm_mb(&mut self) -> Result<(), DecodeError> {
        let idx = self.idx;
        let mut bit = if let Some(cab) = &self.cab {
            cab.pcm_start() * 8
        } else {
            self.br.align_to_byte();
            self.br.position().0 * 8
        };
        let data = self.rbsp;
        let depth = self.depth as usize;
        let chroma_samples = if self.cfi == 0 { 0 } else { 2 * 8 * self.mch };
        let total = (256 + chroma_samples) * depth;
        if bit + total > data.len() * 8 {
            return Err(BAD);
        }
        let mut read = || {
            let mut v = 0i32;
            for _ in 0..depth {
                v = (v << 1) | ((data[bit >> 3] >> (7 - (bit & 7))) & 1) as i32;
                bit += 1;
            }
            P::new(v)
        };
        for y in 0..16 {
            for x in 0..16 {
                self.planes[0][self.ly + y * self.ls + x] = read();
            }
        }
        if self.cfi != 0 {
            for plane in 1..3 {
                for y in 0..self.mch {
                    for x in 0..8 {
                        self.planes[plane][self.lc + y * self.cs + x] = read();
                    }
                }
            }
        }
        let end = bit.div_ceil(8);
        if let Some(cab) = &mut self.cab {
            cab.restart(end);
        } else {
            // The bit reader has no seek: skip what was read.
            while self.br.position().0 < end {
                let _ = self.br.read_bits(8);
            }
        }
        let qp0 = [self.qpc_of(0, self.pps.chroma_qp_index_offset), self.qpc_of(0, self.pps.second_chroma_qp_index_offset)];
        let mb = &mut self.mbs[idx];
        mb.kind = PCM;
        mb.nz = [16; 32];
        mb.cbp = 0x2f;
        mb.cbf_dc = 7;
        mb.mask = 0xffff;
        mb.qp = 0;
        mb.qpc = qp0;
        self.last_dqp = false;
        Ok(())
    }

    // ----- Residual -----

    /// Non-zero count of a luma or chroma block's neighbour, `None` if unavailable.
    #[inline]
    fn nz_a(&self, plane: usize, bx: usize, by: usize) -> Option<u8> {
        if plane == 0 {
            if bx > 0 {
                Some(self.mbs[self.idx].nz[by * 4 + bx - 1])
            } else {
                self.left(by as i32 * 4, 16)
                    .map(|(m, row)| self.mbs[m].nz[(row as usize / 4) * 4 + 3])
            }
        } else {
            let base = 8 + 8 * plane;
            if bx > 0 {
                Some(self.mbs[self.idx].nz[base + by * 2 + bx - 1])
            } else {
                self.left(by as i32 * 4, self.mch as i32)
                    .map(|(m, row)| self.mbs[m].nz[base + (row as usize / 4) * 2 + 1])
            }
        }
    }

    #[inline]
    fn nz_b(&self, plane: usize, bx: usize, by: usize) -> Option<u8> {
        if plane == 0 {
            if by > 0 {
                Some(self.mbs[self.idx].nz[(by - 1) * 4 + bx])
            } else {
                self.nb_b.map(|m| self.mbs[m].nz[12 + bx])
            }
        } else {
            let base = 8 + 8 * plane;
            if by > 0 {
                Some(self.mbs[self.idx].nz[base + (by - 1) * 2 + bx])
            } else {
                self.nb_b
                    .map(|m| self.mbs[m].nz[base + (self.mch / 4 - 1) * 2 + bx])
            }
        }
    }

    /// nC of 9.2.1.
    #[inline]
    fn nc(&self, plane: usize, bx: usize, by: usize) -> i32 {
        match (self.nz_a(plane, bx, by), self.nz_b(plane, bx, by)) {
            (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
            (Some(a), None) => a as i32,
            (None, Some(b)) => b as i32,
            _ => 0,
        }
    }

    /// Context increment of coded_block_flag from neighbours' non-zero counts.
    #[inline]
    fn cbf_inc(&self, plane: usize, bx: usize, by: usize) -> usize {
        let default = self.mbs[self.idx].intra();
        let a = self.nz_a(plane, bx, by).map_or(default, |n| n != 0);
        let b = self.nz_b(plane, bx, by).map_or(default, |n| n != 0);
        a as usize + 2 * b as usize
    }

    /// Context increment for a DC block's coded_block_flag; `bit` picks luma, Cb or Cr.
    fn cbf_inc_dc(&self, bit: u8) -> usize {
        let default = self.mbs[self.idx].intra();
        let of = |m: usize| self.mbs[m].cbf_dc & (1 << bit) != 0;
        let a = self.mb_a().map_or(default, of);
        let b = self.nb_b.map_or(default, of);
        a as usize + 2 * b as usize
    }

    /// Reads one CABAC block into `lev_pos` (positions in the block's list) and `lev_val`,
    /// highest position first. Returns the count; the coded_block_flag must have been
    /// read already, except for 8x8 blocks, which have none.
    fn read_block_cabac(&mut self, cat: usize, max: usize) -> Result<usize, DecodeError> {
        const SIG_OFF: [usize; 6] = [0, 15, 29, 44, 47, 0];
        const ABS_OFF: [usize; 6] = [0, 10, 20, 30, 39, 0];
        let (sig, last, abs) = if cat == 5 {
            if self.field {
                (436, 451, 426)
            } else {
                (402, 417, 426)
            }
        } else if self.field {
            (277 + SIG_OFF[cat], 338 + SIG_OFF[cat], 227 + ABS_OFF[cat])
        } else {
            (105 + SIG_OFF[cat], 166 + SIG_OFF[cat], 227 + ABS_OFF[cat])
        };
        let num_c8 = if self.mch == 16 { 2 } else { 1 };
        let mut count = 0;
        let mut positions = [0u8; 64];
        let mut i = 0;
        while i < max - 1 {
            let (si, li) = match cat {
                3 => {
                    let k = (i / num_c8).min(2);
                    (k, k)
                }
                5 => (
                    if self.field {
                        SIGNIFICANT_COEFF_FLAG_FIELD_OFFSET_8X8[i] as usize
                    } else {
                        SIGNIFICANT_COEFF_FLAG_OFFSET_8X8[i] as usize
                    },
                    LAST_COEFF_FLAG_OFFSET_8X8[i] as usize,
                ),
                _ => (i, i),
            };
            if self.bin(sig + si) {
                positions[count] = i as u8;
                count += 1;
                if self.bin(last + li) {
                    break;
                }
            }
            i += 1;
        }
        if i == max - 1 {
            positions[count] = i as u8;
            count += 1;
        }
        let (mut eq1, mut gt1) = (0usize, 0usize);
        let gt_limit = 4 - (cat == 3) as usize;
        for n in (0..count).rev() {
            let ctx = abs + if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let mut level = 1i32;
            if self.bin(ctx) {
                let ctx = abs + 5 + gt1.min(gt_limit);
                level = 2;
                while level < 15 && self.bin(ctx) {
                    level += 1;
                }
                if level >= 15 {
                    let mut k = 0;
                    while self.bypass() {
                        level += 1 << k;
                        k += 1;
                        if k > 24 {
                            return Err(BAD);
                        }
                    }
                    while k > 0 {
                        k -= 1;
                        level += (self.bypass() as i32) << k;
                    }
                }
                gt1 += 1;
            } else {
                eq1 += 1;
            }
            let out = count - 1 - n;
            self.lev_pos[out] = positions[n];
            self.lev_val[out] = if self.bypass() { -level } else { level };
        }
        Ok(count)
    }

    /// Reads one CAVLC block the same way. Returns the count, or total_coeff.
    fn read_block_cavlc(&mut self, nc: i32, max: usize) -> Result<usize, DecodeError> {
        let mut coeffs = [0i32; 16];
        let total = parse_residual_block_cavlc(&mut self.br, &mut coeffs[..max], max, nc)
            .map_err(DecodeError::InvalidSyntax)? as usize;
        let mut count = 0;
        if total != 0 {
            for i in (0..max).rev() {
                if coeffs[i] != 0 {
                    self.lev_pos[count] = i as u8;
                    self.lev_val[count] = coeffs[i];
                    count += 1;
                }
            }
        }
        self.cavlc_count = count;
        Ok(total)
    }

    #[inline(always)]
    fn scale4(value: i32, scale: i32, qp: i32) -> i32 {
        let product = value as i64 * scale as i64;
        let d = if qp >= 24 {
            product << (qp / 6 - 4)
        } else {
            (product + (1i64 << (3 - qp / 6))) >> (4 - qp / 6)
        };
        d.clamp(-(1 << 21), 1 << 21) as i32
    }

    #[inline(always)]
    fn scale8(value: i32, scale: i32, qp: i32) -> i32 {
        let product = value as i64 * scale as i64;
        let d = if qp >= 36 {
            product << (qp / 6 - 6)
        } else {
            (product + (1i64 << (5 - qp / 6))) >> (6 - qp / 6)
        };
        d.clamp(-(1 << 21), 1 << 21) as i32
    }

    /// residual(): levels are scaled as they're read.
    fn residual(&mut self, intra: bool) -> Result<(), DecodeError> {
        let cabac = self.pps.entropy_coding_mode_flag;
        let idx = self.idx;
        let kind = self.mbs[idx].kind;
        let t8 = self.mbs[idx].t8;
        let cbp = self.mbs[idx].cbp;
        let qp = self.qp_prev + self.qp_off;
        let field = self.field;
        let scan4: &[u8; 16] = if field { &FIELD4 } else { &ZIG4 };
        let scan8: &[u8; 64] = if field { &FIELD8 } else { &ZIG8 };
        let list4 = if intra { 0 } else { 3 };
        let scales = self.scales;
        let scale = &scales.s4[list4][(qp % 6) as usize];
        self.luma_any = 0;
        self.chroma_any = [0; 2];
        if kind == I16 {
            let mut count = 0;
            if cabac {
                let inc = self.cbf_inc_dc(0);
                if self.bin(85 + inc) {
                    count = self.read_block_cabac(0, 16)?;
                    self.mbs[idx].cbf_dc |= 1;
                }
            } else {
                let nc = self.nc(0, 0, 0);
                let _ = self.read_block_cavlc(nc, 16)?;
                count = self.cavlc_count;
            }
            if count != 0 {
                let mut dc = [0i32; 16];
                for n in 0..count {
                    let pos = scan4[self.lev_pos[n] as usize] as usize;
                    dc[pos] = self.lev_val[n];
                }
                super::xform::hadamard4(&mut dc);
                let ls = scale[0];
                for (blk, &value) in dc.iter().enumerate() {
                    // Fixed like 8x8's: qP 36 is the break.
                    let d = Self::scale8(value, ls, qp);
                    let order = BLK_ORDER[blk] as usize;
                    self.coef[order * 16] = d;
                    if d != 0 {
                        self.luma_any |= 1 << order;
                    }
                }
            }
        }
        for b8 in 0..4usize {
            if cbp & (1 << b8) == 0 {
                continue;
            }
            if t8 {
                let list8 = if intra { 0 } else { 1 };
                let s8 = &scales.s8[list8][(qp % 6) as usize];
                let base = b8 * 64;
                let (x0, y0) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                if cabac {
                    let count = self.read_block_cabac(5, 64)?;
                    for n in 0..count {
                        let at = scan8[self.lev_pos[n] as usize] as usize;
                        self.coef[base + at] = Self::scale8(self.lev_val[n], s8[at], qp);
                    }
                    if count != 0 {
                        self.luma_any |= 0xf << (b8 * 4);
                    }
                    let mb = &mut self.mbs[idx];
                    for y in y0..y0 + 2 {
                        for x in x0..x0 + 2 {
                            mb.nz[y * 4 + x] = count as u8;
                        }
                    }
                    if count != 0 {
                        mb.mask |= 0x33 << (y0 * 4 + x0);
                    }
                } else {
                    let mut any = false;
                    for i in 0..4 {
                        let (bx, by) = (x0 + (i & 1), y0 + (i >> 1));
                        let nc = self.nc(0, bx, by);
                        let total = self.read_block_cavlc(nc, 16)?;
                        let count = self.cavlc_count;
                        for n in 0..count {
                            let at = scan8[self.lev_pos[n] as usize * 4 + i] as usize;
                            self.coef[base + at] = Self::scale8(self.lev_val[n], s8[at], qp);
                        }
                        self.mbs[idx].nz[by * 4 + bx] = total as u8;
                        any |= count != 0;
                    }
                    if any {
                        self.luma_any |= 0xf << (b8 * 4);
                        self.mbs[idx].mask |= 0x33 << (y0 * 4 + x0);
                    }
                }
            } else {
                for i in 0..4 {
                    let blk = b8 * 4 + i;
                    let (bx, by) = (BLK_X[blk] as usize, BLK_Y[blk] as usize);
                    let (cat, first, max) = if kind == I16 { (1, 1, 15) } else { (2, 0, 16) };
                    let mut count = 0;
                    if cabac {
                        let inc = self.cbf_inc(0, bx, by);
                        let base = 85 + if kind == I16 { 4 } else { 8 };
                        if self.bin(base + inc) {
                            count = self.read_block_cabac(cat, max)?;
                        }
                        self.mbs[idx].nz[by * 4 + bx] = count as u8;
                    } else {
                        let nc = self.nc(0, bx, by);
                        let total = self.read_block_cavlc(nc, max)?;
                        count = self.cavlc_count;
                        self.mbs[idx].nz[by * 4 + bx] = total as u8;
                    }
                    if count != 0 {
                        let base = blk * 16;
                        for n in 0..count {
                            let at = scan4[self.lev_pos[n] as usize + first] as usize;
                            self.coef[base + at] = Self::scale4(self.lev_val[n], scale[at], qp);
                        }
                        self.luma_any |= 1 << blk;
                        self.mbs[idx].mask |= 1 << (by * 4 + bx);
                    }
                }
            }
        }
        if self.cfi != 0 && cbp >> 4 != 0 {
            self.residual_chroma(intra, cbp >> 4, cabac)?;
        }
        Ok(())
    }

    fn residual_chroma(&mut self, intra: bool, chroma: u8, cabac: bool) -> Result<(), DecodeError> {
        let idx = self.idx;
        let field = self.field;
        let scan4: &[u8; 16] = if field { &FIELD4 } else { &ZIG4 };
        let scales = self.scales;
        let blocks = self.mch / 2; // 4 or 8 blocks per plane
        let offsets = [
            self.pps.chroma_qp_index_offset,
            self.pps.second_chroma_qp_index_offset,
        ];
        for plane in 0..2usize {
            let mut count = 0;
            if cabac {
                let inc = self.cbf_inc_dc(1 + plane as u8);
                if self.bin(85 + 12 + inc) {
                    count = self.read_block_cabac(3, blocks)?;
                    self.mbs[idx].cbf_dc |= 2 << plane;
                }
            } else {
                let nc = if blocks == 8 { -2 } else { -1 };
                let _ = self.read_block_cavlc(nc, blocks)?;
                count = self.cavlc_count;
            }
            if count == 0 {
                continue;
            }
            let qpc = self.qpc_of(self.qp_prev, offsets[plane]) as i32 + self.qp_off;
            let list = (if intra { 1 } else { 4 }) + plane;
            let mut dc = [0i32; 8];
            for n in 0..count {
                dc[self.lev_pos[n] as usize] = self.lev_val[n];
            }
            if blocks == 4 {
                super::xform::chroma_dc_420(&mut dc);
                let ls = scales.s4[list][(qpc % 6) as usize][0];
                for b in 0..4 {
                    let d = ((dc[b] as i64 * ls as i64) << (qpc / 6)) >> 5;
                    let d = d.clamp(-(1 << 21), 1 << 21) as i32;
                    self.ccoef[plane][b * 16] = d;
                    if d != 0 {
                        self.chroma_any[plane] |= 1 << b;
                    }
                }
            } else {
                super::xform::chroma_dc_422(&mut dc);
                let qp_dc = qpc + 3;
                let ls = scales.s4[list][(qp_dc % 6) as usize][0];
                for b in 0..8 {
                    let d = Self::scale8(dc[b], ls, qp_dc);
                    self.ccoef[plane][b * 16] = d;
                    if d != 0 {
                        self.chroma_any[plane] |= 1 << b;
                    }
                }
            }
        }
        if chroma & 2 != 0 {
            for plane in 0..2usize {
                let qpc = self.qpc_of(self.qp_prev, offsets[plane]) as i32 + self.qp_off;
                let list = (if intra { 1 } else { 4 }) + plane;
                let scale = &scales.s4[list][(qpc % 6) as usize];
                for b in 0..blocks {
                    let (bx, by) = (b & 1, b >> 1);
                    let nz_at = 16 + plane * 8 + b;
                    let mut count = 0;
                    if cabac {
                        let inc = self.cbf_inc(plane + 1, bx, by);
                        if self.bin(85 + 16 + inc) {
                            count = self.read_block_cabac(4, 15)?;
                        }
                        self.mbs[idx].nz[nz_at] = count as u8;
                    } else {
                        let nc = self.nc(plane + 1, bx, by);
                        let total = self.read_block_cavlc(nc, 15)?;
                        count = self.cavlc_count;
                        self.mbs[idx].nz[nz_at] = total as u8;
                    }
                    if count != 0 {
                        for n in 0..count {
                            let at = scan4[self.lev_pos[n] as usize + 1] as usize;
                            self.ccoef[plane][b * 16 + at] =
                                Self::scale4(self.lev_val[n], scale[at], qpc);
                        }
                        self.chroma_any[plane] |= 1 << b;
                    }
                }
            }
        }
        Ok(())
    }
}

