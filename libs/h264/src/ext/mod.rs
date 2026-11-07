//! The decoder core: frames, field pictures and MBAFF, up to 10 bits, with monochrome,
//! 4:2:0 and 4:2:2 chroma. It's written for all of these from the start, so each
//! macroblock path has one implementation, generic over the sample type. It's our own
//! work from the specification, and shares only parameter set parsing, CAVLC tables and
//! spec tables with the vendored decoder. See `libs/h264/UPSTREAM.md`.

mod cabac;
mod deblock;
mod dpb;
mod intra;
mod mb;
mod mc;
mod motion;
mod parse;
mod pixel;
mod recon;
mod xform;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::bitstream::BitstreamReader;
use crate::decoder::{Frame, Limits, NativeFrame};
use crate::error::DecodeError;
use crate::nal::{NalUnit, NalUnitType};
use crate::pps::Pps;
use crate::slice::{SliceHeader, SliceType, parse_slice_header};
use crate::sps::Sps;

use self::cabac::{CONTEXTS, Cabac, init_contexts};
use self::deblock::Deblock;
use self::dpb::{BOTTOM, Dpb, FRAME, MbMotion, PicData, PocResult, Store, TOP};
use self::mb::{MbInfo, Scales, Sd};
use self::pixel::Pixel;

/// Hands 8-bit progressive 4:2:0 streams to the vendored macroblock paths instead of this
/// core. It's off because those paths differ from FFmpeg on 20 of the FATE conformance
/// streams that this core matches, and this core isn't slower. See `UPSTREAM.md`.
const VENDORED_FOR_PROGRESSIVE: bool = false;

/// Whether this stream needs this core rather than the vendored one.
pub(crate) fn wanted(sps: &Sps) -> bool {
    !VENDORED_FOR_PROGRESSIVE
        || sps.chroma_format_idc != 1
        || sps.bit_depth_luma_minus8 != 0
        || sps.bit_depth_chroma_minus8 != 0
        || !sps.frame_mbs_only_flag
}

/// Fallible allocation of a filled vector.
fn try_vec<T: Clone>(value: T, len: usize) -> Result<Vec<T>, DecodeError> {
    let mut items = Vec::new();
    items
        .try_reserve_exact(len)
        .map_err(|_| DecodeError::OutOfMemory)?;
    items.resize(len, value);
    Ok(items)
}

/// Estimates the memory a stream needs on this core. Pictures that aren't 8-bit 4:2:0
/// also take converted planes while they wait for display.
pub(crate) fn stream_memory(sps: &Sps) -> usize {
    let mbs = sps.mb_width() as usize * sps.mb_height() as usize;
    let chroma = match sps.chroma_format_idc {
        0 => 0,
        1 => 128,
        _ => 256,
    };
    let sample = if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
        2
    } else {
        1
    };
    let plane = (256 + chroma) * sample;
    let display = if plane == 384 { 384 } else { 384 + plane };
    let refs = sps.max_num_ref_frames as usize + 1;
    let queued = sps.reorder_depth() + 1;
    let per_mb = refs * (plane + core::mem::size_of::<MbMotion>())
        + queued * display
        + core::mem::size_of::<MbInfo>();
    mbs.saturating_mul(per_mb)
}

/// The picture in progress: a frame, or the fields of one.
struct Cur<P> {
    planes: [Vec<P>; 3],
    motion: Vec<MbMotion>,
    /// Identity of the picture within a frame: the fields of this frame so far.
    structure: u8,
    frame_num: u32,
    nal_ref_idc: u8,
    idr: bool,
    idr_pic_id: Option<u32>,
    pps_id: u32,
    lsb: u32,
    delta_bottom: i32,
    delta0: [i32; 2],
    poc: PocResult,
    long_term_flag: bool,
    mmco: Vec<(u32, u32)>,
    tag: u64,
    width: u32,
    height: u32,
    crop_x: u32,
    crop_y: u32,
    /// `poc_reset` for the finished frame.
    reset: bool,
    any_idr: bool,
    mbaff: bool,
}

