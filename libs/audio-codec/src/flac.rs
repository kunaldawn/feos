//! FLAC decoding (RFC 9639), from native `.flac` files and from Ogg.
//!
//! The decoder is pure integer. Samples are reduced to signed 16 bits by
//! dropping low bits (or widened by shifting left when shallower), the same
//! way [`crate::wav`] treats deep PCM. The stream's MD5 is not checked; each
//! frame's CRC-8 and CRC-16 are.
//!
//! Working memory is one block per channel, at most 65535 samples of 8 bytes
//! for 8 channels (4 MiB), allocated fallibly when a frame first needs it.
//! CPU is bounded by the output buffer as well as the input: a frame is only
//! decoded once it is known to fit, and costs at most 32 multiplies a sample.
//!
//! [`decode`] stops at the first damaged frame and reports it. A stream cut
//! off in the middle of a frame is not an error if whole frames came before:
//! the result simply has fewer frames than [`info`] promised.

use alloc::vec::Vec;

use feos_source::{Cache, Source};

use crate::bits::{try_vec, MsbReader};
use crate::error::{AudioError, Result};
use crate::ogg::{self, PacketReader, Page};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// The contents of a `STREAMINFO` metadata block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamInfo {
    /// Smallest block size in the stream, in samples.
    pub min_block_size: u16,
    /// Largest block size in the stream, in samples.
    pub max_block_size: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels, 1 to 8.
    pub channels: u16,
    /// Bits per sample, 4 to 32.
    pub bits_per_sample: u8,
    /// Total samples per channel, or 0 when the encoder did not know.
    pub total_frames: u64,
}

const MAX_BLOCK: usize = 65535;

const fn crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
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

const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = (index as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
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

static CRC8: [u8; 256] = crc8_table();
static CRC16: [u16; 256] = crc16_table();

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0, |crc, &byte| CRC8[(crc ^ byte) as usize])
}

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0, |crc, &byte| {
        (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ byte) as usize]
    })
}

fn parse_stream_info(data: &[u8]) -> Result<StreamInfo> {
    let bytes = data.get(..34).ok_or(AudioError::UnexpectedEnd)?;
    let packed = u64::from_be_bytes([
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17],
    ]);
    let info = StreamInfo {
        min_block_size: u16::from_be_bytes([bytes[0], bytes[1]]),
        max_block_size: u16::from_be_bytes([bytes[2], bytes[3]]),
        sample_rate: (packed >> 44) as u32,
        channels: ((packed >> 41) & 7) as u16 + 1,
        bits_per_sample: ((packed >> 36) & 31) as u8 + 1,
        total_frames: packed & 0xf_ffff_ffff,
    };
    if info.sample_rate == 0 {
        return Err(AudioError::InvalidData("FLAC sample rate is zero"));
    }
    if info.bits_per_sample < 4 {
        return Err(AudioError::InvalidData("bad FLAC sample depth"));
    }
    Ok(info)
}

/// The length of a leading ID3v2 tag, which some taggers put before audio.
pub(crate) fn id3_len(input: &[u8]) -> usize {
    match input.get(..10) {
        Some(tag) if tag.starts_with(b"ID3") && tag[6..10].iter().all(|b| b & 0x80 == 0) => {
            let size = tag[6..10]
                .iter()
                .fold(0usize, |size, &b| (size << 7) | b as usize);
            let footer = if tag[5] & 0x10 != 0 { 10 } else { 0 };
            10 + size + footer
        }
        _ => 0,
    }
}

/// Finds `STREAMINFO` and the first frame of a native stream.
fn parse_native(input: &[u8]) -> Result<(StreamInfo, usize)> {
    let start = id3_len(input);
    if input.get(start..start.saturating_add(4)) != Some(b"fLaC".as_slice()) {
        return Err(AudioError::UnknownFormat);
    }
    let mut pos = start + 4;
    let mut stream = None;
    loop {
        let header = input.get(pos..pos + 4).ok_or(AudioError::UnexpectedEnd)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        pos += 4;
        if stream.is_none() {
            if kind != 0 {
                return Err(AudioError::InvalidData("FLAC stream has no STREAMINFO"));
            }
            stream = Some(parse_stream_info(input.get(pos..).unwrap_or(&[]))?);
        }
        pos = pos.checked_add(len).ok_or(AudioError::Overflow)?;
        if last {
            break;
        }
    }
    match stream {
        Some(stream) => Ok((stream, pos)),
        None => Err(AudioError::InvalidData("FLAC stream has no STREAMINFO")),
    }
}

