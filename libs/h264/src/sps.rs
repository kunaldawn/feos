use alloc::vec::Vec;

use crate::bitstream::BitstreamReader;
use crate::error::DecodeError;

/// H.264 Table 7-2: Default 4x4 scaling list for Intra (in scan order).
#[rustfmt::skip]
pub const DEFAULT_SCALING_4X4_INTRA: [u8; 16] = [
     6, 13, 13, 20,
    20, 20, 28, 28,
    28, 28, 32, 32,
    32, 37, 37, 42,
];

/// H.264 Table 7-2: Default 4x4 scaling list for Inter (in scan order).
#[rustfmt::skip]
pub const DEFAULT_SCALING_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20,
    20, 20, 24, 24,
    24, 24, 27, 27,
    27, 30, 30, 34,
];

/// H.264 Table 7-4: Default 8x8 scaling list for Intra, in raster order, which
/// is the order `dequant_8x8` reads 8x8 lists in.
#[rustfmt::skip]
pub const DEFAULT_SCALING_8X8_INTRA: [u8; 64] = [
     6, 10, 13, 16, 18, 23, 25, 27,
    10, 11, 16, 18, 23, 25, 27, 29,
    13, 16, 18, 23, 25, 27, 29, 31,
    16, 18, 23, 25, 27, 29, 31, 33,
    18, 23, 25, 27, 29, 31, 33, 36,
    23, 25, 27, 29, 31, 33, 36, 38,
    25, 27, 29, 31, 33, 36, 38, 40,
    27, 29, 31, 33, 36, 38, 40, 42,
];

/// H.264 Table 7-4: Default 8x8 scaling list for Inter, in raster order.
#[rustfmt::skip]
pub const DEFAULT_SCALING_8X8_INTER: [u8; 64] = [
     9, 13, 15, 17, 19, 21, 22, 24,
    13, 13, 17, 19, 21, 22, 24, 25,
    15, 17, 19, 21, 22, 24, 25, 27,
    17, 19, 21, 22, 24, 25, 27, 28,
    19, 21, 22, 24, 25, 27, 28, 30,
    21, 22, 24, 25, 27, 28, 30, 32,
    22, 24, 25, 27, 28, 30, 32, 33,
    24, 25, 27, 28, 30, 32, 33, 35,
];

/// Zigzag scan position to raster position for an 8x8 block.
#[rustfmt::skip]
const ZIGZAG_8X8_RASTER: [u8; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// Largest coded dimension in macroblocks: 16384 luma samples.
const MAX_DIMENSION_MBS: u32 = 1024;

/// Flat scaling list (no custom scaling) — all 16s.
pub const FLAT_SCALING_4X4: [u8; 16] = [16; 16];

/// Table 7-2 fallback for an absent 4x4 scaling list.
pub(crate) fn fallback_4x4(lists: &[[u8; 16]; 6], index: usize) -> [u8; 16] {
    match index {
        0 => DEFAULT_SCALING_4X4_INTRA,
        3 => DEFAULT_SCALING_4X4_INTER,
        _ => lists[index - 1],
    }
}

/// Sequence Parameter Set (H.264 spec section 7.3.2.1).
#[derive(Debug)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_set0_flag: bool,
    pub constraint_set1_flag: bool,
    pub constraint_set2_flag: bool,
    pub constraint_set3_flag: bool,
    pub constraint_set4_flag: bool,
    pub constraint_set5_flag: bool,
    pub level_idc: u8,
    pub seq_parameter_set_id: u32,

    // High profile and above fields
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    pub seq_scaling_matrix_present_flag: bool,
    /// 4x4 scaling matrices [0..5]: Intra Y, Intra Cb, Intra Cr, Inter Y, Inter Cb, Inter Cr.
    /// Default is all 16s (flat scaling). Stored in zigzag scan order.
    pub scaling_list_4x4: [[u8; 16]; 6],
    /// 8x8 scaling matrices [0..1]: Intra Y, Inter Y (for 4:2:0).
    /// Default is all 16s. Stored in raster order within each 8x8 block.
    pub scaling_list_8x8: [[u8; 64]; 2],

    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub offset_for_ref_frame: Vec<i32>,

    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,

    pub frame_cropping_flag: bool,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,

    pub vui_parameters_present_flag: bool,

    /// VUI timing info: numerator of clock tick. Set when
    /// `vui_parameters_present_flag` and `timing_info_present_flag` are both
    /// true. The frame rate is `time_scale / (2 * num_units_in_tick)` for
    /// progressive content (spec E.2.1).
    pub num_units_in_tick: Option<u32>,
    /// VUI timing info: denominator of clock tick (typically a multiple of
    /// the frame rate, e.g. 60000 for 29.97 fps).
    pub time_scale: Option<u32>,
    /// VUI timing info: when set, frames are emitted at a fixed rate of
    /// `time_scale / (2 * num_units_in_tick)`.
    pub fixed_frame_rate_flag: bool,
    /// Display and colour metadata from the VUI (spec Annex E).
    pub vui: Vui,
}