/// Decoder state over one sample type.
struct Core<P: Pixel> {
    dpb: Dpb<P>,
    cur: Option<Cur<P>>,
    /// Where the current frame's store is in the DPB.
    mbs: Vec<MbInfo>,
    preds: Box<mb::Preds<P>>,
    /// Motion storage of frames no longer in use.
    spare_motion: Vec<Vec<MbMotion>>,
    scales: Box<Scales>,
    scales_for: Option<u32>,
    slice_id: u32,
    w: usize,
    h: usize,
    cfi: u32,
    depth: u32,
    mch: usize,
    interlaced: bool,
    output: VecDeque<Frame>,
    keep_native: bool,
    frame_mbs_only: bool,
    prev_had_mmco5: bool,
    /// Samples and motion of the frame between its fields.
    frame_planes: Option<([Vec<P>; 3], Vec<MbMotion>)>,
    first_tag: u64,
    first_reset: bool,
    first_idr: bool,
    meta: Option<(u32, u32, u32, u32)>,
    concealed: u32,
    out_of_memory: bool,
}

impl<P: Pixel> Core<P> {
    fn new(sps: &Sps, keep_native: bool) -> Result<Self, DecodeError> {
        let (w, h) = (sps.mb_width() as usize, sps.mb_height() as usize);
        Ok(Self {
            dpb: Dpb::new(),
            cur: None,
            mbs: try_vec(MbInfo::default(), w * h)?,
            preds: Box::new([[[P::default(); 256]; 3]; 2]),
            spare_motion: Vec::new(),
            scales: Box::new(Scales::new()),
            scales_for: None,
            slice_id: 0,
            w,
            h,
            cfi: sps.chroma_format_idc,
            depth: sps.bit_depth_luma_minus8 + 8,
            mch: match sps.chroma_format_idc {
                0 => 0,
                1 => 8,
                _ => 16,
            },
            interlaced: !sps.frame_mbs_only_flag,
            output: VecDeque::new(),
            keep_native,
            frame_mbs_only: sps.frame_mbs_only_flag,
            prev_had_mmco5: false,
            frame_planes: None,
            first_tag: 0,
            first_reset: false,
            first_idr: false,
            meta: None,
            concealed: 0,
            out_of_memory: false,
        })
    }

    fn matches(&self, sps: &Sps) -> bool {
        self.w == sps.mb_width() as usize
            && self.h == sps.mb_height() as usize
            && self.cfi == sps.chroma_format_idc
            && self.depth == sps.bit_depth_luma_minus8 + 8
            && self.frame_mbs_only == sps.frame_mbs_only_flag
    }

    /// Whether this slice belongs to a new picture (7.4.1.2.4).
    fn starts_picture(&self, h: &SliceHeader, nal: &NalUnit, pps_id: u32, structure: u8) -> bool {
        let Some(cur) = &self.cur else {
            return true;
        };
        let idr = nal.nal_unit_type == NalUnitType::SliceIdr;
        cur.structure != structure
            || cur.frame_num != h.frame_num
            || cur.pps_id != pps_id
            || (cur.nal_ref_idc == 0) != (nal.nal_ref_idc == 0)
            || cur.lsb != h.pic_order_cnt_lsb
            || cur.delta_bottom != h.delta_pic_order_cnt_bottom
            || cur.delta0 != h.delta_pic_order_cnt
            || cur.idr != idr
            || (idr && cur.idr_pic_id != h.idr_pic_id)
    }

    fn decode_slice(
        &mut self,
        nal: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        tag: u64,
    ) -> Result<(), DecodeError> {
        let (h, reader) =
            parse_slice_header(&nal.rbsp, sps, pps, nal.nal_unit_type, nal.nal_ref_idc)?;
        if !matches!(h.slice_type, SliceType::I | SliceType::P | SliceType::B) {
            return Err(DecodeError::Unsupported("SP and SI slices not supported"));
        }
        let structure = if !h.field_pic_flag {
            FRAME
        } else if h.bottom_field_flag {
            BOTTOM
        } else {
            TOP
        };
        if self.starts_picture(&h, nal, pps.pic_parameter_set_id, structure) {
            self.finish_picture(sps);
            self.start_picture(&h, nal, sps, pps, structure, tag)?;
        }
        self.run_slice(&h, reader, nal, sps, pps, structure)
    }

