//! Decoding audio that arrives as separate packets, as out of a video container.

use alloc::vec::Vec;

use crate::aac::{AacDecoder, AudioSpecificConfig};
use crate::error::{AudioError, Result};
use crate::flac::FlacPacketDecoder;
use crate::mp3_decode::Mp3Decoder;
use crate::pcm;
use crate::vorbis::VorbisDecoder;

/// What a stream of packets holds, with any configuration the container stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PacketFormat<'a> {
    /// AAC, with an AudioSpecificConfig.
    Aac(&'a [u8]),
    /// MPEG audio.
    Mp3,
    /// Vorbis, with its headers as Matroska stores them.
    Vorbis(&'a [u8]),
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
    Vorbis(VorbisDecoder<'a>),
    Flac(FlacPacketDecoder),
    Pcm {
        bytes: u8,
        big_endian: bool,
        float: bool,
    },
    MuLaw,
    ALaw,
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
        PacketFormat::Vorbis(private) => Kind::Vorbis(VorbisDecoder::from_private(private)?),
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
                (config.sample_rate, config.channels() as u16)
            }
            Kind::Vorbis(decoder) => (decoder.sample_rate(), decoder.channels() as u16),
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
        Ok(())
    }

    /// Interleaved samples an output buffer should hold for a typical packet.
    /// A larger one comes back as [`AudioError::BufferTooSmall`], and it's fine
    /// to grow the buffer and ask again: nothing has changed.
    pub fn typical_samples(&self) -> usize {
        match &self.kind {
            Kind::Aac(_) => 4096 * self.channels as usize,
            Kind::Mp3(_) => 2304 * 2,
            Kind::Vorbis(decoder) => decoder.max_packet_samples(),
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
            Kind::Aac(decoder) => decoder.decode_raw_frame(packet, out),
            Kind::Vorbis(decoder) => decoder.decode_packet(packet, out),
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
