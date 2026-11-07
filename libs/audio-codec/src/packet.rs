//! Decoding audio that arrives as separate packets, as out of a video container.

use alloc::vec::Vec;

use crate::aac::{AacDecoder, AudioSpecificConfig};
use crate::error::{AudioError, Result};
use crate::flac::FlacPacketDecoder;
use crate::mp3_decode::Mp3Decoder;
use crate::opus::OpusPacketDecoder;
use crate::pcm;
use crate::vorbis::VorbisDecoder;

/// What a stream of packets holds, with any configuration the container stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PacketFormat<'a> {
    /// AAC, with an AudioSpecificConfig.
    Aac(&'a [u8]),
    /// MPEG audio.
    Mp3,
    /// MPEG audio, in packets that split frames.
    Mp3Split,
    /// Vorbis, with its headers as Matroska stores them.
    Vorbis(&'a [u8]),
    /// Opus, with an `OpusHead` or the contents of an MP4 `dOps` box. Comes out at 48 kHz.
    Opus(&'a [u8]),
    /// FLAC, with its metadata.
    Flac(&'a [u8]),
    /// Linear PCM. Eight-bit samples are signed.
    Pcm {
        /// Bytes per sample.
        bytes: u8,
        /// Most significant byte first.
        big_endian: bool,
        /// IEEE floating point.
        float: bool,
    },
    /// G.711 µ-law.
    MuLaw,
    /// G.711 A-law.
    ALaw,
}

enum Kind<'a> {
    Aac(AacDecoder),
    Mp3(Mp3Decoder),
    /// Frames split across packets: what's left of the last, and whether the packet
    /// being asked about has already gone in.
    Mp3Split {
        decoder: Mp3Decoder,
        carry: Vec<u8>,
        added: bool,
    },
    Vorbis(VorbisDecoder<'a>),
    Opus(OpusPacketDecoder),
    Flac(FlacPacketDecoder),
    Pcm {
        bytes: u8,
        big_endian: bool,
        float: bool,
    },
    MuLaw,
    ALaw,
}

/// The most held over from one packet to the next, which only a stream with no valid
/// frames would reach.
const MAX_CARRY: usize = 64 * 1024;

/// Finds the next whole MPEG audio frame: `(bytes before it, its length)`. `None`
/// when more is needed.
fn whole_frame(data: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0;
    while at + 4 <= data.len() {
        if let Some(len) = mpeg_frame_len(&data[at..at + 4]) {
            return (at + len <= data.len()).then_some((at, len));
        }
        at += 1;
    }
    None
}

/// The length of the frame a header starts, or `None` if it isn't a header.
fn mpeg_frame_len(head: &[u8]) -> Option<usize> {
    const BITRATES: [[u16; 16]; 5] = [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
    ];
    if head[0] != 0xff || head[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (head[1] >> 3) & 3;
    let layer = (head[1] >> 1) & 3;
    let rate_index = ((head[2] >> 2) & 3) as usize;
    let padding = ((head[2] >> 1) & 1) as usize;
    if version == 1 || layer == 0 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let table = match (mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, _) => 2,
        (false, 3) => 3,
        (false, _) => 4,
    };
    let bitrate = BITRATES[table][(head[2] >> 4) as usize] as usize * 1000;
    if bitrate == 0 {
        return None;
    }
    let base = [44_100, 48_000, 32_000][rate_index];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    Some(match layer {
        3 => (12 * bitrate / rate + padding) * 4,
        2 => 144 * bitrate / rate + padding,
        _ => (if mpeg1 { 144 } else { 72 }) * bitrate / rate + padding,
    })
}

/// A decoder for packets. Its properties come from the codec's own headers when
/// it has them, and otherwise from what the container said.
pub struct PacketDecoder<'a> {
    kind: Kind<'a>,
    sample_rate: u32,
    channels: u16,
    format: PacketFormat<'a>,
}

fn open_kind<'a>(format: PacketFormat<'a>) -> Result<Kind<'a>> {
    Ok(match format {
        PacketFormat::Aac(config) => {
            Kind::Aac(AacDecoder::new(AudioSpecificConfig::parse(config)?)?)
        }
        PacketFormat::Mp3 => Kind::Mp3(Mp3Decoder::new()),
        PacketFormat::Mp3Split => Kind::Mp3Split {
            decoder: Mp3Decoder::new(),
            carry: Vec::new(),
            added: false,
        },
        PacketFormat::Vorbis(private) => Kind::Vorbis(VorbisDecoder::from_private(private)?),
        PacketFormat::Opus(head) => Kind::Opus(OpusPacketDecoder::new(head)?),
        PacketFormat::Flac(private) => Kind::Flac(FlacPacketDecoder::new(private)?),
        PacketFormat::Pcm {
            bytes,
            big_endian,
            float,
        } => {
            if !matches!(
                (bytes, float),
                (1 | 2 | 3 | 4, false) | (4 | 8, true)
            ) {
                return Err(AudioError::UnsupportedFeature("PCM sample format"));
            }
            Kind::Pcm {
                bytes,
                big_endian,
                float,
            }
        }
        PacketFormat::MuLaw => Kind::MuLaw,
        PacketFormat::ALaw => Kind::ALaw,
    })
}