    /// Starts a frame or field: second fields go on in the frame of the first.
    fn start_picture(
        &mut self,
        h: &SliceHeader,
        nal: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        structure: u8,
        tag: u64,
    ) -> Result<(), DecodeError> {
        let idr = nal.nal_unit_type == NalUnitType::SliceIdr;
        let second = self.dpb.stores.iter().position(|s| s.current).filter(|&at| {
            let s = &self.dpb.stores[at];
            structure != FRAME
                && s.present == FRAME ^ structure
                && s.frame_num == h.frame_num
                && !idr
                && !self.prev_had_mmco5
        });
        if self.dpb.stores.iter().any(|s| s.current) && second.is_none() {
            // A first field that never got its second.
            self.finish_frame(sps);
        }
        self.dpb.set_max_refs(sps.max_num_ref_frames);
        let poc = self
            .dpb
            .compute_poc(sps, h, nal.nal_unit_type, nal.nal_ref_idc, structure);
        if second.is_none() {
            if idr {
                self.dpb.stores.clear();
                self.dpb.prev_ref_frame_num = 0;
            } else if h.frame_num != self.dpb.prev_ref_frame_num
                && h.frame_num
                    != (self.dpb.prev_ref_frame_num + 1) % (1 << (sps.log2_max_frame_num_minus4 + 4).min(30))
            {
                self.dpb.fill_gaps(sps, h.frame_num);
                self.dpb.collect(&mut self.spare_motion);
            }
        }
        let (planes, motion, keep) = match self.frame_planes.take() {
            Some((p, m)) if second.is_some() => (p, m, true),
            _ => {
                let (w, h_mbs) = (self.w, self.h);
                let luma = w * h_mbs * 256;
                let chroma = w * h_mbs * 8 * self.mch;
                (
                    [
                        try_vec(P::default(), luma)?,
                        try_vec(P::default(), chroma)?,
                        try_vec(P::default(), chroma)?,
                    ],
                    match self.spare_motion.pop() {
                        Some(motion) => motion,
                        None => try_vec(MbMotion::default(), w * h_mbs)?,
                    },
                    false,
                )
            }
        };
        if !keep {
            // Everything else about a macroblock is written when it's decoded.
            for mb in self.mbs.iter_mut() {
                mb.slice = 0;
            }
            self.slice_id = 0;
            self.concealed = 0;
        }
        match second {
            Some(at) => {
                let s = &mut self.dpb.stores[at];
                if structure == TOP {
                    s.poc[0] = poc.top;
                } else {
                    s.poc[1] = poc.bottom;
                }
                s.present = FRAME;
            }
            None => {
                let uid = self.dpb.new_uid();
                self.dpb.stores.push(Store {
                    data: None,
                    uid,
                    frame_num: h.frame_num,
                    frame_num_wrap: h.frame_num as i32,
                    lt_idx: 0,
                    short: 0,
                    long: 0,
                    poc: [
                        if structure == BOTTOM { poc.bottom } else { poc.top },
                        if structure == TOP { poc.top } else { poc.bottom },
                    ],
                    present: structure,
                    coded_fields: structure != FRAME,
                    mbaff: h.mbaff_frame_flag,
                    non_existing: false,
                    current: true,
                });
            }
        }
        self.cur = Some(Cur {
            planes,
            motion,
            structure,
            frame_num: h.frame_num,
            nal_ref_idc: nal.nal_ref_idc,
            idr,
            idr_pic_id: h.idr_pic_id,
            pps_id: pps.pic_parameter_set_id,
            lsb: h.pic_order_cnt_lsb,
            delta_bottom: h.delta_pic_order_cnt_bottom,
            delta0: h.delta_pic_order_cnt,
            poc,
            long_term_flag: h.long_term_reference_flag,
            mmco: Vec::new(),
            tag: if second.is_some() { self.first_tag } else { tag },
            width: sps.width(),
            height: sps.height(),
            crop_x: sps.crop_left(),
            crop_y: sps.crop_top(),
            reset: idr || self.first_reset && second.is_some(),
            any_idr: idr || self.first_idr && second.is_some(),
            mbaff: h.mbaff_frame_flag,
        });
        self.scales_for = None;
        Ok(())
    }

