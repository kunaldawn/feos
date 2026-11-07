use alloc::vec::Vec;

use alloc::collections::{BTreeMap, VecDeque};

use alloc::sync::Arc;

use crate::bitstream::BitstreamReader;
use crate::deblock::{self, DeblockParams, MbInfo, MbType};
use crate::decode_cabac::CabacMbResult;
use crate::dpb::{DecodedPicture, Dpb, ReferenceStatus};
use crate::error::DecodeError;
use crate::mv_pred::WeightContext;
use crate::nal::{NalUnit, NalUnitType};
use crate::pps::{Pps, parse_pps};
use crate::slice::{SliceType, parse_slice_header};
use crate::slice_context::{SliceContext, SliceParams};
use crate::sps::{Sps, parse_sps};

/// A decoded YUV 4:2:0 frame.
///
/// The planes hold the whole coded picture and are shared with the decoder's
/// reference buffer, so a cropped stream such as 1080p needs no extra copy.
/// Use [`y_row`](Self::y_row), [`u_row`](Self::u_row) and
/// [`v_row`](Self::v_row) to read the visible picture: `width × height` luma
/// samples and `(width/2) × (height/2)` samples per chroma plane.
///
/// Frames returned by [`Decoder::decode_nal`] are emitted in **decode order**,
/// not display order. To present them in the intended order, sort by
/// `pic_order_cnt` within each GOP, or use [`OrderedDecoder`].
#[derive(Debug, Clone)]
pub struct Frame {
    /// Display width in luma samples.
    pub width: u32,
    /// Display height in luma samples.
    pub height: u32,
    /// Coded luma plane.
    pub y: Arc<Vec<u8>>,
    /// Coded Cb chroma plane.
    pub u: Arc<Vec<u8>>,
    /// Coded Cr chroma plane.
    pub v: Arc<Vec<u8>>,
    /// Bytes between luma rows.
    pub y_stride: usize,
    /// Bytes between chroma rows.
    pub uv_stride: usize,
    /// Offset of the first visible luma sample.
    pub y_offset: usize,
    /// Offset of the first visible sample of each chroma plane.
    pub uv_offset: usize,
    /// Picture order count — the H.264 spec's display ordering value.
    /// Use this to sort frames into display order within a GOP.
    pub pic_order_cnt: i32,
    /// True when this picture restarts picture order counting (an IDR, or a
    /// picture with memory management operation 5): everything decoded earlier
    /// is displayed before it.
    pub poc_reset: bool,
    /// True for an IDR picture.
    pub is_idr: bool,
    /// The value of [`Decoder::set_tag`] when this picture's first slice was
    /// decoded. Lets a demuxer tie output frames back to container samples.
    pub tag: u64,
    /// Macroblocks that no slice delivered, filled in from an earlier picture.
    pub concealed_mbs: u32,
    /// Coded bits per sample. `y`, `u` and `v` are always 8 bits.
    pub bit_depth: u8,
    /// `chroma_format_idc` as coded: `y`, `u` and `v` are always 4:2:0.
    pub chroma_format: u8,
    /// The samples as coded, when asked for with [`Decoder::set_keep_native`].
    pub native: Option<NativeFrame>,
}

/// Samples of a plane as coded.
#[derive(Debug, Clone)]
pub enum NativePlane {
    /// 8 bits per sample.
    U8(Arc<Vec<u8>>),
    /// 9 or more bits per sample.
    U16(Arc<Vec<u16>>),
}

/// A picture as coded, in its own bit depth and chroma format. Uses the
/// coded picture's strides; `Frame::width` and `height` give the visible size.
#[derive(Debug, Clone)]
pub struct NativeFrame {
    /// Luma.
    pub y: NativePlane,
    /// Cb.
    pub u: NativePlane,
    /// Cr.
    pub v: NativePlane,
    /// Samples between luma rows.
    pub stride: usize,
    /// Samples between chroma rows.
    pub chroma_stride: usize,
    /// Chroma rows in the coded picture.
    pub chroma_rows: usize,
    /// Offset of the first visible luma sample.
    pub y_offset: usize,
    /// Offset of the first visible chroma sample.
    pub chroma_offset: usize,
    /// Visible chroma rows.
    pub chroma_height: usize,
}

impl Frame {
    /// Visible samples of luma row `row`.
    pub fn y_row(&self, row: usize) -> &[u8] {
        let start = self.y_offset + row * self.y_stride;
        &self.y[start..start + self.width as usize]
    }

    /// Visible samples of Cb row `row`.
    pub fn u_row(&self, row: usize) -> &[u8] {
        let start = self.uv_offset + row * self.uv_stride;
        &self.u[start..start + self.width as usize / 2]
    }

    /// Visible samples of Cr row `row`.
    pub fn v_row(&self, row: usize) -> &[u8] {
        let start = self.uv_offset + row * self.uv_stride;
        &self.v[start..start + self.width as usize / 2]
    }

    /// Appends the visible picture to `out` as coded: planar, with samples over 8 bits as
    /// 16-bit little endian. Empty without [`native`](Self::native).
    pub fn append_native(&self, out: &mut Vec<u8>) {
        let Some(native) = &self.native else {
            return;
        };
        let mut put = |plane: &NativePlane, offset: usize, stride: usize, width: usize, rows: usize| {
            for row in 0..rows {
                let at = offset + row * stride;
                match plane {
                    NativePlane::U8(p) => out.extend_from_slice(&p[at..at + width]),
                    NativePlane::U16(p) => {
                        for value in &p[at..at + width] {
                            out.extend_from_slice(&value.to_le_bytes());
                        }
                    }
                }
            }
        };
        let (w, h) = (self.width as usize, self.height as usize);
        put(&native.y, native.y_offset, native.stride, w, h);
        put(&native.u, native.chroma_offset, native.chroma_stride, w / 2, native.chroma_height);
        put(&native.v, native.chroma_offset, native.chroma_stride, w / 2, native.chroma_height);
    }

    /// Appends the visible picture to `out` as planar I420.
    pub fn append_i420(&self, out: &mut Vec<u8>) {
        for row in 0..self.height as usize {
            out.extend_from_slice(self.y_row(row));
        }
        for row in 0..self.height as usize / 2 {
            out.extend_from_slice(self.u_row(row));
        }
        for row in 0..self.height as usize / 2 {
            out.extend_from_slice(self.v_row(row));
        }
    }
}

/// Resource limits for a [`Decoder`]. The kernel heap is small and allocation
/// failure is fatal there, so pictures are checked against these before any
/// storage is allocated, and every picture allocation is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted coded width in luma samples.
    pub max_width: u32,
    /// Largest accepted coded height in luma samples.
    pub max_height: u32,
    /// Largest accepted estimate of the memory a stream needs: reference
    /// pictures, reorder queue and per-macroblock working state.
    pub max_memory: usize,
}

impl Default for Limits {
    /// Up to 1080p at conforming reference counts, within a 64 MiB heap.
    fn default() -> Self {
        Self {
            max_width: 16384,
            max_height: 16384,
            max_memory: 48 * 1024 * 1024,
        }
    }
}

/// Bytes per macroblock in the planes of one picture.
const PLANE_BYTES_PER_MB: usize = 384;
/// Bytes per macroblock kept alongside a reference picture for direct
/// prediction: two motion vector planes and reference index and POC planes.
const MOTION_BYTES_PER_MB: usize = 16 * (4 + 4 + 1 + 1 + 4);
/// Bytes per macroblock of working state, excluding motion vectors kept with
/// the picture.
const SCRATCH_BYTES_PER_MB: usize = 24
    + 2 * 16 * 4
    + core::mem::size_of::<MbInfo>()
    + 16
    + core::mem::size_of::<u16>()
    + 4
    + 16
    + 3
    + core::mem::size_of::<u16>();

/// Estimates the memory a stream needs at its reference and reorder depths.
fn stream_memory(sps: &Sps) -> usize {
    let mbs = sps.mb_width() as usize * sps.mb_height() as usize;
    let references = sps.max_num_ref_frames as usize + 1;
    // Frames queued for output hold planes only. One more is held by a caller
    // that keeps the current frame for redraws.
    let queued = sps.reorder_depth() + 1;
    let per_mb = references * (PLANE_BYTES_PER_MB + MOTION_BYTES_PER_MB)
        + queued * PLANE_BYTES_PER_MB
        + SCRATCH_BYTES_PER_MB;
    mbs.saturating_mul(per_mb)
}

/// Allocates a filled vector, reporting failure instead of aborting.
fn try_vec<T: Clone>(value: T, len: usize) -> Result<Vec<T>, DecodeError> {
    let mut items = Vec::new();
    items
        .try_reserve_exact(len)
        .map_err(|_| DecodeError::OutOfMemory)?;
    items.resize(len, value);
    Ok(items)
}

/// Overwrites `items` with `len` copies of `value`, reusing its storage.
fn refill<T: Clone>(items: &mut Vec<T>, value: T, len: usize) -> Result<(), DecodeError> {
    items.clear();
    items
        .try_reserve_exact(len)
        .map_err(|_| DecodeError::OutOfMemory)?;
    items.resize(len, value);
    Ok(())
}

/// Mutable picture storage used while decoding. It is kept private so the
/// public frame API can share completed planes with the DPB without copying
/// the decoded pixels.
pub(crate) struct WorkFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub pic_order_cnt: i32,
}

/// Per-macroblock working state. It isn't needed once a picture is finished,
/// so one set is recycled between pictures instead of being reallocated.
#[derive(Default)]
struct MbScratch {
    nc_luma: Vec<u8>,
    nc_cb: Vec<u8>,
    nc_cr: Vec<u8>,
    mvd_store: Vec<[i16; 2]>,
    mvd_store_l1: Vec<[i16; 2]>,
    mb_info: Vec<deblock::MbInfo>,
    i4x4_modes: Vec<u8>,
    // CABAC neighbor context state
    mb_cbp: Vec<u16>,
    mb_chroma_pred: Vec<u8>,
    mb_is_8x8dct: Vec<bool>,
    mb_skip: Vec<bool>,
    /// Macroblocks that decoded, so that the rest can be concealed.
    mb_done: Vec<bool>,
    mb_is_direct: Vec<bool>,
    blk_is_direct: Vec<bool>,
    is_i16x16: Vec<bool>,
    /// Per-MB slice ID for slice boundary detection. MBs from different
    /// slices are treated as unavailable for CABAC context and MV prediction.
    mb_slice_id: Vec<u16>,
    /// Per-MB-pair field decoding flag (MBAFF only). Indexed by pair address.
    mb_field_decoding: Vec<bool>,
}

impl MbScratch {
    /// Sizes and clears the state for a picture of `total_mbs` macroblocks.
    fn prepare(&mut self, total_mbs: usize) -> Result<(), DecodeError> {
        refill(&mut self.nc_luma, 0u8, total_mbs * 16)?;
        refill(&mut self.nc_cb, 0u8, total_mbs * 4)?;
        refill(&mut self.nc_cr, 0u8, total_mbs * 4)?;
        refill(&mut self.mvd_store, [0i16; 2], total_mbs * 16)?;
        refill(&mut self.mvd_store_l1, [0i16; 2], total_mbs * 16)?;
        refill(&mut self.mb_info, deblock::MbInfo::default(), total_mbs)?;
        refill(&mut self.i4x4_modes, 2u8, total_mbs * 16)?;
        refill(&mut self.mb_cbp, 0u16, total_mbs)?;
        refill(&mut self.mb_chroma_pred, 0u8, total_mbs)?;
        refill(&mut self.mb_is_8x8dct, false, total_mbs)?;
        refill(&mut self.mb_skip, false, total_mbs)?;
        refill(&mut self.mb_done, false, total_mbs)?;
        refill(&mut self.mb_is_direct, false, total_mbs)?;
        refill(&mut self.blk_is_direct, false, total_mbs * 16)?;
        refill(&mut self.is_i16x16, false, total_mbs)?;
        refill(&mut self.mb_slice_id, 0u16, total_mbs)?;
        refill(&mut self.mb_field_decoding, false, total_mbs.div_ceil(2))?;
        Ok(())
    }
}

/// In-progress picture state shared across slices within the same frame.
struct PictureState {
    frame: WorkFrame,
    frame_num: u32,
    poc: i32,
    nal_unit_type: NalUnitType,
    nal_ref_idc: u8,
    // Motion data kept with the picture for direct prediction.
    mv_store_l0: Vec<[i16; 2]>,
    mv_store_l1: Vec<[i16; 2]>,
    ref_idx_store_l0: Vec<i8>,
    ref_poc_store_l0: Vec<i32>,
    ref_idx_store_l1: Vec<i8>,
    scratch: MbScratch,
    /// Current slice ID counter (incremented for each new slice).
    current_slice_id: u16,
    // Slice header info for finalization
    mmco_ops: Vec<(u32, u32)>,
    long_term_reference_flag: bool,
    is_intra_slice: bool,
    /// `delta_pic_order_cnt_bottom`, for the POC reset of MMCO 5.
    delta_poc_bottom: i32,
    mb_width: u32,
    mb_height: u32,
    /// Luma columns and rows hidden by the SPS crop.
    crop_x: u32,
    crop_y: u32,
    /// Where the last slice started.
    last_first_mb: u32,
    /// True if this picture uses MBAFF (mb_adaptive_frame_field_flag && !field_pic_flag).
    mbaff_frame_flag: bool,
    tag: u64,
}

