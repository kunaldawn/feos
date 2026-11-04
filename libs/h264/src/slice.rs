use alloc::vec::Vec;

use crate::bitstream::BitstreamReader;
use crate::error::DecodeError;
use crate::nal::NalUnitType;
use crate::pps::Pps;
use crate::sps::Sps;

/// Upper bound on MMCO commands per picture: each of up to 32 fields' worth of
/// references can be marked twice, plus the two commands that act on all.
const MAX_MMCO_OPS: usize = 66;
/// `difference_of_pic_nums_minus1` is below MaxPicNum, at most 2^16.
const MAX_PIC_NUM: u32 = 1 << 16;
/// Long-term frame indices are below MaxLongTermFrameIdx, at most 16 for frames.
const MAX_LONG_TERM_IDX: u32 = 16;

/// Slice types per H.264 Table 7-6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    pub fn from_raw(val: u32) -> Result<Self, &'static str> {
        match val {
            0 | 5 => Ok(SliceType::P),
            1 | 6 => Ok(SliceType::B),
            2 | 7 => Ok(SliceType::I),
            3 | 8 => Ok(SliceType::Sp),
            4 | 9 => Ok(SliceType::Si),
            _ => Err("invalid slice_type"),
        }
    }
}

/// Slice header (H.264 spec section 7.3.3).
#[derive(Debug)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pic_parameter_set_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: Option<u32>,
    pub no_output_of_prior_pics_flag: bool,
    pub long_term_reference_flag: bool,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    // POC fields (stored for DPB ordering)
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    /// Number of active L0 reference frames for P/B slices (1-based).
    pub num_ref_idx_l0_active: u32,
    /// Number of active L1 reference frames for B slices (1-based).
    pub num_ref_idx_l1_active: u32,
    /// Spatial (true) vs temporal (false) direct mode for B slices.
    pub direct_spatial_mv_pred_flag: bool,
    /// MMCO operations: (op_code, parameter). Currently only op=1 (mark short-term unused).
    pub mmco_ops: Vec<(u32, u32)>,
    /// Ref pic list modification ops for L0: (idc, abs_diff_pic_num_minus1).
    pub ref_list_mod_l0: Vec<(u32, u32)>,
    /// Ref pic list modification ops for L1.
    pub ref_list_mod_l1: Vec<(u32, u32)>,
    /// CABAC context initialization index (0-2) for P/B slices. Only valid when entropy_coding_mode_flag=1.
    pub cabac_init_idc: u32,
    /// Weighted prediction table (spec 7.3.3.2).
    pub weight_table: Option<PredWeightTable>,
    /// True if current picture is a field picture (only when !frame_mbs_only_flag).
    pub field_pic_flag: bool,
    /// True if current field is the bottom field (only when field_pic_flag).
    pub bottom_field_flag: bool,
    /// MbaffFrameFlag = mb_adaptive_frame_field_flag && !field_pic_flag
    pub mbaff_frame_flag: bool,
}

/// Weighted prediction parameters from pred_weight_table().
/// Stores per-reference weight and offset for luma and chroma.
#[derive(Debug, Clone)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    /// Per-reference weights for L0.
    pub l0: Vec<RefWeight>,
    /// L1 weights (B-slices only).
    pub l1: Vec<RefWeight>,
}

#[derive(Debug, Clone)]
pub struct RefWeight {
    pub luma_weight: i32,
    pub luma_offset: i32,
    pub chroma_weight: [i32; 2], // Cb, Cr
    pub chroma_offset: [i32; 2], // Cb, Cr
}

impl SliceHeader {
    pub fn qp_y(&self, pps: &Pps) -> i32 {
        26 + pps.pic_init_qp_minus26 + self.slice_qp_delta
    }
}