/// Display and colour metadata carried in the VUI (spec Annex E). Every field
/// has the value the spec uses when the syntax element is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vui {
    /// Sample aspect ratio as `(width, height)`; `(0, 0)` when unspecified.
    pub sample_aspect_ratio: (u16, u16),
    /// `video_full_range_flag`: luma spans 0..=255 rather than 16..=235.
    pub full_range: bool,
    /// `colour_primaries`; 2 means unspecified.
    pub colour_primaries: u8,
    /// `transfer_characteristics`; 2 means unspecified.
    pub transfer_characteristics: u8,
    /// `matrix_coefficients`; 2 means unspecified.
    pub matrix_coefficients: u8,
    /// `max_num_reorder_frames` when `bitstream_restriction_flag` is set.
    pub max_num_reorder_frames: Option<u8>,
    /// `max_dec_frame_buffering` when `bitstream_restriction_flag` is set.
    pub max_dec_frame_buffering: Option<u8>,
}

impl Default for Vui {
    fn default() -> Self {
        Self {
            sample_aspect_ratio: (0, 0),
            full_range: false,
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            max_num_reorder_frames: None,
            max_dec_frame_buffering: None,
        }
    }
}

/// Stream properties a demuxer or player needs from an SPS, without having to
/// start a decoder. See [`sps_info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    /// `profile_idc`.
    pub profile_idc: u8,
    /// `level_idc`.
    pub level_idc: u8,
    /// Visible width in luma samples, after cropping.
    pub width: u32,
    /// Visible height in luma samples, after cropping.
    pub height: u32,
    /// Coded width in luma samples, always a multiple of 16.
    pub coded_width: u32,
    /// Coded height in luma samples, always a multiple of 16.
    pub coded_height: u32,
    /// Frame rate from the VUI, as `(numerator, denominator)`.
    pub frame_rate: Option<(u32, u32)>,
    /// Display and colour metadata.
    pub vui: Vui,
    /// Why this decoder can't reconstruct the stream, if it can't.
    pub unsupported: Option<&'static str>,
}

/// Parses an SPS RBSP (NAL header byte already stripped) into the properties a
/// demuxer needs. Syntax errors are reported; a valid but unsupported stream
/// isn't an error here, and is described by [`SpsInfo::unsupported`].
pub fn sps_info(rbsp: &[u8]) -> Result<SpsInfo, DecodeError> {
    let sps = parse_sps(rbsp)?;
    Ok(SpsInfo {
        profile_idc: sps.profile_idc,
        level_idc: sps.level_idc,
        width: sps.width(),
        height: sps.height(),
        coded_width: sps.coded_width(),
        coded_height: sps.coded_height(),
        frame_rate: sps.frame_rate(),
        vui: sps.vui,
        unsupported: sps.check_supported().err(),
    })
}

