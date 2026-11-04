//! H.264 NAL unit framing — Annex B and AVCC parsers.
//!
//! Provides two parser entry points, each also available as an iterator
//! ([`annex_b_units`], [`avcc_units`]) that doesn't build a list:
//!
//! - [`parse_annex_b`] for start-code delimited bitstreams
//!   (`.h264` files, RTP payloads, broadcast TS).
//! - [`parse_avcc`] + [`parse_avcc_config`] for length-prefixed bitstreams
//!   (NAL units inside MP4/MKV containers).
//!
//! Both produce [`NalUnit`] values that can be fed directly to
//! [`Decoder::decode_nal`](crate::decoder::Decoder::decode_nal).

use alloc::vec::Vec;

use alloc::borrow::Cow;

/// H.264 NAL unit type identifier (spec Table 7-1).
///
/// Only the types this decoder cares about are named explicitly; everything
/// else falls into [`NalUnitType::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalUnitType {
    Slice,               // 1
    SliceDataA,          // 2
    SliceDataB,          // 3
    SliceDataC,          // 4
    SliceIdr,            // 5
    Sei,                 // 6
    Sps,                 // 7
    Pps,                 // 8
    AccessUnitDelimiter, // 9
    EndOfSequence,       // 10
    EndOfStream,         // 11
    FillerData,          // 12
    Other(u8),
}

impl From<u8> for NalUnitType {
    fn from(val: u8) -> Self {
        match val {
            1 => NalUnitType::Slice,
            2 => NalUnitType::SliceDataA,
            3 => NalUnitType::SliceDataB,
            4 => NalUnitType::SliceDataC,
            5 => NalUnitType::SliceIdr,
            6 => NalUnitType::Sei,
            7 => NalUnitType::Sps,
            8 => NalUnitType::Pps,
            9 => NalUnitType::AccessUnitDelimiter,
            10 => NalUnitType::EndOfSequence,
            11 => NalUnitType::EndOfStream,
            12 => NalUnitType::FillerData,
            v => NalUnitType::Other(v),
        }
    }
}

/// A parsed H.264 NAL unit.
///
/// Contains the unit type, reference indicator, and the Raw Byte Sequence
/// Payload (RBSP) — which is the NAL payload with emulation prevention bytes
/// removed.
///
/// The `rbsp` field is a [`Cow`]: when no emulation prevention bytes are
/// present (the common case), it borrows directly from the input slice with
/// no allocation.
#[derive(Debug)]
pub struct NalUnit<'a> {
    /// `nal_ref_idc` from the NAL header (0..3). Non-zero means the NAL
    /// belongs to a reference picture.
    pub nal_ref_idc: u8,
    /// NAL unit type from the NAL header (5 bits).
    pub nal_unit_type: NalUnitType,
    /// RBSP payload (emulation prevention bytes removed, or borrowed directly
    /// from the input when no emulation prevention bytes are present).
    pub rbsp: Cow<'a, [u8]>,
}

/// Split an Annex B bytestream into NAL units.
///
/// Handles both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes
/// and removes emulation prevention bytes from each NAL's RBSP payload.
/// NALs with the `forbidden_zero_bit` set are skipped.
///
/// # Example
///
/// ```no_run
/// use feos_h264::nal::parse_annex_b;
///
/// let bitstream = std::fs::read("video.h264").unwrap();
/// let nals = parse_annex_b(&bitstream);
/// for nal in &nals {
///     println!("{:?}, ref_idc={}, {} bytes",
///              nal.nal_unit_type, nal.nal_ref_idc, nal.rbsp.len());
/// }
/// ```
///
/// The list is capped at [`MAX_ANNEX_B_NALS`] entries so that a file of
/// nothing but start codes can't exhaust the heap. Prefer [`annex_b_units`]
/// for anything larger than a sample: it needs no list at all.
pub fn parse_annex_b(data: &[u8]) -> Vec<NalUnit<'_>> {
    let mut nals = Vec::new();
    for nal in annex_b_units(data).take(MAX_ANNEX_B_NALS) {
        if nals.try_reserve(1).is_err() {
            break;
        }
        nals.push(nal);
    }
    nals
}

