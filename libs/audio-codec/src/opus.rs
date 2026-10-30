//! Minimal Opus packet parsing.
//!
//! This module understands the TOC (table of contents) byte and the frame
//! count encoding. It does not perform SILK/CELT decoding.

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