/// Table A-1 MaxDpbMbs for a `level_idc`. Unknown levels get the largest.
fn max_dpb_mbs(level_idc: u8, constraint_set3: bool) -> u32 {
    match level_idc {
        9 | 10 => 396,
        11 if constraint_set3 => 396, // Level 1b
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    }
}

impl Sps {
    /// Coded width in macroblocks.
    pub fn mb_width(&self) -> u32 {
        self.pic_width_in_mbs_minus1.saturating_add(1)
    }

    /// Coded height of a frame in macroblocks. Always even when
    /// `frame_mbs_only_flag` is clear, which MBAFF geometry relies on.
    pub fn mb_height(&self) -> u32 {
        self.pic_height_in_map_units_minus1
            .saturating_add(1)
            .saturating_mul(if self.frame_mbs_only_flag { 1 } else { 2 })
    }

    /// Coded width in luma samples.
    pub fn coded_width(&self) -> u32 {
        self.mb_width().saturating_mul(16)
    }

    /// Coded height in luma samples.
    pub fn coded_height(&self) -> u32 {
        self.mb_height().saturating_mul(16)
    }

    /// Luma columns cropped from the left edge.
    pub fn crop_left(&self) -> u32 {
        self.frame_crop_left_offset
            .saturating_mul(self.crop_unit_x())
    }

    /// Luma rows cropped from the top edge.
    pub fn crop_top(&self) -> u32 {
        self.frame_crop_top_offset.saturating_mul(self.crop_unit_y())
    }

    /// Rejects valid syntax this decoder can't reconstruct, rather than
    /// producing garbage.
    pub fn check_supported(&self) -> Result<(), &'static str> {
        if self.chroma_format_idc > 2 {
            return Err("4:4:4 chroma is not supported");
        }
        if self.bit_depth_luma_minus8 != self.bit_depth_chroma_minus8 {
            return Err("different luma and chroma bit depths are not supported");
        }
        if self.bit_depth_luma_minus8 > 2 {
            return Err("more than 10 bits per sample is not supported");
        }
        if self.qpprime_y_zero_transform_bypass_flag {
            return Err("lossless transform bypass is not supported");
        }
        Ok(())
    }

    /// Frames a decoder must hold back to output in display order: the VUI's
    /// `max_num_reorder_frames`, otherwise the level's limit.
    pub fn reorder_depth(&self) -> usize {
        if self.pic_order_cnt_type == 2 {
            return 0; // Output order equals decode order.
        }
        if let Some(frames) = self.vui.max_num_reorder_frames {
            return (frames as usize).min(16);
        }
        // Intra-only profiles with constraint_set3 never reorder (A.3.2).
        if self.constraint_set3_flag && matches!(self.profile_idc, 44 | 86 | 100 | 110 | 122 | 244)
        {
            return 0;
        }
        let mbs = self.mb_width().saturating_mul(self.mb_height()).max(1);
        let frames = max_dpb_mbs(self.level_idc, self.constraint_set3_flag) / mbs;
        (frames as usize).clamp(self.max_num_ref_frames as usize, 16)
    }

    /// Frame rate from VUI timing info, if present, as `(numerator, denominator)`.
    ///
    /// For progressive content (the common case), this is
    /// `time_scale / (2 * num_units_in_tick)`. For example, an x264 stream
    /// at 29.97 fps would return `Some((60000, 2002))`.
    ///
    /// Returns `None` if the SPS does not contain VUI timing info, or if
    /// the values are zero (which would be a division-by-zero).
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let num_units = self.num_units_in_tick?;
        let time_scale = self.time_scale?;
        if num_units == 0 || time_scale == 0 {
            return None;
        }
        // Spec E.2.1: clock_tick = num_units_in_tick / time_scale
        // For progressive frame coding, each frame is 2 ticks → fps = time_scale / (2 * num_units_in_tick)
        Some((time_scale, 2u32.saturating_mul(num_units)))
    }

    /// Frame rate as a single floating-point value, computed from
    /// [`frame_rate`](Self::frame_rate). Returns `None` if VUI timing info
    /// is not present.
    pub fn frame_rate_f64(&self) -> Option<f64> {
        let (num, den) = self.frame_rate()?;
        Some(num as f64 / den as f64)
    }

    /// Width in pixels (accounting for cropping).
    pub fn width(&self) -> u32 {
        let crop_x = if self.frame_cropping_flag {
            self.frame_crop_left_offset
                .saturating_add(self.frame_crop_right_offset)
                .saturating_mul(self.crop_unit_x())
        } else {
            0
        };
        self.coded_width().saturating_sub(crop_x)
    }

    /// Height in pixels (accounting for cropping).
    pub fn height(&self) -> u32 {
        let crop_y = if self.frame_cropping_flag {
            self.frame_crop_top_offset
                .saturating_add(self.frame_crop_bottom_offset)
                .saturating_mul(self.crop_unit_y())
        } else {
            0
        };
        self.coded_height().saturating_sub(crop_y)
    }

    fn crop_unit_x(&self) -> u32 {
        if self.chroma_format_idc == 0 {
            1
        } else {
            // SubWidthC: 2 for 4:2:0 and 4:2:2, 1 for 4:4:4
            if self.chroma_format_idc == 3 { 1 } else { 2 }
        }
    }

    fn crop_unit_y(&self) -> u32 {
        let sub_height_c = if self.chroma_format_idc == 1 { 2 } else { 1 };
        let frame_factor = if self.frame_mbs_only_flag { 1 } else { 2 };
        if self.chroma_format_idc == 0 {
            frame_factor
        } else {
            sub_height_c * frame_factor
        }
    }
}

