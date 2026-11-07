//! A VP8 decoder, written for feos from RFC 6386 and checked against FFmpeg's output.
//!
//! The decoder is plain safe Rust, and nothing in a stream can make it read out of
//! bounds: the bool decoder reads zeros past its partition's end, which is how the
//! format says to treat it, and motion vectors reach reference pictures through a
//! clamped copy when they point near an edge. Memory is claimed fallibly and only when
//! a key frame changes the size. Past that, decoding allocates nothing.
//!
//! Frames are decoded a macroblock at a time into a planar picture, then the loop filter
//! runs over the whole picture. It has to come after: intra prediction reads the
//! unfiltered neighbours.

mod bool_decoder;
mod filter;
mod predict;
mod tables;

use alloc::vec::Vec;

use bool_decoder::BoolDecoder;
use tables::*;

use crate::error::{Result, VideoError};
use crate::index::Index;
use crate::pixel::{self, YuvCoefficients};

const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const TM_PRED: u8 = 3;
const B_PRED: u8 = 4;
const NEARESTMV: u8 = 5;
const NEARMV: u8 = 6;
const ZEROMV: u8 = 7;
const NEWMV: u8 = 8;
const SPLITMV: u8 = 9;

const B_DC_PRED: u8 = 0;
const B_TM_PRED: u8 = 1;
const B_VE_PRED: u8 = 2;
const B_HE_PRED: u8 = 3;
const B_LD_PRED: u8 = 4;
const B_RD_PRED: u8 = 5;
const B_VR_PRED: u8 = 6;
const B_VL_PRED: u8 = 7;
const B_HD_PRED: u8 = 8;
const B_HU_PRED: u8 = 9;

const CURRENT: u8 = 0;
const LAST: u8 = 1;
const GOLDEN: u8 = 2;
const ALTREF: u8 = 3;

const ZIGZAG: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const BANDS: [u8; 17] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7, 0];

const CAT1: [u8; 1] = [159];
const CAT2: [u8; 2] = [165, 145];
const CAT3: [u8; 3] = [173, 148, 140];
const CAT4: [u8; 4] = [176, 155, 140, 135];
const CAT5: [u8; 5] = [180, 157, 141, 134, 130];
const CAT6: [u8; 11] = [254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129];

/// A motion vector, in eighths of a pixel of luma as this code reads them.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Mv {
    y: i16,
    x: i16,
}

impl Mv {
    const ZERO: Mv = Mv { y: 0, x: 0 };

    fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

/// What's kept of each macroblock. Kept across frames for the segment map, and as
/// context within one.
#[derive(Clone, Copy)]
struct MbInfo {
    y_mode: u8,
    uv_mode: u8,
    reference: u8,
    segment: u8,
    skip: bool,
    /// It has coefficients, which decides whether its inner edges get filtered.
    coded: bool,
    partitioning: u8,
    mv: Mv,
    /// Subblock modes for `B_PRED`, or the implied ones.
    modes: [u8; 16],
    /// Subblock vectors for `SPLITMV`.
    mvs: [Mv; 16],
}

impl MbInfo {
    const OUTSIDE: MbInfo = MbInfo {
        y_mode: DC_PRED,
        uv_mode: DC_PRED,
        reference: CURRENT,
        segment: 0,
        skip: false,
        coded: false,
        partitioning: 0,
        mv: Mv::ZERO,
        modes: [B_DC_PRED; 16],
        mvs: [Mv::ZERO; 16],
    };
}

/// A decoded picture, macroblock aligned.
pub struct Frame {
    /// Luma.
    pub y: Vec<u8>,
    /// Blue difference, at half the size each way.
    pub u: Vec<u8>,
    /// Red difference, at half the size each way.
    pub v: Vec<u8>,
}

/// Which of the format's features a stream has used: how a test knows its fixture does
/// what it's for.
#[derive(Clone, Copy, Default, Debug)]
pub struct Vp8Stats {
    /// Frames decoded.
    pub frames: u32,
    /// Frames only for other frames to refer to.
    pub hidden: u32,
    /// Frames with more than one token partition.
    pub partitioned: u32,
    /// Frames with segmentation on.
    pub segmented: u32,
    /// Macroblocks predicted from the golden picture.
    pub golden_blocks: u32,
    /// Macroblocks predicted from the alternate picture.
    pub altref_blocks: u32,
    /// Macroblocks with split motion vectors.
    pub split_blocks: u32,
    /// Intra macroblocks in inter frames.
    pub intra_in_inter: u32,
    /// Frames using the simple loop filter.
    pub simple_filter: u32,
    /// The highest bitstream version seen.
    pub version: u8,
    /// Frames that used sign bias.
    pub sign_biased: u32,
}

/// Probabilities that persist between frames unless a frame says not to.
#[derive(Clone)]
struct Entropy {
    coeff: [[[[u8; 11]; 3]; 8]; 4],
    mv: [[u8; 19]; 2],
    y_mode: [u8; 4],
    uv_mode: [u8; 3],
}

impl Entropy {
    fn default() -> Self {
        Self {
            coeff: DEFAULT_COEFF_PROBS,
            mv: DEFAULT_MV_PROBS,
            y_mode: [112, 86, 140, 37],
            uv_mode: [162, 101, 204],
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Segmentation {
    enabled: bool,
    update_map: bool,
    absolute: bool,
    quant: [i8; 4],
    filter: [i8; 4],
    tree: [u8; 3],
}

#[derive(Clone, Copy, Default)]
struct LoopFilter {
    simple: bool,
    level: u8,
    sharpness: u8,
    delta_enabled: bool,
    ref_delta: [i8; 4],
    mode_delta: [i8; 4],
}

#[derive(Clone, Copy, Default)]
struct Quant {
    index: u8,
    y1_dc: i8,
    y2_dc: i8,
    y2_ac: i8,
    uv_dc: i8,
    uv_ac: i8,
}

/// Dequantisation factors, DC then AC.
#[derive(Clone, Copy, Default)]
struct Factors {
    y1: [i16; 2],
    y2: [i16; 2],
    uv: [i16; 2],
}

/// What one call to [`Vp8Decoder::decode`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// A picture to show.
    Shown,
    /// A picture only for later frames to refer to.
    Hidden,
    /// An empty packet: whatever was showing stays.
    Nothing,
}

/// Reads a key frame's size without decoding it.
pub fn key_frame_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 || data[0] & 1 != 0 || data[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let width = u16::from_le_bytes([data[6], data[7]]) as u32 & 0x3fff;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32 & 0x3fff;
    (width > 0 && height > 0).then_some((width, height))
}

/// Whether a packet says it's a key frame.
pub(crate) fn is_key_frame(data: &[u8]) -> bool {
    data.first().is_some_and(|tag| tag & 1 == 0)
}

/// A stateful decoder over the samples of one VP8 track.
pub(crate) struct Vp8Stream {
    decoder: Vp8Decoder,
    /// Samples `..next` have gone through the decoder.
    next: u32,
    coefficients: YuvCoefficients,
    last_error: Option<VideoError>,
}

impl Vp8Stream {
    pub(crate) fn new(max_bytes: usize, coefficients: YuvCoefficients) -> Self {
        Self {
            decoder: Vp8Decoder::new(max_bytes),
            next: 0,
            coefficients,
            last_error: None,
        }
    }

    /// Brings the picture up to sample `index` and writes it out. `Ok(None)` when
    /// `budget` frames ran out first. A hidden frame shows what was showing.
    pub(crate) fn advance(
        &mut self,
        table: &Index,
        expand: &mut dyn FnMut(usize) -> Result<Option<Vec<u8>>>,
        input: &[u8],
        index: u32,
        budget: u32,
        out: &mut [u8],
    ) -> Result<Option<(u32, u32)>> {
        let key = crate::decoder::key_at_or_before(table, index) as u32;
        if index + 1 < self.next.max(1) || key >= self.next {
            self.next = key;
            self.decoder.reset();
        }
        let mut fed = 0;
        while self.next <= index {
            if fed >= budget {
                return Ok(None);
            }
            fed += 1;
            let at = self.next as usize;
            self.next += 1;
            let loaded = expand(at)?;
            let sample = loaded.as_deref().unwrap_or_else(|| table.bytes(input, at));
            // A damaged frame only loses itself: what refers to it looks wrong until
            // the next key frame.
            match self.decoder.decode(sample) {
                Ok(_) => {}
                Err(error) if error.is_fatal() => return Err(error),
                Err(error) => self.last_error = Some(error),
            }
        }
        let Some((frame, stride)) = self.decoder.picture() else {
            return Err(self.last_error.take().unwrap_or(VideoError::InvalidData("no VP8 picture")));
        };
        let (width, height) = self.decoder.size();
        crate::error::require_rgba(out, width, height)?;
        let chroma_stride = stride / 2;
        for (row, line) in out
            .chunks_exact_mut(width as usize * 4)
            .take(height as usize)
            .enumerate()
        {
            let chroma = row / 2;
            let width = width as usize;
            let chroma_width = width.div_ceil(2);
            pixel::yuv420_row(
                &self.coefficients,
                &frame.y[row * stride..row * stride + width],
                &frame.u[chroma * chroma_stride..chroma * chroma_stride + chroma_width],
                &frame.v[chroma * chroma_stride..chroma * chroma_stride + chroma_width],
                line,
            );
        }
        Ok(Some((width, height)))
    }
}

/// The decoder.
pub struct Vp8Decoder {
    width: u32,
    height: u32,
    mb_cols: usize,
    mb_rows: usize,
    max_bytes: usize,
    /// Pictures, and which is which. There can be four in use at once.
    frames: [Frame; 5],
    last: usize,
    golden: usize,
    altref: usize,
    /// The picture last decoded, shown or not.
    newest: Option<usize>,
    /// The picture to show, which a hidden frame doesn't change.
    shown: Option<usize>,
    sign_bias: [bool; 4],
    entropy: Entropy,
    segmentation: Segmentation,
    filter: LoopFilter,
    quant: Quant,
    prob_intra: u8,
    prob_last: u8,
    prob_golden: u8,
    prob_skip: Option<u8>,
    /// With a border of one on the left and top.
    info: Vec<MbInfo>,
    /// Non-zero flags above each macroblock: four luma, two each chroma, and Y2.
    above_nonzero: Vec<[u8; 9]>,
    version: u8,
    stats: Vp8Stats,
}

impl Vp8Decoder {
    /// A decoder that refuses pictures needing more than `max_bytes` of memory.
    pub fn new(max_bytes: usize) -> Self {
        let empty = || Frame {
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        };
        Self {
            width: 0,
            height: 0,
            mb_cols: 0,
            mb_rows: 0,
            max_bytes,
            frames: [empty(), empty(), empty(), empty(), empty()],
            last: 0,
            golden: 0,
            altref: 0,
            newest: None,
            shown: None,
            sign_bias: [false; 4],
            entropy: Entropy::default(),
            segmentation: Segmentation::default(),
            filter: LoopFilter::default(),
            quant: Quant::default(),
            prob_intra: 0,
            prob_last: 0,
            prob_golden: 0,
            prob_skip: None,
            info: Vec::new(),
            above_nonzero: Vec::new(),
            version: 0,
            stats: Vp8Stats::default(),
        }
    }

    /// What the stream has used so far.
    pub fn stats(&self) -> Vp8Stats {
        self.stats
    }

    /// The display size, from the last key frame.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The picture on show, and its stride in luma pixels.
    pub fn picture(&self) -> Option<(&Frame, usize)> {
        self.shown.map(|index| (&self.frames[index], self.mb_cols * 16))
    }

    /// Forgets everything before the next key frame, for a seek.
    pub fn reset(&mut self) {
        self.newest = None;
        self.shown = None;
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.width = width;
        self.height = height;
        let (cols, rows) = ((width as usize + 15) / 16, (height as usize + 15) / 16);
        if cols == self.mb_cols && rows == self.mb_rows {
            return Ok(());
        }
        let luma = (cols * 16)
            .checked_mul(rows * 16)
            .ok_or(VideoError::Overflow)?;
        let chroma = luma / 4;
        let total = (luma + 2 * chroma).saturating_mul(self.frames.len());
        if total > self.max_bytes {
            return Err(VideoError::LimitExceeded("VP8 frame memory"));
        }
        self.mb_cols = 0;
        self.mb_rows = 0;
        self.newest = None;
        self.shown = None;
        for frame in &mut self.frames {
            sized(&mut frame.y, luma)?;
            sized(&mut frame.u, chroma)?;
            sized(&mut frame.v, chroma)?;
        }
        let cells = (cols + 1) * (rows + 1);
        self.info.clear();
        self.info
            .try_reserve_exact(cells)
            .map_err(|_| VideoError::OutOfMemory)?;
        self.info.resize(cells, MbInfo::OUTSIDE);
        self.above_nonzero.clear();
        self.above_nonzero
            .try_reserve_exact(cols)
            .map_err(|_| VideoError::OutOfMemory)?;
        self.above_nonzero.resize(cols, [0; 9]);
        self.mb_cols = cols;
        self.mb_rows = rows;
        Ok(())
    }

    /// A picture no reference is using.
    fn free_frame(&self) -> usize {
        (0..self.frames.len())
            .find(|&index| {
                index != self.last
                    && index != self.golden
                    && index != self.altref
                    && Some(index) != self.shown
            })
            .unwrap_or(0)
    }

    /// Decodes a packet. An error leaves the references as they were.
    pub fn decode(&mut self, data: &[u8]) -> Result<Decoded> {
        if data.is_empty() {
            return Ok(Decoded::Nothing);
        }
        if data.len() < 3 {
            return Err(VideoError::UnexpectedEnd);
        }
        let tag = u32::from_le_bytes([data[0], data[1], data[2], 0]);
        let key = tag & 1 == 0;
        let version = ((tag >> 1) & 7) as u8;
        let show = (tag >> 4) & 1 != 0;
        let first_size = (tag >> 5) as usize;
        if version > 3 {
            return Err(VideoError::InvalidData("VP8 version"));
        }
        let mut rest = &data[3..];
        if key {
            let (width, height) =
                key_frame_size(data).ok_or(VideoError::InvalidData("VP8 key frame header"))?;
            self.resize(width, height)?;
            rest = &rest[7..];
        } else if self.newest.is_none() {
            return Err(VideoError::InvalidData("VP8 stream needs a key frame first"));
        }
        if first_size > rest.len() {
            return Err(VideoError::UnexpectedEnd);
        }
        self.version = version;
        let (first, rest) = rest.split_at(first_size);
        let mut bool = BoolDecoder::new(first);

        if key {
            // Colour space and clamping. Pixels are always clamped here, which is
            // what a stream that says it doesn't need it comes to anyway.
            let _ = bool.bits(2);
            self.segmentation = Segmentation::default();
            self.filter = LoopFilter::default();
        }
        self.read_segmentation(&mut bool);
        self.filter.simple = bool.flag();
        self.filter.level = bool.bits(6) as u8;
        self.filter.sharpness = bool.bits(3) as u8;
        self.filter.delta_enabled = bool.flag();
        if self.filter.delta_enabled && bool.flag() {
            for slot in self.filter.ref_delta.iter_mut() {
                if let Some(value) = bool.maybe_signed(6) {
                    *slot = value as i8;
                }
            }
            for slot in self.filter.mode_delta.iter_mut() {
                if let Some(value) = bool.maybe_signed(6) {
                    *slot = value as i8;
                }
            }
        }
        let partitions = 1usize << bool.bits(2);
        let mut decoders = token_partitions(rest, partitions)?;
        self.quant = Quant {
            index: bool.bits(7) as u8,
            y1_dc: bool.maybe_signed(4).unwrap_or(0) as i8,
            y2_dc: bool.maybe_signed(4).unwrap_or(0) as i8,
            y2_ac: bool.maybe_signed(4).unwrap_or(0) as i8,
            uv_dc: bool.maybe_signed(4).unwrap_or(0) as i8,
            uv_ac: bool.maybe_signed(4).unwrap_or(0) as i8,
        };
        let factors = self.dequant_factors();

        let (mut refresh_golden, mut refresh_altref) = (true, true);
        let (mut copy_golden, mut copy_altref) = (0, 0);
        if !key {
            refresh_golden = bool.flag();
            refresh_altref = bool.flag();
            if !refresh_golden {
                copy_golden = bool.bits(2);
            }
            if !refresh_altref {
                copy_altref = bool.bits(2);
            }
            self.sign_bias[GOLDEN as usize] = bool.flag();
            self.sign_bias[ALTREF as usize] = bool.flag();
        } else {
            self.sign_bias = [false; 4];
        }
        let refresh_entropy = bool.flag();
        let refresh_last = key || bool.flag();
        if key {
            self.entropy = Entropy::default();
        }
        let saved = (!refresh_entropy).then(|| self.entropy.clone());
        for (i, block) in self.entropy.coeff.iter_mut().enumerate() {
            for (j, band) in block.iter_mut().enumerate() {
                for (k, context) in band.iter_mut().enumerate() {
                    for (l, prob) in context.iter_mut().enumerate() {
                        if bool.read(COEFF_UPDATE_PROBS[i][j][k][l]) {
                            *prob = bool.bits(8) as u8;
                        }
                    }
                }
            }
        }
        self.prob_skip = bool.flag().then(|| bool.bits(8) as u8);
        if !key {
            self.prob_intra = bool.bits(8) as u8;
            self.prob_last = bool.bits(8) as u8;
            self.prob_golden = bool.bits(8) as u8;
            if bool.flag() {
                for prob in self.entropy.y_mode.iter_mut() {
                    *prob = bool.bits(8) as u8;
                }
            }
            if bool.flag() {
                for prob in self.entropy.uv_mode.iter_mut() {
                    *prob = bool.bits(8) as u8;
                }
            }
            for (i, component) in self.entropy.mv.iter_mut().enumerate() {
                for (j, prob) in component.iter_mut().enumerate() {
                    if bool.read(MV_UPDATE_PROBS[i][j]) {
                        let value = bool.bits(7);
                        *prob = if value != 0 { (value << 1) as u8 } else { 1 };
                    }
                }
            }
        }

        // The picture to fill. Reference pictures are read at the same time, so it's
        // taken out of the array for as long as this takes.
        let target = self.free_frame();
        let mut frame = core::mem::replace(
            &mut self.frames[target],
            Frame {
                y: Vec::new(),
                u: Vec::new(),
                v: Vec::new(),
            },
        );
        for slot in self.above_nonzero.iter_mut() {
            *slot = [0; 9];
        }
        for row in 0..self.mb_rows {
            let mut left_nonzero = [0u8; 9];
            let partition = &mut decoders[row & (partitions - 1)];
            for col in 0..self.mb_cols {
                self.read_modes(&mut bool, key, row, col);
                let mut coeffs = [0i16; 25 * 16];
                let at = self.cell(row, col);
                let info = self.info[at];
                let coded = if info.skip {
                    // Y2's flags stay as they were for a macroblock with no Y2.
                    let has_y2 = info.y_mode != B_PRED && info.y_mode != SPLITMV;
                    let (keep_above, keep_left) = (self.above_nonzero[col][8], left_nonzero[8]);
                    self.above_nonzero[col] = [0; 9];
                    left_nonzero = [0; 9];
                    if !has_y2 {
                        self.above_nonzero[col][8] = keep_above;
                        left_nonzero[8] = keep_left;
                    }
                    false
                } else {
                    let segment = if self.segmentation.enabled { info.segment } else { 0 };
                    read_tokens(
                        partition,
                        &self.entropy.coeff,
                        &factors[segment as usize],
                        info.y_mode,
                        &mut self.above_nonzero[col],
                        &mut left_nonzero,
                        &mut coeffs,
                    )
                };
                self.info[at].coded = coded;
                self.stats.golden_blocks += (info.reference == GOLDEN) as u32;
                self.stats.altref_blocks += (info.reference == ALTREF) as u32;
                self.stats.split_blocks += (info.y_mode == SPLITMV) as u32;
                self.stats.intra_in_inter += (!key && info.reference == CURRENT) as u32;
                self.reconstruct(&mut frame, row, col, &mut coeffs);
            }
        }
        self.run_loop_filter(&mut frame, key);
        self.frames[target] = frame;

        if let Some(saved) = saved {
            self.entropy = saved;
        }
        // In this order: a copy to golden sees the new alternate.
        match copy_altref {
            1 => self.altref = self.last,
            2 => self.altref = self.golden,
            _ => {}
        }
        match copy_golden {
            1 => self.golden = self.last,
            2 => self.golden = self.altref,
            _ => {}
        }
        if refresh_golden {
            self.golden = target;
        }
        if refresh_altref {
            self.altref = target;
        }
        if refresh_last {
            self.last = target;
        }
        self.newest = Some(target);
        self.stats.frames += 1;
        self.stats.hidden += !show as u32;
        self.stats.partitioned += (partitions > 1) as u32;
        self.stats.segmented += self.segmentation.enabled as u32;
        self.stats.simple_filter += self.filter.simple as u32;
        self.stats.version = self.stats.version.max(version);
        self.stats.sign_biased += (self.sign_bias[GOLDEN as usize] || self.sign_bias[ALTREF as usize]) as u32;
        if show {
            self.shown = Some(target);
            Ok(Decoded::Shown)
        } else {
            Ok(Decoded::Hidden)
        }
    }

    fn read_segmentation(&mut self, bool: &mut BoolDecoder<'_>) {
        let segmentation = &mut self.segmentation;
        segmentation.enabled = bool.flag();
        segmentation.update_map = false;
        if !segmentation.enabled {
            return;
        }
        segmentation.update_map = bool.flag();
        let update_data = bool.flag();
        if update_data {
            segmentation.absolute = bool.flag();
            for slot in segmentation.quant.iter_mut() {
                *slot = bool.maybe_signed(7).unwrap_or(0) as i8;
            }
            for slot in segmentation.filter.iter_mut() {
                *slot = bool.maybe_signed(6).unwrap_or(0) as i8;
            }
        }
        if segmentation.update_map {
            for slot in segmentation.tree.iter_mut() {
                *slot = if bool.flag() { bool.bits(8) as u8 } else { 255 };
            }
        }
    }

    fn dequant_factors(&self) -> [Factors; 4] {
        let mut out = [Factors::default(); 4];
        for (segment, factors) in out.iter_mut().enumerate() {
            let mut base = self.quant.index as i32;
            if self.segmentation.enabled {
                let value = self.segmentation.quant[segment] as i32;
                base = if self.segmentation.absolute { value } else { base + value };
            }
            let dc = |delta: i8| DC_Q[(base + delta as i32).clamp(0, 127) as usize] as i32;
            let ac = |delta: i8| AC_Q[(base + delta as i32).clamp(0, 127) as usize] as i32;
            factors.y1 = [dc(self.quant.y1_dc) as i16, ac(0) as i16];
            factors.y2 = [
                (dc(self.quant.y2_dc) * 2) as i16,
                (ac(self.quant.y2_ac) * 155 / 100).max(8) as i16,
            ];
            factors.uv = [
                dc(self.quant.uv_dc).min(132) as i16,
                ac(self.quant.uv_ac) as i16,
            ];
        }
        out
    }

    /// Where a macroblock is in `info`, whose first row and column are outside the frame.
    fn cell(&self, row: usize, col: usize) -> usize {
        (row + 1) * (self.mb_cols + 1) + col + 1
    }

    fn read_modes(&mut self, bool: &mut BoolDecoder<'_>, key: bool, row: usize, col: usize) {
        let at = self.cell(row, col);
        let above_at = at - (self.mb_cols + 1);
        let left = self.info[at - 1];
        let above = self.info[above_at];
        let mut info = self.info[at];
        if self.segmentation.update_map {
            let tree = self.segmentation.tree;
            info.segment = if bool.read(tree[0]) {
                2 + bool.read(tree[2]) as u8
            } else {
                bool.read(tree[1]) as u8
            };
        } else if key {
            info.segment = 0;
        }
        info.skip = match self.prob_skip {
            Some(prob) => bool.read(prob),
            None => false,
        };
        info.partitioning = 0;
        if key {
            info.reference = CURRENT;
            info.mv = Mv::ZERO;
            info.y_mode = read_kf_y_mode(bool);
            if info.y_mode == B_PRED {
                for b in 0..16 {
                    let a = if b < 4 { above.modes[b + 12] } else { info.modes[b - 4] };
                    let l = if b & 3 == 0 { left.modes[b + 3] } else { info.modes[b - 1] };
                    info.modes[b] = read_b_mode(bool, &KF_B_MODE_PROBS[a as usize][l as usize]);
                }
            } else {
                info.modes = [implied_b_mode(info.y_mode); 16];
            }
            info.uv_mode = read_uv_mode(bool, &[142, 114, 183]);
        } else if bool.read(self.prob_intra) {
            info.reference = if bool.read(self.prob_last) {
                GOLDEN + bool.read(self.prob_golden) as u8
            } else {
                LAST
            };
            self.read_inter(bool, &mut info, row, col, above_at, at);
        } else {
            info.reference = CURRENT;
            info.mv = Mv::ZERO;
            info.y_mode = read_y_mode(bool, &self.entropy.y_mode);
            if info.y_mode == B_PRED {
                const DEFAULT: [u8; 9] = [120, 90, 79, 133, 87, 85, 80, 111, 151];
                for b in 0..16 {
                    info.modes[b] = read_b_mode(bool, &DEFAULT);
                }
            } else {
                info.modes = [implied_b_mode(info.y_mode); 16];
            }
            info.uv_mode = read_uv_mode(bool, &self.entropy.uv_mode);
        }
        self.info[at] = info;
    }

    /// Reads an inter macroblock's mode and vectors. `info.reference` is already set.
    fn read_inter(
        &self,
        bool: &mut BoolDecoder<'_>,
        info: &mut MbInfo,
        row: usize,
        col: usize,
        above_at: usize,
        at: usize,
    ) {
        let above = &self.info[above_at];
        let left = &self.info[at - 1];
        let above_left = &self.info[above_at - 1];
        let (mut near, cnt) = find_near_mvs(info.reference, above, left, above_left, &self.sign_bias);
        // How far a vector can point and still have a macroblock inside a one
        // macroblock border, in eighths of a pixel.
        let bounds = Bounds {
            left: -(((col + 1) << 7) as i32),
            right: ((self.mb_cols - col) << 7) as i32,
            top: -(((row + 1) << 7) as i32),
            bottom: ((self.mb_rows - row) << 7) as i32,
        };
        let probs = [
            MV_COUNTS_TO_PROBS[cnt[0] as usize][0],
            MV_COUNTS_TO_PROBS[cnt[1] as usize][1],
            MV_COUNTS_TO_PROBS[cnt[2] as usize][2],
            MV_COUNTS_TO_PROBS[cnt[3] as usize][3],
        ];
        info.y_mode = if !bool.read(probs[0]) {
            ZEROMV
        } else if !bool.read(probs[1]) {
            NEARESTMV
        } else if !bool.read(probs[2]) {
            NEARMV
        } else if !bool.read(probs[3]) {
            NEWMV
        } else {
            SPLITMV
        };
        info.uv_mode = info.y_mode;
        let best = bounds.clamp(near[0]);
        near[1] = bounds.clamp(near[1]);
        near[2] = bounds.clamp(near[2]);
        match info.y_mode {
            NEARESTMV => info.mv = near[1],
            NEARMV => info.mv = near[2],
            ZEROMV => info.mv = Mv::ZERO,
            NEWMV => {
                let delta = read_mv(bool, &self.entropy.mv);
                info.mv = Mv {
                    y: best.y.wrapping_add(delta.y),
                    x: best.x.wrapping_add(delta.x),
                };
            }
            _ => {
                let partitioning = if !bool.read(110) {
                    3
                } else if !bool.read(111) {
                    2
                } else {
                    bool.read(150) as u8
                };
                info.partitioning = partitioning;
                let map = &MV_PARTITIONS[partitioning as usize];
                let mut done = 0u32;
                let mut part = 0u8;
                while done != 0xffff {
                    let Some(first) = map.iter().position(|&p| p == part) else {
                        break;
                    };
                    let left_mv = if first & 3 == 0 {
                        if left.y_mode == SPLITMV { left.mvs[first + 3] } else { left.mv }
                    } else {
                        info.mvs[first - 1]
                    };
                    let above_mv = if first < 4 {
                        if above.y_mode == SPLITMV { above.mvs[first + 12] } else { above.mv }
                    } else {
                        info.mvs[first - 4]
                    };
                    let context = if left_mv == above_mv {
                        if above_mv.is_zero() { 4 } else { 3 }
                    } else if above_mv.is_zero() {
                        2
                    } else if left_mv.is_zero() {
                        1
                    } else {
                        0
                    };
                    let probs = &SUBMV_REF_PROBS[context];
                    let mv = if !bool.read(probs[0]) {
                        left_mv
                    } else if !bool.read(probs[1]) {
                        above_mv
                    } else if !bool.read(probs[2]) {
                        Mv::ZERO
                    } else {
                        let delta = read_mv(bool, &self.entropy.mv);
                        Mv {
                            y: best.y.wrapping_add(delta.y),
                            x: best.x.wrapping_add(delta.x),
                        }
                    };
                    for (b, &p) in map.iter().enumerate() {
                        if p == part {
                            info.mvs[b] = mv;
                            done |= 1 << b;
                        }
                    }
                    part += 1;
                }
                info.mv = info.mvs[15];
            }
        }
    }

    fn run_loop_filter(&self, frame: &mut Frame, key: bool) {
        if self.filter.level == 0 {
            return;
        }
        let stride = self.mb_cols * 16;
        let chroma_stride = self.mb_cols * 8;
        for row in 0..self.mb_rows {
            for col in 0..self.mb_cols {
                let info = &self.info[self.cell(row, col)];
                let level = self.mb_filter_level(info);
                if level == 0 {
                    continue;
                }
                let mut interior = level >> if self.filter.sharpness > 4 { 2 } else if self.filter.sharpness > 0 { 1 } else { 0 };
                if self.filter.sharpness > 0 {
                    interior = interior.min(9 - self.filter.sharpness as i32);
                }
                let interior = interior.max(1);
                let mut hev = (level >= 15) as i32 + (level >= 40) as i32;
                if !key && level >= 20 {
                    hev += 1;
                }
                let inner = info.coded || info.y_mode == SPLITMV || info.y_mode == B_PRED;
                let (y, x) = (row * 16, col * 16);
                let at = y * stride + x;
                if self.filter.simple {
                    let mb_limit = (level + 2) * 2 + interior;
                    let b_limit = level * 2 + interior;
                    let plane = &mut frame.y;
                    if col > 0 {
                        filter::simple_v(plane, at, stride, mb_limit);
                    }
                    if inner {
                        for offset in [4, 8, 12] {
                            filter::simple_v(plane, at + offset, stride, b_limit);
                        }
                    }
                    if row > 0 {
                        filter::simple_h(plane, at, stride, mb_limit);
                    }
                    if inner {
                        for offset in [4, 8, 12] {
                            filter::simple_h(plane, at + offset * stride, stride, b_limit);
                        }
                    }
                    continue;
                }
                let chroma_at = (y / 2) * chroma_stride + x / 2;
                let limits = filter::Limits {
                    edge: level,
                    interior,
                    hev,
                };
                if col > 0 {
                    filter::mb_v(&mut frame.y, at, stride, 16, limits);
                    filter::mb_v(&mut frame.u, chroma_at, chroma_stride, 8, limits);
                    filter::mb_v(&mut frame.v, chroma_at, chroma_stride, 8, limits);
                }
                if inner {
                    for offset in [4, 8, 12] {
                        filter::sub_v(&mut frame.y, at + offset, stride, 16, limits);
                    }
                    filter::sub_v(&mut frame.u, chroma_at + 4, chroma_stride, 8, limits);
                    filter::sub_v(&mut frame.v, chroma_at + 4, chroma_stride, 8, limits);
                }
                if row > 0 {
                    filter::mb_h(&mut frame.y, at, stride, 16, limits);
                    filter::mb_h(&mut frame.u, chroma_at, chroma_stride, 8, limits);
                    filter::mb_h(&mut frame.v, chroma_at, chroma_stride, 8, limits);
                }
                if inner {
                    for offset in [4, 8, 12] {
                        filter::sub_h(&mut frame.y, at + offset * stride, stride, 16, limits);
                    }
                    filter::sub_h(&mut frame.u, chroma_at + 4 * chroma_stride, chroma_stride, 8, limits);
                    filter::sub_h(&mut frame.v, chroma_at + 4 * chroma_stride, chroma_stride, 8, limits);
                }
            }
        }
    }

    fn mb_filter_level(&self, info: &MbInfo) -> i32 {
        let mut level = self.filter.level as i32;
        if self.segmentation.enabled {
            let value = self.segmentation.filter[info.segment as usize] as i32;
            level = if self.segmentation.absolute { value } else { level + value };
        }
        level = level.clamp(0, 63);
        if self.filter.delta_enabled {
            level += self.filter.ref_delta[info.reference as usize] as i32;
            if info.reference == CURRENT {
                if info.y_mode == B_PRED {
                    level += self.filter.mode_delta[0] as i32;
                }
            } else if info.y_mode == ZEROMV {
                level += self.filter.mode_delta[1] as i32;
            } else if info.y_mode == SPLITMV {
                level += self.filter.mode_delta[3] as i32;
            } else {
                level += self.filter.mode_delta[2] as i32;
            }
        }
        level.clamp(0, 63)
    }

    fn reconstruct(&self, frame: &mut Frame, row: usize, col: usize, coeffs: &mut [i16; 25 * 16]) {
        let info = &self.info[self.cell(row, col)];
        let has_y2 = info.y_mode != B_PRED && info.y_mode != SPLITMV;
        if has_y2 && info.coded {
            let mut y2 = [0i16; 16];
            y2.copy_from_slice(&coeffs[24 * 16..25 * 16]);
            let dc = predict::inverse_walsh(&y2);
            for (block, value) in dc.iter().enumerate() {
                coeffs[block * 16] = *value;
            }
        }
        if info.reference == CURRENT {
            predict::intra(frame, self.mb_cols, self.mb_rows, row, col, info.y_mode, info.uv_mode, &info.modes, coeffs);
        } else {
            let reference = match info.reference {
                LAST => self.last,
                GOLDEN => self.golden,
                _ => self.altref,
            };
            let pictures = predict::Reference {
                frame: &self.frames[reference],
                cols: self.mb_cols,
                rows: self.mb_rows,
                filters: if self.version == 0 { &SIXTAP_FILTERS } else { &BILINEAR_FILTERS },
                full_pixel: self.version == 3,
            };
            predict::inter(frame, &pictures, row, col, info.y_mode == SPLITMV, info.mv, &info.mvs, coeffs);
        }
    }
}

fn sized(buffer: &mut Vec<u8>, len: usize) -> Result<()> {
    buffer.clear();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| VideoError::OutOfMemory)?;
    buffer.resize(len, 0);
    Ok(())
}

fn token_partitions(data: &[u8], count: usize) -> Result<Vec<BoolDecoder<'_>>> {
    let table = (count - 1) * 3;
    if data.len() < table {
        return Err(VideoError::UnexpectedEnd);
    }
    let (sizes, mut rest) = data.split_at(table);
    let mut decoders = Vec::new();
    decoders
        .try_reserve_exact(count)
        .map_err(|_| VideoError::OutOfMemory)?;
    for index in 0..count {
        let part = if index + 1 < count {
            let at = index * 3;
            let size = u32::from_le_bytes([sizes[at], sizes[at + 1], sizes[at + 2], 0]) as usize;
            // A partition can be short of its stated size only in a truncated file,
            // and what it's missing reads as zeros.
            let (part, after) = rest.split_at(size.min(rest.len()));
            rest = after;
            part
        } else {
            rest
        };
        decoders.push(BoolDecoder::new(part));
    }
    Ok(decoders)
}

struct Bounds {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

impl Bounds {
    fn clamp(&self, mv: Mv) -> Mv {
        Mv {
            y: (mv.y as i32).clamp(self.top, self.bottom) as i16,
            x: (mv.x as i32).clamp(self.left, self.right) as i16,
        }
    }
}

fn biased(mb: &MbInfo, reference: u8, sign_bias: &[bool; 4], mv: Mv) -> Mv {
    if sign_bias[mb.reference as usize] != sign_bias[reference as usize] {
        Mv {
            y: mv.y.wrapping_neg(),
            x: mv.x.wrapping_neg(),
        }
    } else {
        mv
    }
}

/// Finds candidates from the neighbours: `[best, nearest, near]`, and the counts that
/// choose the mode's probabilities.
fn find_near_mvs(
    reference: u8,
    above: &MbInfo,
    left: &MbInfo,
    above_left: &MbInfo,
    sign_bias: &[bool; 4],
) -> ([Mv; 4], [u8; 4]) {
    let mut near = [Mv::ZERO; 4];
    let mut cnt = [0u8; 4];
    let mut index = 0;
    if above.reference != CURRENT {
        if !above.mv.is_zero() {
            index += 1;
            near[index] = biased(above, reference, sign_bias, above.mv);
        }
        cnt[index] += 2;
    }
    if left.reference != CURRENT {
        if !left.mv.is_zero() {
            let mv = biased(left, reference, sign_bias, left.mv);
            if mv != near[index] {
                index += 1;
                near[index] = mv;
            }
            cnt[index] += 2;
        } else {
            cnt[0] += 2;
        }
    }
    if above_left.reference != CURRENT {
        if !above_left.mv.is_zero() {
            let mv = biased(above_left, reference, sign_bias, above_left.mv);
            if mv != near[index] {
                index += 1;
                near[index] = mv;
            }
            cnt[index] += 1;
        } else {
            cnt[0] += 1;
        }
    }
    // Three different vectors, and the last the same as the first?
    if cnt[3] != 0 && near[index] == near[1] {
        cnt[1] += 1;
    }
    cnt[3] = (((above.y_mode == SPLITMV) as u8 + (left.y_mode == SPLITMV) as u8) * 2)
        + (above_left.y_mode == SPLITMV) as u8;
    if cnt[2] > cnt[1] {
        cnt.swap(1, 2);
        near.swap(1, 2);
    }
    if cnt[1] >= cnt[0] {
        near[0] = near[1];
    }
    (near, cnt)
}

fn read_mv_component(bool: &mut BoolDecoder<'_>, probs: &[u8; 19]) -> i16 {
    const IS_SHORT: usize = 0;
    const SIGN: usize = 1;
    const SHORT: usize = 2;
    const BITS: usize = 9;
    const LONG_WIDTH: usize = 10;
    let mut x: i16 = 0;
    if bool.read(probs[IS_SHORT]) {
        for i in 0..3 {
            x += (bool.read(probs[BITS + i]) as i16) << i;
        }
        for i in (4..LONG_WIDTH).rev() {
            x += (bool.read(probs[BITS + i]) as i16) << i;
        }
        if x & 0xfff0u16 as i16 == 0 || bool.read(probs[BITS + 3]) {
            x += 8;
        }
    } else {
        // A three-level tree: 0-3 and 4-7, then pairs.
        let p = &probs[SHORT..SHORT + 7];
        x = if !bool.read(p[0]) {
            if !bool.read(p[1]) {
                bool.read(p[2]) as i16
            } else {
                2 + bool.read(p[3]) as i16
            }
        } else if !bool.read(p[4]) {
            4 + bool.read(p[5]) as i16
        } else {
            6 + bool.read(p[6]) as i16
        };
    }
    if x != 0 && bool.read(probs[SIGN]) {
        x = -x;
    }
    x << 1
}

fn read_mv(bool: &mut BoolDecoder<'_>, probs: &[[u8; 19]; 2]) -> Mv {
    let y = read_mv_component(bool, &probs[0]);
    let x = read_mv_component(bool, &probs[1]);
    Mv { y, x }
}

fn read_kf_y_mode(bool: &mut BoolDecoder<'_>) -> u8 {
    if !bool.read(145) {
        B_PRED
    } else if !bool.read(156) {
        if !bool.read(163) { DC_PRED } else { V_PRED }
    } else if !bool.read(128) {
        H_PRED
    } else {
        TM_PRED
    }
}

fn read_y_mode(bool: &mut BoolDecoder<'_>, probs: &[u8; 4]) -> u8 {
    if !bool.read(probs[0]) {
        DC_PRED
    } else if !bool.read(probs[1]) {
        if !bool.read(probs[2]) { V_PRED } else { H_PRED }
    } else if !bool.read(probs[3]) {
        TM_PRED
    } else {
        B_PRED
    }
}

fn read_uv_mode(bool: &mut BoolDecoder<'_>, probs: &[u8; 3]) -> u8 {
    if !bool.read(probs[0]) {
        DC_PRED
    } else if !bool.read(probs[1]) {
        V_PRED
    } else if !bool.read(probs[2]) {
        H_PRED
    } else {
        TM_PRED
    }
}

fn read_b_mode(bool: &mut BoolDecoder<'_>, probs: &[u8; 9]) -> u8 {
    if !bool.read(probs[0]) {
        return B_DC_PRED;
    }
    if !bool.read(probs[1]) {
        return B_TM_PRED;
    }
    if !bool.read(probs[2]) {
        return B_VE_PRED;
    }
    if !bool.read(probs[3]) {
        if !bool.read(probs[4]) {
            B_HE_PRED
        } else if !bool.read(probs[5]) {
            B_RD_PRED
        } else {
            B_VR_PRED
        }
    } else if !bool.read(probs[6]) {
        B_LD_PRED
    } else if !bool.read(probs[7]) {
        B_VL_PRED
    } else if !bool.read(probs[8]) {
        B_HD_PRED
    } else {
        B_HU_PRED
    }
}

fn implied_b_mode(y_mode: u8) -> u8 {
    match y_mode {
        V_PRED => B_VE_PRED,
        H_PRED => B_HE_PRED,
        TM_PRED => B_TM_PRED,
        _ => B_DC_PRED,
    }
}

/// Reads one macroblock's coefficients, dequantised, and says if any block had any.
fn read_tokens(
    bool: &mut BoolDecoder<'_>,
    probs: &[[[[u8; 11]; 3]; 8]; 4],
    factors: &Factors,
    y_mode: u8,
    above: &mut [u8; 9],
    left: &mut [u8; 9],
    coeffs: &mut [i16; 25 * 16],
) -> bool {
    let mut any = false;
    let has_y2 = y_mode != B_PRED && y_mode != SPLITMV;
    if has_y2 {
        let context = (above[8] + left[8]) as usize;
        let coded = read_block(bool, &probs[1], context, 0, factors.y2, block_of(coeffs, 24));
        above[8] = coded as u8;
        left[8] = coded as u8;
        any |= coded;
    }
    let (kind, first) = if has_y2 { (0, 1) } else { (3, 0) };
    for block in 0..16 {
        let (x, y) = (block & 3, block >> 2);
        let context = (above[x] + left[y]) as usize;
        let coded = read_block(bool, &probs[kind], context, first, factors.y1, block_of(coeffs, block));
        above[x] = coded as u8;
        left[y] = coded as u8;
        any |= coded;
    }
    for plane in 0..2 {
        for block in 0..4 {
            let (x, y) = (4 + plane * 2 + (block & 1), 4 + plane * 2 + (block >> 1));
            let context = (above[x] + left[y]) as usize;
            let coded = read_block(
                bool,
                &probs[2],
                context,
                0,
                factors.uv,
                block_of(coeffs, 16 + plane * 4 + block),
            );
            above[x] = coded as u8;
            left[y] = coded as u8;
            any |= coded;
        }
    }
    any
}

fn block_of(coeffs: &mut [i16; 25 * 16], block: usize) -> &mut [i16] {
    &mut coeffs[block * 16..(block + 1) * 16]
}

fn read_extra(bool: &mut BoolDecoder<'_>, probs: &[u8]) -> i32 {
    probs
        .iter()
        .fold(0, |value, &prob| (value << 1) | bool.read(prob) as i32)
}

/// Reads one block's tokens. Returns whether it read any.
fn read_block(
    bool: &mut BoolDecoder<'_>,
    probs: &[[[u8; 11]; 3]; 8],
    context: usize,
    first: usize,
    factors: [i16; 2],
    out: &mut [i16],
) -> bool {
    let mut c = first;
    let mut prob = &probs[BANDS[c] as usize][context];
    // The end of a block can't come straight after a zero.
    let mut check_end = true;
    while c < 16 {
        if check_end && !bool.read(prob[0]) {
            break;
        }
        if !bool.read(prob[1]) {
            c += 1;
            prob = &probs[BANDS[c] as usize][0];
            check_end = false;
            continue;
        }
        let value = if !bool.read(prob[2]) {
            1
        } else if !bool.read(prob[3]) {
            if !bool.read(prob[4]) {
                2
            } else if !bool.read(prob[5]) {
                3
            } else {
                4
            }
        } else if !bool.read(prob[6]) {
            if !bool.read(prob[7]) {
                5 + read_extra(bool, &CAT1)
            } else {
                7 + read_extra(bool, &CAT2)
            }
        } else if !bool.read(prob[8]) {
            if !bool.read(prob[9]) {
                11 + read_extra(bool, &CAT3)
            } else {
                19 + read_extra(bool, &CAT4)
            }
        } else if !bool.read(prob[10]) {
            35 + read_extra(bool, &CAT5)
        } else {
            67 + read_extra(bool, &CAT6)
        };
        let signed = if bool.flag() { -value } else { value };
        let factor = factors[(c > 0) as usize] as i32;
        out[ZIGZAG[c] as usize] = (signed * factor) as i16;
        c += 1;
        prob = &probs[BANDS[c] as usize][if value == 1 { 1 } else { 2 }];
        check_end = true;
    }
    c != first
}