impl<'a> PacketDecoder<'a> {
    /// Creates a decoder. `sample_rate` and `channels` are the container's, used
    /// when the codec has nothing better. An MP3 stream states both in each
    /// frame, and they're updated when a frame has decoded.
    pub fn new(format: PacketFormat<'a>, sample_rate: u32, channels: u16) -> Result<Self> {
        let kind = open_kind(format)?;
        let (sample_rate, channels) = match &kind {
            Kind::Aac(decoder) => {
                let config = decoder.config();
                (config.output_rate(), config.output_channels() as u16)
            }
            Kind::Vorbis(decoder) => (decoder.sample_rate(), decoder.channels() as u16),
            Kind::Opus(decoder) => (48_000, decoder.channels()),
            Kind::Flac(decoder) => {
                let stream = decoder.stream_info();
                (stream.sample_rate, stream.channels)
            }
            _ => (sample_rate, channels),
        };
        if channels == 0 {
            return Err(AudioError::InvalidData("no audio channels"));
        }
        Ok(Self {
            kind,
            sample_rate,
            channels,
            format,
        })
    }

    /// The sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The number of interleaved channels.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Forgets all decoder state, for a seek.
    pub fn reset(&mut self) -> Result<()> {
        self.kind = open_kind(self.format)?;
        // A seek's lands past what the stream's start says to drop.
        if let Kind::Opus(decoder) = &mut self.kind {
            decoder.forget();
        }
        Ok(())
    }

    /// Interleaved samples an output buffer should hold for a typical packet.
    /// A larger one comes back as [`AudioError::BufferTooSmall`], and it's fine
    /// to grow the buffer and ask again: nothing has changed.
    pub fn typical_samples(&self) -> usize {
        match &self.kind {
            Kind::Aac(_) => 4096 * self.channels as usize,
            Kind::Mp3(_) => 2304 * 2,
            Kind::Mp3Split { .. } => 2304 * 8,
            Kind::Vorbis(decoder) => decoder.max_packet_samples(),
            Kind::Opus(decoder) => decoder.max_packet_samples(),
            Kind::Flac(decoder) => {
                let stream = decoder.stream_info();
                stream.max_block_size as usize * stream.channels as usize
            }
            _ => 4096 * self.channels as usize,
        }
    }