fn is_high_profile(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// Parse an SPS from RBSP data (NAL header byte already stripped).
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps, DecodeError> {
    let mut r = BitstreamReader::new(rbsp);

    let profile_idc = r.read_bits(8)? as u8;
    let constraint_set0_flag = r.read_bit()? != 0;
    let constraint_set1_flag = r.read_bit()? != 0;
    let constraint_set2_flag = r.read_bit()? != 0;
    let constraint_set3_flag = r.read_bit()? != 0;
    let constraint_set4_flag = r.read_bit()? != 0;
    let constraint_set5_flag = r.read_bit()? != 0;
    let _reserved_zero_2bits = r.read_bits(2)?;
    let level_idc = r.read_bits(8)? as u8;
    let seq_parameter_set_id = r.read_ue()?;
    if seq_parameter_set_id > 31 {
        return Err(DecodeError::InvalidSyntax(
            "seq_parameter_set_id out of range",
        ));
    }

    let mut chroma_format_idc = 1; // default
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    let mut qpprime_y_zero_transform_bypass_flag = false;
    let mut seq_scaling_matrix_present_flag = false;
    // Default: flat scaling (all 16s) when seq_scaling_matrix_present_flag is false
    let mut scaling_list_4x4 = [FLAT_SCALING_4X4; 6];
    let mut scaling_list_8x8 = [[16u8; 64]; 2];

    if is_high_profile(profile_idc) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc > 3 {
            return Err(DecodeError::InvalidSyntax("chroma_format_idc out of range"));
        }
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = r.read_bit()? != 0;
        }
        bit_depth_luma_minus8 = r.read_ue()?;
        bit_depth_chroma_minus8 = r.read_ue()?;
        qpprime_y_zero_transform_bypass_flag = r.read_bit()? != 0;
        seq_scaling_matrix_present_flag = r.read_bit()? != 0;
        if seq_scaling_matrix_present_flag {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let present = r.read_bit()? != 0;
                if present {
                    if i < 6 {
                        scaling_list_4x4[i] = parse_list_4x4(&mut r, i)?;
                    } else if i < 8 {
                        scaling_list_8x8[i - 6] = parse_list_8x8(&mut r, i - 6)?;
                    } else {
                        let _ = parse_scaling_list::<64>(&mut r)?;
                    }
                } else if i < 6 {
                    // Fallback per H.264 Table 7-2:
                    // i=0: Default_4x4_Intra, i=3: Default_4x4_Inter
                    // i=1,2: copy from previous, i=4,5: copy from previous
                    scaling_list_4x4[i] = fallback_4x4(&scaling_list_4x4, i);
                } else if i < 8 {
                    scaling_list_8x8[i - 6] = if i == 6 {
                        DEFAULT_SCALING_8X8_INTRA
                    } else {
                        DEFAULT_SCALING_8X8_INTER
                    };
                }
            }
        }
    }

    if bit_depth_luma_minus8 > 6 || bit_depth_chroma_minus8 > 6 {
        return Err(DecodeError::InvalidSyntax("bit depth out of range"));
    }

    let log2_max_frame_num_minus4 = r.read_ue()?;
    if log2_max_frame_num_minus4 > 12 {
        return Err(DecodeError::InvalidSyntax(
            "log2_max_frame_num_minus4 out of range",
        ));
    }
    let pic_order_cnt_type = r.read_ue()?;
    if pic_order_cnt_type > 2 {
        return Err(DecodeError::InvalidSyntax("pic_order_cnt_type out of range"));
    }

    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    let mut delta_pic_order_always_zero_flag = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom_field = 0;
    let mut num_ref_frames_in_pic_order_cnt_cycle = 0;
    let mut offset_for_ref_frame = Vec::new();

    if pic_order_cnt_type == 0 {
        log2_max_pic_order_cnt_lsb_minus4 = r.read_ue()?;
        if log2_max_pic_order_cnt_lsb_minus4 > 12 {
            return Err(DecodeError::InvalidSyntax(
                "log2_max_pic_order_cnt_lsb_minus4 out of range",
            ));
        }
    } else if pic_order_cnt_type == 1 {
        delta_pic_order_always_zero_flag = r.read_bit()? != 0;
        offset_for_non_ref_pic = r.read_se()?;
        offset_for_top_to_bottom_field = r.read_se()?;
        num_ref_frames_in_pic_order_cnt_cycle = r.read_ue()?;
        if num_ref_frames_in_pic_order_cnt_cycle > 255 {
            return Err(DecodeError::InvalidSyntax(
                "num_ref_frames_in_pic_order_cnt_cycle out of range",
            ));
        }
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            offset_for_ref_frame.push(r.read_se()?);
        }
    }

    let max_num_ref_frames = r.read_ue()?;
    if max_num_ref_frames > 16 {
        return Err(DecodeError::InvalidSyntax("max_num_ref_frames out of range"));
    }
    let gaps_in_frame_num_value_allowed_flag = r.read_bit()? != 0;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()? != 0;

    // Level 6.2 tops out at 8192x4320; 16384 on either side leaves headroom
    // without letting the macroblock arithmetic below overflow.
    if pic_width_in_mbs_minus1 >= MAX_DIMENSION_MBS
        || pic_height_in_map_units_minus1 >= MAX_DIMENSION_MBS
    {
        return Err(DecodeError::InvalidSyntax("SPS dimensions out of range"));
    }

    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = r.read_bit()? != 0;
    }

    let direct_8x8_inference_flag = r.read_bit()? != 0;

    let frame_cropping_flag = r.read_bit()? != 0;
    let mut frame_crop_left_offset = 0;
    let mut frame_crop_right_offset = 0;
    let mut frame_crop_top_offset = 0;
    let mut frame_crop_bottom_offset = 0;
    if frame_cropping_flag {
        frame_crop_left_offset = r.read_ue()?;
        frame_crop_right_offset = r.read_ue()?;
        frame_crop_top_offset = r.read_ue()?;
        frame_crop_bottom_offset = r.read_ue()?;
    }

    // The crop must leave at least one sample in each direction.
    let chroma_array_is_mono = separate_colour_plane_flag || chroma_format_idc == 0;
    let crop_unit_x: u64 = if chroma_array_is_mono || chroma_format_idc == 3 {
        1
    } else {
        2
    };
    let frame_factor: u64 = if frame_mbs_only_flag { 1 } else { 2 };
    let crop_unit_y: u64 = if !chroma_array_is_mono && chroma_format_idc == 1 {
        2 * frame_factor
    } else {
        frame_factor
    };
    let coded_w = (pic_width_in_mbs_minus1 as u64 + 1) * 16;
    let coded_h = (pic_height_in_map_units_minus1 as u64 + 1) * 16 * frame_factor;
    if (frame_crop_left_offset as u64 + frame_crop_right_offset as u64) * crop_unit_x >= coded_w
        || (frame_crop_top_offset as u64 + frame_crop_bottom_offset as u64) * crop_unit_y >= coded_h
    {
        return Err(DecodeError::InvalidSyntax("frame crop out of range"));
    }

    let vui_parameters_present_flag = r.read_bit()? != 0;
    let mut num_units_in_tick: Option<u32> = None;
    let mut time_scale: Option<u32> = None;
    let mut fixed_frame_rate_flag = false;
    let mut vui = Vui::default();
    if vui_parameters_present_flag {
        // Streams with a damaged VUI still decode, so keep what was read.
        let _ = parse_vui(
            &mut r,
            &mut vui,
            &mut num_units_in_tick,
            &mut time_scale,
            &mut fixed_frame_rate_flag,
        );
    }

    Ok(Sps {
        profile_idc,
        constraint_set0_flag,
        constraint_set1_flag,
        constraint_set2_flag,
        constraint_set3_flag,
        constraint_set4_flag,
        constraint_set5_flag,
        level_idc,
        seq_parameter_set_id,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        qpprime_y_zero_transform_bypass_flag,
        seq_scaling_matrix_present_flag,
        scaling_list_4x4,
        scaling_list_8x8,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        num_ref_frames_in_pic_order_cnt_cycle,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_value_allowed_flag,
        pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        frame_cropping_flag,
        frame_crop_left_offset,
        frame_crop_right_offset,
        frame_crop_top_offset,
        frame_crop_bottom_offset,
        vui_parameters_present_flag,
        num_units_in_tick,
        time_scale,
        fixed_frame_rate_flag,
        vui,
    })
}

