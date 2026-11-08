//! Finds where frames start in a stream of NAL units without decoding it. Two field
//! pictures make one frame, which a look at `first_mb_in_slice` alone can't tell.

use crate::bitstream::BitstreamReader;
use crate::nal::{NalUnitType, parse_nal_bytes};
use crate::sps::parse_sps;

#[derive(Clone, Copy)]
struct SpsFacts {
    frame_mbs_only: bool,
    frame_num_bits: u8,
    separate_plane: bool,
}

/// Bytes of a slice NAL that hold everything looked at here.
const SLICE_PREFIX: usize = 32;

/// Tracks parameter sets and fields over a stream's NAL units, in order.
pub struct FrameBoundary {
    sps: [Option<SpsFacts>; 32],
    pps_sps: [u8; 256],
    /// A first field waiting for its second: its parity (bottom?) and frame_num.
    open_field: Option<(bool, u32)>,
}

impl Default for FrameBoundary {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameBoundary {
    /// Starts with no parameter sets known.
    pub fn new() -> Self {
        Self {
            sps: [None; 32],
            pps_sps: [255; 256],
            open_field: None,
        }
    }

    /// Takes the next NAL unit, without its start code, and tells whether it's the first
    /// slice of a new frame.
    pub fn starts_frame(&mut self, nal: &[u8]) -> bool {
        match nal.first().map(|header| header & 0x1f) {
            Some(7) => {
                if let Some(unit) = parse_nal_bytes(nal)
                    && let Ok(sps) = parse_sps(&unit.rbsp)
                        && let Some(slot) = self.sps.get_mut(sps.seq_parameter_set_id as usize) {
                            *slot = Some(SpsFacts {
                                frame_mbs_only: sps.frame_mbs_only_flag,
                                frame_num_bits: sps.log2_max_frame_num_minus4 as u8 + 4,
                                separate_plane: sps.separate_colour_plane_flag,
                            });
                        }
                false
            }
            Some(8) => {
                if let Some(unit) = parse_nal_bytes(nal) {
                    let mut r = BitstreamReader::new(&unit.rbsp);
                    if let (Ok(pps_id), Ok(sps_id)) = (r.read_ue(), r.read_ue())
                        && pps_id < 256 && sps_id < 32 {
                            self.pps_sps[pps_id as usize] = sps_id as u8;
                        }
                }
                false
            }
            Some(1 | 5) => self.slice(nal),
            _ => false,
        }
    }

    fn slice(&mut self, nal: &[u8]) -> bool {
        // first_mb_in_slice leads, so a set top bit means it's zero. Emulation
        // prevention can't affect this byte.
        if nal.get(1).is_none_or(|byte| byte & 0x80 == 0) {
            return false;
        }
        let idr = nal[0] & 0x1f == 5;
        let field = self.field_of(nal);
        let second = match (field, self.open_field) {
            (Some((bottom, frame_num)), Some((open_bottom, open_num))) => {
                !idr && bottom != open_bottom && frame_num == open_num
            }
            _ => false,
        };
        self.open_field = if second { None } else { field };
        !second
    }

    /// A field slice's parity and frame_num.
    fn field_of(&self, nal: &[u8]) -> Option<(bool, u32)> {
        let unit = parse_nal_bytes(&nal[..nal.len().min(SLICE_PREFIX)])?;
        if !matches!(unit.nal_unit_type, NalUnitType::Slice | NalUnitType::SliceIdr) {
            return None;
        }
        let mut r = BitstreamReader::new(&unit.rbsp);
        r.read_ue().ok()?;
        r.read_ue().ok()?;
        let pps_id = r.read_ue().ok()? as usize;
        let sps_id = *self.pps_sps.get(pps_id)? as usize;
        let sps = (*self.sps.get(sps_id)?)?;
        if sps.frame_mbs_only {
            return None;
        }
        if sps.separate_plane {
            r.read_bits(2).ok()?;
        }
        let frame_num = r.read_bits(sps.frame_num_bits).ok()?;
        if r.read_bit().ok()? == 0 {
            return None;
        }
        let bottom = r.read_bit().ok()? != 0;
        Some((bottom, frame_num))
    }
}
