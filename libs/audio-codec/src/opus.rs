//! Opus: packet parsing here, and decoding by `feos-opus`.

use feos_opus::{OpusDecoder, OpusError};

use crate::error::{AudioError, Result};

/// Information extracted from an Opus packet's TOC byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketInfo {
    /// Configuration number (0-31).
    pub config: u8,
    /// Whether the packet is stereo.
    pub stereo: bool,
    /// Number of frames in the packet.
    pub frame_count: u8,
    /// Duration of each frame in samples at 48 kHz.
    pub frame_samples: u16,
}

impl PacketInfo {
    /// Total number of samples at 48 kHz in this packet.
    pub fn samples(&self) -> usize {
        self.frame_count as usize * self.frame_samples as usize
    }
}

fn frame_samples(config: u8) -> u16 {
    match config {
        0..=3 => [480, 960, 1920, 2880][(config & 3) as usize],
        4..=7 => [480, 960, 1920, 2880][(config & 3) as usize],
        8..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        12..=13 => [480, 960][(config & 1) as usize],
        14..=15 => [480, 960][(config & 1) as usize],
        16..=19 => [120, 240, 480, 960][(config & 3) as usize],
        20..=23 => [120, 240, 480, 960][(config & 3) as usize],
        24..=27 => [120, 240, 480, 960][(config & 3) as usize],
        _ => [120, 240, 480, 960][(config & 3) as usize],
    }
}

/// Parses the TOC byte and frame-count code of an Opus packet.
pub fn parse_packet(packet: &[u8]) -> Result<PacketInfo> {
    let toc = *packet.first().ok_or(AudioError::UnexpectedEnd)?;
    let config = toc >> 3;
    let stereo = (toc >> 2) & 1 != 0;
    let code = toc & 3;
    let (frame_count, header_len) = match code {
        0 => (1u8, 1usize),
        1 | 2 => (2u8, 1usize),
        _ => {
            let second = *packet.get(1).ok_or(AudioError::UnexpectedEnd)?;
            let count = second & 0x3f;
            if count == 0 || count > 48 {
                return Err(AudioError::InvalidData("bad Opus frame count"));
            }
            let vbr = second & 0x80 != 0;
            let padding = second & 0x40 != 0;
            let mut header_len = 2usize;
            if padding {
                loop {
                    let byte = *packet.get(header_len).ok_or(AudioError::UnexpectedEnd)?;
                    header_len += 1;
                    if byte != 0xff {
                        break;
                    }
                }
            }
            let _ = vbr;
            (count, header_len)
        }
    };
    if packet.len() < header_len {
        return Err(AudioError::UnexpectedEnd);
    }
    Ok(PacketInfo {
        config,
        stereo,
        frame_count,
        frame_samples: frame_samples(config),
    })
}

/// What a stream's header says that decoding needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OpusConfig {
    pub channels: u16,
    /// Samples at 48 kHz at the start that are the decoder settling, not the sound.
    pub pre_skip: u16,
    /// A gain to apply, in 256ths of a decibel.
    pub gain: i16,
}

impl OpusConfig {
    /// Reads an `OpusHead` packet, as Ogg and Matroska have it, or the body of an MP4
    /// `dOps` box, which has the same fields big-endian after no magic.
    pub(crate) fn parse(data: &[u8]) -> Result<Self> {
        let (channels, pre_skip, gain, family) = match data.strip_prefix(b"OpusHead") {
            Some(rest) => {
                let head = rest.get(..11).ok_or(AudioError::UnexpectedEnd)?;
                (
                    head[1],
                    u16::from_le_bytes([head[2], head[3]]),
                    i16::from_le_bytes([head[8], head[9]]),
                    head[10],
                )
            }
            None => {
                let head = data.get(..11).ok_or(AudioError::UnexpectedEnd)?;
                (
                    head[1],
                    u16::from_be_bytes([head[2], head[3]]),
                    i16::from_be_bytes([head[8], head[9]]),
                    head[10],
                )
            }
        };
        if channels == 0 {
            return Err(AudioError::InvalidData("Opus stream has no channels"));
        }
        if channels > 2 || family != 0 {
            return Err(AudioError::UnsupportedFeature("Opus with more than two channels"));
        }
        Ok(Self {
            channels: channels as u16,
            pre_skip,
            gain,
        })
    }
}

/// `10^(gain / 5120)`, by its series: gains are rare, and small when they're not.
fn gain_factor(gain: i16) -> f32 {
    let x = gain as f32 * core::f32::consts::LN_10 / 5120.0;
    let halves = (x.abs() / 0.5) as i32 + 1;
    let step = x / halves as f32;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..10 {
        term *= step / n as f32;
        sum += term;
    }
    let mut value = 1.0;
    for _ in 0..halves {
        value *= sum;
    }
    value
}

/// Decodes the packets of one Opus stream to 16-bit samples at 48 kHz, the rate Opus
/// streams come back at here.
pub struct OpusPacketDecoder {
    decoder: OpusDecoder,
    channels: u16,
    /// Samples still to be dropped from the start.
    skip: usize,
    factor: Option<f32>,
}

impl OpusPacketDecoder {
    /// Takes the stream's `OpusHead`, or an MP4 `dOps` box's contents.
    pub fn new(head: &[u8]) -> Result<Self> {
        let config = OpusConfig::parse(head)?;
        Ok(Self {
            decoder: OpusDecoder::new(48_000, config.channels as usize).map_err(map_error)?,
            channels: config.channels,
            skip: config.pre_skip as usize,
            factor: (config.gain != 0).then(|| gain_factor(config.gain)),
        })
    }

    /// The number of interleaved channels.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// The most interleaved samples one packet can make.
    pub fn max_packet_samples(&self) -> usize {
        OpusDecoder::MAX_FRAME_SIZE_48K * self.channels as usize
    }

    /// Forgets what it was decoding, for a seek. Nothing more gets dropped from the start.
    pub fn forget(&mut self) {
        self.decoder.reset();
        self.skip = 0;
    }

    /// Decodes a packet to `out`, and returns the sample frames written.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        let channels = self.channels as usize;
        let needed = self.max_packet_samples();
        if out.len() < needed {
            return Err(AudioError::BufferTooSmall {
                required: needed,
                available: out.len(),
            });
        }
        let frames = self.decoder.decode(packet, out, false).map_err(map_error)?;
        let samples = frames * channels;
        if let Some(factor) = self.factor {
            for sample in &mut out[..samples] {
                *sample = (*sample as f32 * factor).clamp(-32768.0, 32767.0) as i16;
            }
        }
        let dropped = self.skip.min(frames);
        self.skip -= dropped;
        if dropped > 0 {
            out.copy_within(dropped * channels..samples, 0);
        }
        Ok(frames - dropped)
    }
}

fn map_error(error: OpusError) -> AudioError {
    match error {
        OpusError::InvalidPacket => AudioError::InvalidData("bad Opus packet"),
        OpusError::InternalError => AudioError::InvalidData("bad Opus packet"),
        OpusError::BufferTooSmall => AudioError::BufferTooSmall {
            required: 0,
            available: 0,
        },
        OpusError::InvalidArgument(_) => AudioError::InvalidData("bad Opus parameters"),
    }
}
