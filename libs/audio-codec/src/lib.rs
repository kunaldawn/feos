//! `feos-audio-codec` is a dependency-free audio codec library for the feos
//! kernel.
//!
//! All decoding is done into a caller-provided interleaved signed 16-bit
//! buffer and never allocates.
//!
//! | Format | Decode | Encode |
//! |--------|--------|--------|
//! | WAVE (PCM, float, A-law, µ-law) | yes | PCM16 |
//! | AIFF / AIFF-C | PCM 8/16/24/32-bit (big or little endian) | PCM16 |
//! | AU / SND | µ-law, A-law, PCM, float | no |
//! | MP3 (MPEG Layer III) | yes | no |
//! | Ogg Opus | headers, pages and packet TOC only | no |
//!
//! ```
//! use feos_audio_codec::{decode, detect, AudioFormat};
//!
//! // A 1-frame mono 8 kHz PCM16 WAVE file.
//! let mut wav = [0u8; 64];
//! let len = feos_audio_codec::wav::encode_i16(8000, 1, &[1234], &mut wav).unwrap();
//! let wav = &wav[..len];
//! assert_eq!(detect(wav), Some(AudioFormat::Wav));
//!
//! let mut pcm = [0i16; 1];
//! let decoded = decode(wav, &mut pcm, &mut []).unwrap();
//! assert_eq!(decoded.frames, 1);
//! assert_eq!(pcm[0], 1234);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod aiff;
pub mod au;
mod error;
mod mp3;
mod mp3_decode;
pub mod ogg;
pub mod opus;
mod pcm;
pub mod wav;

pub use error::{AudioError, Result};
pub use mp3_decode::Mp3Decoder;

/// Audio container formats understood by this crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioFormat {
    /// RIFF/WAVE.
    Wav,
    /// AIFF or AIFF-C.
    Aiff,
    /// Sun/NeXT AU.
    Au,
    /// MPEG-1/2/2.5 Layer III.
    Mp3,
    /// Ogg container with an Opus stream.
    OggOpus,
}

impl AudioFormat {
    /// A short, lowercase name for the format.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Au => "au",
            Self::Mp3 => "mp3",
            Self::OggOpus => "ogg-opus",
        }
    }
}

/// Basic properties of an encoded audio stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioInfo {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Number of sample frames.
    pub frames: u64,
    /// Container / codec family.
    pub format: AudioFormat,
}

impl AudioInfo {
    /// Duration in milliseconds, rounded down.
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            self.frames.saturating_mul(1000) / self.sample_rate as u64
        }
    }

    /// Number of `i16` elements needed to hold the whole stream.
    pub fn sample_count(&self) -> Result<usize> {
        (self.frames as usize)
            .checked_mul(self.channels as usize)
            .ok_or(AudioError::Overflow)
    }
}

/// Result of a successful decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedAudio {
    /// Stream properties.
    pub info: AudioInfo,
    /// Sample frames actually written.
    pub frames: usize,
}

/// Detects an audio format from its signature.
pub fn detect(input: &[u8]) -> Option<AudioFormat> {
    if input.len() >= 12 && &input[..4] == b"RIFF" && &input[8..12] == b"WAVE" {
        Some(AudioFormat::Wav)
    } else if input.len() >= 12
        && &input[..4] == b"FORM"
        && (&input[8..12] == b"AIFF" || &input[8..12] == b"AIFC")
    {
        Some(AudioFormat::Aiff)
    } else if input.len() >= 4 && &input[..4] == b".snd" {
        Some(AudioFormat::Au)
    } else if input.len() >= 4 && &input[..4] == b"OggS" {
        // Only Opus-in-Ogg is supported; check the first page payload.
        let mut pages = ogg::Pages::new(input);
        match pages.next() {
            Some(Ok(page)) if page.data.starts_with(b"OpusHead") => Some(AudioFormat::OggOpus),
            _ => None,
        }
    } else if input.len() >= 3 && &input[..3] == b"ID3" {
        Some(AudioFormat::Mp3)
    } else if input.len() >= 2
        && input[0] == 0xff
        && (input[1] & 0xe0) == 0xe0
        && (input[1] & 0x06) != 0
    {
        Some(AudioFormat::Mp3)
    } else {
        None
    }
}

/// Reads the properties of an encoded audio stream.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    match detect(input).ok_or(AudioError::UnknownFormat)? {
        AudioFormat::Wav => wav::info(input),
        AudioFormat::Aiff => aiff::info(input),
        AudioFormat::Au => au::info(input),
        AudioFormat::Mp3 => mp3_decode::info(input),
        AudioFormat::OggOpus => ogg::info(input),
    }
}

/// Extra working memory needed by [`decode`].
///
/// Currently every decoder works in place or on the stack, so this returns 0.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    match detect(input).ok_or(AudioError::UnknownFormat)? {
        AudioFormat::Wav => wav::scratch_len(input),
        AudioFormat::Aiff => aiff::scratch_len(input),
        AudioFormat::Au => au::scratch_len(input),
        AudioFormat::Mp3 => mp3_decode::scratch_len(input),
        AudioFormat::OggOpus => ogg::scratch_len(input),
    }
}

/// Decodes any supported stream into interleaved signed 16-bit samples.
///
/// Ogg Opus is detected but its packets cannot be decoded yet; this returns
/// [`AudioError::UnsupportedFeature`].
pub fn decode(input: &[u8], out: &mut [i16], _scratch: &mut [u8]) -> Result<DecodedAudio> {
    match detect(input).ok_or(AudioError::UnknownFormat)? {
        AudioFormat::Wav => wav::decode(input, out),
        AudioFormat::Aiff => aiff::decode(input, out),
        AudioFormat::Au => au::decode(input, out),
        AudioFormat::Mp3 => mp3_decode::decode(input, out),
        AudioFormat::OggOpus => ogg::decode(input, out),
    }
}
