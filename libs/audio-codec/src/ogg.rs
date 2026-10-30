//! Ogg container demuxing and Opus stream headers.
//!
//! Opus packet decoding is not implemented in this crate yet; this module
//! provides format detection, `OpusHead` metadata and Ogg page iteration so
//! callers can inspect an Opus stream without allocating.

use crate::error::{need, AudioError, Result};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// Parsed `OpusHead` identification header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpusHead {
    /// Encoder version.
    pub version: u8,
    /// Number of output channels.
    pub channels: u8,
    /// Samples to discard at the start of the stream (at 48 kHz).
    pub pre_skip: u16,
    /// Original input sample rate, informational only.
    pub input_sample_rate: u32,
    /// Output gain in Q7.8 dB.
    pub output_gain: i16,
    /// Channel mapping family.
    pub mapping_family: u8,
}

/// An Ogg page header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Page<'a> {
    /// Absolute byte offset of the page in the input.
    pub offset: usize,
    /// `header_type` flags.
    pub flags: u8,
    /// Granule position.
    pub granule: u64,
    /// Stream serial number.
    pub serial: u32,
    /// Page sequence number.
    pub sequence: u32,
    /// Page payload.
    pub data: &'a [u8],
}

/// Iterates over the Ogg pages in a byte slice.
pub struct Pages<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Pages<'a> {
    /// Creates a page iterator for an Ogg stream.
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }
}

impl<'a> Iterator for Pages<'a> {
    type Item = Result<Page<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.input.len() {
            return None;
        }
        match parse_page(self.input, self.pos) {
            Ok((page, next)) => {
                self.pos = next;
                Some(Ok(page))
            }
            Err(error) => {
                self.pos = self.input.len();
                Some(Err(error))
            }
        }
    }
}

fn le_u32(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn parse_page(input: &[u8], pos: usize) -> Result<(Page<'_>, usize)> {
    need(input.len(), pos, 27)?;
    if &input[pos..pos + 4] != b"OggS" {
        return Err(AudioError::InvalidData("missing Ogg capture pattern"));
    }
    if input[pos + 4] != 0 {
        return Err(AudioError::UnsupportedFeature("Ogg stream version"));
    }
    let flags = input[pos + 5];
    let granule = u64::from_le_bytes([
        input[pos + 6],
        input[pos + 7],
        input[pos + 8],
        input[pos + 9],
        input[pos + 10],
        input[pos + 11],
        input[pos + 12],
        input[pos + 13],
    ]);
    let serial = le_u32(input, pos + 14)?;
    let sequence = le_u32(input, pos + 18)?;
    let segments = input[pos + 26] as usize;
    need(input.len(), pos + 27, segments)?;
    let table = &input[pos + 27..pos + 27 + segments];
    let body_len: usize = table.iter().map(|&value| value as usize).sum();
    let data_start = pos + 27 + segments;
    need(input.len(), data_start, body_len)?;
    let data = &input[data_start..data_start + body_len];
    let end = data_start + body_len;

    // Verify the CRC with the checksum field treated as zero.
    let mut header = [0u8; 27];
    header.copy_from_slice(&input[pos..pos + 27]);
    header[22..26].fill(0);
    let mut crc = ogg_crc(&header);
    crc = ogg_crc_update(crc, table);
    crc = ogg_crc_update(crc, data);
    let expected = le_u32(input, pos + 22)?;
    if crc != expected {
        return Err(AudioError::InvalidData("Ogg page checksum mismatch"));
    }

    Ok((
        Page {
            offset: pos,
            flags,
            granule,
            serial,
            sequence,
            data,
        },
        end,
    ))
}

fn ogg_crc_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Parses the first Ogg page as an `OpusHead` identification header.
pub fn opus_head(input: &[u8]) -> Result<OpusHead> {
    let mut pages = Pages::new(input);
    let page = pages.next().ok_or(AudioError::UnexpectedEnd)??;
    if page.data.len() < 19 || &page.data[..8] != b"OpusHead" {
        return Err(AudioError::InvalidData("not an Ogg Opus stream"));
    }
    let data = page.data;
    let head = OpusHead {
        version: data[8],
        channels: data[9],
        pre_skip: u16::from_le_bytes([data[10], data[11]]),
        input_sample_rate: le_u32(data, 12)?,
        output_gain: i16::from_le_bytes([data[16], data[17]]),
        mapping_family: data[18],
    };
    if head.version > 15 {
        return Err(AudioError::UnsupportedFeature("OpusHead version"));
    }
    if head.channels == 0 {
        return Err(AudioError::InvalidData("Opus stream has no channels"));
    }
    if head.mapping_family != 0 {
        // stream count, coupled count and the channel mapping table.
        let needed = 19 + 2 + head.channels as usize;
        if data.len() < needed {
            return Err(AudioError::UnexpectedEnd);
        }
    }
    Ok(head)
}

/// Returns the properties of an Ogg Opus stream.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let head = opus_head(input)?;
    let mut last_granule = 0u64;
    for page in Pages::new(input) {
        let page = page?;
        if page.granule != u64::MAX {
            last_granule = page.granule;
        }
    }
    let frames = last_granule.saturating_sub(head.pre_skip as u64);
    Ok(AudioInfo {
        sample_rate: 48_000,
        channels: head.channels as u16,
        frames,
        format: AudioFormat::OggOpus,
    })
}

/// Opus decoding needs no scratch space because it is not implemented yet.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Opus packet decoding is not implemented.
///
/// The function always returns [`AudioError::UnsupportedFeature`] after
/// validating the stream header. Use [`opus_head`] and [`Pages`] to inspect
/// the container.
pub fn decode(input: &[u8], _out: &mut [i16]) -> Result<DecodedAudio> {
    let _ = opus_head(input)?;
    Err(AudioError::UnsupportedFeature(
        "Opus packet decoding is not implemented",
    ))
}
