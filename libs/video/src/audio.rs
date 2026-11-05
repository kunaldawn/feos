//! The audio track of a video file: what's in it and where its packets are.
//!
//! This crate doesn't decode audio. It locates packets, as byte ranges with
//! presentation times, for a caller to hand to `feos-audio-codec`. A file whose
//! audio can't be understood plays as silent video, so nothing here is an error
//! to the video track: [`audio_track`] just returns `None`.

use alloc::vec::Vec;
use core::ops::Range;

use crate::{avi, detect, flv, matroska, mp4, ContainerFormat};

/// Most packets an audio track may have, which bounds its table at about 6 MiB.
pub(crate) const MAX_PACKETS: usize = 1 << 18;

/// How an audio track is coded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioCodec {
    /// AAC, with an AudioSpecificConfig in [`AudioTrack::config`].
    Aac,
    /// MPEG audio.
    Mp3,
    /// Vorbis, with its headers in [`AudioTrack::config`] as Matroska stores
    /// them.
    Vorbis,
    /// FLAC, with its metadata in [`AudioTrack::config`].
    Flac,
    /// Opus.
    Opus,
    /// Linear PCM.
    Pcm {
        /// Bytes in each sample.
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
    /// Something else.
    Other,
}

impl AudioCodec {
    /// A short name, for display.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Mp3 => "MP3",
            Self::Vorbis => "Vorbis",
            Self::Flac => "FLAC",
            Self::Opus => "Opus",
            Self::Pcm { .. } => "PCM",
            Self::MuLaw => "G.711 u-law",
            Self::ALaw => "G.711 A-law",
            Self::Other => "unknown",
        }
    }
}

/// One audio packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioPacket {
    /// Byte offset in the file.
    pub offset: usize,
    /// Bytes in the packet.
    pub size: u32,
    /// When it starts, in milliseconds.
    pub pts_ms: u64,
}

/// An audio track found by [`audio_track`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AudioTrack {
    /// How it's coded.
    pub codec: AudioCodec,
    /// Sample rate as the container states it, in Hz.
    pub sample_rate: u32,
    /// Interleaved channels as the container states them.
    pub channels: u16,
    /// The codec's configuration, as a byte range of the file.
    pub config: Range<usize>,
    /// The packets, in order.
    pub packets: Vec<AudioPacket>,
}

impl AudioTrack {
    /// The index of the packet playing at a time.
    pub fn packet_at(&self, ms: u64) -> usize {
        self.packets
            .partition_point(|packet| packet.pts_ms <= ms)
            .saturating_sub(1)
    }

    /// One packet's bytes, or `None` if it's out of range.
    pub fn packet<'a>(&self, input: &'a [u8], index: usize) -> Option<&'a [u8]> {
        let packet = self.packets.get(index)?;
        input.get(packet.offset..packet.offset.checked_add(packet.size as usize)?)
    }
}

/// Appends a packet, within the limit, without being able to abort the kernel.
pub(crate) fn push_packet(
    packets: &mut Vec<AudioPacket>,
    input_len: usize,
    offset: usize,
    size: usize,
    pts_ms: u64,
) -> Option<()> {
    if packets.len() >= MAX_PACKETS || size > u32::MAX as usize {
        return None;
    }
    if offset.checked_add(size)? > input_len {
        return None;
    }
    if packets.try_reserve(1).is_err() {
        return None;
    }
    packets.push(AudioPacket {
        offset,
        size: size as u32,
        pts_ms,
    });
    Some(())
}

/// Finds the first audio track of a video file, if it has one this can read.
pub fn audio_track(input: &[u8]) -> Option<AudioTrack> {
    let track = match detect(input)? {
        ContainerFormat::Mp4 => mp4::audio_track(input),
        ContainerFormat::Matroska => matroska::audio_track(input),
        ContainerFormat::Flv => flv::audio_track(input),
        ContainerFormat::Avi => avi::audio_track(input),
        _ => None,
    }?;
    (!track.packets.is_empty()).then_some(track)
}