/// Streaming H.264 decoder.
///
/// Feed NAL units one at a time with [`decode_nal`](Self::decode_nal) and
/// receive decoded frames as they become available. Call [`flush`](Self::flush)
/// at end-of-stream to retrieve any final buffered frame.
///
/// Internally maintains parameter set tables (SPS, PPS), a Decoded Picture
/// Buffer (DPB), and any in-progress slice state.
///
/// # Example
///
/// ```no_run
/// use feos_h264::decoder::Decoder;
/// use feos_h264::nal::parse_annex_b;
///
/// let bitstream = std::fs::read("input.h264").unwrap();
/// let nals = parse_annex_b(&bitstream);
/// let mut decoder = Decoder::new();
///
/// for nal in &nals {
///     if let Ok(Some(frame)) = decoder.decode_nal(nal) {
///         println!("Decoded {}x{} frame, POC={}",
///                  frame.width, frame.height, frame.pic_order_cnt);
///     }
/// }
/// if let Some(frame) = decoder.flush() {
///     println!("Final frame, POC={}", frame.pic_order_cnt);
/// }
/// ```
pub struct Decoder {
    sps_table: BTreeMap<u32, Sps>,
    pps_table: BTreeMap<u32, Pps>,
    dpb: Dpb,
    /// In-progress picture being assembled from one or more slices.
    pending: Option<PictureState>,
    /// Frames finalized before a later NAL failed.  Keep them queued so a
    /// recoverable slice error cannot silently discard decoded output.
    deferred_output: VecDeque<Frame>,
    limits: Limits,
    /// Working state saved from the last finished picture.
    scratch_pool: Option<MbScratch>,
    /// Reorder depth of the active SPS.
    reorder_depth: usize,
    tag: u64,
    /// The core for field pictures, MBAFF, high bit depths and 4:2:2.
    ext: crate::ext::ExtDecoder,
    /// The SPS that core last worked from.
    ext_sps: Option<u32>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// Create a new decoder with empty parameter set tables and DPB, and
    /// default [`Limits`].
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    /// Create a new decoder that refuses streams beyond `limits` with
    /// [`DecodeError::OutOfMemory`].
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            sps_table: BTreeMap::new(),
            pps_table: BTreeMap::new(),
            dpb: Dpb::new(0),
            pending: None,
            deferred_output: VecDeque::new(),
            limits,
            scratch_pool: None,
            reorder_depth: 0,
            tag: 0,
            ext: crate::ext::ExtDecoder::new(),
            ext_sps: None,
        }
    }

    /// Keeps each frame's samples as coded, in [`Frame::native`], for callers
    /// that want more than the 8-bit 4:2:0 display planes. It applies to streams
    /// that aren't 8-bit progressive 4:2:0 and started after this call. It holds
    /// their memory while the frame lives.
    pub fn set_keep_native(&mut self, keep: bool) {
        self.ext.set_keep_native(keep);
    }

    /// Sets the tag recorded on pictures started from now on. See
    /// [`Frame::tag`].
    pub fn set_tag(&mut self, tag: u64) {
        self.tag = tag;
    }

    /// Forgets all pictures so decoding can restart at an IDR, keeping the
    /// parameter sets and recyclable storage.
    pub fn reset(&mut self) {
        if let Some(ps) = self.pending.take() {
            self.scratch_pool = Some(ps.scratch);
        }
        self.dpb.reset();
        self.deferred_output.clear();
        self.ext.reset();
    }

    /// Feed a single NAL unit to the decoder.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` — a decoded frame is ready (in **decode order**;
    ///   sort by `pic_order_cnt` for display order). Note that the returned
    ///   frame belongs to the *previous* picture: when this NAL starts a new
    ///   picture, the decoder finalizes the previous one and returns it.
    /// - `Ok(None)` — the NAL was consumed but no frame is ready yet
    ///   (e.g., the first slice of a multi-slice picture). Any NAL type,
    ///   including SPS, PPS, and SEI, may instead return a frame deferred from
    ///   an earlier recoverable slice error.
    /// - `Err(e)` — the NAL was malformed or used an unsupported feature.
    ///
    /// Call [`flush`](Self::flush) after the last NAL to retrieve any
    /// remaining buffered frame.
    pub fn decode_nal(&mut self, nal: &NalUnit) -> Result<Option<Frame>, DecodeError> {
        match nal.nal_unit_type {
            NalUnitType::Sps => {
                let sps = parse_sps(&nal.rbsp)?;
                sps.check_supported().map_err(DecodeError::Unsupported)?;
                self.sps_table.insert(sps.seq_parameter_set_id, sps);
                Ok(self.take_deferred_output())
            }
            NalUnitType::Pps => {
                let pps_id_sps = {
                    // Peek at seq_parameter_set_id to find the right SPS
                    let mut peek = BitstreamReader::new(&nal.rbsp);
                    let _ = peek.read_ue(); // pic_parameter_set_id
                    peek.read_ue().ok()
                };
                let sps_ref = pps_id_sps.and_then(|id| self.sps_table.get(&id));
                let pps = parse_pps(&nal.rbsp, sps_ref)?;
                self.pps_table.insert(pps.pic_parameter_set_id, pps);
                Ok(self.take_deferred_output())
            }
            NalUnitType::Sei => Ok(self.take_deferred_output()),
            NalUnitType::SliceIdr | NalUnitType::Slice => {
                if let Some(result) = self.decode_ext_slice(nal) {
                    return result;
                }
                if self.ext.is_active() {
                    self.flush_ext();
                }
                // Peek at first_mb_in_slice to detect new vs continuation slice
                let mut peek = BitstreamReader::new(&nal.rbsp);
                let first_mb = peek.read_ue().unwrap_or(0);

                // Check if this is a new picture: first_mb==0 means first
                // slice of a new picture. Continuation slices (first_mb > 0)
                // belong to the same picture even for IDR NALs.
                // A lost first slice mustn't merge what's left of this picture into the
                // last one, so frame_num and the rest of 7.4.1.2.4's easy tests count too.
                let is_new_picture = first_mb == 0 || self.starts_new_picture(nal, first_mb);

                // Finalize pending frame if a new picture starts
                let prev_frame = if is_new_picture {
                    self.finalize_pending()
                } else {
                    None
                };

                // Decode this slice (creates or continues PictureState).
                // For CAVLC multi-slice, end-of-slice detection may fail,
                // causing errors from reading past the slice boundary. If
                // we had a pending picture, decode_slice keeps it, and the
                // already-decoded MBs are valid, so the error is treated as
                // the end of the slice.
                let result = self.decode_slice(nal);
                if let Some(frame) = prev_frame {
                    self.deferred_output.push_back(frame);
                }
                if let Err(e) = result {
                    if self.pending.is_none() || matches!(e, DecodeError::OutOfMemory) {
                        return Err(e);
                    }
                }
                Ok(self.take_deferred_output())
            }
            _ => Ok(self.take_deferred_output()),
        }
    }

    /// Finalize any pending picture and return it. Call this after the last
    /// NAL has been fed via [`decode_nal`](Self::decode_nal) to retrieve the
    /// final frame, which is otherwise held internally awaiting a new picture
    /// to trigger its release. Call it until it returns `None` if decoding
    /// reported errors, as a frame may also be queued from before an error.
    pub fn flush(&mut self) -> Option<Frame> {
        if let Some(frame) = self.finalize_pending() {
            self.deferred_output.push_back(frame);
        }
        self.flush_ext();
        self.take_deferred_output()
    }

    /// Hands a slice to the other core if its stream belongs there, and gives the
    /// result. `None` means it belongs here.
    fn decode_ext_slice(&mut self, nal: &NalUnit) -> Option<Result<Option<Frame>, DecodeError>> {
        let mut peek = BitstreamReader::new(&nal.rbsp);
        peek.read_ue().ok()?;
        peek.read_ue().ok()?;
        let pps_id = peek.read_ue().ok()?;
        let sps_id = self.pps_table.get(&pps_id)?.seq_parameter_set_id;
        if !crate::ext::wanted(self.sps_table.get(&sps_id)?) {
            return None;
        }
        if let Some(frame) = self.finalize_pending() {
            self.deferred_output.push_back(frame);
        }
        let pps = self.pps_table.get(&pps_id)?;
        let sps = self.sps_table.get(&sps_id)?;
        self.reorder_depth = sps.reorder_depth();
        self.ext_sps = Some(sps.seq_parameter_set_id);
        let result = self.ext.decode_slice(nal, sps, pps, self.tag, &self.limits);
        while let Some(frame) = self.ext.pop_output() {
            self.deferred_output.push_back(frame);
        }
        Some(match result {
            // What's decoded stays, and the rest of the picture gets concealed.
            Err(e @ (DecodeError::OutOfMemory | DecodeError::Unsupported(_))) => Err(e),
            _ => Ok(self.take_deferred_output()),
        })
    }

    /// Finishes whatever the other core has in progress.
    fn flush_ext(&mut self) {
        let sps = self.ext_sps.and_then(|id| self.sps_table.get(&id));
        self.ext.flush(sps);
        while let Some(frame) = self.ext.pop_output() {
            self.deferred_output.push_back(frame);
        }
    }

    /// Frames an [`OrderedDecoder`] must hold back for the active SPS.
    pub(crate) fn reorder_depth(&self) -> usize {
        self.reorder_depth
    }

    #[inline]
    fn take_deferred_output(&mut self) -> Option<Frame> {
        self.deferred_output.pop_front()
    }

    /// Frame rate from the most recently parsed SPS's VUI timing info, as
    /// `(numerator, denominator)`. Returns `None` if no SPS has been parsed
    /// yet, if the SPS does not contain VUI timing info, or if the values
    /// are zero.
    ///
    /// For example, a stream encoded at 29.97 fps would return
    /// `Some((60000, 2002))`. Use [`frame_rate_f64`](Self::frame_rate_f64)
    /// for a single floating-point value.
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        // Return the first SPS that has timing info. For typical streams
        // there's only one SPS, so this is unambiguous.
        self.sps_table.values().find_map(|s| s.frame_rate())
    }

    /// Frame rate as a single floating-point value. Convenience wrapper
    /// around [`frame_rate`](Self::frame_rate).
    pub fn frame_rate_f64(&self) -> Option<f64> {
        let (n, d) = self.frame_rate()?;
        Some(n as f64 / d as f64)
    }

    /// Finalize the pending picture: apply deblocking, insert into DPB, return frame.
    /// Whether a slice that doesn't start at the first macroblock belongs to a picture
    /// other than the one in progress.
    fn starts_new_picture(&self, nal: &NalUnit, first_mb: u32) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return true;
        };
        if first_mb <= pending.last_first_mb {
            return true;
        }
        let mut peek = BitstreamReader::new(&nal.rbsp);
        let _ = peek.read_ue();
        let _ = peek.read_ue();
        let Ok(pps_id) = peek.read_ue() else {
            return false;
        };
        let Some(pps) = self.pps_table.get(&pps_id) else {
            return false;
        };
        let Some(sps) = self.sps_table.get(&pps.seq_parameter_set_id) else {
            return false;
        };
        let Ok(frame_num) = peek.read_bits(sps.log2_max_frame_num_minus4 as u8 + 4) else {
            return false;
        };
        frame_num != pending.frame_num
            || (nal.nal_ref_idc == 0) != (pending.nal_ref_idc == 0)
            || (nal.nal_unit_type == NalUnitType::SliceIdr)
                != (pending.nal_unit_type == NalUnitType::SliceIdr)
    }

    /// Fills in the macroblocks no slice delivered, from the most recent reference
    /// picture, so that damage looks like a moment's stutter in part of the picture. With
    /// nothing to copy from, it's mid-grey. Later pictures predict from this as they would
    /// from anything else, so the damage fades only as they replace it.
    fn conceal_missing(&mut self, ps: &mut PictureState) -> u32 {
        let total = ps.mb_width as usize * ps.mb_height as usize;
        let missing = ps.scratch.mb_done.iter().take(total).filter(|&&done| !done).count();
        if missing == 0 {
            return 0;
        }
        let (width, height) = (ps.mb_width as usize * 16, ps.mb_height as usize * 16);
        let max_frame_num = self.dpb.max_frame_num();
        let source = self
            .dpb
            .short_term_ref_list(ps.frame_num, max_frame_num)
            .into_iter()
            .find(|picture| picture.width as usize == width && picture.height as usize == height);
        for mb in 0..total {
            if ps.scratch.mb_done[mb] {
                continue;
            }
            let (x, y) = (mb % ps.mb_width as usize, mb / ps.mb_width as usize);
            for row in 0..16 {
                let at = (y * 16 + row) * width + x * 16;
                let out = &mut ps.frame.y[at..at + 16];
                match &source {
                    Some(picture) => out.copy_from_slice(&picture.y[at..at + 16]),
                    None => out.fill(128),
                }
            }
            for row in 0..8 {
                let at = (y * 8 + row) * (width / 2) + x * 8;
                for (out, from) in [
                    (&mut ps.frame.u, source.as_ref().map(|picture| &picture.u)),
                    (&mut ps.frame.v, source.as_ref().map(|picture| &picture.v)),
                ] {
                    let out = &mut out[at..at + 8];
                    match from {
                        Some(from) => out.copy_from_slice(&from[at..at + 8]),
                        None => out.fill(128),
                    }
                }
            }
            // No filtering across what never decoded: its edges aren't block edges.
            ps.scratch.mb_info[mb] = deblock::MbInfo::concealed();
        }
        missing as u32
    }

    fn finalize_pending(&mut self) -> Option<Frame> {
        let mut ps = self.pending.take()?;
        let concealed = self.conceal_missing(&mut ps);

        // Apply deblocking filter
        deblock::filter_frame(
            &mut ps.frame,
            &ps.scratch.mb_info,
            ps.mb_width as usize,
            ps.mbaff_frame_flag,
        );
        self.scratch_pool = Some(ps.scratch);

        let is_idr = ps.nal_unit_type == NalUnitType::SliceIdr;
        if is_idr {
            self.dpb.clear();
        }

        let reference = if ps.nal_ref_idc > 0 {
            if is_idr && ps.long_term_reference_flag {
                ReferenceStatus::LongTerm(0) // IDR with long_term_reference_flag → LT idx 0
            } else {
                ReferenceStatus::ShortTerm
            }
        } else {
            ReferenceStatus::Unused
        };

        let max_frame_num = self.dpb.max_frame_num();
        // picNumX = CurrPicNum - (difference_of_pic_nums_minus1 + 1), which is
        // negative when the target's frame_num is above the current one. The
        // DPB stores frame_num, so map back through MaxFrameNum (spec 8.2.4.1).
        let pic_num_x = |diff_minus1: u32| -> u32 {
            (ps.frame_num as i64 - diff_minus1 as i64 - 1).rem_euclid(max_frame_num as i64) as u32
        };
        let mut poc_reset = is_idr;
        for &(op, param) in &ps.mmco_ops {
            match op {
                1 => self.dpb.mark_short_term_unused(pic_num_x(param & 0xFFFF)),
                2 => self.dpb.mark_long_term_unused(param),
                3 => self
                    .dpb
                    .assign_long_term(pic_num_x(param & 0xFFFF), param >> 16),
                4 => self.dpb.set_max_long_term_frame_idx(param),
                5 => {
                    self.dpb.clear_all_refs();
                    poc_reset = true;
                }
                6 => {
                    // Will be applied after insert (current pic must be in DPB first)
                }
                _ => {}
            }
        }

        // After MMCO 5 the picture counts as frame_num 0 with its POC rebased
        // to zero, for everything decoded later (spec 8.2.1, 7.4.3).
        let has_mmco5 = ps.mmco_ops.iter().any(|&(op, _)| op == 5);
        let (frame_num, poc) = if has_mmco5 {
            self.dpb.reset_after_mmco5(ps.delta_poc_bottom);
            (0, 0)
        } else {
            (ps.frame_num, ps.poc)
        };

        // The DPB and the returned frame share the coded-size planes. Motion
        // compensation indexes reference pictures using the macroblock-aligned
        // stride; the frame's offsets and strides describe the visible crop.
        let coded_w = ps.mb_width as usize * 16;
        let coded_h = ps.mb_height as usize * 16;
        let full_y = Arc::new(ps.frame.y);
        let full_u = Arc::new(ps.frame.u);
        let full_v = Arc::new(ps.frame.v);
        let output = Frame {
            width: ps.frame.width,
            height: ps.frame.height,
            y: full_y.clone(),
            u: full_u.clone(),
            v: full_v.clone(),
            y_stride: coded_w,
            uv_stride: coded_w / 2,
            y_offset: ps.crop_y as usize * coded_w + ps.crop_x as usize,
            uv_offset: (ps.crop_y as usize / 2) * (coded_w / 2) + ps.crop_x as usize / 2,
            pic_order_cnt: poc,
            poc_reset,
            is_idr,
            tag: ps.tag,
            concealed_mbs: concealed,
            bit_depth: 8,
            chroma_format: 1,
            native: None,
        };
        let pic = Arc::new(DecodedPicture {
            y: full_y,
            u: full_u,
            v: full_v,
            width: coded_w as u32,
            height: coded_h as u32,
            frame_num,
            pic_order_cnt: poc,
            long_term_frame_idx: crate::dpb::LongTermIdx::new(match reference {
                ReferenceStatus::LongTerm(idx) => Some(idx),
                _ => None,
            }),
            mv_l0: ps.mv_store_l0,
            ref_idx_l0: ps.ref_idx_store_l0,
            ref_poc_l0: ps.ref_poc_store_l0,
            mv_l1: ps.mv_store_l1,
            ref_idx_l1: ps.ref_idx_store_l1,
            mb_width: ps.mb_width,
            is_intra: ps.is_intra_slice,
        });

        self.dpb.insert(pic, reference);

        // MMCO op=6: mark current picture as long-term (after insert)
        for &(op, param) in &ps.mmco_ops {
            if op == 6 {
                self.dpb.mark_current_as_long_term(param, frame_num);
            }
        }

        Some(output)
    }

    fn decode_slice(&mut self, nal: &NalUnit) -> Result<(), DecodeError> {
        // The PPS id is part of the common slice-header prefix and must be
        // used before parsing the remaining header.  Selecting the first PPS
        // in the map makes streams with multiple parameter sets decode with
        // the wrong SPS or fail nondeterministically.
        let mut peek = BitstreamReader::new(&nal.rbsp);
        let _first_mb_in_slice = peek
            .read_ue()
            .map_err(|_| DecodeError::InvalidSyntax("missing first_mb_in_slice"))?;
        let _slice_type = peek
            .read_ue()
            .map_err(|_| DecodeError::InvalidSyntax("missing slice_type"))?;
        let pps_id = peek
            .read_ue()
            .map_err(|_| DecodeError::InvalidSyntax("missing pic_parameter_set_id"))?;
        let pps = self
            .pps_table
            .get(&pps_id)
            .ok_or(DecodeError::InvalidSyntax("no PPS available"))?;
        let sps = self
            .sps_table
            .get(&pps.seq_parameter_set_id)
            .ok_or(DecodeError::InvalidSyntax("no SPS available"))?;

        let (header, mut reader) =
            parse_slice_header(&nal.rbsp, sps, pps, nal.nal_unit_type, nal.nal_ref_idc)?;

        if header.slice_type != SliceType::I
            && header.slice_type != SliceType::P
            && header.slice_type != SliceType::B
        {
            return Err(DecodeError::from("unsupported slice type"));
        }
        if header.field_pic_flag {
            return Err(DecodeError::Unsupported("field pictures not yet supported"));
        }
        let is_p_slice = header.slice_type == SliceType::P;
        let is_b_slice = header.slice_type == SliceType::B;

        // Compute POC for current picture (needed for B-slice ref list construction)
        let current_poc = self
            .dpb
            .compute_poc(sps, &header, nal.nal_unit_type, nal.nal_ref_idc);
        // Interlaced coding isn't implemented: the field macroblock paths don't
        // match a reference decoder under CAVLC, and CABAC ignores the field flag.
        if header.mbaff_frame_flag {
            return Err(DecodeError::Unsupported("MBAFF is not supported"));
        }

        // Build reference picture lists
        let max_pic_num = 1u32 << sps.log2_max_frame_num_minus4.saturating_add(4).min(31);
        let mut ref_pic_list = if is_p_slice {
            let mut refs = self.dpb.short_term_ref_list(header.frame_num, max_pic_num);
            // Pad ref list if shorter than num_ref_idx_l0_active (spec 8.2.4.2.1:
            // if the list is shorter, duplicate the last entry to fill)
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l0_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };
        let mut _ref_pic_list_l0 = if is_b_slice {
            let mut refs = self.dpb.ref_list_l0_b(current_poc);
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l0_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };
        let mut _ref_pic_list_l1 = if is_b_slice {
            let mut refs = self.dpb.ref_list_l1_b(current_poc);
            if !refs.is_empty() {
                while refs.len() < header.num_ref_idx_l1_active as usize {
                    refs.push(refs.last().unwrap().clone());
                }
            }
            refs
        } else {
            vec![]
        };

        // Apply ref_pic_list_modification (spec 8.2.4.3)
        if is_p_slice && !header.ref_list_mod_l0.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut ref_pic_list,
                &header.ref_list_mod_l0,
                header.frame_num,
                max_pic_num,
            );
        }
        if is_b_slice && !header.ref_list_mod_l0.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut _ref_pic_list_l0,
                &header.ref_list_mod_l0,
                header.frame_num,
                max_pic_num,
            );
        }
        if is_b_slice && !header.ref_list_mod_l1.is_empty() {
            Dpb::apply_ref_list_modification(
                &mut _ref_pic_list_l1,
                &header.ref_list_mod_l1,
                header.frame_num,
                max_pic_num,
            );
        }

        // Weighted prediction mode:
        // 0 = no weighting (default)
        // 1 = explicit weights (P-slice weighted_pred_flag=1, or B-slice weighted_bipred_idc=1)
        // 2 = implicit weights (B-slice weighted_bipred_idc=2)
        let use_weight = if (is_p_slice && pps.weighted_pred_flag)
            || (is_b_slice && pps.weighted_bipred_idc == 1)
        {
            1
        } else if is_b_slice && pps.weighted_bipred_idc == 2 {
            2
        } else {
            0
        };

        // Implicit weighted prediction: compute L0 weight from POC distances.
        // implicit_weights[l0_idx][l1_idx] = w0. L1 weight = 64 - w0. Fixed log2_denom=5.
        let implicit_weights: Vec<Vec<i32>> = if use_weight == 2 {
            _ref_pic_list_l0
                .iter()
                .map(|ref_l0| {
                    _ref_pic_list_l1
                        .iter()
                        .map(|ref_l1| {
                            let td = ref_l1
                                .pic_order_cnt
                                .saturating_sub(ref_l0.pic_order_cnt)
                                .clamp(-128, 127);
                            if td == 0 {
                                32
                            } else {
                                let tb = current_poc
                                    .saturating_sub(ref_l0.pic_order_cnt)
                                    .clamp(-128, 127);
                                let tx = (16384 + (td.abs() / 2)) / td;
                                let w1 = (tb * tx + 32) >> 8;
                                if !(-64..=128).contains(&w1) {
                                    32
                                } else {
                                    64 - w1
                                }
                            }
                        })
                        .collect()
                })
                .collect()
        } else {
            vec![]
        };

        let wctx = WeightContext {
            use_weight,
            wt: header.weight_table.as_ref(),
            implicit_weights: &implicit_weights,
        };

        let width = sps.width();
        let height = sps.height();
        // Geometry comes from the SPS's coded units, not the cropped size:
        // crops of 16 samples or more would otherwise change the stride, and
        // MBAFF relies on the even height of frame_mbs_only_flag == 0.
        let mb_width = sps.mb_width();
        let mb_height = sps.mb_height();
        let coded_width = sps.coded_width();
        let coded_height = sps.coded_height();
        if width == 0 || height == 0 {
            return Err(DecodeError::InvalidSyntax("SPS dimensions out of range"));
        }
        let total_mbs = (mb_width * mb_height) as usize;

        // Reference pictures are read with this picture's geometry, so a
        // parameter set change without an IDR can't use the old ones.
        let refs_match = |refs: &[Arc<DecodedPicture>]| {
            refs.iter()
                .all(|r| r.width == coded_width && r.height == coded_height)
        };
        if !refs_match(&ref_pic_list)
            || !refs_match(&_ref_pic_list_l0)
            || !refs_match(&_ref_pic_list_l1)
        {
            return Err(DecodeError::InvalidSyntax(
                "reference picture size differs from the current picture",
            ));
        }
        if (is_p_slice && ref_pic_list.is_empty())
            || (is_b_slice && (_ref_pic_list_l0.is_empty() || _ref_pic_list_l1.is_empty()))
        {
            return Err(DecodeError::InvalidSyntax(
                "inter slice without reference pictures",
            ));
        }

        let slice_qp = header.qp_y(pps);

        // Create or reuse PictureState for multi-slice support.
        // For continuation slices (first_mb > 0), reuse the pending state
        // so per-MB data from earlier slices is visible for MV prediction,
        // CABAC neighbor contexts, and deblocking.
        let is_continuation = header.first_mb_in_slice > 0
            && self
                .pending
                .as_ref()
                .is_some_and(|ps| ps.mb_width == mb_width && ps.mb_height == mb_height);
        let mut ps = if let Some(ps) = self.pending.take().filter(|_| is_continuation) {
            ps
        } else {
            if coded_width > self.limits.max_width
                || coded_height > self.limits.max_height
                || stream_memory(sps) > self.limits.max_memory
            {
                return Err(DecodeError::OutOfMemory);
            }
            self.dpb.set_max_ref_frames(sps.max_num_ref_frames);
            self.reorder_depth = sps.reorder_depth();
            let mut scratch = self.scratch_pool.take().unwrap_or_default();
            scratch.prepare(total_mbs)?;
            PictureState {
                frame: WorkFrame {
                    width,
                    height,
                    y: try_vec(0u8, (coded_width * coded_height) as usize)?,
                    u: try_vec(0u8, (coded_width * coded_height / 4) as usize)?,
                    v: try_vec(0u8, (coded_width * coded_height / 4) as usize)?,
                    pic_order_cnt: current_poc,
                },
                frame_num: header.frame_num,
                poc: current_poc,
                nal_unit_type: nal.nal_unit_type,
                nal_ref_idc: nal.nal_ref_idc,
                mv_store_l0: try_vec([0i16; 2], total_mbs * 16)?,
                mv_store_l1: try_vec([0i16; 2], total_mbs * 16)?,
                ref_idx_store_l0: try_vec(-1i8, total_mbs * 16)?,
                ref_poc_store_l0: try_vec(-1i32, total_mbs * 16)?,
                ref_idx_store_l1: try_vec(-1i8, total_mbs * 16)?,
                scratch,
                current_slice_id: 0,
                mmco_ops: Vec::new(),
                long_term_reference_flag: false,
                is_intra_slice: false,
                delta_poc_bottom: 0,
                mb_width,
                mb_height,
                crop_x: sps.crop_left(),
                crop_y: sps.crop_top(),
                mbaff_frame_flag: header.mbaff_frame_flag,
                tag: self.tag,
                last_first_mb: 0,
            }
        };

        // Destructure so the macroblock loop below reads as plain locals.
        let PictureState {
            frame,
            mv_store_l0,
            mv_store_l1,
            ref_idx_store_l0,
            ref_poc_store_l0,
            ref_idx_store_l1,
            scratch,
            current_slice_id,
            ..
        } = &mut ps;
        let MbScratch {
            nc_luma,
            nc_cb,
            nc_cr,
            mvd_store,
            mvd_store_l1,
            mb_info,
            i4x4_modes,
            mb_cbp,
            mb_chroma_pred,
            mb_is_8x8dct,
            mb_skip,
            mb_done,
            mb_is_direct,
            blk_is_direct,
            is_i16x16,
            mb_slice_id,
            mb_field_decoding,
        } = scratch;

        // Increment slice ID for continuation slices so boundary checks work
        if is_continuation {
            *current_slice_id = current_slice_id.wrapping_add(1);
        }
        let this_slice_id = *current_slice_id;

        // Each slice reinitializes its own QP from the slice header
        let mut prev_mb_qp = slice_qp;
        let mut last_qp_delta_nonzero = false;

        // CABAC or CAVLC?
        let use_cabac = pps.entropy_coding_mode_flag;

        // Initialize CABAC engine if needed
        let cabac_byte_pos = if use_cabac {
            let (pos, _data) = reader.cabac_start();
            Some(pos)
        } else {
            None
        };
        // Create CabacReader from original RBSP data (avoids borrow conflict with reader)
        let mut cabac_reader =
            cabac_byte_pos.map(|pos| crate::cabac::CabacReader::new(&nal.rbsp, pos));
        let mut cabac_state = if use_cabac {
            crate::cabac::init_cabac_states(
                slice_qp,
                header.slice_type == SliceType::I,
                header.cabac_init_idc,
            )
        } else {
            [0u8; 1024]
        };

        let mut mb_skip_run: i32 = -1; // -1 = not initialized for P slices

        let stride = coded_width as usize;
        let mbaff = header.mbaff_frame_flag;
        let mut mb_ly_stride = stride;
        let mut mb_ly_offset = 0usize;
        let mut mb_lc_stride = (coded_width / 2) as usize;
        let mut mb_lc_offset = 0usize;

        // Macro to construct a SliceContext from the local variables.
        // Used at each call site that delegates to a SliceContext method.
        macro_rules! make_ctx {
            () => {
                SliceContext {
                    frame: &mut *frame,
                    stride,
                    width: coded_width,
                    height: coded_height,
                    mb_width,
                    nc_luma: &mut *nc_luma,
                    nc_cb: &mut *nc_cb,
                    nc_cr: &mut *nc_cr,
                    mv_store_l0: &mut *mv_store_l0,
                    mv_store_l1: &mut *mv_store_l1,
                    ref_idx_store_l0: &mut *ref_idx_store_l0,
                    ref_poc_store_l0: &mut *ref_poc_store_l0,
                    ref_idx_store_l1: &mut *ref_idx_store_l1,
                    mvd_store: &mut *mvd_store,
                    mvd_store_l1: &mut *mvd_store_l1,
                    mb_info: &mut *mb_info,
                    i4x4_modes: &mut *i4x4_modes,
                    mb_cbp: &mut *mb_cbp,
                    mb_chroma_pred: &mut *mb_chroma_pred,
                    mb_is_8x8dct: &mut *mb_is_8x8dct,
                    mb_skip: &mut *mb_skip,
                    mb_is_direct: &mut *mb_is_direct,
                    blk_is_direct: &mut *blk_is_direct,
                    is_i16x16: &mut *is_i16x16,
                    mb_slice_id: &mut *mb_slice_id,
                    this_slice_id,
                    prev_mb_qp,
                    last_qp_delta_nonzero,
                    mbaff,
                    mb_field_decoding: &mut *mb_field_decoding,
                    ly_stride: mb_ly_stride,
                    ly_offset: mb_ly_offset,
                    lc_stride: mb_lc_stride,
                    lc_offset: mb_lc_offset,
                }
            };
        }

        let params = SliceParams {
            is_p_slice,
            is_b_slice,
            use_weight,
            current_poc,
            direct_spatial_mv_pred_flag: header.direct_spatial_mv_pred_flag,
            direct_8x8_inference_flag: sps.direct_8x8_inference_flag,
            transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
            scaling_list_4x4: &pps.scaling_list_4x4,
            scaling_list_8x8: &pps.scaling_list_8x8,
            constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
            chroma_qp_index_offset: pps.chroma_qp_index_offset,
            second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
            deblock: DeblockParams {
                filter_idc: header.disable_deblocking_filter_idc as u8,
                alpha_offset_div2: header.slice_alpha_c0_offset_div2 as i8,
                beta_offset_div2: header.slice_beta_offset_div2 as i8,
                chroma_qp_offset: [
                    pps.chroma_qp_index_offset as i8,
                    pps.second_chroma_qp_index_offset as i8,
                ],
                slice_id: this_slice_id,
            },
            ref_pic_list: &ref_pic_list,
            ref_pic_list_l0: &_ref_pic_list_l0,
            ref_pic_list_l1: &_ref_pic_list_l1,
            num_ref_idx_l0_active: header.num_ref_idx_l0_active,
            num_ref_idx_l1_active: header.num_ref_idx_l1_active,
            wctx: &wctx,
            first_mb_in_slice: header.first_mb_in_slice,
            slice_qp,
            is_i_slice: header.slice_type == SliceType::I,
            cabac_init_idc: header.cabac_init_idc,
        };

        // In MBAFF, first_mb_in_slice is a pair address
        let mut mb_idx = if mbaff {
            (header.first_mb_in_slice as usize) * 2
        } else {
            header.first_mb_in_slice as usize
        };
        let slice_result = (|| -> Result<(), DecodeError> {
            if mb_idx >= total_mbs {
                return Err(DecodeError::InvalidSyntax("first_mb_in_slice out of range"));
            }
            while mb_idx < total_mbs {
            // CAVLC end-of-slice: check before reading any new syntax elements.
            // Skip this check when counting down a skip run (no reads needed).
            if !use_cabac && mb_skip_run <= 0 && !reader.more_rbsp_data() {
                break;
            }

            // The macroblock before this one in the slice finished, or this wouldn't be here.
            if mb_idx > 0 && mb_slice_id[mb_idx - 1] == this_slice_id {
                mb_done[mb_idx - 1] = true;
            }
            // Stamp this MB with the current slice ID for boundary detection
            mb_slice_id[mb_idx] = this_slice_id;
            // Compute pixel position
            let (mb_x, mb_y) = if mbaff {
                let pair_addr = mb_idx / 2;
                let pair_col = pair_addr % mb_width as usize;
                let pair_row = pair_addr / mb_width as usize;
                let x = pair_col * 16;
                let y = pair_row * 32 + (mb_idx % 2) * 16;
                (x, y)
            } else {
                (
                    (mb_idx % mb_width as usize) * 16,
                    (mb_idx / mb_width as usize) * 16,
                )
            };

            // CABAC decode path
            if use_cabac {
                let cr = cabac_reader.as_mut().unwrap();
                let st = &mut cabac_state;

                // MBAFF: end_of_slice_flag handled inside decode_cabac_mb
                // (with MBAFF-adjusted first_mb comparison)

                // MBAFF CABAC: mb_field_decoding_flag
                // TODO: x264 all-frame MBAFF CABAC streams need investigation.
                // Disabled for now — infer all pairs as frame-coded.

                {
                    let mut ctx = make_ctx!();
                    match ctx.decode_cabac_mb(cr, st, &nal.rbsp, mb_idx, mb_x, mb_y, &params)? {
                        CabacMbResult::EndOfSlice => break,
                        CabacMbResult::Decoded => {}
                    }
                    prev_mb_qp = ctx.prev_mb_qp;
                    last_qp_delta_nonzero = ctx.last_qp_delta_nonzero;
                }
                if mbaff && mb_idx % 2 != 0 {
                    let term = cr.get_cabac_terminate();
                    if term != 0 {
                        mb_idx += 1;
                        break;
                    }
                }

                mb_idx += 1;
                continue;
            }

            // P/B-slice skip run handling
            if is_p_slice || is_b_slice {
                if mb_skip_run < 0 {
                    mb_skip_run = reader.read_ue()? as i32;
                }
                if mb_skip_run > 0 {
                    mb_skip_run -= 1;
                    mb_skip[mb_idx] = true; // Mark as skipped for MBAFF field flag inference
                    // Set layout for skip MBs (field_flag already known)
                    {
                        let mut ctx = make_ctx!();
                        ctx.set_mb_layout(mb_idx, mb_x, mb_y);
                        mb_ly_stride = ctx.ly_stride;
                        mb_ly_offset = ctx.ly_offset;
                        mb_lc_stride = ctx.lc_stride;
                        mb_lc_offset = ctx.lc_offset;
                    }
                    if is_p_slice {
                        // P_Skip: MV = median predictor, ref_idx = 0, no residual
                        make_ctx!().decode_p_skip_mb(mb_idx, mb_x, mb_y, &params);
                    } else {
                        // B_Skip: spatial/temporal direct MV + MC, no residual
                        make_ctx!().decode_b_skip_mb(mb_idx, mb_x, mb_y, &params);
                    }
                    mb_info[mb_idx] = MbInfo {
                        mb_type: MbType::Inter,
                        qp_y: prev_mb_qp,
                        ..Default::default()
                    };
                    mb_idx += 1;
                    continue;
                }
                // mb_skip_run == 0: parse the next MB normally
                mb_skip_run = -1; // reset for next iteration

                // MBAFF: read mb_field_decoding_flag for this pair
                // (spec 7.3.4: read before first non-skipped MB of pair)
                if mbaff {
                    let is_top = mb_idx % 2 == 0;
                    let top_was_skipped = !is_top && mb_skip[mb_idx - 1];
                    if is_top || top_was_skipped {
                        mb_field_decoding[mb_idx / 2] = reader.read_bit()? != 0;
                    }
                }
            }

            // MBAFF I-slice: read mb_field_decoding_flag before MB decode
            if mbaff && !(is_p_slice || is_b_slice) {
                let is_top = mb_idx % 2 == 0;
                if is_top {
                    mb_field_decoding[mb_idx / 2] = reader.read_bit()? != 0;
                }
            }

            {
                let mut ctx = make_ctx!();
                ctx.set_mb_layout(mb_idx, mb_x, mb_y);
                ctx.decode_cavlc_mb(&mut reader, mb_idx, mb_x, mb_y, &params)?;
                prev_mb_qp = ctx.prev_mb_qp;
                last_qp_delta_nonzero = ctx.last_qp_delta_nonzero;
            }
            mb_idx += 1;

            // CAVLC end-of-slice: spec says "while (more_rbsp_data())"
            // after each MB. For single-slice, this naturally ends at
            // total_mbs. For multi-slice, it stops at each slice boundary.
            if !use_cabac && !reader.more_rbsp_data() {
                break;
            }
        }

            Ok(())
        })();
        // The slice's last macroblock, if it ended cleanly.
        if slice_result.is_ok() && mb_idx > 0 && mb_slice_id[mb_idx - 1] == this_slice_id {
            mb_done[mb_idx - 1] = true;
        }

        // Post-loop: fill deblock info and ref POC table
        let first_mb = if mbaff {
            (header.first_mb_in_slice as usize).saturating_mul(2)
        } else {
            header.first_mb_in_slice as usize
        };
        if first_mb < total_mbs {
            make_ctx!().finalize_mb_info(first_mb, mb_idx.min(total_mbs), &params);
        }

        // Deblocking and DPB insertion happen in finalize_pending().
        ps.last_first_mb = header.first_mb_in_slice;
        ps.frame_num = header.frame_num;
        ps.poc = current_poc;
        ps.nal_unit_type = nal.nal_unit_type;
        ps.nal_ref_idc = nal.nal_ref_idc;
        ps.long_term_reference_flag = header.long_term_reference_flag;
        ps.delta_poc_bottom = header.delta_pic_order_cnt_bottom;
        // Motion data from I and P slices differs, so the picture only counts
        // as intra if all its slices were.
        let intra_slice = header.slice_type == SliceType::I;
        ps.is_intra_slice = intra_slice && (!is_continuation || ps.is_intra_slice);
        ps.mbaff_frame_flag = header.mbaff_frame_flag;
        ps.mmco_ops = header.mmco_ops;

        match slice_result {
            Ok(()) => {
                self.pending = Some(ps);
                Ok(())
            }
            Err(e) => {
                // What's decoded so far stays, and the rest gets concealed. A failed first
                // slice leaves all of it to conceal, which is still a picture on time.
                self.pending = Some(ps);
                Err(e)
            }
        }
    }
}

