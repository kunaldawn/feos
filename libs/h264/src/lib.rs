#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
// Some H.264 syntax fields and optimized helper paths are retained for
// profiles/features that are not enabled by the default decoder yet.
#![allow(dead_code)]
#![doc = include_str!("../README.md")]

#[macro_use]
extern crate alloc;

// Test modules use filesystem-backed regression fixtures, while the library
// itself remains `no_std` for every normal build.
#[cfg(test)]
extern crate std;

mod bitstream;
mod boundary;
mod cabac;
mod cabac_tables;
mod cavlc;
mod deblock;
mod decode_cabac;
mod decode_cavlc;
pub mod decoder;
mod dpb;
mod error;
mod ext;
mod inter_pred;
mod intra_pred;
mod mv_pred;
pub mod nal;
mod neighbor;
mod once;
mod pps;
mod residual;
pub mod sei;
mod slice;
mod slice_context;
mod sps;

pub use boundary::FrameBoundary;
pub use decoder::{Decoder, Frame, Limits, NativeFrame, NativePlane, OrderedDecoder};
pub use error::DecodeError;
pub use nal::{
    AvccConfig, NalUnit, NalUnitType, annex_b_ranges, annex_b_units, avcc_ranges, avcc_units,
    parse_annex_b, parse_avcc, parse_avcc_config, parse_nal_bytes,
};
pub use sps::{SpsInfo, Vui, sps_info};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_parser_accepts_start_code_variants() {
        let stream = [0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0x3c];
        let nals = nal::parse_annex_b(&stream);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].nal_unit_type, NalUnitType::Sps);
        assert_eq!(nals[1].nal_unit_type, NalUnitType::Pps);
    }

    #[test]
    fn decoder_rejects_slice_without_parameter_sets() {
        let stream = [0, 0, 1, 0x41, 0x80];
        let nals = nal::parse_annex_b(&stream);
        let mut decoder = Decoder::new();
        assert!(decoder.decode_nal(&nals[0]).is_err());
    }
}