    /// Decodes one packet to `out`, and returns the sample frames written.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        let channels = self.channels as usize;
        match &mut self.kind {
            Kind::Aac(decoder) => {
                let frames = decoder.decode_raw_frame(packet, out)?;
                // A first frame can show SBR the config didn't mention.
                let config = decoder.config();
                self.sample_rate = config.output_rate();
                self.channels = config.output_channels() as u16;
                Ok(frames)
            }
            Kind::Vorbis(decoder) => decoder.decode_packet(packet, out),
            Kind::Opus(decoder) => decoder.decode(packet, out),
            Kind::Flac(decoder) => decoder.decode(packet, out),
            Kind::Mp3(decoder) => {
                let mut pos = 0;
                let mut written = 0;
                while pos < packet.len() {
                    let consumed = decoder.decode_frame(&packet[pos..], out, &mut written)?;
                    if consumed == 0 {
                        break;
                    }
                    pos += consumed;
                }
                if decoder.channels() != 0 {
                    self.channels = decoder.channels() as u16;
                    self.sample_rate = decoder.sample_rate() as u32;
                }
                Ok(written / (self.channels as usize).max(1))
            }
            Kind::Mp3Split { decoder, carry, added } => {
                if !*added {
                    if carry.len() + packet.len() > MAX_CARRY {
                        carry.clear();
                    }
                    carry.try_reserve(packet.len()).map_err(|_| AudioError::OutOfMemory)?;
                    carry.extend_from_slice(packet);
                    *added = true;
                }
                let mut pos = 0;
                let mut written = 0;
                while let Some(len) = whole_frame(&carry[pos..]) {
                    let (skip, frame) = len;
                    pos += skip;
                    match decoder.decode_frame(&carry[pos..pos + frame], out, &mut written) {
                        Ok(_) => pos += frame,
                        // Nothing of this frame is lost: it stays for the next call.
                        Err(AudioError::BufferTooSmall { .. }) if written > 0 => break,
                        Err(error @ AudioError::BufferTooSmall { .. }) => {
                            carry.drain(..pos);
                            return Err(error);
                        }
                        Err(_) => pos += frame,
                    }
                }
                carry.drain(..pos);
                *added = false;
                if decoder.channels() != 0 {
                    self.channels = decoder.channels() as u16;
                    self.sample_rate = decoder.sample_rate() as u32;
                }
                Ok(written / (self.channels as usize).max(1))
            }
            Kind::Pcm {
                bytes,
                big_endian,
                float,
            } => {
                let size = *bytes as usize;
                let frames = packet.len() / (size * channels);
                let samples = frames * channels;
                if out.len() < samples {
                    return Err(AudioError::BufferTooSmall {
                        required: samples,
                        available: out.len(),
                    });
                }
                let (big_endian, float) = (*big_endian, *float);
                for (slot, sample) in out.iter_mut().zip(packet.chunks_exact(size)) {
                    *slot = pcm_sample(sample, big_endian, float);
                }
                Ok(frames)
            }
            Kind::MuLaw | Kind::ALaw => {
                let frames = packet.len() / channels;
                let samples = frames * channels;
                if out.len() < samples {
                    return Err(AudioError::BufferTooSmall {
                        required: samples,
                        available: out.len(),
                    });
                }
                let mu = matches!(self.kind, Kind::MuLaw);
                for (slot, &byte) in out.iter_mut().zip(&packet[..samples]) {
                    *slot = if mu {
                        pcm::mulaw_to_i16(byte)
                    } else {
                        pcm::alaw_to_i16(byte)
                    };
                }
                Ok(frames)
            }
        }
    }
}

fn pcm_sample(sample: &[u8], big_endian: bool, float: bool) -> i16 {
    let mut ordered = [0u8; 8];
    let size = sample.len();
    for (index, &byte) in sample.iter().enumerate() {
        ordered[if big_endian { index } else { size - 1 - index }] = byte;
    }
    match (size, float) {
        (1, _) => ((ordered[0] as i8) as i16) << 8,
        (2, _) => i16::from_be_bytes([ordered[0], ordered[1]]),
        (3, _) => pcm::i24_to_i16([ordered[0], ordered[1], ordered[2]], true),
        (4, false) => pcm::i32_to_i16(i32::from_be_bytes([
            ordered[0], ordered[1], ordered[2], ordered[3],
        ])),
        (4, true) => pcm::f32_to_i16(f32::from_bits(u32::from_be_bytes([
            ordered[0], ordered[1], ordered[2], ordered[3],
        ]))),
        _ => pcm::f64_to_i16(f64::from_bits(u64::from_be_bytes(ordered))),
    }
}

/// A growable buffer that can't abort the kernel: returns `false` when short.
pub fn grow_samples(buffer: &mut Vec<i16>, required: usize) -> bool {
    if required <= buffer.len() {
        return true;
    }
    if buffer.try_reserve_exact(required - buffer.len()).is_err() {
        return false;
    }
    buffer.resize(required, 0);
    true
}