/// Parses the VUI (spec E.1.1), stopping at the first truncated field.
fn parse_vui(
    r: &mut BitstreamReader,
    vui: &mut Vui,
    num_units_in_tick: &mut Option<u32>,
    time_scale: &mut Option<u32>,
    fixed_frame_rate_flag: &mut bool,
) -> Result<(), &'static str> {
    if r.read_bit()? != 0 {
        // Table E-1; 255 is Extended_SAR.
        const SAR: [(u16, u16); 17] = [
            (0, 0),
            (1, 1),
            (12, 11),
            (10, 11),
            (16, 11),
            (40, 33),
            (24, 11),
            (20, 11),
            (32, 11),
            (80, 33),
            (18, 11),
            (15, 11),
            (64, 33),
            (160, 99),
            (4, 3),
            (3, 2),
            (2, 1),
        ];
        let aspect_ratio_idc = r.read_bits(8)? as usize;
        if aspect_ratio_idc == 255 {
            let w = r.read_bits(16)? as u16;
            let h = r.read_bits(16)? as u16;
            vui.sample_aspect_ratio = (w, h);
        } else if let Some(&sar) = SAR.get(aspect_ratio_idc) {
            vui.sample_aspect_ratio = sar;
        }
    }
    if r.read_bit()? != 0 {
        r.read_bit()?; // overscan_appropriate_flag
    }
    if r.read_bit()? != 0 {
        r.read_bits(3)?; // video_format
        vui.full_range = r.read_bit()? != 0;
        if r.read_bit()? != 0 {
            vui.colour_primaries = r.read_bits(8)? as u8;
            vui.transfer_characteristics = r.read_bits(8)? as u8;
            vui.matrix_coefficients = r.read_bits(8)? as u8;
        }
    }
    if r.read_bit()? != 0 {
        r.read_ue()?; // chroma_sample_loc_type_top_field
        r.read_ue()?; // chroma_sample_loc_type_bottom_field
    }
    if r.read_bit()? != 0 {
        *num_units_in_tick = Some(r.read_bits(32)?);
        *time_scale = Some(r.read_bits(32)?);
        *fixed_frame_rate_flag = r.read_bit()? != 0;
    }
    let nal_hrd = r.read_bit()? != 0;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.read_bit()? != 0;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.read_bit()?; // low_delay_hrd_flag
    }
    r.read_bit()?; // pic_struct_present_flag
    if r.read_bit()? != 0 {
        // bitstream_restriction
        r.read_bit()?; // motion_vectors_over_pic_boundaries_flag
        r.read_ue()?; // max_bytes_per_pic_denom
        r.read_ue()?; // max_bits_per_mb_denom
        r.read_ue()?; // log2_max_mv_length_horizontal
        r.read_ue()?; // log2_max_mv_length_vertical
        let reorder = r.read_ue()?;
        let buffering = r.read_ue()?;
        // A reader past the end returns zero bits, so only trust these when
        // real data was left to hold them.
        if r.bits_remaining() > 0 && reorder <= buffering && buffering <= 16 {
            vui.max_num_reorder_frames = Some(reorder as u8);
            vui.max_dec_frame_buffering = Some(buffering as u8);
        }
    }
    Ok(())
}

