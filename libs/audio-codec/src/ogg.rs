//! Ogg container demuxing and codec dispatch.
//!
//! Vorbis and FLAC streams are decoded by [`crate::vorbis`] and [`crate::flac`].
//! Opus packet decoding is not implemented: this module provides `OpusHead`
//! metadata and Ogg page iteration so callers can inspect an Opus stream, and
//! [`decode`] reports [`AudioError::UnsupportedFeature`] for it.
//!
//! A multiplexed file is handled by following the first logical stream whose
//! codec is known. A chained file plays its first such stream only.

use alloc::vec::Vec;

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
    /// Segment table: how `data` splits into packet fragments.
    pub lacing: &'a [u8],
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

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

fn ogg_crc_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ byte) as usize];
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
    let mut crc = ogg_crc_update(0, &input[pos..pos + 22]);
    crc = ogg_crc_update(crc, &[0; 4]);
    crc = ogg_crc_update(crc, &input[pos + 26..pos + 27]);
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
            lacing: table,
        },
        end,
    ))
}

const FLAG_CONTINUED: u8 = 0x01;
const FLAG_BOS: u8 = 0x02;
const FLAG_EOS: u8 = 0x04;

/// The largest packet [`PacketReader`] reassembles. Real packets are far
/// smaller; a Vorbis setup header is the largest, at tens of kilobytes.
const MAX_PACKET: usize = 8 << 20;

/// The codec of a logical stream, from the first packet on its first page.
fn codec_of(data: &[u8]) -> Option<AudioFormat> {
    if data.starts_with(b"OpusHead") {
        Some(AudioFormat::OggOpus)
    } else if data.len() >= 7 && data[0] == 1 && &data[1..7] == b"vorbis" {
        Some(AudioFormat::OggVorbis)
    } else if data.len() >= 5 && data[0] == 0x7f && &data[1..5] == b"FLAC" {
        Some(AudioFormat::OggFlac)
    } else {
        None
    }
}

/// Finds the first logical stream with a known codec among the initial
/// beginning-of-stream pages. Returns its codec and first page.
pub(crate) fn find_stream(input: &[u8]) -> Result<Option<(AudioFormat, Page<'_>)>> {
    for page in Pages::new(input) {
        let page = page?;
        if page.flags & FLAG_BOS == 0 {
            break;
        }
        if let Some(format) = codec_of(page.data) {
            return Ok(Some((format, page)));
        }
    }
    Ok(None)
}

/// The largest usable granule position of one logical stream. Stops quietly
/// at a truncated or damaged page: everything before it still counts.
pub(crate) fn last_granule(input: &[u8], serial: u32) -> u64 {
    let mut last = 0;
    for page in Pages::new(input) {
        let Ok(page) = page else { break };
        if page.serial == serial && page.granule != u64::MAX {
            last = page.granule;
        }
    }
    last
}

/// The granule position on the final page of a stream that has one. Only that
/// position shortens the last packet's output.
pub(crate) fn final_granule(input: &[u8], serial: u32) -> Option<u64> {
    Pages::new(input)
        .map_while(|page| page.ok())
        .filter(|page| page.serial == serial && page.flags & FLAG_EOS != 0)
        .map(|page| page.granule)
        .find(|&granule| granule != u64::MAX)
}

/// Reassembles the packets of one logical stream.
pub(crate) struct PacketReader<'a> {
    input: &'a [u8],
    pos: usize,
    serial: u32,
    /// The current page's payload and segment table, with cursors.
    data: &'a [u8],
    lacing: &'a [u8],
    data_pos: usize,
    lacing_pos: usize,
    /// The packet being assembled.
    packet: Vec<u8>,
    /// A fragment of a packet is held in `packet`.
    partial: bool,
    /// The current page is the stream's last.
    last_page: bool,
}

impl<'a> PacketReader<'a> {
    pub(crate) fn new(input: &'a [u8], serial: u32) -> Self {
        Self {
            input,
            pos: 0,
            serial,
            data: &[],
            lacing: &[],
            data_pos: 0,
            lacing_pos: 0,
            packet: Vec::new(),
            partial: false,
            last_page: false,
        }
    }