    fn run_slice(
        &mut self,
        h: &SliceHeader,
        mut reader: BitstreamReader,
        nal: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        structure: u8,
    ) -> Result<(), DecodeError> {
        if self.scales_for != Some(pps.pic_parameter_set_id) {
            self.scales.build(pps);
            self.scales_for = Some(pps.pic_parameter_set_id);
        }
        let is_b = h.slice_type == SliceType::B;
        let is_i = h.slice_type == SliceType::I;
        let Some(cur) = self.cur.as_mut() else {
            return Err(DecodeError::InvalidSyntax("no picture in progress"));
        };
        let cur_at = self.dpb.stores.iter().position(|s| s.current);
        let poc_pair = cur_at.map_or([0, 0], |at| self.dpb.stores[at].poc);
        let cur_poc = if structure == FRAME {
            poc_pair[0].min(poc_pair[1])
        } else {
            poc_pair[(structure == BOTTOM) as usize]
        };
        let lists = if is_i {
            [Vec::new(), Vec::new()]
        } else {
            let lists = self.dpb.build_lists(h, sps, is_b, structure, cur_poc);
            if lists[0].is_empty() || (is_b && lists[1].is_empty()) {
                return Err(DecodeError::InvalidSyntax(
                    "inter slice without reference pictures",
                ));
            }
            lists
        };
        self.slice_id += 1;
        let (depth, qp_off) = (self.depth, 6 * (self.depth as i32 - 8));
        let slice_qp = 26 + pps.pic_init_qp_minus26 + h.slice_qp_delta;
        let cabac_on = pps.entropy_coding_mode_flag;
        let mut ctx = [0u8; CONTEXTS];
        let cab = if cabac_on {
            let (pos, _) = reader.cabac_start();
            init_contexts(
                &mut ctx,
                slice_qp,
                if is_i { None } else { Some(h.cabac_init_idc) },
            );
            Some(Cabac::new(&nal.rbsp, pos))
        } else {
            None
        };
        let mut sd = Sd {
            planes: &mut cur.planes,
            motion: &mut cur.motion,
            mbs: &mut self.mbs,
            sps,
            pps,
            h,
            scales: &self.scales,
            lists,
            w: self.w,
            frame_h: self.h,
            pic_mbs: if structure == FRAME {
                self.w * self.h
            } else {
                self.w * self.h / 2
            },
            structure,
            mbaff: h.mbaff_frame_flag,
            cfi: self.cfi,
            mch: self.mch,
            depth,
            max: (1 << depth) - 1,
            qp_off,
            slice_id: self.slice_id,
            is_b,
            is_i,
            cur_poc: poc_pair,
            rbsp: &nal.rbsp,
            br: reader,
            cab,
            ctx,
            qp_prev: slice_qp,
            last_dqp: false,
            addr: 0,
            idx: 0,
            mbx: 0,
            mby: 0,
            field: false,
            lower: false,
            parity: structure,
            nb_a: None,
            nb_b: None,
            nb_c: None,
            nb_d: None,
            d_row: 15,
            ly: 0,
            ls: 0,
            lc: 0,
            cs: 0,
            coef: [0; 256],
            ccoef: [[0; 128]; 2],
            luma_any: 0,
            chroma_any: [0; 2],
            lev_pos: [0; 64],
            lev_val: [0; 64],
            cavlc_count: 0,
            i16_mode: 0,
            direct_ready: false,
            direct_ref: [0; 2],
            direct_mv: [[0; 2]; 2],
            preds: Some(&mut *self.preds),
        };
        let first = h.first_mb_in_slice as usize * if h.mbaff_frame_flag { 2 } else { 1 };
        let result = sd.decode_slice_data(first);
        cur.mmco.clone_from(&h.mmco_ops);
        cur.long_term_flag = h.long_term_reference_flag;
        result
    }