/// Skips hrd_parameters() (spec E.1.2).
fn skip_hrd(r: &mut BitstreamReader) -> Result<(), &'static str> {
    let cpb_cnt_minus1 = r.read_ue()?;
    if cpb_cnt_minus1 > 31 {
        return Err("cpb_cnt_minus1 out of range");
    }
    r.read_bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.read_ue()?;
        r.read_ue()?;
        r.read_bit()?;
    }
    r.read_bits(20)?; // four 5-bit delay and length fields
    Ok(())
}

/// Parse a scaling list from the bitstream (H.264 spec 7.3.2.1.1).
/// Returns a flat array of `N` scale values in scan order, or `None` when the
/// list asks for the default list (useDefaultScalingMatrixFlag).
pub fn parse_scaling_list<const N: usize>(
    r: &mut BitstreamReader,
) -> Result<Option<[u8; N]>, DecodeError> {
    let mut scaling_list = [0u8; N];
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for (index, entry) in scaling_list.iter_mut().enumerate() {
        if next_scale != 0 {
            let delta = r.read_se()?;
            if !(-128..=127).contains(&delta) {
                return Err(DecodeError::InvalidSyntax("delta_scale out of range"));
            }
            next_scale = (last_scale + delta + 256).rem_euclid(256);
            if index == 0 && next_scale == 0 {
                return Ok(None);
            }
        }
        let val = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
        *entry = val as u8;
        last_scale = val;
    }
    Ok(Some(scaling_list))
}