/// Most NAL units [`parse_annex_b`] returns from one call.
pub const MAX_ANNEX_B_NALS: usize = 1 << 16;

/// Iterates the NAL units of an Annex B bytestream without allocating a list.
///
/// Emulation prevention is removed as each unit is produced, so only the unit
/// being read is held in memory. A unit whose unescaped copy can't be allocated
/// is skipped.
pub fn annex_b_units(data: &[u8]) -> AnnexBUnits<'_> {
    AnnexBUnits {
        ranges: annex_b_ranges(data),
    }
}

/// Iterator returned by [`annex_b_units`].
#[derive(Clone)]
pub struct AnnexBUnits<'a> {
    ranges: AnnexBRanges<'a>,
}

impl<'a> Iterator for AnnexBUnits<'a> {
    type Item = NalUnit<'a>;

    fn next(&mut self) -> Option<NalUnit<'a>> {
        let data = self.ranges.data;
        for range in self.ranges.by_ref() {
            if let Some(nal) = parse_nal_bytes(&data[range]) {
                return Some(nal);
            }
        }
        None
    }
}

/// Iterates the byte ranges of the NAL units in an Annex B bytestream, header
/// byte included and trailing zeros excluded. Nothing is copied or unescaped,
/// so this suits indexing a file.
pub fn annex_b_ranges(data: &[u8]) -> AnnexBRanges<'_> {
    let next = find_start_code(data, 0).map(|(pos, _)| pos);
    AnnexBRanges { data, next }
}

/// Iterator returned by [`annex_b_ranges`].
#[derive(Clone)]
pub struct AnnexBRanges<'a> {
    data: &'a [u8],
    next: Option<usize>,
}

impl Iterator for AnnexBRanges<'_> {
    type Item = core::ops::Range<usize>;

    fn next(&mut self) -> Option<core::ops::Range<usize>> {
        loop {
            let start = self.next?;
            if start >= self.data.len() {
                self.next = None;
                return None;
            }
            let (end, next) = match find_start_code(self.data, start) {
                Some((pos, sc_start)) => {
                    // Strip trailing zeros before the start code
                    let mut end = sc_start;
                    while end > start && self.data[end - 1] == 0 {
                        end -= 1;
                    }
                    (end, Some(pos))
                }
                None => (self.data.len(), None),
            };
            self.next = next;
            if end > start {
                return Some(start..end);
            }
        }
    }
}

/// Parse a single NAL unit from raw bytes (header byte + payload).
/// Returns `None` if the slice is empty, has the forbidden_zero_bit set, or
/// its unescaped payload can't be allocated.
pub fn parse_nal_bytes(nal_data: &[u8]) -> Option<NalUnit<'_>> {
    if nal_data.is_empty() {
        return None;
    }
    let header = nal_data[0];
    // forbidden_zero_bit (MSB) must be 0
    if header & 0x80 != 0 {
        return None;
    }
    let nal_ref_idc = (header >> 5) & 0x03;
    let nal_unit_type = NalUnitType::from(header & 0x1F);
    let rbsp = remove_emulation_prevention(&nal_data[1..])?;
    Some(NalUnit {
        nal_ref_idc,
        nal_unit_type,
        rbsp,
    })
}

/// Configuration parsed from an MP4 `avcC` (AVCDecoderConfigurationRecord) box.
///
/// In MP4/MKV containers, the SPS and PPS parameter sets are stored
/// out-of-band in the `avcC` configuration box rather than inline with the
/// sample data. After parsing the box, callers must feed the SPS/PPS NALs
/// to the decoder once before decoding any samples, then use [`parse_avcc`]
/// (with [`length_size`](Self::length_size)) to parse the length-prefixed
/// NALs from each sample.
///
/// # Example
///
/// ```no_run
/// use feos_h264::decoder::Decoder;
/// use feos_h264::nal::{parse_avcc, parse_avcc_config};
///
/// // Get this from your MP4 demuxer's `avcC` box
/// let avcc_box: &[u8] = unimplemented!();
/// let cfg = parse_avcc_config(avcc_box).unwrap();
///
/// let mut decoder = Decoder::new();
/// // Feed parameter sets once at startup
/// for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
///     decoder.decode_nal(nal).unwrap();
/// }
///
/// // For each MP4 sample, decode its NALs
/// let sample: &[u8] = unimplemented!();
/// for nal in parse_avcc(sample, cfg.length_size) {
///     if let Ok(Some(frame)) = decoder.decode_nal(&nal) {
///         // handle frame
///     }
/// }
/// ```
#[derive(Debug)]
pub struct AvccConfig<'a> {
    /// Number of bytes used for length prefixes in sample data (1, 2, or 4).
    /// Pass this value to [`parse_avcc`] when parsing samples.
    pub length_size: usize,
    /// SPS NAL units extracted from the configuration record. Feed these to
    /// the decoder before any sample data.
    pub sps_nals: Vec<NalUnit<'a>>,
    /// PPS NAL units extracted from the configuration record. Feed these to
    /// the decoder before any sample data.
    pub pps_nals: Vec<NalUnit<'a>>,
}