/// H.264 decoder with built-in display-order reordering.
///
/// Wraps [`Decoder`] with a reorder buffer that emits frames in display order
/// (sorted by picture order count) instead of decode order. This eliminates
/// the need for callers to track GOP boundaries and sort frames manually,
/// and avoids a common pitfall around IDR boundary handling.
///
/// Use this when you want to display, render, or write frames in their
/// intended visual order. Use [`Decoder`] directly if you need raw decode
/// order or want to manage reordering yourself.
///
/// # How it works
///
/// Internally tracks GOP boundaries (each IDR or POC reset starts a new GOP)
/// and buffers decoded frames. Frames are emitted in `(gop_id, pic_order_cnt)`
/// order. On each boundary, all buffered frames from the previous GOP are
/// drained and returned. The buffer depth follows the SPS, which bounds both
/// latency and memory for streams with infrequent IDRs.
///
/// # Example
///
/// ```no_run
/// use feos_h264::decoder::OrderedDecoder;
/// use feos_h264::nal::parse_annex_b;
///
/// let bitstream = std::fs::read("input.h264").unwrap();
/// let nals = parse_annex_b(&bitstream);
/// let mut decoder = OrderedDecoder::new();
///
/// for nal in &nals {
///     // decode_nal returns 0 or more frames in display order
///     for frame in decoder.decode_nal(nal).unwrap() {
///         println!("Display POC={}", frame.pic_order_cnt);
///     }
/// }
/// // Drain any remaining buffered frames
/// for frame in decoder.flush() {
///     println!("Final POC={}", frame.pic_order_cnt);
/// }
/// ```
pub struct OrderedDecoder {
    inner: Decoder,
    /// Buffered frames awaiting display-order release. Each entry is
    /// `(gop_id, frame)` so frames from different GOPs don't interleave.
    buffer: Vec<(u32, Frame)>,
    /// Monotonically incrementing GOP id, bumped on each IDR boundary.
    gop_id: u32,
    /// Maximum frames to keep in the buffer before forcing the lowest
    /// (oldest) one out, or `None` to follow the active SPS.
    max_depth: Option<usize>,
}