/// A parsed frame header.
struct FrameHeader {
    block: usize,
    channel_code: u8,
    bits: u32,
    /// Bytes up to and including the header's CRC-8.
    len: usize,
    /// The frame's number, or for a stream with blocks of varying sizes, its first
    /// sample's.
    number: u64,
    variable: bool,
}

/// Parses the frame header at the start of `data`. `Ok(None)` means the bytes
/// are not a frame header at all; running out of input is `UnexpectedEnd`.
fn parse_frame_header(data: &[u8], stream: &StreamInfo) -> Result<Option<FrameHeader>> {
    let head = data.get(..4).ok_or(AudioError::UnexpectedEnd)?;
    if head[0] != 0xff || head[1] & 0xfe != 0xf8 || head[3] & 1 != 0 {
        return Ok(None);
    }
    let block_code = head[2] >> 4;
    let rate_code = head[2] & 15;
    let channel_code = head[3] >> 4;
    let bits_code = (head[3] >> 1) & 7;
    if block_code == 0 || rate_code == 15 || channel_code > 10 || bits_code == 3 {
        return Ok(None);
    }

    // The frame or sample number, in the UTF-8 style coding, up to 7 bytes.
    let lead = *data.get(4).ok_or(AudioError::UnexpectedEnd)?;
    let extra = match lead {
        0x00..=0x7f => 0,
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        0xf8..=0xfb => 4,
        0xfc..=0xfd => 5,
        0xfe => 6,
        _ => return Ok(None),
    };
    let mut pos = 5;
    let mut number = match lead {
        0x00..=0x7f => lead as u64,
        0xfe => 0,
        _ => (lead & (0x3f >> extra)) as u64,
    };
    for _ in 0..extra {
        let byte = *data.get(pos).ok_or(AudioError::UnexpectedEnd)?;
        if byte & 0xc0 != 0x80 {
            return Ok(None);
        }
        number = (number << 6) | (byte & 0x3f) as u64;
        pos += 1;
    }

    let mut field = |width: usize| -> Result<u32> {
        let bytes = data.get(pos..pos + width).ok_or(AudioError::UnexpectedEnd)?;
        pos += width;
        Ok(bytes.iter().fold(0, |value, &b| (value << 8) | b as u32))
    };
    let block = match block_code {
        1 => 192,
        2..=5 => 144usize << block_code,
        6 => field(1)? as usize + 1,
        7 => field(2)? as usize + 1,
        _ => 256usize << (block_code - 8),
    };
    if block > MAX_BLOCK {
        return Ok(None);
    }
    let rate = match rate_code {
        0 => stream.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => field(1)? * 1000,
        13 => field(2)?,
        _ => field(2)? * 10,
    };
    let bits = match bits_code {
        0 => stream.bits_per_sample as u32,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        _ => 32,
    };
    let crc = *data.get(pos).ok_or(AudioError::UnexpectedEnd)?;
    if crc8(&data[..pos]) != crc {
        return Ok(None);
    }
    let channels = if channel_code < 8 {
        channel_code as u16 + 1
    } else {
        2
    };
    // The output is one interleaved stream, so these cannot change.
    if rate != stream.sample_rate || channels != stream.channels {
        return Err(AudioError::UnsupportedFeature(
            "FLAC stream changes format mid-stream",
        ));
    }
    Ok(Some(FrameHeader {
        number,
        variable: head[1] & 1 != 0,
        block,
        channel_code,
        bits,
        len: pos + 1,
    }))
}