    /// Finishes the picture in progress, if any: conceal, deblock, mark.
    fn finish_picture(&mut self, sps: &Sps) {
        let Some(mut cur) = self.cur.take() else {
            return;
        };
        self.conceal(&mut cur);
        Deblock {
            planes: &mut cur.planes,
            motion: &cur.motion,
            mbs: &self.mbs,
            w: self.w,
            frame_h: self.h,
            structure: cur.structure,
            mbaff: cur.mbaff,
            mch: self.mch,
            depth: self.depth,
        }
        .run();
        let Some(at) = self.dpb.stores.iter().position(|s| s.current) else {
            return;
        };
        let mut had5 = false;
        if cur.nal_ref_idc != 0 {
            had5 = self.dpb.mark(
                at,
                sps,
                cur.frame_num,
                cur.structure,
                cur.idr,
                cur.long_term_flag,
                &cur.mmco,
            );
        }
        self.prev_had_mmco5 = had5;
        let mut temp_top = None;
        if had5 {
            let s = &mut self.dpb.stores[at];
            s.frame_num = 0;
            s.frame_num_wrap = 0;
            let temp = match cur.structure {
                FRAME => s.poc[0].min(s.poc[1]),
                TOP => s.poc[0],
                _ => s.poc[1],
            };
            if cur.structure != BOTTOM {
                s.poc[0] -= temp;
            }
            if cur.structure != TOP {
                s.poc[1] -= temp;
            }
            temp_top = Some(s.poc[0]);
            cur.reset = true;
        }
        self.dpb.finish_poc(
            &cur.poc,
            cur.frame_num,
            cur.nal_ref_idc != 0,
            cur.lsb,
            temp_top,
            cur.structure,
        );
        let structure = cur.structure;
        self.first_tag = cur.tag;
        self.first_reset = cur.reset;
        self.first_idr = cur.any_idr;
        self.meta = Some((cur.width, cur.height, cur.crop_x, cur.crop_y));
        self.frame_planes = Some((cur.planes, cur.motion));
        if self.dpb.stores[at].present == FRAME || structure == FRAME {
            self.finish_frame(sps);
        }
    }

    /// Hands a finished frame over: it joins the DPB's samples and goes to output.
    fn finish_frame(&mut self, _sps: &Sps) {
        let Some(at) = self.dpb.stores.iter().position(|s| s.current) else {
            return;
        };
        let Some((planes, motion)) = self.frame_planes.take() else {
            self.dpb.stores[at].current = false;
            self.dpb.collect(&mut self.spare_motion);
            return;
        };
        let [y, u, v] = planes;
        let planes = [Arc::new(y), Arc::new(u), Arc::new(v)];
        let data = Arc::new(PicData {
            planes: planes.clone(),
            motion,
        });
        let store = &mut self.dpb.stores[at];
        store.data = Some(data);
        store.current = false;
        let poc = store.frame_poc();
        self.dpb.collect(&mut self.spare_motion);
        let (width, height, crop_x, crop_y) = self.meta.unwrap_or((0, 0, 0, 0));
        match self.make_frame(planes, width, height, crop_x, crop_y, poc) {
            Ok(frame) => self.output.push_back(frame),
            Err(_) => self.out_of_memory = true,
        }
    }

    /// Builds the output frame: native planes, and 8-bit 4:2:0 for the display path.
    fn make_frame(
        &self,
        planes: [Arc<Vec<P>>; 3],
        width: u32,
        height: u32,
        crop_x: u32,
        crop_y: u32,
        poc: i32,
    ) -> Result<Frame, DecodeError> {
        let (w, h) = (self.w * 16, self.h * 16);
        let native_stride_c = self.w * 8;
        let shared = (self.cfi == 1)
            .then(|| {
                let y = P::share_u8(&planes[0])?;
                let u = P::share_u8(&planes[1])?;
                let v = P::share_u8(&planes[2])?;
                Some((y, u, v))
            })
            .flatten();
        let (y, u, v) = match shared {
            Some(all) => all,
            None => self.convert(&planes)?,
        };
        let sub_y = if self.cfi == 1 { 2 } else { 1 };
        let native = (self.keep_native).then(|| NativeFrame {
            y: P::native(&planes[0]),
            u: P::native(&planes[1]),
            v: P::native(&planes[2]),
            stride: w,
            chroma_stride: native_stride_c,
            chroma_rows: if self.cfi == 0 { 0 } else { h / sub_y },
            y_offset: crop_y as usize * w + crop_x as usize,
            chroma_offset: (crop_y as usize / sub_y) * native_stride_c + crop_x as usize / 2,
            chroma_height: if self.cfi == 0 { 0 } else { height as usize / sub_y },
        });
        Ok(Frame {
            width,
            height,
            y,
            u,
            v,
            y_stride: w,
            uv_stride: w / 2,
            y_offset: crop_y as usize * w + crop_x as usize,
            uv_offset: (crop_y as usize / 2) * (w / 2) + crop_x as usize / 2,
            pic_order_cnt: poc,
            poc_reset: self.first_reset,
            is_idr: self.first_idr,
            tag: self.first_tag,
            concealed_mbs: self.concealed,
            bit_depth: self.depth as u8,
            chroma_format: self.cfi as u8,
            native,
        })
    }

