//! Streaming MPEG audio decoding: MP3, and Layers I and II.
//!
//! The actual decoder is a vendored copy of Raph Levien's safe-Rust port of
//! minimp3 (MIT OR Apache-2.0), with Layers I and II ported here from minimp3
//! itself (CC0). See `src/mp3/mod.rs` for attribution.
//!
//! # Gapless playback
//!
//! An encoder starts each file with silence (its own delay, plus the 529
//! samples a Layer III decoder's output lags by) and pads the end out to a whole
//! frame. LAME, and ffmpeg through it, writes a silent first frame holding an
//! Xing or Info header and a LAME tag that says how much. When the first frame
//! has that header, [`Mp3Decoder`] gives nothing for it, drops the leading
//! silence and stops at the stated end, as ffmpeg and other players do, so
//! the music starts on its first sample and lengths are exact.

use crate::error::{AudioError, Result};
use crate::mp3::{Decoder, FrameInfo, MAX_SAMPLES_PER_FRAME};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// How many samples a Layer III decoder's output lags its input by.
const DECODER_DELAY: u64 = 529;

/// What an Xing or Info header, and a LAME tag after it, say about a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct XingTag {
    /// Audio frames in the file, not counting the one holding the header.
    pub frames: Option<u32>,
    /// Samples (per channel) in each frame.
    pub samples_per_frame: u32,
    /// The encoder's delay and end padding, in samples, from a LAME tag.
    pub delay: u32,
    pub padding: u32,
    /// The 100-entry seek table, if there is one.
    pub toc: Option<[u8; 100]>,
}

impl XingTag {
    /// Leading samples to drop: the encoder's delay and the decoder's.
    pub fn skip(&self) -> u64 {
        DECODER_DELAY + self.delay as u64
    }

    /// The samples (per channel) of music, when the header gives a frame count.
    pub fn length(&self) -> Option<u64> {
        let frames = self.frames? as u64 * self.samples_per_frame as u64;
        Some(frames.saturating_sub(self.delay as u64 + self.padding as u64))
    }
}

/// Reads the Xing or Info header in a Layer III frame, `frame` starting at its
/// header. `None` when it isn't there.
pub fn xing_tag(frame: &[u8]) -> Option<XingTag> {
    let head = frame.get(..4)?;
    if head[0] != 0xff || head[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (head[1] >> 3) & 3; // 3: MPEG 1, 2: MPEG 2, 0: MPEG 2.5
    let layer = (head[1] >> 1) & 3; // 1: Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let mpeg1 = version == 3;
    let mono = head[3] >> 6 == 3;
    let crc = if head[1] & 1 == 0 { 2 } else { 0 };
    let side = match (mpeg1, mono) {
        (true, false) => 32,
        (true, true) => 17,
        (false, false) => 17,
        (false, true) => 9,
    };
    let mut at = 4 + crc + side;
    let id = frame.get(at..at + 4)?;
    if id != b"Xing" && id != b"Info" {
        return None;
    }
    let word = |at: usize| -> Option<u32> {
        let bytes = frame.get(at..at + 4)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let flags = word(at + 4)?;
    at += 8;
    let mut tag = XingTag {
        samples_per_frame: if mpeg1 { 1152 } else { 576 },
        ..XingTag::default()
    };
    if flags & 1 != 0 {
        tag.frames = Some(word(at)?);
        at += 4;
    }
    if flags & 2 != 0 {
        at += 4;
    }
    if flags & 4 != 0 {
        let mut toc = [0u8; 100];
        toc.copy_from_slice(frame.get(at..at + 100)?);
        tag.toc = Some(toc);
        at += 100;
    }
    if flags & 8 != 0 {
        at += 4;
    }
    // The LAME tag: 9 bytes of encoder name, then at 21 the delay and padding
    // as two 12-bit numbers. Encoders other than LAME and ffmpeg's leave it out
    // or fill it with something else.
    if let Some(lame) = frame.get(at..at + 24) {
        if [&b"LAME"[..], b"Lavf", b"Lavc"].contains(&&lame[..4]) {
            let (a, b, c) = (lame[21] as u32, lame[22] as u32, lame[23] as u32);
            tag.delay = (a << 4) | (b >> 4);
            tag.padding = ((b & 0x0f) << 8) | c;
        }
    }
    Some(tag)
}

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
    /// Whether a frame has been seen, so the next one isn't the first.
    started: bool,
    /// Leading samples (per channel) still to drop.
    skip: u64,
    /// Samples (per channel) given out so far, and the most there are.
    given: u64,
    end: Option<u64>,
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
            started: false,
            skip: 0,
            given: 0,
            end: None,
        }
    }

    /// A decoder for after a jump into the middle of the stream at `frame`
    /// (in samples per channel). It keeps the stream's end, but looks for no
    /// header, as the first frame it sees won't be the file's first.
    pub fn after_seek(&self, frame: u64) -> Self {
        Self {
            started: true,
            given: frame,
            end: self.end,
            ..Self::new()
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
        if !self.started {
            self.started = true;
            let header = input.get(self.frame.frame_offset..consumed).unwrap_or(&[]);
            if let Some(tag) = xing_tag(header) {
                // The header's frame is silence: nothing of it is music.
                self.skip = tag.skip();
                self.end = tag.length();
                return Ok(consumed);
            }
        }
        // Leading silence off the front, and padding off the end.
        let mut first = 0;
        let mut samples = samples;
        if self.skip > 0 {
            let dropped = (self.skip as usize).min(samples);
            self.skip -= dropped as u64;
            first = dropped;
            samples -= dropped;
        }
        if let Some(end) = self.end {
            samples = samples.min(end.saturating_sub(self.given) as usize);
        }
        self.given += samples as u64;
        let first = first * self.frame.channels;
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
            out[*written..end].copy_from_slice(&pcm[first..first + count]);
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
    let mut tag = None;
    let mut first = true;
    while pos < input.len() {
        let samples = decoder.decode_frame(&input[pos..], None, &mut frame);
        if frame.frame_bytes == 0 {
            break;
        }
        if first {
            first = false;
            let end = pos.saturating_add(frame.frame_bytes).min(input.len());
            tag = xing_tag(input.get(pos + frame.frame_offset..end).unwrap_or(&[]));
            if tag.is_some() {
                sample_rate = frame.hz;
                channels = frame.channels;
                pos += frame.frame_bytes;
                continue;
            }
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
    // What the decoder will give: the stated length, or what's there less the
    // leading silence.
    if let Some(tag) = tag {
        frames = frames.saturating_sub(tag.skip());
        if let Some(length) = tag.length() {
            frames = frames.min(length);
        }
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