/// Parse an MP4 `avcC` (AVCDecoderConfigurationRecord) box per ISO/IEC 14496-15.
///
/// The input is the raw box payload (not including the box header). Returns
/// the SPS/PPS NAL units and the length-field size needed by `parse_avcc`.
///
/// Layout:
/// ```text
/// configurationVersion         u8 (must be 1)
/// AVCProfileIndication         u8
/// profile_compatibility        u8
/// AVCLevelIndication           u8
/// reserved (6 bits) | lengthSizeMinusOne (2 bits)  u8
/// reserved (3 bits) | numOfSequenceParameterSets (5 bits)  u8
/// for each SPS:
///   sequenceParameterSetLength u16 (big-endian)
///   sequenceParameterSetNALUnit
/// numOfPictureParameterSets    u8
/// for each PPS:
///   pictureParameterSetLength  u16 (big-endian)
///   pictureParameterSetNALUnit
/// ```
pub fn parse_avcc_config(data: &[u8]) -> Result<AvccConfig<'_>, &'static str> {
    if data.len() < 7 {
        return Err("avcC: too short");
    }
    if data[0] != 1 {
        return Err("avcC: unsupported configurationVersion");
    }
    // data[1..4] are profile/compat/level — informational, not needed here
    let length_size = ((data[4] & 0x03) + 1) as usize;
    if length_size != 1 && length_size != 2 && length_size != 4 {
        return Err("avcC: invalid lengthSizeMinusOne");
    }
    let num_sps = (data[5] & 0x1F) as usize;

    let mut off = 6;
    let mut sps_nals = Vec::with_capacity(num_sps);
    for _ in 0..num_sps {
        if off + 2 > data.len() {
            return Err("avcC: truncated SPS length");
        }
        let len = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        if off + len > data.len() {
            return Err("avcC: truncated SPS data");
        }
        if let Some(nal) = parse_nal_bytes(&data[off..off + len]) {
            sps_nals.push(nal);
        }
        off += len;
    }

    if off >= data.len() {
        return Err("avcC: missing PPS count");
    }
    let num_pps = data[off] as usize;
    off += 1;

    let mut pps_nals = Vec::with_capacity(num_pps);
    for _ in 0..num_pps {
        if off + 2 > data.len() {
            return Err("avcC: truncated PPS length");
        }
        let len = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        if off + len > data.len() {
            return Err("avcC: truncated PPS data");
        }
        if let Some(nal) = parse_nal_bytes(&data[off..off + len]) {
            pps_nals.push(nal);
        }
        off += len;
    }

    Ok(AvccConfig {
        length_size,
        sps_nals,
        pps_nals,
    })
}

/// Parse a length-prefixed AVCC sample into NAL units.
///
/// `data` is the raw sample payload from an MP4 `mdat` chunk. `length_size`
/// is the number of bytes used for each length prefix (1, 2, or 4 — typically
/// 4, taken from `AvccConfig::length_size`).
///
/// Note: this only parses the per-sample NAL units. The SPS/PPS configuration
/// is stored separately in the MP4 `avcC` box and must be parsed with
/// `parse_avcc_config` and fed to the decoder before any sample NALs.
pub fn parse_avcc(data: &[u8], length_size: usize) -> Vec<NalUnit<'_>> {
    avcc_units(data, length_size).collect()
}