    /// 8-bit 4:2:0 planes of a picture that's deeper or has more chroma.
    #[allow(clippy::type_complexity)]
    fn convert(
        &self,
        planes: &[Arc<Vec<P>>; 3],
    ) -> Result<(Arc<Vec<u8>>, Arc<Vec<u8>>, Arc<Vec<u8>>), DecodeError> {
        let shift = self.depth - 8;
        let round = if shift == 0 { 0 } else { 1 << (shift - 1) };
        let to8 = |v: i32| ((v + round) >> shift).min(255) as u8;
        let (w, h) = (self.w * 16, self.h * 16);
        let (cw, ch) = (self.w * 8, self.h * 8);
        let mut y = try_vec(0u8, w * h)?;
        for (out, value) in y.iter_mut().zip(planes[0].iter()) {
            *out = to8(value.get());
        }
        let mut u = try_vec(128u8, cw * ch)?;
        let mut v = try_vec(128u8, cw * ch)?;
        for (out, plane) in [(&mut u, &planes[1]), (&mut v, &planes[2])] {
            if self.cfi == 1 {
                for (o, value) in out.iter_mut().zip(plane.iter()) {
                    *o = to8(value.get());
                }
            } else if self.cfi == 2 {
                // Two rows into one; in an interlaced frame, each from its own field.
                for row in 0..ch {
                    let (a, b) = if self.interlaced {
                        let first = (row / 2) * 4 + (row & 1);
                        (first, first + 2)
                    } else {
                        (row * 2, row * 2 + 1)
                    };
                    let (a, b) = (a * cw, b * cw);
                    for x in 0..cw {
                        out[row * cw + x] = to8((plane[a + x].get() + plane[b + x].get() + 1) >> 1);
                    }
                }
            }
        }
        Ok((Arc::new(y), Arc::new(u), Arc::new(v)))
    }

    /// Macroblocks no slice delivered take the last reference's samples, or grey.
    fn conceal(&mut self, cur: &mut Cur<P>) {
        let w = self.w;
        let (rowstep, base, rows) = match cur.structure {
            FRAME => (w, 0, self.h),
            TOP => (2 * w, 0, self.h / 2),
            _ => (2 * w, w, self.h / 2),
        };
        let source = self
            .dpb
            .stores
            .iter()
            .filter(|s| s.data.is_some() && (s.short | s.long) != 0)
            .max_by_key(|s| s.uid)
            .and_then(|s| s.data.clone());
        let grey = P::new(1 << (self.depth - 1));
        let mut missing = 0;
        for row in 0..rows {
            for x in 0..w {
                let idx = x + row * rowstep + base;
                if self.mbs[idx].slice != 0 {
                    continue;
                }
                missing += 1;
                let (line_mul, odd) = match cur.structure {
                    FRAME => (16, 0),
                    TOP => (32, 0),
                    _ => (32, 1),
                };
                let mul = if cur.structure == FRAME { 1 } else { 2 };
                for r in 0..16 {
                    let at = (row * line_mul + odd + r * mul) * w * 16 + x * 16;
                    match &source {
                        Some(s) => cur.planes[0][at..at + 16].copy_from_slice(&s.planes[0][at..at + 16]),
                        None => cur.planes[0][at..at + 16].fill(grey),
                    }
                }
                for pl in 1..3 {
                    for r in 0..self.mch {
                        let at = (row * self.mch * mul + odd + r * mul) * w * 8 + x * 8;
                        match &source {
                            Some(s) => cur.planes[pl][at..at + 8].copy_from_slice(&s.planes[pl][at..at + 8]),
                            None => cur.planes[pl][at..at + 8].fill(grey),
                        }
                    }
                }
                cur.motion[idx] = MbMotion::default();
                // Not filtered: its edges aren't block edges.
            }
        }
        self.concealed += missing;
    }