/// Reads the residual of a subframe into `out[order..]`.
fn read_residual(
    reader: &mut MsbReader<'_>,
    out: &mut [i64],
    order: usize,
    parse_only: bool,
) -> Result<()> {
    let block = out.len();
    let (param_bits, escape) = match reader.read(2)? {
        0 => (4, 15),
        1 => (5, 31),
        _ => return Err(AudioError::InvalidData("reserved FLAC residual coding")),
    };
    let partition_order = reader.read(4)?;
    let partitions = 1usize << partition_order;
    if !block.is_multiple_of(partitions) {
        return Err(AudioError::InvalidData("bad FLAC partition order"));
    }
    let partition_len = block >> partition_order;
    if partition_len < order {
        return Err(AudioError::InvalidData("bad FLAC partition order"));
    }

    let mut pos = order;
    for partition in 0..partitions {
        let count = if partition == 0 {
            partition_len - order
        } else {
            partition_len
        };
        let param = reader.read(param_bits)?;
        let samples = out
            .get_mut(pos..pos + count)
            .ok_or(AudioError::InvalidData("bad FLAC partition order"))?;
        pos += count;
        if param == escape {
            let width = reader.read(5)?;
            if width == 0 && parse_only {
                continue;
            }
            for sample in samples {
                *sample = reader.read_signed(width)?;
            }
        } else {
            for sample in samples {
                let quotient = reader.unary()?;
                // Encoders keep residuals within 32 bits. This limit leaves
                // the sum in `predict` far from overflowing an `i64`.
                if quotient >= (1 << 36) >> param {
                    return Err(AudioError::InvalidData("FLAC residual is too large"));
                }
                let low = reader.read(param)? as u64;
                let value = (quotient << param) | low;
                *sample = ((value >> 1) as i64) ^ -((value & 1) as i64);
            }
        }
    }
    Ok(())
}

/// Checks that a decoded sample fits the subframe's depth, which also keeps
/// every later prediction well inside `i64`: 33 + 15 + 5 bits at the most.
#[inline]
fn check_range(value: i64, bits: u32) -> Result<i64> {
    let limit = 1i64 << (bits - 1);
    if value < -limit || value >= limit {
        return Err(AudioError::InvalidData("FLAC sample out of range"));
    }
    Ok(value)
}

const FIXED: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];