/// Iterates the NAL units of a length-prefixed AVCC sample without building a
/// list. Stops at a truncated unit or an invalid `length_size`. A unit whose
/// unescaped copy can't be allocated is skipped.
pub fn avcc_units(data: &[u8], length_size: usize) -> AvccUnits<'_> {
    AvccUnits {
        ranges: avcc_ranges(data, length_size),
    }
}

/// Iterator returned by [`avcc_units`].
#[derive(Clone)]
pub struct AvccUnits<'a> {
    ranges: AvccRanges<'a>,
}

impl<'a> Iterator for AvccUnits<'a> {
    type Item = NalUnit<'a>;

    fn next(&mut self) -> Option<NalUnit<'a>> {
        let data = self.ranges.data;
        for range in self.ranges.by_ref() {
            if let Some(nal) = parse_nal_bytes(&data[range]) {
                return Some(nal);
            }
        }
        None
    }
}

/// Iterates the byte ranges of the NAL units of an AVCC sample, header byte
/// included. Nothing is copied or unescaped, so this suits indexing a file.
pub fn avcc_ranges(data: &[u8], length_size: usize) -> AvccRanges<'_> {
    AvccRanges {
        data,
        length_size,
        pos: 0,
    }
}

/// Iterator returned by [`avcc_ranges`].
#[derive(Clone)]
pub struct AvccRanges<'a> {
    data: &'a [u8],
    length_size: usize,
    pos: usize,
}

impl Iterator for AvccRanges<'_> {
    type Item = core::ops::Range<usize>;

    fn next(&mut self) -> Option<core::ops::Range<usize>> {
        let i = self.pos;
        let data = self.data;
        let header = data.get(i..i.checked_add(self.length_size)?)?;
        let len = match *header {
            [a] => a as usize,
            [a, b] => u16::from_be_bytes([a, b]) as usize,
            [a, b, c, d] => u32::from_be_bytes([a, b, c, d]) as usize,
            _ => return None,
        };
        let start = i + self.length_size;
        let end = start.checked_add(len).filter(|&end| end <= data.len())?;
        self.pos = end;
        Some(start..end)
    }
}

/// Find the next start code starting from `offset`.
/// Returns (position after start code, position of start code beginning).
fn find_start_code(data: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut i = offset;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some((i + 3, i));
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i + 4, i));
            }
        }
        i += 1;
    }
    None
}

