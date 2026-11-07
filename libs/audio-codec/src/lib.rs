//! `feos-audio-codec` is a dependency-free audio codec library for the feos
//! kernel.
//!
//! All decoding is done into a caller-provided interleaved signed 16-bit
//! buffer. The PCM containers and MP3 never allocate. FLAC, Vorbis and AAC need
//! block-sized working memory and their tables, which they allocate fallibly and
//! within fixed limits, so a hostile file gets [`AudioError::OutOfMemory`] or
//! [`AudioError::InvalidData`] rather than a kernel abort.
//!
//! Decoder CPU is bounded by the sizes of the input and the output buffer. Size
//! `out` from a trusted limit rather than from [`info`] alone: a file's stated
//! length is only a claim.
//!
//! | Format | Decode | Encode |
//! |--------|--------|--------|
//! | WAVE (PCM, float, A-law, µ-law) | yes | PCM16 |
//! | AIFF / AIFF-C | PCM 8/16/24/32-bit (big or little endian) | PCM16 |
//! | AU / SND | µ-law, A-law, PCM, float | no |
//! | MPEG audio: MP3, Layers I and II | yes | no |
//! | FLAC, native and in Ogg | yes, to 16 bits | no |
//! | Ogg Vorbis | yes | no |
//! | AAC (LC, Main, LTP, HE and HE v2), in ADTS or raw frames | yes | no |
//! | Ogg Opus | SILK, CELT and hybrid, by `feos-opus`, up to eight channels | yes |
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

extern crate alloc;

pub mod aac;
mod aac_tables;
mod ps;
mod sbr;
mod sbr_tables;
pub mod aiff;
pub mod au;
mod bits;
mod error;
pub mod flac;
mod mdct;
mod mp3;
mod mp3_decode;
pub mod ogg;
pub mod opus;
pub mod packet;
mod pcm;
pub mod source;
pub mod stream;
pub mod vorbis;
pub mod wav;

pub use error::{AudioError, Result};
pub use mp3_decode::Mp3Decoder;
pub use packet::{PacketDecoder, PacketFormat};
pub use feos_source::Source;
pub use source::SourceStream;
pub use stream::AudioStream;

/// Audio container formats understood by this crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioFormat {
    /// RIFF/WAVE.
    Wav,
    /// AIFF or AIFF-C.
    Aiff,
    /// Sun/NeXT AU.
    Au,
    /// MPEG-1/2/2.5 audio, in any layer.
    Mp3,
    /// Ogg container with an Opus stream.
    OggOpus,
    /// Ogg container with a Vorbis stream.
    OggVorbis,
    /// Ogg container with a FLAC stream.
    OggFlac,
    /// Native FLAC.
    Flac,
    /// AAC in ADTS framing.
    Aac,
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
            Self::OggVorbis => "ogg-vorbis",
            Self::OggFlac => "ogg-flac",
            Self::Flac => "flac",
            Self::Aac => "aac",
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
///
/// An Ogg file is identified by the codec of its first known logical stream;
/// an Ogg file with damaged headers or another codec is `None`, and [`info`]
/// says which.
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
        match ogg::find_stream(input) {
            Ok(Some((format, _))) => Some(format),
            _ => None,
        }
    } else if input.len() >= 4 && &input[..4] == b"fLaC" {
        Some(AudioFormat::Flac)
    } else if input.len() >= 3 && &input[..3] == b"ID3" {
        // Tagged files are nearly always MP3, but anything can follow a tag.
        match input.get(flac::id3_len(input)..).and_then(|rest| rest.get(..4)) {
            Some(b"fLaC") => Some(AudioFormat::Flac),
            Some(rest) if rest[0] == 0xff && rest[1] & 0xf6 == 0xf0 => Some(AudioFormat::Aac),
            _ => Some(AudioFormat::Mp3),
        }
    } else if input.len() >= 2 && input[0] == 0xff && input[1] & 0xf6 == 0xf0 {
        // The layer bits are always 0 in ADTS and never in MPEG audio.
        Some(AudioFormat::Aac)
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

/// [`detect`], with the reason when it finds nothing.
fn detect_or_err(input: &[u8]) -> Result<AudioFormat> {
    detect(input).ok_or_else(|| {
        if input.starts_with(b"OggS") {
            match ogg::find_stream(input) {
                Err(error) => error,
                // A valid Ogg file with no stream this crate knows.
                Ok(_) => AudioError::UnsupportedFormat,
            }
        } else {
            AudioError::UnknownFormat
        }
    })
}

/// Reads the properties of an encoded audio stream.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    match detect_or_err(input)? {
        AudioFormat::Wav => wav::info(input),
        AudioFormat::Aiff => aiff::info(input),
        AudioFormat::Au => au::info(input),
        AudioFormat::Mp3 => mp3_decode::info(input),
        AudioFormat::Flac => flac::info(input),
        AudioFormat::Aac => aac::info(input),
        AudioFormat::OggOpus | AudioFormat::OggVorbis | AudioFormat::OggFlac => ogg::info(input),
    }
}

/// Extra working memory needed by [`decode`] from the caller.
///
/// This is always 0: every decoder works in place, on the stack, or on memory
/// it allocates itself, fallibly.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    detect_or_err(input)?;
    Ok(0)
}

/// Decodes any supported stream into interleaved signed 16-bit samples.
///
pub fn decode(input: &[u8], out: &mut [i16], _scratch: &mut [u8]) -> Result<DecodedAudio> {
    match detect_or_err(input)? {
        AudioFormat::Wav => wav::decode(input, out),
        AudioFormat::Aiff => aiff::decode(input, out),
        AudioFormat::Au => au::decode(input, out),
        AudioFormat::Mp3 => mp3_decode::decode(input, out),
        AudioFormat::Flac => flac::decode(input, out),
        AudioFormat::Aac => aac::decode(input, out),
        AudioFormat::OggOpus | AudioFormat::OggVorbis | AudioFormat::OggFlac => {
            ogg::decode(input, out)
        }
    }
}
