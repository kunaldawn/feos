//! H.264 / AVC glue around the vendored `feos-h264` decoder.
//!
//! The decoder itself works on NAL units and returns planar 4:2:0 frames. This
//! module parses SPS dimensions for `info`, and adapts Annex-B and AVCC
//! (length-prefixed) samples into RGBA frames.

use crate::bitstream::BitReader;
use crate::error::{require_rgba, Result, VideoError};
use crate::pixel;

use feos_h264::{
    parse_annex_b, parse_avcc, parse_avcc_config, Decoder as RawDecoder, Frame, NalUnitType,
    OrderedDecoder,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct H264Info {
    pub width: u32,
    pub height: u32,
}

fn skip_scaling_list(br: &mut BitReader<'_>, size: usize) -> Result<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = br.read_se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Ok(())
}

/// Parses the display dimensions from an SPS RBSP.
pub(crate) fn sps_dimensions(rbsp: &[u8]) -> Result<H264Info> {
    let mut br = BitReader::new(rbsp);
    let profile_idc = br.read_bits(8)? as u8;
    let _constraints = br.read_bits(8)?;
    let _level = br.read_bits(8)?;
    let _sps_id = br.read_ue()?;

    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane = 0u32;
    if matches!(
        profile_idc,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244
    ) {
        chroma_format_idc = br.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = br.read_bit()? as u32;
        }
        let _bit_depth_luma = br.read_ue()?;
        let _bit_depth_chroma = br.read_ue()?;
        let _qpprime = br.read_bit()?;
        if br.read_bit()? == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if br.read_bit()? == 1 {
                    skip_scaling_list(&mut br, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_max_frame_num = br.read_ue()?;
    let pic_order_cnt_type = br.read_ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_poc = br.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        br.read_bit()?;
        br.read_se()?;
        br.read_se()?;
        let count = br.read_ue()?;
        for _ in 0..count {
            br.read_se()?;
        }
    }
    let _max_num_ref_frames = br.read_ue()?;
    br.read_bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = br.read_ue()?.saturating_add(1);
    let height_units = br.read_ue()?.saturating_add(1);
    let frame_mbs_only = br.read_bit()? as u32;
    if frame_mbs_only == 0 {
        br.read_bit()?; // mb_adaptive_frame_field_flag
    }
    br.read_bit()?; // direct_8x8_inference_flag
    let (crop_left, crop_right, crop_top, crop_bottom) = if br.read_bit()? == 1 {
        (br.read_ue()?, br.read_ue()?, br.read_ue()?, br.read_ue()?)
    } else {
        (0, 0, 0, 0)
    };

    if chroma_format_idc > 3 || (separate_colour_plane != 0 && chroma_format_idc != 3) {
        return Err(VideoError::UnsupportedFeature("H.264 chroma format"));
    }
    let sub_width = if chroma_format_idc == 1 || chroma_format_idc == 2 {
        2
    } else {
        1
    };
    let sub_height = if chroma_format_idc == 1 { 2 } else { 1 };
    let crop_unit_x = if chroma_format_idc == 0 { 1 } else { sub_width };
    let crop_unit_y = if chroma_format_idc == 0 {
        2 - frame_mbs_only
    } else {
        sub_height * (2 - frame_mbs_only)
    };

    let width = width_mbs
        .checked_mul(16)
        .and_then(|v| v.checked_sub((crop_left + crop_right) * crop_unit_x))
        .ok_or(VideoError::Overflow)?;
    let height = (2 - frame_mbs_only)
        .checked_mul(height_units)
        .and_then(|v| v.checked_mul(16))
        .and_then(|v| v.checked_sub((crop_top + crop_bottom) * crop_unit_y))
        .ok_or(VideoError::Overflow)?;
    if width == 0 || height == 0 {
        return Err(VideoError::InvalidData("H.264 dimensions"));
    }
    Ok(H264Info { width, height })
}

fn sps_from_avcc(config: &[u8]) -> Result<&[u8]> {
    if config.len() < 7 || config[0] != 1 {
        return Err(VideoError::InvalidData("avcC header"));
    }
    let count = config[5] & 0x1f;
    if count == 0 {
        return Err(VideoError::InvalidData("avcC SPS count"));
    }
    let len = u16::from_be_bytes([config[6], config[7]]) as usize;
    if config.len() < 8 + len {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok(&config[8..8 + len])
}

pub(crate) fn info_from_avcc(config: &[u8]) -> Result<H264Info> {
    let sps = sps_from_avcc(config)?;
    sps_dimensions(sps)
}

pub(crate) fn info_from_annex_b(input: &[u8]) -> Result<H264Info> {
    for nal in parse_annex_b(input) {
        if nal.nal_unit_type == NalUnitType::Sps {
            return sps_dimensions(&nal.rbsp);
        }
    }
    Err(VideoError::InvalidData("H.264 stream has no SPS"))
}

/// Counts coded pictures in an Annex-B stream.
pub(crate) fn annex_b_frame_count(input: &[u8]) -> u32 {
    let mut count = 0u32;
    for nal in parse_annex_b(input) {
        if matches!(
            nal.nal_unit_type,
            NalUnitType::Slice | NalUnitType::SliceIdr
        ) {
            let mut br = BitReader::new(&nal.rbsp);
            if br.read_ue().unwrap_or(0) == 0 {
                count = count.saturating_add(1);
            }
        }
    }
    count
}

fn frame_to_rgba(frame: &Frame, out: &mut [u8]) -> Result<()> {
    require_rgba(out, frame.width, frame.height)?;
    pixel::yuv420_to_rgba(&frame.y, &frame.u, &frame.v, frame.width, frame.height, out)
}

fn frame_rate_delay_ms(frame_rate: Option<(u32, u32)>) -> u32 {
    match frame_rate {
        Some((num, den)) if num != 0 && den != 0 => {
            ((den as u64 * 1000) / num as u64).min(u32::MAX as u64) as u32
        }
        _ => 0,
    }
}

/// Decodes frame `target` from an Annex-B elementary stream.
pub(crate) fn decode_annex_b(input: &[u8], target: u32, out: &mut [u8]) -> Result<u32> {
    let nals = parse_annex_b(input);
    if nals.is_empty() {
        return Err(VideoError::UnknownFormat);
    }
    let mut decoder = OrderedDecoder::new();
    let mut index = 0u32;
    for nal in &nals {
        for frame in decoder.decode_nal(nal)? {
            if index == target {
                frame_to_rgba(&frame, out)?;
                return Ok(frame_rate_delay_ms(decoder.frame_rate()));
            }
            index = index.saturating_add(1);
        }
    }
    for frame in decoder.flush() {
        if index == target {
            frame_to_rgba(&frame, out)?;
            return Ok(frame_rate_delay_ms(decoder.frame_rate()));
        }
        index = index.saturating_add(1);
    }
    Err(VideoError::FrameOutOfRange {
        index: target,
        count: index,
    })
}

/// A stateful AVCC decoder that can be fed one container sample at a time.
pub(crate) struct AvccDecoder {
    decoder: OrderedDecoder,
    length_size: usize,
    index: u32,
    target: u32,
    done: bool,
}

impl AvccDecoder {
    pub(crate) fn new(config: &[u8], target: u32) -> Result<Self> {
        let avcc = parse_avcc_config(config).map_err(|error| VideoError::InvalidData(error))?;
        let mut decoder = OrderedDecoder::new();
        if avcc.sps_nals.is_empty() || avcc.pps_nals.is_empty() {
            return Err(VideoError::InvalidData("avcC parameter sets"));
        }
        for nal in avcc.sps_nals.iter().chain(avcc.pps_nals.iter()) {
            // Parameter sets never produce a picture, but feeding them can
            // surface a malformed SPS/PPS early, which is worth reporting.
            let _ = decoder.decode_nal(nal)?;
        }
        Ok(Self {
            decoder,
            length_size: avcc.length_size,
            index: 0,
            target,
            done: false,
        })
    }

    /// Feeds one length-prefixed sample. Returns `true` when `target` has been
    /// written into `out`.
    pub(crate) fn push(&mut self, sample: &[u8], out: &mut [u8]) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        if sample.len() < self.length_size {
            return Err(VideoError::UnexpectedEnd);
        }
        for nal in parse_avcc(sample, self.length_size) {
            for frame in self.decoder.decode_nal(&nal)? {
                if self.index == self.target {
                    frame_to_rgba(&frame, out)?;
                    self.done = true;
                    return Ok(true);
                }
                self.index = self.index.saturating_add(1);
            }
        }
        Ok(false)
    }

    /// Flushes the final buffered picture.
    pub(crate) fn finish(&mut self, out: &mut [u8]) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        for frame in self.decoder.flush() {
            if self.index == self.target {
                frame_to_rgba(&frame, out)?;
                self.done = true;
                return Ok(true);
            }
            self.index = self.index.saturating_add(1);
        }
        Ok(false)
    }

    pub(crate) fn frame_rate(&self) -> Option<(u32, u32)> {
        self.decoder.frame_rate()
    }
}

/// A stateful decoder for containers whose samples are Annex-B access units.
pub(crate) struct AnnexBDecoder {
    decoder: OrderedDecoder,
    index: u32,
    target: u32,
    done: bool,
}

impl AnnexBDecoder {
    pub(crate) fn new(target: u32) -> Self {
        Self {
            decoder: OrderedDecoder::new(),
            index: 0,
            target,
            done: false,
        }
    }

    pub(crate) fn push(&mut self, sample: &[u8], out: &mut [u8]) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        for nal in parse_annex_b(sample) {
            for frame in self.decoder.decode_nal(&nal)? {
                if self.index == self.target {
                    frame_to_rgba(&frame, out)?;
                    self.done = true;
                    return Ok(true);
                }
                self.index = self.index.saturating_add(1);
            }
        }
        Ok(false)
    }

    pub(crate) fn finish(&mut self, out: &mut [u8]) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        for frame in self.decoder.flush() {
            if self.index == self.target {
                frame_to_rgba(&frame, out)?;
                self.done = true;
                return Ok(true);
            }
            self.index = self.index.saturating_add(1);
        }
        Ok(false)
    }

    pub(crate) fn frame_rate(&self) -> Option<(u32, u32)> {
        self.decoder.frame_rate()
    }
}

/// Returns `true` if a sample begins with an Annex-B start code.
pub(crate) fn is_annex_b(sample: &[u8]) -> bool {
    sample.starts_with(&[0, 0, 1]) || sample.starts_with(&[0, 0, 0, 1])
}

/// Decodes one Annex-B sample, using a fresh decoder. This is only correct for
/// an intra-only sample or for frame 0; containers use [`AvccDecoder`] for
/// streams that need reference frames.
pub(crate) fn decode_annex_b_sample(sample: &[u8], out: &mut [u8]) -> Result<()> {
    let nals = parse_annex_b(sample);
    let mut decoder = RawDecoder::new();
    for nal in &nals {
        if let Some(frame) = decoder.decode_nal(nal)? {
            frame_to_rgba(&frame, out)?;
            return Ok(());
        }
    }
    if let Some(frame) = decoder.flush() {
        frame_to_rgba(&frame, out)?;
        return Ok(());
    }
    Err(VideoError::InvalidData("H.264 sample produced no frame"))
}