impl Default for OrderedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderedDecoder {
    /// Create a new ordering decoder whose reorder buffer depth follows the
    /// active SPS: its `max_num_reorder_frames`, otherwise the level's limit.
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    /// Create an ordering decoder that follows the active SPS and refuses
    /// streams beyond `limits`.
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            inner: Decoder::with_limits(limits),
            buffer: Vec::new(),
            gop_id: 0,
            max_depth: None,
        }
    }

    /// Create a new ordering decoder with a custom maximum buffer depth.
    /// Larger values give more reordering headroom for unusual bitstreams
    /// but increase end-to-end latency and memory use.
    pub fn with_max_depth(max_depth: usize) -> Self {
        Self {
            inner: Decoder::new(),
            buffer: Vec::new(),
            gop_id: 0,
            max_depth: Some(max_depth.max(1)),
        }
    }

    /// See [`Decoder::set_keep_native`].
    pub fn set_keep_native(&mut self, keep: bool) {
        self.inner.set_keep_native(keep);
    }

    /// Sets the tag recorded on pictures started from now on. See
    /// [`Frame::tag`].
    pub fn set_tag(&mut self, tag: u64) {
        self.inner.set_tag(tag);
    }

    /// Forgets all pictures so decoding can restart at an IDR, keeping the
    /// parameter sets.
    pub fn reset(&mut self) {
        self.inner.reset();
        self.buffer.clear();
        self.gop_id = 0;
    }

    /// Feed a single NAL unit and return any frames that are now ready
    /// for display, in display order.
    ///
    /// Most NALs return an empty `Vec` (parameter sets, mid-GOP slices that
    /// don't yet free up the head of the buffer). When the internal buffer
    /// fills up or when an IDR boundary completes a GOP, one or more frames
    /// are released.
    pub fn decode_nal(&mut self, nal: &NalUnit) -> Result<Vec<Frame>, DecodeError> {
        let mut output = Vec::new();

        // Decode the NAL. Note that decode_nal returns the PREVIOUS frame
        // (the picture that just got finalized when this NAL started a new
        // one).
        if let Some(frame) = self.inner.decode_nal(nal)? {
            self.push(frame, &mut output);
        }

        // Bound buffer depth to limit reorder latency.
        let depth = self.max_depth.unwrap_or(self.inner.reorder_depth());
        while self.buffer.len() > depth {
            output.push(self.pop_lowest());
        }

        Ok(output)
    }

    /// Buffers a frame. A picture that resets POC ends the previous GOP, and
    /// the whole of that GOP is displayed before it.
    fn push(&mut self, frame: Frame, output: &mut Vec<Frame>) {
        if frame.poc_reset {
            self.gop_id = self.gop_id.wrapping_add(1);
            self.drain_completed_gops(output);
        }
        self.buffer.push((self.gop_id, frame));
    }

    /// Flush all remaining buffered frames at end-of-stream, in display order.
    /// Call this once after the last `decode_nal`.
    pub fn flush(&mut self) -> Vec<Frame> {
        let mut output = Vec::new();
        // First, get any final pending frames from the inner decoder.
        while let Some(frame) = self.inner.flush() {
            self.push(frame, &mut output);
        }
        // Drain everything in (gop, poc) order.
        self.buffer.sort_by_key(|(g, f)| (*g, f.pic_order_cnt));
        output.extend(self.buffer.drain(..).map(|(_, f)| f));
        output
    }

    /// Frame rate from the most recently parsed SPS's VUI timing info,
    /// as `(numerator, denominator)`. See [`Decoder::frame_rate`] for details.
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        self.inner.frame_rate()
    }

    /// Frame rate as a single floating-point value.
    /// See [`Decoder::frame_rate_f64`].
    pub fn frame_rate_f64(&self) -> Option<f64> {
        self.inner.frame_rate_f64()
    }

    /// Drain all frames whose `gop_id < self.gop_id`, sorted by display order,
    /// into `output`.
    fn drain_completed_gops(&mut self, output: &mut Vec<Frame>) {
        let cur = self.gop_id;
        // Stable partition: keep current-GOP frames, extract older ones.
        let mut completed: Vec<(u32, Frame)> = Vec::new();
        let mut remaining: Vec<(u32, Frame)> = Vec::with_capacity(self.buffer.len());
        for entry in self.buffer.drain(..) {
            if entry.0 < cur {
                completed.push(entry);
            } else {
                remaining.push(entry);
            }
        }
        completed.sort_by_key(|(g, f)| (*g, f.pic_order_cnt));
        output.extend(completed.into_iter().map(|(_, f)| f));
        self.buffer = remaining;
    }

    /// Remove and return the buffered frame with the lowest `(gop, poc)`.
    fn pop_lowest(&mut self) -> Frame {
        let idx = self
            .buffer
            .iter()
            .enumerate()
            .min_by_key(|(_, (g, f))| (*g, f.pic_order_cnt))
            .map(|(i, _)| i)
            .unwrap();
        self.buffer.remove(idx).1
    }
}