/// Parses a 4x4 list `index` (0-5), resolving "use default".
pub(crate) fn parse_list_4x4(
    r: &mut BitstreamReader,
    index: usize,
) -> Result<[u8; 16], DecodeError> {
    Ok(parse_scaling_list::<16>(r)?.unwrap_or(if index < 3 {
        DEFAULT_SCALING_4X4_INTRA
    } else {
        DEFAULT_SCALING_4X4_INTER
    }))
}

/// Parses an 8x8 list (0 Intra Y, 1 Inter Y), resolving "use default", and
/// returns it in raster order.
pub(crate) fn parse_list_8x8(
    r: &mut BitstreamReader,
    index: usize,
) -> Result<[u8; 64], DecodeError> {
    let Some(scan) = parse_scaling_list::<64>(r)? else {
        return Ok(if index == 0 {
            DEFAULT_SCALING_8X8_INTRA
        } else {
            DEFAULT_SCALING_8X8_INTER
        });
    };
    let mut raster = [0u8; 64];
    for (value, &position) in scan.iter().zip(ZIGZAG_8X8_RASTER.iter()) {
        raster[position as usize] = *value;
    }
    Ok(raster)
}

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;
    use crate::nal::{NalUnitType, parse_annex_b};

    #[test]
    fn test_parse_sps_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sps)
            .unwrap();
        let sps = parse_sps(&sps_nal.rbsp).unwrap();

        assert_eq!(sps.profile_idc, 66); // Baseline
        assert!(sps.constraint_set1_flag); // Constrained Baseline
        assert_eq!(sps.level_idc, 10); // Level 1.0
        assert_eq!(sps.seq_parameter_set_id, 0);
        assert_eq!(sps.chroma_format_idc, 1); // 4:2:0 (default for baseline)
        assert_eq!(sps.width(), 16);
        assert_eq!(sps.height(), 16);
        assert!(sps.frame_mbs_only_flag);
        assert_eq!(sps.max_num_ref_frames, 0);
        assert_eq!(sps.pic_order_cnt_type, 2);
    }

    #[test]
    fn test_parse_sps_vui_frame_rate() {
        // preset_medium.h264's SPS contains VUI timing info with
        // num_units_in_tick=1, time_scale=60, which gives a frame rate of
        // 60 / (2 * 1) = 30 fps per spec E.2.1. (Note that ffprobe's
        // avg_frame_rate is derived from frame count / duration and may
        // differ — this test verifies the SPS parser, not the avg rate.)
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/preset_medium.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sps)
            .unwrap();
        let sps = parse_sps(&sps_nal.rbsp).unwrap();

        assert!(sps.vui_parameters_present_flag);
        assert_eq!(sps.num_units_in_tick, Some(1));
        assert_eq!(sps.time_scale, Some(60));
        let (num, den) = sps.frame_rate().expect("expected VUI timing info");
        let fps = num as f64 / den as f64;
        assert!(
            (fps - 30.0).abs() < 1e-6,
            "expected 30 fps, got {} ({}/{})",
            fps,
            num,
            den
        );
        assert_eq!(sps.frame_rate_f64(), Some(30.0));
    }

    #[test]
    fn test_frame_rate_returns_none_without_timing() {
        // Construct a synthetic Sps with no timing info to verify
        // frame_rate() returns None gracefully.
        let mut sps = Sps {
            profile_idc: 66,
            constraint_set0_flag: false,
            constraint_set1_flag: false,
            constraint_set2_flag: false,
            constraint_set3_flag: false,
            constraint_set4_flag: false,
            constraint_set5_flag: false,
            level_idc: 30,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass_flag: false,
            seq_scaling_matrix_present_flag: false,
            scaling_list_4x4: [[16; 16]; 6],
            scaling_list_8x8: [[16; 64]; 2],
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            offset_for_ref_frame: vec![],
            max_num_ref_frames: 1,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            frame_cropping_flag: false,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
            vui_parameters_present_flag: false,
            num_units_in_tick: None,
            time_scale: None,
            fixed_frame_rate_flag: false,
            vui: Vui::default(),
        };
        assert!(sps.frame_rate().is_none());
        assert!(sps.frame_rate_f64().is_none());

        // Also: zero values should yield None (avoid division by zero)
        sps.num_units_in_tick = Some(0);
        sps.time_scale = Some(60);
        assert!(sps.frame_rate().is_none());
    }
}