/// Decodes one subframe into `out`, whose length is the block size. With
/// `parse_only` this just finds the subframe's end, in time linear in its size
/// in the input, and `out` is left meaningless.
fn read_subframe(
    reader: &mut MsbReader<'_>,
    out: &mut [i64],
    bits: u32,
    parse_only: bool,
) -> Result<()> {
    if reader.bit()? {
        return Err(AudioError::InvalidData("reserved FLAC subframe bit"));
    }
    let kind = reader.read(6)?;
    let wasted = if reader.bit()? {
        let zeros = reader.unary()?;
        if zeros >= bits as u64 {
            return Err(AudioError::InvalidData("too many FLAC wasted bits"));
        }
        zeros as u32 + 1
    } else {
        0
    };
    if wasted >= bits {
        return Err(AudioError::InvalidData("too many FLAC wasted bits"));
    }
    let bits = bits - wasted;
    let block = out.len();

    match kind {
        0 => {
            let value = reader.read_signed(bits)?;
            if !parse_only {
                out.fill(value);
            }
        }
        1 => {
            for sample in out.iter_mut() {
                *sample = reader.read_signed(bits)?;
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            if order > block {
                return Err(AudioError::InvalidData("FLAC predictor order exceeds block"));
            }
            for sample in &mut out[..order] {
                *sample = reader.read_signed(bits)?;
            }
            read_residual(reader, out, order, parse_only)?;
            if !parse_only {
                predict(out, FIXED[order], 0, bits)?;
            }
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            if order > block {
                return Err(AudioError::InvalidData("FLAC predictor order exceeds block"));
            }
            for sample in &mut out[..order] {
                *sample = reader.read_signed(bits)?;
            }
            let precision = reader.read(4)?;
            if precision == 15 {
                return Err(AudioError::InvalidData("bad FLAC predictor precision"));
            }
            let shift = reader.read_signed(5)?;
            if shift < 0 {
                return Err(AudioError::InvalidData("negative FLAC predictor shift"));
            }
            let mut coefficients = [0i64; 32];
            for coefficient in &mut coefficients[..order] {
                *coefficient = reader.read_signed(precision + 1)?;
            }
            read_residual(reader, out, order, parse_only)?;
            if !parse_only {
                predict(out, &coefficients[..order], shift as u32, bits)?;
            }
        }
        _ => return Err(AudioError::InvalidData("reserved FLAC subframe type")),
    }

    if wasted != 0 && !parse_only {
        for sample in out.iter_mut() {
            *sample <<= wasted;
        }
    }
    Ok(())
}

/// Turns residuals into samples: `coefficients[0]` weighs the newest sample.
fn predict(out: &mut [i64], coefficients: &[i64], shift: u32, bits: u32) -> Result<()> {
    let order = coefficients.len();
    for sample in out.iter().take(order) {
        check_range(*sample, bits)?;
    }
    for index in order..out.len() {
        let history = &out[index - order..index];
        let mut sum = 0i64;
        for (coefficient, past) in coefficients.iter().zip(history.iter().rev()) {
            sum += coefficient * past;
        }
        out[index] = check_range(out[index] + (sum >> shift), bits)?;
    }
    Ok(())
}

/// Reduces a sample of the given depth to 16 bits.
#[inline]
fn to_i16(value: i64, bits: u32) -> i16 {
    let scaled = if bits > 16 {
        value >> (bits - 16)
    } else {
        value << (16 - bits)
    };
    scaled.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

/// Frame-level state shared by native and Ogg streams.
struct FrameDecoder {
    stream: StreamInfo,
    /// One block per channel, `channels * block` long, grown as needed.
    samples: Vec<i64>,
}

impl FrameDecoder {
    fn new(stream: StreamInfo) -> Self {
        Self {
            stream,
            samples: Vec::new(),
        }
    }

    /// Decodes the frame at the start of `data` to `out` (`None` discards the
    /// samples, and skips all the arithmetic). Returns the frame's length in
    /// bytes.
    fn decode(
        &mut self,
        data: &[u8],
        header: &FrameHeader,
        out: Option<&mut [i16]>,
    ) -> Result<usize> {
        let channels = self.stream.channels as usize;
        let block = header.block;
        let total = block * channels;
        if self.samples.len() < total {
            self.samples = Vec::new();
            self.samples = try_vec(total, 0i64)?;
        }
        let mut reader = MsbReader::new(data.get(header.len..).ok_or(AudioError::UnexpectedEnd)?);
        for channel in 0..channels {
            // A side channel needs one extra bit.
            let extra = match (header.channel_code, channel) {
                (8, 1) | (9, 0) | (10, 1) => 1,
                _ => 0,
            };
            let start = channel * block;
            read_subframe(
                &mut reader,
                &mut self.samples[start..start + block],
                header.bits + extra,
                out.is_none(),
            )?;
        }
        reader.align();
        let body = reader.byte_pos();
        let crc = reader.read(16)? as u16;
        let end = header.len + body;
        if crc16(&data[..end]) != crc {
            return Err(AudioError::InvalidData("FLAC frame checksum mismatch"));
        }

        let Some(out) = out else {
            return Ok(end + 2);
        };
        if header.channel_code >= 8 {
            let (first, second) = self.samples[..total].split_at_mut(block);
            for (a, b) in first.iter_mut().zip(second.iter_mut()) {
                match header.channel_code {
                    // left/side
                    8 => *b = *a - *b,
                    // side/right
                    9 => *a += *b,
                    // mid/side
                    _ => {
                        let mid = (*a << 1) | (*b & 1);
                        let side = *b;
                        *a = (mid + side) >> 1;
                        *b = (mid - side) >> 1;
                    }
                }
            }
        }
        for (channel, plane) in self.samples[..total].chunks_exact(block).enumerate() {
            for (index, &sample) in plane.iter().enumerate() {
                out[index * channels + channel] = to_i16(sample, header.bits);
            }
        }
        Ok(end + 2)
    }
}

/// A stateful decoder for a native `.flac` stream, one frame at a time.
pub struct FlacDecoder<'a> {
    input: &'a [u8],
    pos: usize,
    frames: FrameDecoder,
}

impl<'a> FlacDecoder<'a> {
    /// Parses the metadata of a native FLAC stream.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let (stream, pos) = parse_native(input)?;
        Ok(Self {
            input,
            pos,
            frames: FrameDecoder::new(stream),
        })
    }

    /// The stream's `STREAMINFO`.
    pub fn stream_info(&self) -> StreamInfo {
        self.frames.stream
    }

    /// Finds the next frame header, skipping anything that is not one.
    fn next_header(&mut self) -> Result<Option<FrameHeader>> {
        while let Some(rest) = self.input.get(self.pos..) {
            if rest.len() < 2 {
                break;
            }
            match parse_frame_header(rest, &self.frames.stream) {
                Ok(Some(header)) => return Ok(Some(header)),
                Ok(None) => self.pos += 1,
                Err(AudioError::UnexpectedEnd) => break,
                Err(error) => {
                    self.pos += 1;
                    return Err(error);
                }
            }
        }
        self.pos = self.input.len();
        Ok(None)
    }

    /// Decodes the next frame, appending interleaved samples to `out`.
    ///
    /// Returns the sample frames written, or `None` at the end of the stream,
    /// which includes a final frame cut off by truncation. After an error the
    /// decoder has moved past the damaged frame's header, so it can continue.
    ///
    /// `out` must hold the frame: at most 65535 sample frames, and
    /// [`StreamInfo::max_block_size`] in any stream that keeps its promise.
    pub fn next_frame(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        let Some(header) = self.next_header()? else {
            return Ok(None);
        };
        let required = header.block * self.frames.stream.channels as usize;
        if out.len() < required {
            return Err(AudioError::BufferTooSmall {
                required,
                available: out.len(),
            });
        }
        let rest = &self.input[self.pos..];
        match self.frames.decode(rest, &header, Some(&mut out[..required])) {
            Ok(len) => {
                self.pos += len;
                Ok(Some(header.block))
            }
            Err(AudioError::UnexpectedEnd) => {
                self.pos = self.input.len();
                Ok(None)
            }
            Err(error) => {
                self.pos += header.len;
                Err(error)
            }
        }
    }

    /// Decodes the next frame without keeping its samples.
    fn skip_frame(&mut self) -> Result<Option<usize>> {
        let Some(header) = self.next_header()? else {
            return Ok(None);
        };
        let rest = &self.input[self.pos..];
        match self.frames.decode(rest, &header, None) {
            Ok(len) => {
                self.pos += len;
                Ok(Some(header.block))
            }
            Err(AudioError::UnexpectedEnd) => {
                self.pos = self.input.len();
                Ok(None)
            }
            Err(error) => {
                self.pos += header.len;
                Err(error)
            }
        }
    }
}

