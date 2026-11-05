//! Streaming MPEG audio decoding: MP3, and Layers I and II.
//!
//! The actual decoder is a vendored copy of Raph Levien's safe-Rust port of
//! minimp3 (MIT OR Apache-2.0), with Layers I and II ported here from minimp3
//! itself (CC0). See `src/mp3/mod.rs` for attribution.

use crate::error::{AudioError, Result};
use crate::mp3::{Decoder, FrameInfo, MAX_SAMPLES_PER_FRAME};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// A stateful MP3 decoder.
///
/// The decoder keeps the bit reservoir and overlap state between calls. It is
/// roughly 6.5 KB; callers that care about stack usage should keep it in a
/// static or other long-lived storage.
pub struct Mp3Decoder {
    decoder: Decoder,
    frame: FrameInfo,
    channels: usize,
    sample_rate: usize,
}

impl Default for Mp3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Mp3Decoder {
    /// Creates a decoder.
    pub fn new() -> Self {
        Self {
            decoder: Decoder::new(),
            frame: FrameInfo::default(),
            channels: 0,
            sample_rate: 0,
        }
    }

    /// Number of channels in the most recently decoded frame (0 before the
    /// first frame).
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Sample rate of the most recently decoded frame.
    pub fn sample_rate(&self) -> usize {
        self.sample_rate
    }

    /// Decodes the next frame, appending interleaved samples to `out` at
    /// `written`, and returns the number of input bytes consumed.
    ///
    /// A return value of zero means no further frame could be found. Frames
    /// that cannot be decoded yet (for example the first frame before the bit
    /// reservoir is filled) consume input but append no samples.
    pub fn decode_frame(
        &mut self,
        input: &[u8],
        out: &mut [i16],
        written: &mut usize,
    ) -> Result<usize> {
        let mut pcm = [0i16; MAX_SAMPLES_PER_FRAME];
        // A search that finds no frame leaves the field untouched.
        self.frame.layer = 0;
        let samples = self
            .decoder
            .decode_frame(input, Some(&mut pcm), &mut self.frame);
        let consumed = self.frame.frame_bytes;
        if consumed == 0 {
            return Ok(0);
        }
        if self.frame.channels != 0 {
            self.channels = self.frame.channels;
            self.sample_rate = self.frame.hz;
        }
        let count = samples
            .checked_mul(self.frame.channels)
            .ok_or(AudioError::Overflow)?;
        if count != 0 {
            let end = written.checked_add(count).ok_or(AudioError::Overflow)?;
            if end > out.len() {
                return Err(AudioError::BufferTooSmall {
                    required: end,
                    available: out.len(),
                });
            }
            out[*written..end].copy_from_slice(&pcm[..count]);
            *written = end;
        }
        Ok(consumed)
    }
}

/// Scans an MP3 stream for its first frame and estimates its length.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let mut decoder = Decoder::new();
    let mut frame = FrameInfo::default();
    let mut pos = 0usize;
    let mut frames = 0u64;
    let mut sample_rate = 0usize;
    let mut channels = 0usize;
    while pos < input.len() {
        let samples = decoder.decode_frame(&input[pos..], None, &mut frame);
        if frame.frame_bytes == 0 {
            break;
        }
        if samples != 0 {
            if channels == 0 {
                sample_rate = frame.hz;
                channels = frame.channels;
            }
            frames += samples as u64;
        }
        pos += frame.frame_bytes;
    }
    if channels == 0 {
        return Err(AudioError::InvalidData("no MP3 frame found"));
    }
    Ok(AudioInfo {
        sample_rate: sample_rate as u32,
        channels: channels as u16,
        frames,
        format: AudioFormat::Mp3,
    })
}

/// Decodes an entire MP3 stream to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let mut decoder = Mp3Decoder::new();
    let mut offset = 0usize;
    let mut written = 0usize;
    while offset < input.len() {
        let consumed = decoder.decode_frame(&input[offset..], out, &mut written)?;
        if consumed == 0 {
            break;
        }
        offset += consumed;
    }
    if decoder.sample_rate == 0 {
        return Err(AudioError::InvalidData("no MP3 frame found"));
    }
    let channels = decoder.channels as u16;
    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: decoder.sample_rate as u32,
            channels,
            frames: (written / channels.max(1) as usize) as u64,
            format: AudioFormat::Mp3,
        },
        frames: written / channels.max(1) as usize,
    })
}