/// Parse a slice header from RBSP data. Returns the header and a reader
/// positioned at the start of slice data (macroblock layer).
pub fn parse_slice_header<'a>(
    rbsp: &'a [u8],
    sps: &Sps,
    pps: &Pps,
    nal_unit_type: NalUnitType,
    nal_ref_idc: u8,
) -> Result<(SliceHeader, BitstreamReader<'a>), DecodeError> {
    let mut r = BitstreamReader::new(rbsp);

    let first_mb_in_slice = r.read_ue()?;
    let slice_type_raw = r.read_ue()?;
    let slice_type = SliceType::from_raw(slice_type_raw)?;
    let pic_parameter_set_id = r.read_ue()?;
    if sps.separate_colour_plane_flag {
        let _colour_plane_id = r.read_bits(2)?;
    }

    let frame_num_bits = sps.log2_max_frame_num_minus4 + 4;
    let frame_num = r.read_bits(frame_num_bits as u8)?;
    // field_pic_flag / bottom_field_flag (spec 7.3.3)
    let mut field_pic_flag = false;
    let mut bottom_field_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = r.read_bit()? != 0;
        if field_pic_flag {
            bottom_field_flag = r.read_bit()? != 0;
        }
    }
    let mbaff_frame_flag = sps.mb_adaptive_frame_field_flag && !field_pic_flag;
    let mut idr_pic_id = None;
    if nal_unit_type == NalUnitType::SliceIdr {
        idr_pic_id = Some(r.read_ue()?);
    }

    // pic_order_cnt_type == 0: read pic_order_cnt_lsb
    // pic_order_cnt_type == 1: read delta_pic_order_cnt
    // pic_order_cnt_type == 2: nothing
    let mut pic_order_cnt_lsb = 0u32;
    let mut delta_pic_order_cnt_bottom = 0i32;
    let mut delta_pic_order_cnt = [0i32; 2];

    if sps.pic_order_cnt_type == 0 {
        let poc_lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
        pic_order_cnt_lsb = r.read_bits(poc_lsb_bits as u8)?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt_bottom = r.read_se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        delta_pic_order_cnt[0] = r.read_se()?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            delta_pic_order_cnt[1] = r.read_se()?;
        }
    }

    if pps.redundant_pic_cnt_present_flag {
        let _redundant_pic_cnt = r.read_ue()?;
    }

    // direct_spatial_mv_pred_flag (B-slices only, spec 7.3.3)
    let mut direct_spatial_mv_pred_flag = false;
    if slice_type == SliceType::B {
        direct_spatial_mv_pred_flag = r.read_bit()? != 0;
    }

    // num_ref_idx_active_override (for P/B slices)
    let mut num_ref_idx_l0_active = if slice_type == SliceType::I || slice_type == SliceType::Si {
        0
    } else {
        pps.num_ref_idx_l0_default_active_minus1 + 1
    };
    let mut num_ref_idx_l1_active = if slice_type == SliceType::B {
        pps.num_ref_idx_l1_default_active_minus1 + 1
    } else {
        0
    };
    if slice_type != SliceType::I && slice_type != SliceType::Si {
        let num_ref_idx_active_override_flag = r.read_bit()? != 0;
        if num_ref_idx_active_override_flag {
            num_ref_idx_l0_active = r.read_ue()? + 1;
            if num_ref_idx_l0_active > 32 {
                return Err(DecodeError::InvalidSyntax(
                    "active L0 reference count out of range",
                ));
            }
            if slice_type == SliceType::B {
                num_ref_idx_l1_active = r.read_ue()? + 1;
                if num_ref_idx_l1_active > 32 {
                    return Err(DecodeError::InvalidSyntax(
                        "active L1 reference count out of range",
                    ));
                }
            }
        }
    }

    // ref_pic_list_modification (spec 7.3.3.1)
    let mut ref_list_mod_l0 = Vec::new();
    let mut ref_list_mod_l1 = Vec::new();
    if slice_type != SliceType::I && slice_type != SliceType::Si {
        ref_list_mod_l0 = read_ref_list_modification(&mut r, num_ref_idx_l0_active)?;
        if slice_type == SliceType::B {
            ref_list_mod_l1 = read_ref_list_modification(&mut r, num_ref_idx_l1_active)?;
        }
    }

    // pred_weight_table (spec 7.3.3.2)
    let needs_weight_table = (slice_type == SliceType::P && pps.weighted_pred_flag)
        || (slice_type == SliceType::B && pps.weighted_bipred_idc == 1);
    let weight_table = if needs_weight_table {
        let luma_log2_weight_denom = r.read_ue()?;
        let chroma_log2_weight_denom = r.read_ue()?;
        // Spec constrains these to [0, 7]. Clamp to prevent shift overflow.
        if luma_log2_weight_denom > 7 || chroma_log2_weight_denom > 7 {
            return Err(DecodeError::InvalidSyntax("log2_weight_denom out of range"));
        }
        let luma_def = 1i32 << luma_log2_weight_denom;
        let chroma_def = 1i32 << chroma_log2_weight_denom;

        let l0 = read_weight_list(&mut r, num_ref_idx_l0_active, luma_def, chroma_def)?;
        let l1 = if slice_type == SliceType::B {
            read_weight_list(&mut r, num_ref_idx_l1_active, luma_def, chroma_def)?
        } else {
            Vec::new()
        };

        Some(PredWeightTable {
            luma_log2_weight_denom,
            chroma_log2_weight_denom,
            l0,
            l1,
        })
    } else {
        None
    };

    // dec_ref_pic_marking (spec 7.3.3.3)
    let mut no_output_of_prior_pics_flag = false;
    let mut long_term_reference_flag = false;
    let mut mmco_ops: Vec<(u32, u32)> = Vec::new();
    if nal_unit_type == NalUnitType::SliceIdr {
        no_output_of_prior_pics_flag = r.read_bit()? != 0;
        long_term_reference_flag = r.read_bit()? != 0;
    } else if nal_ref_idc > 0 {
        let adaptive_ref_pic_marking_mode_flag = r.read_bit()? != 0;
        if adaptive_ref_pic_marking_mode_flag {
            loop {
                let op = r.read_ue()?;
                if op == 0 {
                    break;
                }
                if mmco_ops.len() >= MAX_MMCO_OPS {
                    return Err(DecodeError::InvalidSyntax("too many MMCO commands"));
                }
                match op {
                    1 => {
                        let diff = r.read_ue()?;
                        if diff >= MAX_PIC_NUM {
                            return Err(DecodeError::InvalidSyntax("MMCO picNum out of range"));
                        }
                        mmco_ops.push((1, diff));
                    }
                    2 => {
                        let long_term_pic_num = r.read_ue()?;
                        mmco_ops.push((2, long_term_pic_num));
                    }
                    3 => {
                        let diff = r.read_ue()?;
                        let long_term_frame_idx = r.read_ue()?;
                        if diff >= MAX_PIC_NUM || long_term_frame_idx >= MAX_LONG_TERM_IDX {
                            return Err(DecodeError::InvalidSyntax("MMCO long-term out of range"));
                        }
                        mmco_ops.push((3, diff | (long_term_frame_idx << 16)));
                    }
                    4 => {
                        let max_long_term_frame_idx_plus1 = r.read_ue()?;
                        if max_long_term_frame_idx_plus1 > MAX_LONG_TERM_IDX {
                            return Err(DecodeError::InvalidSyntax("MMCO long-term out of range"));
                        }
                        mmco_ops.push((4, max_long_term_frame_idx_plus1));
                    }
                    5 => {
                        mmco_ops.push((5, 0));
                    }
                    6 => {
                        let long_term_frame_idx = r.read_ue()?;
                        if long_term_frame_idx >= MAX_LONG_TERM_IDX {
                            return Err(DecodeError::InvalidSyntax("MMCO long-term out of range"));
                        }
                        mmco_ops.push((6, long_term_frame_idx));
                    }
                    _ => return Err(DecodeError::InvalidSyntax("invalid MMCO command")),
                }
            }
        }
    }

    // cabac_init_idc: parsed when entropy_coding_mode_flag=1 and slice is not I/SI
    // (spec 7.3.3: comes before slice_qp_delta). Spec 7.4.3 constrains the
    // value to the range [0, 2].
    let cabac_init_idc = if pps.entropy_coding_mode_flag
        && slice_type != SliceType::I
        && slice_type != SliceType::Si
    {
        let v = r.read_ue()?;
        if v > 2 {
            return Err(DecodeError::InvalidSyntax("cabac_init_idc out of range"));
        }
        v
    } else {
        0
    };

    let slice_qp_delta = r.read_se()?;
    let slice_qp = (pps.pic_init_qp_minus26 + 26).saturating_add(slice_qp_delta);
    if !(0..=51).contains(&slice_qp) {
        return Err(DecodeError::InvalidSyntax("slice QP out of range"));
    }
    if slice_type == SliceType::Sp || slice_type == SliceType::Si {
        // Rejected by the decoder before use; the fields below aren't read.
        return Err(DecodeError::Unsupported("SP and SI slices not supported"));
    }

    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset_div2 = 0;
    let mut slice_beta_offset_div2 = 0;
    if pps.deblocking_filter_control_present_flag {
        disable_deblocking_filter_idc = r.read_ue()?;
        if disable_deblocking_filter_idc > 2 {
            return Err(DecodeError::InvalidSyntax(
                "disable_deblocking_filter_idc out of range",
            ));
        }
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = r.read_se()?;
            slice_beta_offset_div2 = r.read_se()?;
            if !(-6..=6).contains(&slice_alpha_c0_offset_div2)
                || !(-6..=6).contains(&slice_beta_offset_div2)
            {
                return Err(DecodeError::InvalidSyntax("deblocking offset out of range"));
            }
        }
    }

    let header = SliceHeader {
        first_mb_in_slice,
        slice_type,
        pic_parameter_set_id,
        frame_num,
        idr_pic_id,
        no_output_of_prior_pics_flag,
        long_term_reference_flag,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        num_ref_idx_l0_active,
        num_ref_idx_l1_active,
        direct_spatial_mv_pred_flag,
        mmco_ops,
        ref_list_mod_l0,
        ref_list_mod_l1,
        cabac_init_idc,
        weight_table,
        field_pic_flag,
        bottom_field_flag,
        mbaff_frame_flag,
    };

    Ok((header, r))
}