// A later decode worker moves decoders and frames between tasks.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Decoder>();
    assert_send::<OrderedDecoder>();
    assert_send::<Frame>();
};

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;
    use crate::nal::parse_annex_b;
    use alloc::string::String;

    #[test]
    fn test_decode_single_idr_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output.len(), expected_yuv.len());
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_decode_multi_mb_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/multi_mb_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/multi_mb_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_decode_i4x4_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/i4x4_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/i4x4_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_decode_deblock_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/deblock_frame.h264"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        // Write decoded output for reference generation
        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output.len(), 6144);

        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/deblock_frame.yuv"
        ))
        .unwrap();
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_decode_mixed_i4x4_i16x16_frame() {
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/mixed_i4x4_frame.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/mixed_i4x4_frame.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 64);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, expected_yuv);
    }

    /// Helper to decode a test file and compare against reference YUV.
    fn decode_and_compare(h264_name: &str, expected_width: u32, expected_height: u32) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let yuv_path = format!("{}/testdata/{}.yuv", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));
        let expected_yuv = std::fs::read(&yuv_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", yuv_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frame = None;
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frame = Some(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frame = Some(f);
        }
        let frame = frame.expect("should have decoded a frame");

        assert_eq!(frame.width, expected_width);
        assert_eq!(frame.height, expected_height);

        let mut output = Vec::new();
        output.extend_from_slice(&frame.y);
        output.extend_from_slice(&frame.u);
        output.extend_from_slice(&frame.v);
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_gradient_48x32() {
        // 3x2 MBs, QP=24, mixed I4x4/I16x16 (66.7% I4x4), gradient luma + colored chroma
        decode_and_compare("gradient_48x32", 48, 32);
    }

    #[test]
    fn test_edges_32x32_qp10() {
        // 2x2 MBs, QP=10 (high quality), high-contrast 8-pixel bar pattern
        decode_and_compare("edges_32x32_qp10", 32, 32);
    }

    #[test]
    fn test_edges_32x32_qp35() {
        // 2x2 MBs, QP=35 (low quality, heavy quantization)
        decode_and_compare("edges_32x32_qp35", 32, 32);
    }

    #[test]
    fn test_smooth_80x48() {
        // 5x3 MBs, QP=22, gentle luma gradient with non-trivial chroma
        decode_and_compare("smooth_80x48", 80, 48);
    }

    #[test]
    fn test_noise_16x16_qp12() {
        // Single MB, QP=12, pseudo-random content stressing CAVLC with many non-zero coefficients
        decode_and_compare("noise_16x16_qp12", 16, 16);
    }

    #[test]
    fn test_scaling_list() {
        decode_and_compare("scaling_test", 32, 32);
    }

    #[test]
    fn test_p_frame() {
        // 32x32, 2 frames: IDR + P-slice with motion (bars shifted right)
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/p_frame_test.h264"
        ))
        .unwrap();
        let expected_yuv = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/p_frame_test.yuv"
        ))
        .unwrap();

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 2, "should decode 2 frames (IDR + P)");
        assert_eq!(frames[0].width, 32);
        assert_eq!(frames[1].width, 32);

        // Compare both frames concatenated
        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }
        assert_eq!(output, expected_yuv);
    }

    /// Helper to decode a multi-frame test and compare all frames against reference.
    fn decode_multiframe_and_compare(
        h264_name: &str,
        expected_frames: usize,
        expected_width: u32,
        expected_height: u32,
    ) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let yuv_path = format!("{}/testdata/{}.yuv", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));
        let expected_yuv = std::fs::read(&yuv_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", yuv_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        // Flush the last pending frame
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(
            frames.len(),
            expected_frames,
            "expected {} frames",
            expected_frames
        );
        for f in &frames {
            assert_eq!(f.width, expected_width);
            assert_eq!(f.height, expected_height);
        }

        // Sort frames by POC for display-order comparison
        // (reference YUV is in display order; decoder outputs in decode order)
        frames.sort_by_key(|f| f.pic_order_cnt);

        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }
        assert_eq!(output, expected_yuv);
    }

    #[test]
    fn test_p_multi_frame() {
        // 64x64, 4 frames: IDR + 3 P-frames with P16x16 (68.8%), P16x8/8x16 (14.6%),
        // I16x16-in-P (16.7%), moving diagonal gradient
        decode_multiframe_and_compare("p_multi_frame", 4, 64, 64);
    }

    #[test]
    fn test_p_skip_heavy() {
        // 64x32, 3 frames: IDR + 2 P with 50% skip, 37.5% I4x4-in-P, 12.5% P16x8/8x16,
        // mostly static with small moving region
        decode_multiframe_and_compare("p_skip_heavy", 3, 64, 32);
    }

    #[test]
    fn test_p_8x8() {
        // 64x64, 3 frames: IDR + 2P with P_8x8 (2.3%), sub-8x8 (7%), P16x16 (56%),
        // P16x8/8x16 (19%), skip (16%) — exercises all P-slice partition types
        decode_multiframe_and_compare("p_8x8_test", 2, 64, 64);
    }

    #[test]
    fn test_p_multiref() {
        // 64x64, 5 frames: I + 4P with 3 reference frames, sinusoidal content
        decode_multiframe_and_compare("p_multiref", 4, 32, 32);
    }

    #[test]
    fn test_b_l0_l1() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_L0_16x16
        // Main profile (required for B-frames with CAVLC).
        // Note: all-I4x4 IDR in Main profile triggers IDCT rounding differences
        // (H.264 Annex A allows ±1 per-pixel tolerance). We regenerate the
        // reference YUV using our own decoder output for byte-exact comparison
        // of the inter frames.
        decode_multiframe_and_compare("b_l0_l1_test", 5, 32, 32);
    }

    #[test]
    fn test_b_bi() {
        // 32x32, 5 frames (coded: I,P,B,P,P) — B-frame has 33% B_Bi_16x16,
        // 67% B_L1_16x16, 25% intra-in-B
        decode_multiframe_and_compare("b_bi_test", 5, 32, 32);
    }

    #[test]
    fn test_b_skip() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_Skip
        // (spatial direct mode)
        decode_multiframe_and_compare("b_skip_test", 5, 32, 32);
    }

    #[test]
    fn test_b_temporal() {
        // 32x32, 5 frames (coded: I,P,B,P,B) — B-frames use 100% B_Skip
        // (temporal direct mode)
        decode_multiframe_and_compare("b_temporal_test", 5, 32, 32);
    }

    #[test]
    fn test_b_partitions() {
        // 64x64, 5 frames (I,B,P,B,P) — B-frames with 37.5% B16x16,
        // 40.6% B16x8/8x16, 5.5% B_8x8, 15.6% direct, 87.9% Bi
        decode_multiframe_and_compare("b_parts_test", 5, 64, 64);
    }

    #[test]
    fn test_b_multi_frame() {
        // 64x64, 8 frames (I,B,P,B,P,B,P,P) — multiple B-frames across
        // the sequence with skip, direct, and various partition types
        decode_multiframe_and_compare("b_multi_test", 8, 64, 64);
    }

    #[test]
    fn test_b_hierarchical() {
        // 64x64, 8 frames with bframes=3, ref=2 — hierarchical B-frames
        // with reference B-frames and ref_pic_list_modification reordering
        decode_multiframe_and_compare("b_hier_test", 8, 64, 64);
    }

    #[test]
    fn test_cabac_i4x4() {
        // 16x16 single-MB CABAC I4x4 frame (Main profile)
        decode_and_compare("cabac_i4x4_test", 16, 16);
    }

    #[test]
    fn test_cabac_i16x16() {
        // 16x16 single-MB CABAC I16x16 DC frame (Main profile)
        decode_and_compare("cabac_i16x16_test", 16, 16);
    }

    #[test]
    fn test_cabac_mixed() {
        // 32x32 multi-MB CABAC I-frame with mixed I4x4/I16x16 (Main profile)
        decode_multiframe_and_compare("cabac_mixed_test", 1, 32, 32);
    }

    #[test]
    fn test_cabac_p_slice() {
        // 32x32, 3 frames: CABAC IDR + 2 P-frames (100% P_L0_16x16)
        decode_multiframe_and_compare("cabac_p_test", 3, 32, 32);
    }

    #[test]
    fn test_cabac_p_parts() {
        // 64x64, 5 frames: CABAC P with P16x16 (25%) + P16x8 (29.7%) + P8x16 (20.3%) +
        // P_8x8 (6.6%) + P_4x4 sub-partitions (5.9%) + skip (10.9%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_p_parts_test", 5, 64, 64);
    }

    #[test]
    fn test_cabac_intra_in_p() {
        // 64x64, 2 frames: CABAC IDR + P-frame with I16x16-in-P (12.5%) +
        // P_L0_16x16 (75%) + P_8x8 (6.25%) + skip (6.25%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_intra_p_test", 2, 64, 64);
    }

    #[test]
    fn test_cabac_b_slice() {
        // 32x32, 15 frames: CABAC B-frames with B_L0_16x16, B_L1_16x16, B_Skip
        // (--no-deblock, spatial direct), byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_test", 15, 32, 32);
    }

    #[test]
    fn test_cabac_b_parts() {
        // 64x64, 10 frames: CABAC B-frames with B16x16 (15.6%) + B16x8 (25%) +
        // B8x16/8x8 (19.5%) + B_Direct spatial (18.8%) + B_Skip (21.9%),
        // L0/L1/Bi mix, P_8x8 sub-partitions, --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_parts_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_intra_in_b() {
        // 64x64, 10 frames: CABAC B-frames with I16x16-in-B (6.2%) + B16x16 (25%) +
        // B_Direct (68.8%) + Bi (58.3%), noisy content, --no-deblock,
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_intra_b_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_b_temporal() {
        // 64x64, 10 frames: CABAC B-frames with temporal direct mode (20.3%) +
        // B16x16 (59.4%) + B_Skip (20.3%), L0/L1/Bi mix, --no-deblock,
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_b_temporal_test", 10, 64, 64);
    }

    #[test]
    fn test_cabac_high_profile() {
        // 64x64, 5 frames: CABAC High profile with 8x8 transform (43.8% inter 8x8),
        // P-only, --no-deblock, medium preset, byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_high_test", 5, 64, 64);
    }

    #[test]
    fn test_cabac_i8x8() {
        // 64x64, 1 frame: CABAC High profile I-slice with 100% I8x8 (DC mode)
        // + varied chroma (dc 6%, h 19%, v 38%, plane 38%).
        // Validates I8x8 chroma decode in the CABAC I-slice path.
        decode_multiframe_and_compare("cabac_i8x8_test", 1, 64, 64);
    }

    #[test]
    fn test_cabac_multiref() {
        // 64x64, 5 frames: CABAC Main profile with ref=2, bframes=1, me=hex,
        // --no-deblock, --no-weightb, qp=26. Exercises CABAC multiref with
        // ref_pic_list_modification and P_L0_L0_16x8 partitions using ref_idx>0.
        decode_multiframe_and_compare("cabac_multiref_test", 5, 64, 64);
    }

    #[test]
    fn test_preset_medium() {
        // 320x240, 60 frames: x264 --preset medium --profile main --no-deblock.
        // CABAC, ref=4, bframes=3, subme=7, me=hex, all partitions.
        // Exercises P_8x8 sub-partitions with multiref, B 16x8/8x16,
        // ref_pic_list_modification, and hierarchical B-frames.
        decode_multiframe_and_compare("preset_medium", 60, 320, 240);
    }

    #[test]
    fn test_preset_medium_deblock() {
        // 320x240, 60 frames: x264 --preset medium --profile main (deblocking ON).
        // Full pipeline: CABAC, ref=4, bframes=3, all partitions, deblocking.
        decode_multiframe_and_compare("preset_medium_deblock", 60, 320, 240);
    }

    #[test]
    fn test_cabac_deblock() {
        // 64x64, 3 frames: CABAC Main profile with deblocking enabled,
        // P_L0_16x16 (84.4%) + I-in-P (12.5%) + skip (3.1%), byte-exact against FFmpeg
        decode_multiframe_and_compare("cabac_deblock_test", 3, 64, 64);
    }

    #[test]
    fn test_deblock_b_frames() {
        // 64x64, 9 frames: CAVLC Main profile with B-frames (bframes=2) and deblocking,
        // B16x16 (2.5%) + B_Direct (10%) + B_Skip (87.5%) + I-in-P (31.3%),
        // byte-exact against FFmpeg
        decode_multiframe_and_compare("deblock_b_test", 9, 64, 64);
    }

    #[test]
    fn test_deblock_b_inter() {
        // 64x64, 5 frames: CAVLC Main profile B-frames with deblocking,
        // B16x16 L0/L1/Bi (78.1%) + B_Direct (12.5%) + B_Skip (9.4%),
        // exercises cross-list deblock bS comparison, byte-exact against FFmpeg
        decode_multiframe_and_compare("deblock_b_inter_test", 5, 64, 64);
    }

    #[test]
    fn test_weighted_p() {
        // 32x32, 10 frames: CAVLC P with explicit weighted prediction (100% weighted,
        // 77.8% chroma weighted), fading content, --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("weighted_p_test", 10, 32, 32);
    }

    #[test]
    fn test_weighted_b_implicit() {
        // 64x64, 10 frames: CABAC B with implicit weighted bi-prediction (idc=2),
        // fading content, B16x16 (25%) + B_Direct (48.4%) + B_Skip (26.6%),
        // --no-deblock, byte-exact against FFmpeg
        decode_multiframe_and_compare("weighted_b_test", 10, 64, 64);
    }

    #[test]
    fn test_realworld() {
        // 320x240, 6 frames: CAVLC Main profile, P16x16 (16.6%) + P16x8 (7.8%) +
        // P8x16 (3.1%) + intra-in-P (3.6%) + skip (68.9%), --no-deblock.
        // Regression test for real-world-sized content with diverse MB types.
        decode_multiframe_and_compare("realworld_test", 6, 320, 240);
    }

    #[test]
    fn test_high_profile() {
        // 320x240, 6 frames: CAVLC High profile with 8x8 transform
        // (28% intra 8x8, 22.8% inter 8x8), --no-deblock.
        decode_multiframe_and_compare("high_profile_test", 6, 320, 240);
    }

    #[test]
    fn test_realworld_b() {
        // 320x240, 9 frames: CAVLC Main profile with B-frames (bframes=2),
        // B16x16 L0/L1/Bi (16.2%) + B16x8/8x16 (7.2%) + B_Direct (2.8%) +
        // B_Skip (73.5%) + P partitions + intra-in-P/B, --no-deblock.
        decode_multiframe_and_compare("realworld_b_test", 9, 320, 240);
    }

    #[test]
    fn test_multislice_cabac_i() {
        // 32x32, 1 frame, 2 slices (1 MB row each): CABAC Main profile I-frame.
        // Tests cross-slice intra prediction boundary handling (spec 6.4.1).
        decode_and_compare("ms_cabac_i_test", 32, 32);
    }

    #[test]
    fn test_multislice_cabac_i4() {
        // 64x64, 1 frame, 4 slices (1 MB row each): CABAC Main profile I-frame.
        // Tests multiple slice boundaries with I4x4 prediction.
        decode_and_compare("ms_cabac_i4_test", 64, 64);
    }

    #[test]
    fn test_multislice_cavlc_i() {
        // 32x32, 1 frame, 2 slices (1 MB row each): CAVLC Baseline profile I-frame.
        // Tests cross-slice nC computation and intra prediction for CAVLC.
        decode_and_compare("ms_cavlc_i_test", 32, 32);
    }

    #[test]
    fn test_multislice_cavlc_p() {
        // 64x64, 5 frames (IDR + 4 P), 4 slices per frame: CAVLC Main profile.
        // Tests multi-slice P-frame decode with cross-slice intra prediction
        // and nC boundary handling across both I and P slices.
        decode_multiframe_and_compare("ms_cavlc_p_test", 5, 64, 64);
    }

    #[test]
    fn test_multislice_cabac_p() {
        // 64x64, 5 frames (IDR + 4 P), 4 slices per frame: CABAC Main profile,
        // no deblocking. Tests multi-slice P-frame CABAC decode with cross-slice
        // intra prediction and CABAC neighbor context boundary handling.
        decode_multiframe_and_compare("ms_cabac_p_test", 5, 64, 64);
    }

    #[test]
    fn test_multislice_cabac_b() {
        // 64x64, 4 frames (IDR + B + B + P), 4 slices per frame: CABAC Main profile,
        // no deblocking, temporal+spatial direct, B_8x8 sub-partitions.
        // Tests multi-slice B-frame decode with cross-slice boundary handling.
        decode_multiframe_and_compare("ms_cabac_b_test", 4, 64, 64);
    }

    #[test]
    fn test_high_p8x8_sub4x4() {
        // 64x64, 6 frames (I+P): High profile, preset slower with P_8x8
        // sub-4x4 partitions + 8x8dct. Tests noSubMbPartSizeLessThan8x8Flag:
        // transform_size_8x8_flag must NOT be read when P_8x8 has sub-4x4
        // sub-partitions.
        decode_multiframe_and_compare("high_p8x8_sub4x4_test", 6, 64, 64);
    }

    #[test]
    fn test_b_temporal_direct_8x8_inference() {
        // 64x64, 4 frames (I,B,B,P): CABAC Main, preset slower, direct=temporal.
        // Tests direct_8x8_inference_flag in temporal direct mode: co-located MV
        // must be read from the representative 4x4 block per 8x8 group, not from
        // each individual 4x4 block.
        decode_multiframe_and_compare("b_temporal_direct_test", 4, 64, 64);
    }

    #[test]
    fn test_high_b_slower() {
        // 64x64, 10 frames: High profile, preset slower, ref=2, bframes=2,
        // 8x8dct, no-deblock. Tests direct_8x8_inference_flag in BOTH temporal
        // and spatial direct modes, noSubMbPartSizeLessThan8x8Flag for
        // transform_size_8x8_flag, and B_8x8 with sub-partition types.
        decode_multiframe_and_compare("high_b_slower_test", 10, 64, 64);
    }

    #[test]
    fn test_high_cavlc_b() {
        // 64x64, 10 frames: CAVLC High profile with bframes=2, ref=2, 8x8dct,
        // all partitions, no-deblock. Validates CAVLC B-frame 8x8 transform path.
        decode_multiframe_and_compare("high_cavlc_b_test", 10, 64, 64);
    }

    #[test]
    fn test_ms_deblock_b_cabac() {
        // 64x64, 8 frames: CABAC Main profile with bframes=2, ref=2, 4 slices,
        // deblocking ON. Tests B-slice CABAC ref_idx context with direct-mode
        // neighbors, MV/MVD zeroing for inactive prediction lists, and
        // multi-slice deblocking.
        decode_multiframe_and_compare("ms_deblock_b_cabac_test", 8, 64, 64);
    }

    #[test]
    fn test_ms_cavlc_b() {
        // 64x64, 8 frames: CAVLC Main profile, bframes=2, ref=2, 4 slices,
        // no-deblock. Tests CAVLC multi-slice B-frame decode.
        decode_multiframe_and_compare("ms_cavlc_b_test", 8, 64, 64);
    }

    #[test]
    fn test_cavlc_deblock_pb() {
        // 64x64, 8 frames: CAVLC Main profile, bframes=1, ref=1,
        // deblocking ON. Tests CAVLC P+B with deblocking filter.
        decode_multiframe_and_compare("cavlc_deblock_pb_test", 8, 64, 64);
    }

    #[test]
    fn test_unaligned_resolution() {
        // 100x76, 6 frames: CABAC High profile, bframes=1, ref=1,
        // no-deblock. Tests non-16-aligned dimensions (coded 112x80).
        decode_multiframe_and_compare("unaligned_100x76_test", 6, 100, 76);
    }

    #[test]
    fn test_cabac_weighted_p() {
        // 64x64, 8 frames: CABAC Main profile, 100% weighted P (fading),
        // bframes=0, ref=2, no-deblock. Tests CABAC explicit weighted P-slice.
        decode_multiframe_and_compare("cabac_weighted_p_test", 8, 64, 64);
    }

    #[test]
    fn test_cavlc_i8x8() {
        // 64x64, 3 frames: CAVLC High profile, all-intra (keyint=1), 8x8dct,
        // no-deblock. Tests CAVLC I8x8 intra prediction with 8x8 transform.
        decode_multiframe_and_compare("cavlc_i8x8_test", 3, 64, 64);
    }

    #[test]
    fn test_cabac_b8x8_direct() {
        // 64x64, 8 frames: CABAC Main profile, constrained_intra_pred_flag=1,
        // bframes=2, ref=2, no-deblock. All-B_8x8 MBs with B_Direct_8x8
        // sub-partitions. Tests per-block direct flag for ref_idx CABAC context.
        decode_multiframe_and_compare("cabac_b8x8_direct_test", 8, 64, 64);
    }

    #[test]
    fn test_constrained_intra() {
        // 64x64, 8 frames: CABAC Main profile, constrained_intra_pred_flag=1,
        // bframes=1, ref=2, no-deblock. Tests that intra MBs in P/B slices
        // treat inter-predicted neighbors as unavailable (spec 8.3.1).
        decode_multiframe_and_compare("constrained_intra_test", 8, 64, 64);
    }

    #[test]
    fn test_high_preset_medium() {
        // 320x240, 30 frames: CABAC High profile, bframes=3, ref=4, 8x8dct
        // (92% intra, 98% inter), no-deblock. Large-resolution stress test
        // with all High profile features active.
        decode_multiframe_and_compare("high_preset_medium_test", 30, 320, 240);
    }

    #[test]
    fn test_high_deblock_medium() {
        // 320x240, 30 frames: CABAC High profile, bframes=3, ref=4, 8x8dct,
        // deblocking ON. Tests 8x8 transform deblocking (skip internal odd
        // edges per spec 8.7.2.1).
        decode_multiframe_and_compare("high_deblock_medium_test", 30, 320, 240);
    }

    #[test]
    fn test_jm_ltr_cavlc() {
        // 64x64, 8 frames: JM encoder, Baseline profile, CAVLC,
        // SetFirstAsLongTerm=1, ref=2. Tests MMCO long-term reference
        // (IDR marked as LT via long_term_reference_flag).
        decode_multiframe_and_compare("jm_ltr_cavlc_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_ltr_cabac() {
        // 64x64, 8 frames: JM encoder, Main profile, CABAC,
        // SetFirstAsLongTerm=1, ref=2. Tests MMCO long-term reference
        // with CABAC entropy coding.
        decode_multiframe_and_compare("jm_ltr_cabac_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_weighted_b_explicit() {
        // 64x64, 8 frames: JM encoder, Main profile, CABAC,
        // weighted_bipred_idc=1 (explicit weighted B), bframes=1.
        // Tests explicit weighted bi-prediction for B-slices.
        decode_multiframe_and_compare("jm_weighted_b_explicit_test", 8, 64, 64);
    }

    #[test]
    fn test_jm_ipcm_cavlc() {
        // 32x32, 4 frames: JM encoder, Baseline profile, CAVLC, QP=0,
        // I_PCM macroblocks with random content. Tests CAVLC I_PCM decode.
        decode_multiframe_and_compare("jm_ipcm_cavlc_test", 4, 32, 32);
    }

    #[test]
    fn test_jm_ipcm_cabac() {
        // 32x32, 4 frames: JM encoder, Main profile, CABAC, QP=0,
        // I_PCM macroblocks with random content. Tests CABAC I_PCM decode
        // with engine reinit and is_i16x16 context flag for I_PCM neighbors.
        decode_multiframe_and_compare("jm_ipcm_cabac_test", 4, 32, 32);
    }

    #[test]
    fn test_jm_poc_type1() {
        // 64x64, 6 frames: JM encoder, Baseline profile, CAVLC,
        // pic_order_cnt_type=1 (delta-based POC). Tests POC type 1 computation.
        decode_multiframe_and_compare("jm_poc_type1_test", 6, 64, 64);
    }

    #[test]
    fn test_jm_poc_type2() {
        // 64x64, 6 frames: JM encoder, Baseline profile, CAVLC,
        // pic_order_cnt_type=2 (frame_num-derived POC). Tests POC type 2 computation.
        decode_multiframe_and_compare("jm_poc_type2_test", 6, 64, 64);
    }

    /// Decode a multi-frame stream and compare output YUV SHA-256 hash.
    /// Used for large-resolution tests where storing the full reference YUV
    /// would be too expensive.
    fn decode_and_compare_hash(
        h264_name: &str,
        expected_frames: usize,
        expected_width: u32,
        expected_height: u32,
        expected_sha256: &str,
    ) {
        let h264_path = format!("{}/testdata/{}.h264", env!("CARGO_MANIFEST_DIR"), h264_name);
        let h264_data = std::fs::read(&h264_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", h264_path, e));

        let nals = parse_annex_b(&h264_data);
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for nal in &nals {
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push(f);
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push(f);
        }
        assert_eq!(
            frames.len(),
            expected_frames,
            "expected {} frames",
            expected_frames
        );
        for f in &frames {
            assert_eq!(f.width, expected_width);
            assert_eq!(f.height, expected_height);
        }

        // Sort by POC for display-order comparison
        frames.sort_by_key(|f| f.pic_order_cnt);

        // Build output YUV and compute SHA-256
        // Use a simple FNV-style hash to avoid pulling in sha2 crate;
        // we use two independent hashes to get collision resistance.
        let mut output = Vec::new();
        for frame in &frames {
            output.extend_from_slice(&frame.y);
            output.extend_from_slice(&frame.u);
            output.extend_from_slice(&frame.v);
        }

        // Compute SHA-256 using the same algorithm as `shasum -a 256`
        // Implemented inline to avoid external dependencies.
        let hash = sha256(&output);
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(hex, expected_sha256, "SHA-256 mismatch for {}", h264_name);
    }

    /// Minimal SHA-256 implementation (FIPS 180-4) for test use only.
    fn sha256(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];

        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];

        // Padding
        let bit_len = (data.len() as u64) * 8;
        let mut padded = data.to_vec();
        padded.push(0x80);
        while (padded.len() % 64) != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in padded.chunks_exact(64) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[4 * i],
                    chunk[4 * i + 1],
                    chunk[4 * i + 2],
                    chunk[4 * i + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }

            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }

            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }

        let mut result = [0u8; 32];
        for (i, &val) in h.iter().enumerate() {
            result[4 * i..4 * i + 4].copy_from_slice(&val.to_be_bytes());
        }
        result
    }

    #[test]
    fn test_1080p() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CABAC, bframes=3, ref=2, no-deblock. 6 B-frames, 3 P-frames, 1 IDR.
        // Hash-based comparison to avoid storing 30MB reference YUV.
        decode_and_compare_hash(
            "1080p_test",
            10,
            1920,
            1080,
            "e795b2188acd5a6f4b819f588e388ab3af1357edb36a62a5f53ba24fbd9a57d4",
        );
    }

    #[test]
    fn test_1080p_deblock() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CABAC, bframes=3, ref=2, deblocking ON.
        decode_and_compare_hash(
            "1080p_deblock_test",
            10,
            1920,
            1080,
            "cb4ebf9c0e470717c7c2f0dd29f1afca172c362414803ee7a132844dd827e3f5",
        );
    }

    #[test]
    fn test_1080p_cavlc() {
        // 1920x1080, 10 frames: mandelbrot source, x264 --preset medium,
        // CAVLC, bframes=3, ref=2, no-deblock.
        decode_and_compare_hash(
            "1080p_cavlc_test",
            10,
            1920,
            1080,
            "d53999477dacff0905a38a3c1ff4e3ca210b634f968cf56c914b76a08eee99da",
        );
    }

    /// Verify that OrderedDecoder produces the same display-order output as
    /// manually decoding with Decoder + sorting by (idr_count, poc). Uses
    /// preset_medium (320x240, 60 frames, ref=4, bframes=3) which exercises
    /// B-frame reordering across multiple GOPs.
    #[test]
    fn test_ordered_decoder_matches_manual_sort() {
        let h264_path = format!("{}/testdata/preset_medium.h264", env!("CARGO_MANIFEST_DIR"));
        let h264_data = std::fs::read(&h264_path).unwrap();
        let nals = parse_annex_b(&h264_data);

        // Reference: manually decode + sort by (idr_count, poc)
        let mut decoder = Decoder::new();
        let mut idr_count: u32 = 0;
        let mut frames: Vec<(u32, i32, Frame)> = Vec::new();
        for nal in &nals {
            let is_idr = nal.nal_unit_type == NalUnitType::SliceIdr;
            if let Some(f) = decoder.decode_nal(nal).unwrap() {
                frames.push((idr_count, f.pic_order_cnt, f));
            }
            if is_idr {
                idr_count += 1;
            }
        }
        if let Some(f) = decoder.flush() {
            frames.push((idr_count, f.pic_order_cnt, f));
        }
        frames.sort_by_key(|(idr, poc, _)| (*idr, *poc));
        let manual_yuv: Vec<u8> = frames
            .iter()
            .flat_map(|(_, _, f)| {
                let mut v = Vec::new();
                v.extend_from_slice(&f.y);
                v.extend_from_slice(&f.u);
                v.extend_from_slice(&f.v);
                v
            })
            .collect();

        // OrderedDecoder: same NALs, automatic ordering
        let mut ordered = OrderedDecoder::new();
        let mut ordered_frames: Vec<Frame> = Vec::new();
        for nal in &nals {
            ordered_frames.extend(ordered.decode_nal(nal).unwrap());
        }
        ordered_frames.extend(ordered.flush());
        let ordered_yuv: Vec<u8> = ordered_frames
            .iter()
            .flat_map(|f| {
                let mut v = Vec::new();
                v.extend_from_slice(&f.y);
                v.extend_from_slice(&f.u);
                v.extend_from_slice(&f.v);
                v
            })
            .collect();

        assert_eq!(ordered_frames.len(), frames.len(), "frame count mismatch");
        assert_eq!(
            ordered_yuv, manual_yuv,
            "OrderedDecoder output differs from manual sort"
        );
    }

    /// Decode an Annex B fuzz regression file. Must not panic.
    fn fuzz_decode_annex_b(name: &str) {
        let path = format!(
            "{}/testdata/fuzz_regressions/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let data = std::fs::read(&path).unwrap();
        let nals = parse_annex_b(&data);
        let mut decoder = Decoder::new();
        for nal in &nals {
            let _ = decoder.decode_nal(nal);
        }
        let _ = decoder.flush();
    }

    /// Decode an AVCC fuzz regression file. The first byte selects the
    /// avcC/sample split point, mirroring the `decode_avcc` fuzz target.
    /// Must not panic.
    fn fuzz_decode_avcc(name: &str) {
        let path = format!(
            "{}/testdata/fuzz_regressions/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let data = std::fs::read(&path).unwrap();
        if data.len() < 2 {
            return;
        }
        let split = (data[0] as usize).min(data.len() - 1);
        let avcc_box = &data[1..1 + split];
        let sample_data = &data[1 + split..];
        let cfg = match crate::nal::parse_avcc_config(avcc_box) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut decoder = Decoder::new();
        for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
            let _ = decoder.decode_nal(nal);
        }
        for nal in crate::nal::parse_avcc(sample_data, cfg.length_size) {
            let _ = decoder.decode_nal(&nal);
        }
        let _ = decoder.flush();
    }

    // --- Fuzz regression tests ---
    // Each test replays a crash input found by libFuzzer and verifies no panic.

    /// Empty ref_pic_list underflow (decode_cabac.rs / decode_cavlc.rs)
    #[test]
    fn test_fuzz_regression_empty_ref_list_no_panic() {
        fuzz_decode_annex_b("decode_annex_b_subtract_overflow.h264");
    }

    /// cabac_init_idc out of range (cabac.rs / slice.rs)
    #[test]
    fn test_fuzz_regression_cabac_init_idc_out_of_range() {
        fuzz_decode_avcc("decode_avcc_cabac_init_idc_oob.bin");
    }

    /// CABAC reader init past end of buffer (cabac.rs)
    #[test]
    fn test_fuzz_regression_cabac_init_past_end() {
        fuzz_decode_avcc("decode_avcc_cabac_init_oob.bin");
    }

    /// CAVLC coefficient position underflow (cavlc.rs)
    #[test]
    fn test_fuzz_regression_cavlc_pos_underflow() {
        fuzz_decode_annex_b("decode_annex_b_cavlc_pos_underflow.h264");
    }

    /// log2_weight_denom shift overflow (slice.rs)
    #[test]
    fn test_fuzz_regression_weight_denom_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_weight_denom_overflow.bin");
    }

    /// ref_pic_safe empty list underflow (mv_pred.rs)
    #[test]
    fn test_fuzz_regression_ref_pic_safe_empty_list() {
        fuzz_decode_annex_b("decode_annex_b_ref_pic_safe_empty.h264");
    }

    /// Temporal direct mode empty L1 list (slice_context.rs)
    #[test]
    fn test_fuzz_regression_temporal_direct_empty_l1() {
        fuzz_decode_avcc("decode_avcc_temporal_direct_empty_l1.bin");
    }

    /// RPLM pic_num modular arithmetic underflow (dpb.rs)
    #[test]
    fn test_fuzz_regression_rplm_pic_num_underflow() {
        fuzz_decode_annex_b("decode_annex_b_rplm_pic_num_underflow.h264");
    }

    /// Dequant multiply overflow (residual.rs / neighbor.rs)
    #[test]
    fn test_fuzz_regression_dequant_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_dequant_mul_overflow.bin");
    }

    /// Deblock QP average add overflow (deblock.rs)
    #[test]
    fn test_fuzz_regression_deblock_qp_overflow() {
        fuzz_decode_avcc("decode_avcc_deblock_qp_overflow.bin");
    }

    /// MC output buffer overrun from malformed block size (inter_pred.rs)
    #[test]
    fn test_fuzz_regression_mc_output_overrun() {
        fuzz_decode_annex_b("decode_annex_b_mc_output_overrun.h264");
    }

    /// SPS crop offset subtraction underflow (sps.rs)
    #[test]
    fn test_fuzz_regression_sps_crop_underflow() {
        fuzz_decode_avcc("decode_avcc_sps_crop_underflow.bin");
    }

    /// CABAC I_PCM frame buffer index out of bounds (decode_cabac.rs:2934)
    #[test]
    fn test_fuzz_regression_cabac_ipcm_frame_overrun() {
        fuzz_decode_annex_b("bug15_cabac_ipcm_frame_overrun.bin");
    }

    /// Divide by zero when coded width is zero (decoder.rs:357)
    #[test]
    fn test_fuzz_regression_divide_by_zero_coded_width() {
        fuzz_decode_avcc("bug16_divide_by_zero_coded_width.bin");
    }

    /// CABAC I_PCM in P/B-slice frame buffer overrun (decode_cabac.rs:797)
    #[test]
    fn test_fuzz_regression_cabac_ipcm_pb_frame_overrun() {
        fuzz_decode_annex_b("cabac_ipcm_pb_frame_overrun.bin");
    }

    /// Divide by zero when coded width is zero — variant 2 (decoder.rs:357)
    #[test]
    fn test_fuzz_regression_divide_by_zero_coded_width_2() {
        fuzz_decode_avcc("decode_avcc_divide_by_zero_2.bin");
    }

    /// SPS width() multiply overflow (sps.rs:124)
    #[test]
    fn test_fuzz_regression_sps_width_overflow() {
        fuzz_decode_avcc("decode_avcc_sps_width_overflow.bin");
    }

    /// Chroma MC ref_plane index out of bounds (inter_pred.rs:813)
    #[test]
    fn test_fuzz_regression_chroma_mc_ref_plane_overrun() {
        fuzz_decode_annex_b("chroma_mc_ref_plane_overrun.bin");
    }

    /// Deblock filter frame buffer overrun (deblock.rs:543)
    #[test]
    fn test_fuzz_regression_deblock_frame_overrun() {
        fuzz_decode_avcc("decode_avcc_deblock_overrun.bin");
    }

    /// CABAC reinit index out of bounds (cabac.rs:219)
    #[test]
    fn test_fuzz_regression_cabac_reinit_overrun() {
        fuzz_decode_annex_b("cabac_reinit_overrun.bin");
    }

    /// CAVLC B sub_mb_type table overrun (decode_cavlc.rs:448)
    #[test]
    fn test_fuzz_regression_cavlc_b_sub_mb_type_overrun() {
        fuzz_decode_annex_b("cavlc_b_sub_mb_type_overrun.bin");
    }

    /// first_mb_in_slice exceeds total MBs (decoder.rs:754)
    #[test]
    fn test_fuzz_regression_first_mb_overrun() {
        fuzz_decode_avcc("decode_avcc_first_mb_overrun.bin");
    }

    /// POC LSB shift overflow (dpb.rs:329)
    #[test]
    fn test_fuzz_regression_poc_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_shift_overflow.bin");
    }

    /// Frame crop slice overrun (decoder.rs:368)
    #[test]
    fn test_fuzz_regression_crop_overrun() {
        fuzz_decode_avcc("decode_avcc_crop_overrun.bin");
    }

    /// max_pic_num shift overflow (decoder.rs:426)
    #[test]
    fn test_fuzz_regression_frame_num_shift_overflow() {
        fuzz_decode_avcc("decode_avcc_frame_num_shift_overflow.bin");
    }

    /// Deblock multiply overflow (deblock.rs:113)
    #[test]
    fn test_fuzz_regression_deblock_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_deblock_mul_overflow.bin");
    }

    /// Continuation slice with SPS dimension mismatch (decoder.rs:763)
    #[test]
    fn test_fuzz_regression_continuation_mismatch() {
        fuzz_decode_avcc("decode_avcc_continuation_mismatch.bin");
    }

    /// SPS scaling list delta add overflow (sps.rs:396)
    #[test]
    fn test_fuzz_regression_scaling_list_overflow() {
        fuzz_decode_avcc("decode_avcc_scaling_list_overflow.bin");
    }

    /// Dequant DC multiply overflow (residual.rs:150)
    #[test]
    fn test_fuzz_regression_dequant_dc_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_dequant_dc_mul_overflow.bin");
    }

    /// POC MSB subtract overflow (dpb.rs:338)
    #[test]
    fn test_fuzz_regression_poc_msb_sub_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_msb_sub_overflow.bin");
    }

    /// 8x8 IDCT subtract overflow (residual.rs:272)
    #[test]
    fn test_fuzz_regression_idct8x8_sub_overflow() {
        fuzz_decode_annex_b("idct8x8_sub_overflow.bin");
    }

    /// 4x4 IDCT add overflow (residual.rs:77)
    #[test]
    fn test_fuzz_regression_idct4x4_add_overflow() {
        fuzz_decode_avcc("decode_avcc_idct4x4_add_overflow.bin");
    }

    /// POC type 1 cycle multiply overflow (dpb.rs:378)
    #[test]
    fn test_fuzz_regression_poc_type1_mul_overflow() {
        fuzz_decode_avcc("decode_avcc_poc_type1_mul_overflow.bin");
    }

    /// MBAFF CAVLC test (64x64, 6-frame, interlaced, CAVLC, frame-coded pairs)
    #[test]
    fn test_mbaff_cavlc() {
        decode_multiframe_and_compare("mbaff_cavlc_test", 6, 64, 64);
    }

    /// MBAFF CAVLC P-frames with complex content (64x64, 8 frames, testsrc2)
    #[test]
    fn test_mbaff_p_cavlc_8f() {
        decode_multiframe_and_compare("mbaff_p_cavlc_8f_test", 8, 64, 64);
    }

    /// MBAFF High profile CAVLC with 8x8 transform (64x64, 6 frames, testsrc2)
    #[test]
    fn test_mbaff_high_cavlc() {
        decode_multiframe_and_compare("mbaff_high_cavlc_test", 6, 64, 64);
    }

    /// MBAFF CABAC I-frame (32x32, 1 frame, testsrc2, High profile)
    #[test]
    fn test_mbaff_cabac_i() {
        decode_multiframe_and_compare("mbaff_cabac_i_test", 1, 32, 32);
    }

    /// MBAFF CABAC I-frame 64x64 (1 frame, testsrc2, High profile)
    #[test]
    fn test_mbaff_cabac_64() {
        decode_multiframe_and_compare("mbaff_cabac_64_test", 1, 64, 64);
    }

    /// MBAFF CABAC P-frames (64x64, 5 frames, testsrc2, keyint=5)
    #[test]
    fn test_mbaff_cabac_p() {
        decode_multiframe_and_compare("mbaff_cabac_p_test", 5, 64, 64);
    }

    /// MBAFF CABAC B-frames (64x64, 8 frames, testsrc2, bframes=2 ref=2)
    #[test]
    fn test_mbaff_cabac_b() {
        decode_multiframe_and_compare("mbaff_cabac_b_test", 8, 64, 64);
    }

    /// MBAFF CAVLC B-frames (64x64, 8 frames, testsrc2, bframes=2 ref=2)
    #[test]
    fn test_mbaff_cavlc_b() {
        decode_multiframe_and_compare("mbaff_cavlc_b_test", 8, 64, 64);
    }

    /// MBAFF CAVLC deblocking (64x64, 8 frames, Main profile, P-only, deblock ON)
    #[test]
    fn test_mbaff_deblock_cavlc() {
        decode_multiframe_and_compare("mbaff_deblock_cavlc_test", 8, 64, 64);
    }

    /// MBAFF CABAC deblocking with B-frames (64x64, 8 frames, Main profile, bframes=2 ref=2)
    #[test]
    fn test_mbaff_deblock_cabac() {
        decode_multiframe_and_compare("mbaff_deblock_cabac_test", 8, 64, 64);
    }

    /// MBAFF High profile with 8x8 DCT + deblocking + B-frames (64x64, 8 frames)
    #[test]
    fn test_mbaff_high_deblock() {
        decode_multiframe_and_compare("mbaff_high_deblock_test", 8, 64, 64);
    }

    /// MBAFF field-coded I-frames (64x64, 3 frames, all-field-coded pairs, keyint=1)
    #[test]
    fn test_mbaff_field_i() {
        decode_multiframe_and_compare("mbaff_field_i_test", 3, 64, 64);
    }

    /// MBAFF field-coded P-frames CAVLC (64x64, 4 frames, all-field-coded pairs)
    #[test]
    fn test_mbaff_field_p() {
        decode_multiframe_and_compare("mbaff_field_p_test", 4, 64, 64);
    }

    /// MBAFF field-coded CABAC (64x64, 4 frames, all-field-coded pairs, Main profile)
    #[test]
    fn test_mbaff_field_cabac() {
        decode_multiframe_and_compare("mbaff_field_cabac_test", 4, 64, 64);
    }

    /// MBAFF field-coded High profile CAVLC with 8x8 DCT (64x64, 4 frames, all-field-coded)
    #[test]
    fn test_mbaff_field_high() {
        decode_multiframe_and_compare("mbaff_field_high_test", 4, 64, 64);
    }
}