fn audio_info(stream: &StreamInfo, frames: u64, format: AudioFormat) -> AudioInfo {
    AudioInfo {
        sample_rate: stream.sample_rate,
        channels: stream.channels,
        frames,
        format,
    }
}

/// Returns the properties of a native FLAC stream.
///
/// When `STREAMINFO` does not state the length this walks every frame, which
/// costs as much as decoding it. Fails on a damaged frame in that case only.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let mut decoder = FlacDecoder::new(input)?;
    let stream = decoder.stream_info();
    let mut frames = stream.total_frames;
    if frames == 0 {
        while let Some(block) = decoder.skip_frame()? {
            frames += block as u64;
        }
    }
    Ok(audio_info(&stream, frames, AudioFormat::Flac))
}

/// FLAC decoding takes no scratch space from the caller.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes a native FLAC stream to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let mut decoder = FlacDecoder::new(input)?;
    let stream = decoder.stream_info();
    let channels = stream.channels as usize;
    let mut written = 0usize;
    loop {
        let rest = out.get_mut(written..).unwrap_or(&mut []);
        match decoder.next_frame(rest) {
            Ok(Some(block)) => written += block * channels,
            Ok(None) => break,
            Err(AudioError::BufferTooSmall { required, .. }) => {
                return Err(AudioError::BufferTooSmall {
                    required: written + required,
                    available: out.len(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let frames = written / channels;
    Ok(DecodedAudio {
        info: audio_info(&stream, frames as u64, AudioFormat::Flac),
        frames,
    })
}

/// Parses the mapping header on the first Ogg FLAC packet.
fn parse_ogg_header(packet: &[u8]) -> Result<StreamInfo> {
    // 0x7f "FLAC" major minor count(2) "fLaC" type(1) length(3) STREAMINFO
    if packet.len() < 13 + 4 + 34 || packet[0] != 0x7f || &packet[1..5] != b"FLAC" {
        return Err(AudioError::InvalidData("bad Ogg FLAC header"));
    }
    if packet[5] != 1 {
        return Err(AudioError::UnsupportedFeature("Ogg FLAC mapping version"));
    }
    if &packet[9..13] != b"fLaC" || packet[13] & 0x7f != 0 {
        return Err(AudioError::InvalidData("bad Ogg FLAC header"));
    }
    parse_stream_info(&packet[17..])
}

/// Returns the properties of an Ogg FLAC stream, given its first page.
pub(crate) fn ogg_info(input: &[u8], page: &Page<'_>) -> Result<AudioInfo> {
    let stream = parse_ogg_header(page.data)?;
    let frames = ogg::last_granule(input, page.serial);
    Ok(audio_info(&stream, frames, AudioFormat::OggFlac))
}

/// Decodes FLAC frames that arrive as packets, as in Matroska.
pub struct FlacPacketDecoder {
    frames: FrameDecoder,
}

impl FlacPacketDecoder {
    /// Takes the stream's metadata: `fLaC` and its blocks, or a bare STREAMINFO.
    pub fn new(private: &[u8]) -> Result<Self> {
        let info = match private.strip_prefix(b"fLaC") {
            Some(rest) => rest.get(4..).ok_or(AudioError::UnexpectedEnd)?,
            None => private,
        };
        Ok(Self {
            frames: FrameDecoder::new(parse_stream_info(info)?),
        })
    }

    /// The stream's `STREAMINFO`.
    pub fn stream_info(&self) -> StreamInfo {
        self.frames.stream
    }

    /// Decodes one frame to `out`, and returns the sample frames written.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        let stream = self.frames.stream;
        let header = match parse_frame_header(packet, &stream) {
            Ok(Some(header)) => header,
            Ok(None) | Err(AudioError::UnexpectedEnd) => {
                return Err(AudioError::InvalidData("bad FLAC frame header"));
            }
            Err(error) => return Err(error),
        };
        let required = header.block * stream.channels as usize;
        if out.len() < required {
            return Err(AudioError::BufferTooSmall {
                required,
                available: out.len(),
            });
        }
        self.frames.decode(packet, &header, Some(&mut out[..required]))?;
        Ok(header.block)
    }
}

/// A stateful decoder for FLAC in Ogg, one frame at a time.
pub struct OggFlacDecoder<'a> {
    reader: PacketReader<'a>,
    frames: FrameDecoder,
}

impl<'a> OggFlacDecoder<'a> {
    /// Finds the Ogg FLAC stream in `input`.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let page = match crate::ogg::find_stream(input)? {
            Some((AudioFormat::OggFlac, page)) => page,
            _ => return Err(AudioError::InvalidData("not an Ogg FLAC stream")),
        };
        let stream = parse_ogg_header(page.data)?;
        let mut reader = PacketReader::new(input, page.serial);
        // The mapping header, already parsed.
        let _ = reader.next_packet()?;
        Ok(Self {
            reader,
            frames: FrameDecoder::new(stream),
        })
    }

    /// The stream's `STREAMINFO`.
    pub fn stream_info(&self) -> StreamInfo {
        self.frames.stream
    }

    /// Decodes the next frame to `out`. Returns the sample frames written, or
    /// `None` at the end of the stream.
    pub fn next_frame(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        let stream = self.frames.stream;
        let Self { reader, frames } = self;
        while let Some(packet) = reader.next_packet()? {
            // Each further packet is a metadata block or exactly one frame.
            if packet.first().is_none_or(|&byte| byte != 0xff) {
                continue;
            }
            let header = match parse_frame_header(packet, &stream) {
                Ok(Some(header)) => header,
                Ok(None) | Err(AudioError::UnexpectedEnd) => {
                    return Err(AudioError::InvalidData("bad FLAC frame header"));
                }
                Err(error) => return Err(error),
            };
            let required = header.block * stream.channels as usize;
            if out.len() < required {
                return Err(AudioError::BufferTooSmall {
                    required,
                    available: out.len(),
                });
            }
            frames.decode(packet, &header, Some(&mut out[..required]))?;
            return Ok(Some(header.block));
        }
        Ok(None)
    }
}

/// Decodes an Ogg FLAC stream, given its first page.
pub(crate) fn ogg_decode(input: &[u8], page: &Page<'_>, out: &mut [i16]) -> Result<DecodedAudio> {
    let stream = parse_ogg_header(page.data)?;
    let channels = stream.channels as usize;
    let mut frames = FrameDecoder::new(stream);
    let mut reader = PacketReader::new(input, page.serial);
    let mut written = 0usize;
    let mut first = true;
    while let Some(packet) = reader.next_packet()? {
        if first {
            // The mapping header, already parsed.
            first = false;
            continue;
        }
        // Each further packet is a metadata block or exactly one frame.
        if packet.first().is_none_or(|&byte| byte != 0xff) {
            continue;
        }
        let header = match parse_frame_header(packet, &stream) {
            Ok(Some(header)) => header,
            Ok(None) | Err(AudioError::UnexpectedEnd) => {
                return Err(AudioError::InvalidData("bad FLAC frame header"));
            }
            Err(error) => return Err(error),
        };
        let required = header.block * channels;
        let end = written.checked_add(required).ok_or(AudioError::Overflow)?;
        if end > out.len() {
            return Err(AudioError::BufferTooSmall {
                required: end,
                available: out.len(),
            });
        }
        frames.decode(packet, &header, Some(&mut out[written..end]))?;
        written = end;
    }
    let count = written / channels;
    Ok(DecodedAudio {
        info: audio_info(&stream, count as u64, AudioFormat::OggFlac),
        frames: count,
    })
}

/// One frame's worth of a native stream on disk: see [`crate::source`].
pub(crate) struct FileFrames {
    frames: FrameDecoder,
    /// Where the first frame is.
    pub audio_start: u64,
}

impl FileFrames {
    /// Walks the metadata blocks, which can be far too large to hold, such as pictures.
    pub(crate) fn open(file: &dyn Source, cache: &mut Cache, start: u64) -> Result<Self> {
        let mut pos = start + 4;
        let mut stream = None;
        loop {
            let header = cache.bytes(file, pos, 4 + 34);
            if header.len() < 4 {
                return Err(AudioError::UnexpectedEnd);
            }
            let last = header[0] & 0x80 != 0;
            let kind = header[0] & 0x7f;
            let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as u64;
            if stream.is_none() {
                if kind != 0 {
                    return Err(AudioError::InvalidData("FLAC stream has no STREAMINFO"));
                }
                stream = Some(parse_stream_info(&header[4..])?);
            }
            pos += 4 + len;
            if last {
                break;
            }
        }
        match stream {
            Some(stream) => Ok(Self {
                frames: FrameDecoder::new(stream),
                audio_start: pos,
            }),
            None => Err(AudioError::InvalidData("FLAC stream has no STREAMINFO")),
        }
    }

    pub(crate) fn stream_info(&self) -> StreamInfo {
        self.frames.stream
    }

    /// Decodes the frame at the start of `data` if there is one, or skips to the next
    /// byte that could be. Returns `(bytes consumed, sample frames)`.
    pub(crate) fn decode(&mut self, data: &[u8], at_end: bool, out: &mut [i16]) -> Result<(usize, usize)> {
        let stream = self.frames.stream;
        let mut skipped = 0;
        loop {
            let rest = &data[skipped..];
            if rest.len() < 2 {
                return Ok((data.len(), 0));
            }
            match parse_frame_header(rest, &stream) {
                Ok(Some(header)) => {
                    let required = header.block * stream.channels as usize;
                    if out.len() < required {
                        return Err(AudioError::BufferTooSmall {
                            required,
                            available: out.len(),
                        });
                    }
                    return match self.frames.decode(rest, &header, Some(&mut out[..required])) {
                        Ok(len) => Ok((skipped + len, header.block)),
                        Err(AudioError::UnexpectedEnd) if !at_end && skipped > 0 => Ok((skipped, 0)),
                        Err(AudioError::UnexpectedEnd) if at_end => Ok((data.len(), 0)),
                        Err(_) => Ok((skipped + header.len, 0)),
                    };
                }
                Ok(None) => skipped += 1,
                Err(AudioError::UnexpectedEnd) => return Ok((data.len().max(1), 0)),
                Err(_) => skipped += 1,
            }
        }
    }

    /// The first frame header at or after the start of `data`, as `(offset, first
    /// sample of the frame)`.
    pub(crate) fn find_frame(&self, data: &[u8]) -> Option<(usize, u64)> {
        let stream = &self.frames.stream;
        for at in 0..data.len().saturating_sub(1) {
            if let Ok(Some(header)) = parse_frame_header(&data[at..], stream) {
                let sample = if header.variable {
                    header.number
                } else {
                    header.number * stream.min_block_size.max(1) as u64
                };
                return Some((at, sample));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crcs_match_their_check_values() {
        assert_eq!(crc8(b"123456789"), 0xf4);
        assert_eq!(crc16(b"123456789"), 0xfee8);
    }

    #[test]
    fn predictors_reject_samples_out_of_range() {
        let mut out = [1000, 1000, 1000];
        assert!(predict(&mut out, &[1], 0, 8).is_err());
        let mut out = [1, 1, 1, 1];
        predict(&mut out, &[1], 0, 16).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn samples_reduce_to_16_bits() {
        assert_eq!(to_i16(0x7f_ffff, 24), 0x7fff);
        assert_eq!(to_i16(-0x80_0000, 24), -0x8000);
        assert_eq!(to_i16(127, 8), 127 << 8);
        assert_eq!(to_i16(1234, 16), 1234);
    }
}