/// Reads ref_pic_list_modification() for one list (spec 7.3.3.1). The spec
/// allows at most `num_active` commands before the terminator, which also
/// bounds the allocation however long the input is.
fn read_ref_list_modification(
    r: &mut BitstreamReader,
    num_active: u32,
) -> Result<Vec<(u32, u32)>, DecodeError> {
    let mut ops = Vec::new();
    if r.read_bit()? == 0 {
        return Ok(ops);
    }
    loop {
        let idc = r.read_ue()?;
        if idc == 3 {
            return Ok(ops);
        }
        if idc > 2 {
            return Err(DecodeError::InvalidSyntax(
                "modification_of_pic_nums_idc out of range",
            ));
        }
        if ops.len() >= num_active as usize {
            return Err(DecodeError::InvalidSyntax(
                "too many reference list modifications",
            ));
        }
        ops.push((idc, r.read_ue()?));
    }
}

/// Explicit weights and offsets are constrained to -128..=127 (spec 7.4.3.2).
fn check_weight(weight: i32, offset: i32) -> Result<(), &'static str> {
    if (-128..=127).contains(&weight) && (-128..=127).contains(&offset) {
        Ok(())
    } else {
        Err("prediction weight out of range")
    }
}

fn read_weight_list(
    r: &mut BitstreamReader,
    count: u32,
    luma_default: i32,
    chroma_default: i32,
) -> Result<Vec<RefWeight>, &'static str> {
    let mut weights = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut weight = RefWeight {
            luma_weight: luma_default,
            luma_offset: 0,
            chroma_weight: [chroma_default; 2],
            chroma_offset: [0; 2],
        };
        if r.read_bit()? != 0 {
            weight.luma_weight = r.read_se()?;
            weight.luma_offset = r.read_se()?;
            check_weight(weight.luma_weight, weight.luma_offset)?;
        }
        if r.read_bit()? != 0 {
            for component in 0..2 {
                weight.chroma_weight[component] = r.read_se()?;
                weight.chroma_offset[component] = r.read_se()?;
                check_weight(weight.chroma_weight[component], weight.chroma_offset[component])?;
            }
        }
        weights.push(weight);
    }
    Ok(weights)
}

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;
    use crate::nal::{NalUnitType, parse_annex_b};
    use crate::pps::parse_pps;
    use crate::sps::parse_sps;

    #[test]
    fn test_parse_slice_header_idr() {
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
        let pps_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Pps)
            .unwrap();
        let idr_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::SliceIdr)
            .unwrap();

        let sps = parse_sps(&sps_nal.rbsp).unwrap();
        let pps = parse_pps(&pps_nal.rbsp, None).unwrap();

        let (header, _reader) =
            parse_slice_header(&idr_nal.rbsp, &sps, &pps, NalUnitType::SliceIdr, 3).unwrap();

        assert_eq!(header.first_mb_in_slice, 0);
        assert_eq!(header.slice_type, SliceType::I);
        assert_eq!(header.pic_parameter_set_id, 0);
        assert_eq!(header.frame_num, 0);
        assert_eq!(header.idr_pic_id, Some(0));
    }
}