    /// Moves to the next page of the stream. `Ok(false)` at the end of input.
    fn next_page(&mut self) -> Result<bool> {
        if self.last_page {
            return Ok(false);
        }
        loop {
            // Skip anything between pages, such as junk appended to a file.
            let Some(skip) = self
                .input
                .get(self.pos..)
                .and_then(|rest| rest.windows(4).position(|w| w == b"OggS"))
            else {
                return Ok(false);
            };
            self.pos += skip;
            let (page, next) = match parse_page(self.input, self.pos) {
                Ok(parsed) => parsed,
                // A cut-off final page is the end of a truncated file.
                Err(AudioError::UnexpectedEnd) => return Ok(false),
                Err(error) => return Err(error),
            };
            self.pos = next;
            if page.serial != self.serial {
                continue;
            }
            if page.flags & FLAG_CONTINUED == 0 {
                // A packet left open has lost its tail.
                self.partial = false;
            }
            self.data = page.data;
            self.lacing = page.lacing;
            self.data_pos = 0;
            self.lacing_pos = 0;
            self.last_page = page.flags & FLAG_EOS != 0;
            if page.flags & FLAG_CONTINUED != 0 && !self.partial {
                // The head of this packet is gone, so skip what is left of it.
                while let Some(&lace) = self.lacing.get(self.lacing_pos) {
                    self.lacing_pos += 1;
                    self.data_pos += lace as usize;
                    if lace != 255 {
                        break;
                    }
                }
            }
            return Ok(true);
        }
    }

    /// The next complete packet, or `None` at the end of the stream.
    pub(crate) fn next_packet(&mut self) -> Result<Option<&[u8]>> {
        loop {
            while let Some(&lace) = self.lacing.get(self.lacing_pos) {
                self.lacing_pos += 1;
                if !self.partial {
                    self.packet.clear();
                    self.partial = true;
                }
                let start = self.data_pos;
                let end = start + lace as usize;
                self.data_pos = end;
                let fragment = self.data.get(start..end).ok_or(AudioError::UnexpectedEnd)?;
                let total = self.packet.len() + fragment.len();
                if total > MAX_PACKET {
                    return Err(AudioError::InvalidData("Ogg packet is too large"));
                }
                if total > self.packet.capacity() {
                    let wanted = total.max(self.packet.capacity() * 2).max(4096).min(MAX_PACKET);
                    self.packet
                        .try_reserve_exact(wanted - self.packet.len())
                        .map_err(|_| AudioError::OutOfMemory)?;
                }
                self.packet.extend_from_slice(fragment);
                if lace != 255 {
                    self.partial = false;
                    return Ok(Some(&self.packet));
                }
            }
            if !self.next_page()? {
                return Ok(None);
            }
        }
    }
}

/// Parses the `OpusHead` identification header of an Ogg Opus stream.
pub fn opus_head(input: &[u8]) -> Result<OpusHead> {
    let page = match find_stream(input)? {
        Some((AudioFormat::OggOpus, page)) => page,
        _ => return Err(AudioError::InvalidData("not an Ogg Opus stream")),
    };
    if page.data.len() < 19 {
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

fn opus_info(input: &[u8], serial: u32) -> Result<AudioInfo> {
    let head = opus_head(input)?;
    let frames = last_granule(input, serial).saturating_sub(head.pre_skip as u64);
    Ok(AudioInfo {
        sample_rate: 48_000,
        channels: head.channels as u16,
        frames,
        format: AudioFormat::OggOpus,
    })
}

/// Returns the properties of an Ogg stream carrying Opus, Vorbis or FLAC.
///
/// Reading [`AudioInfo::format`] tells the codec apart. For a truncated file
/// the frame count covers the complete pages present.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let Some((format, page)) = find_stream(input)? else {
        return Err(AudioError::UnsupportedFormat);
    };
    match format {
        AudioFormat::OggVorbis => crate::vorbis::ogg_info(input, &page),
        AudioFormat::OggFlac => crate::flac::ogg_info(input, &page),
        _ => opus_info(input, page.serial),
    }
}

/// Ogg decoding takes no scratch space from the caller.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes an Ogg Vorbis or Ogg FLAC stream.
///
/// Opus packet decoding is not implemented, so an Opus stream returns
/// [`AudioError::UnsupportedFeature`] after validating its header. Use
/// [`opus_head`] and [`Pages`] to inspect one.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let Some((format, page)) = find_stream(input)? else {
        return Err(AudioError::UnsupportedFormat);
    };
    match format {
        AudioFormat::OggVorbis => crate::vorbis::decode(input, out),
        AudioFormat::OggFlac => crate::flac::ogg_decode(input, &page, out),
        _ => {
            let _ = opus_head(input)?;
            Err(AudioError::UnsupportedFeature(
                "Opus packet decoding is not implemented",
            ))
        }
    }
}