/// Remove emulation prevention bytes (0x03 in 00 00 03 sequences).
/// Returns a borrowed slice when no emulation prevention bytes are found
/// (the common case), avoiding allocation entirely.
fn remove_emulation_prevention(data: &[u8]) -> Option<Cow<'_, [u8]>> {
    // Fast path: scan for 00 00 03. If none found, return borrowed slice.
    let has_epb = data.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 3);
    if !has_epb {
        return Some(Cow::Borrowed(data));
    }

    // Slow path: copy with emulation prevention removal.
    let mut rbsp = Vec::new();
    rbsp.try_reserve_exact(data.len()).ok()?;
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 3 {
            rbsp.push(0);
            rbsp.push(0);
            i += 3; // skip the 0x03 byte
        } else {
            rbsp.push(data[i]);
            i += 1;
        }
    }
    Some(Cow::Owned(rbsp))
}

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;

    #[test]
    fn test_parse_annex_b_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0].nal_unit_type, NalUnitType::Sps);
        assert_eq!(nals[1].nal_unit_type, NalUnitType::Pps);
        assert_eq!(nals[2].nal_unit_type, NalUnitType::Sei);
        assert_eq!(nals[3].nal_unit_type, NalUnitType::SliceIdr);
    }

    #[test]
    fn test_emulation_prevention_removal() {
        let input = [0x00, 0x00, 0x03, 0x01, 0xAB];
        let rbsp = remove_emulation_prevention(&input).unwrap();
        assert_eq!(&*rbsp, &[0x00, 0x00, 0x01, 0xAB]);
        assert!(
            matches!(rbsp, Cow::Owned(_)),
            "should allocate when EPB present"
        );
    }

    #[test]
    fn test_emulation_prevention_zero_copy() {
        // No emulation prevention bytes → should return borrowed slice (no allocation)
        let input = [0x01, 0x02, 0x03, 0x04];
        let rbsp = remove_emulation_prevention(&input).unwrap();
        assert_eq!(&*rbsp, &input);
        assert!(
            matches!(rbsp, Cow::Borrowed(_)),
            "should borrow when no EPB"
        );
    }

    #[test]
    fn test_parse_avcc_two_nals_4byte_length() {
        // Two NAL units with 4-byte length prefixes:
        // SliceIdr (header byte 0x65 = nal_ref_idc=3, type=5) with payload [0xAA]
        // Sps (header byte 0x67 = nal_ref_idc=3, type=7) with payload [0xBB, 0xCC]
        let data = [
            0x00, 0x00, 0x00, 0x02, // length = 2
            0x65, 0xAA, // IDR
            0x00, 0x00, 0x00, 0x03, // length = 3
            0x67, 0xBB, 0xCC, // SPS
        ];
        let nals = parse_avcc(&data, 4);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].nal_unit_type, NalUnitType::SliceIdr);
        assert_eq!(nals[0].nal_ref_idc, 3);
        assert_eq!(&*nals[0].rbsp, &[0xAA]);
        assert_eq!(nals[1].nal_unit_type, NalUnitType::Sps);
        assert_eq!(&*nals[1].rbsp, &[0xBB, 0xCC]);
    }

    #[test]
    fn test_parse_avcc_truncated() {
        // Length says 10 bytes but only 2 follow → stop parsing
        let data = [0x00, 0x00, 0x00, 0x0A, 0x65, 0xAA];
        let nals = parse_avcc(&data, 4);
        assert_eq!(nals.len(), 0);
    }

    #[test]
    fn test_parse_avcc_2byte_length() {
        let data = [
            0x00, 0x02, 0x65, 0xAA, // length=2, IDR + payload
            0x00, 0x01, 0x67, // length=1, SPS header only
        ];
        let nals = parse_avcc(&data, 2);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].nal_unit_type, NalUnitType::SliceIdr);
        assert_eq!(nals[1].nal_unit_type, NalUnitType::Sps);
    }

    #[test]
    fn test_parse_avcc_config_minimal() {
        // Minimal avcC: 1 SPS, 1 PPS, length_size=4
        // Bytes: version=1, profile=66, compat=0, level=30,
        //        reserved+lengthSize: 0xFF (lengthSizeMinusOne=3 → length_size=4)
        //        reserved+numSPS: 0xE1 (numSPS=1)
        //        sps_len=4, sps=[0x67, 0x42, 0x00, 0x1E]
        //        numPPS=1
        //        pps_len=2, pps=[0x68, 0xCE]
        let data = [
            0x01, 0x42, 0x00, 0x1E, 0xFF, // lengthSizeMinusOne = 3
            0xE1, // numOfSequenceParameterSets = 1
            0x00, 0x04, // sps length
            0x67, 0x42, 0x00, 0x1E, // sps NAL (header + 3 bytes RBSP)
            0x01, // numOfPictureParameterSets = 1
            0x00, 0x02, // pps length
            0x68, 0xCE, // pps NAL
        ];
        let cfg = parse_avcc_config(&data).unwrap();
        assert_eq!(cfg.length_size, 4);
        assert_eq!(cfg.sps_nals.len(), 1);
        assert_eq!(cfg.sps_nals[0].nal_unit_type, NalUnitType::Sps);
        assert_eq!(&*cfg.sps_nals[0].rbsp, &[0x42, 0x00, 0x1E]);
        assert_eq!(cfg.pps_nals.len(), 1);
        assert_eq!(cfg.pps_nals[0].nal_unit_type, NalUnitType::Pps);
        assert_eq!(&*cfg.pps_nals[0].rbsp, &[0xCE]);
    }

    #[test]
    fn test_parse_avcc_config_invalid_version() {
        let data = [0x02, 0x42, 0x00, 0x1E, 0xFF, 0xE0, 0x00];
        assert!(parse_avcc_config(&data).is_err());
    }

    #[test]
    fn test_parse_avcc_config_truncated() {
        let data = [0x01, 0x42];
        assert!(parse_avcc_config(&data).is_err());
    }
}