    fn flush(&mut self, sps: Option<&Sps>) {
        if let Some(sps) = sps {
            self.finish_picture(sps);
            self.finish_frame(sps);
        }
    }
}

/// A core of either sample type.
enum Either {
    U8(Core<u8>),
    U16(Core<u16>),
}

/// The decoder for streams the 8-bit progressive path doesn't take.
pub(crate) struct ExtDecoder {
    core: Option<Either>,
    keep_native: bool,
    last_sps_id: Option<u32>,
}

macro_rules! with_core {
    ($core:expr, $c:ident => $body:expr) => {
        match $core {
            Either::U8($c) => $body,
            Either::U16($c) => $body,
        }
    };
}

impl ExtDecoder {
    pub fn new() -> Self {
        Self {
            core: None,
            keep_native: false,
            last_sps_id: None,
        }
    }

    pub fn set_keep_native(&mut self, keep: bool) {
        self.keep_native = keep;
    }

    pub fn is_active(&self) -> bool {
        self.core.is_some()
    }

    /// Decodes a slice whose parameter sets are `sps` and `pps`.
    pub fn decode_slice(
        &mut self,
        nal: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        tag: u64,
        limits: &Limits,
    ) -> Result<(), DecodeError> {
        let fits = self.core.as_ref().is_some_and(|core| with_core!(core, c => c.matches(sps)));
        if !fits {
            if sps.coded_width() > limits.max_width
                || sps.coded_height() > limits.max_height
                || stream_memory(sps) > limits.max_memory
            {
                return Err(DecodeError::OutOfMemory);
            }
            self.flush_core(Some(sps));
            let old_output = self.take_all();
            let mut new = if sps.bit_depth_luma_minus8 == 0 && sps.bit_depth_chroma_minus8 == 0 {
                Either::U8(Core::new(sps, self.keep_native)?)
            } else {
                Either::U16(Core::new(sps, self.keep_native)?)
            };
            with_core!(&mut new, c => c.output = old_output);
            self.core = Some(new);
        }
        self.last_sps_id = Some(sps.seq_parameter_set_id);
        match self.core.as_mut() {
            Some(core) => with_core!(core, c => {
                let result = c.decode_slice(nal, sps, pps, tag);
                if core::mem::take(&mut c.out_of_memory) {
                    return Err(DecodeError::OutOfMemory);
                }
                result
            }),
            None => Ok(()),
        }
    }

    fn flush_core(&mut self, sps: Option<&Sps>) {
        if let Some(core) = self.core.as_mut() {
            with_core!(core, c => c.flush(sps));
        }
    }

    fn take_all(&mut self) -> VecDeque<Frame> {
        match self.core.as_mut() {
            Some(core) => with_core!(core, c => core::mem::take(&mut c.output)),
            None => VecDeque::new(),
        }
    }

    /// Finishes everything in progress.
    pub fn flush(&mut self, sps: Option<&Sps>) {
        self.flush_core(sps);
    }

    pub fn pop_output(&mut self) -> Option<Frame> {
        match self.core.as_mut() {
            Some(core) => with_core!(core, c => c.output.pop_front()),
            None => None,
        }
    }

    pub fn reset(&mut self) {
        if let Some(core) = self.core.as_mut() {
            with_core!(core, c => {
                c.cur = None;
                c.frame_planes = None;
                c.dpb.reset();
                c.output.clear();
                c.slice_id = 0;
                c.prev_had_mmco5 = false;
            });
        }
    }
}
