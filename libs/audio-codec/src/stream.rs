//! A file decoded a piece at a time, for playing rather than converting.
//!
//! [`decode`](crate::decode) needs an output buffer for the whole stream, which
//! a few minutes of audio can't afford. [`AudioStream`] gives one packet's worth
//! per call instead, so a caller can poll for cancellation between them and needs
//! only a small buffer. All the formats look the same from here.

use crate::aac::AdtsDecoder;
use crate::error::{AudioError, Result};
use crate::flac::{FlacDecoder, OggFlacDecoder};
use crate::mp3_decode::Mp3Decoder;
use crate::ogg::OggOpusDecoder;
use crate::vorbis::VorbisDecoder;
use crate::{aiff, au, wav, AudioFormat, AudioInfo};

/// PCM frames decoded per call.
const PCM_CHUNK_FRAMES: usize = 1024;
/// The most interleaved samples one MP3 frame makes.
const MP3_CHUNK_SAMPLES: usize = 2304;
/// A FLAC frame's largest block, with the most channels.
const MAX_FLAC_SAMPLES: usize = 65535 * 8;

type ReadPcm = fn(&[u8], usize, &mut [i16]) -> Result<usize>;

enum Inner<'a> {
    Pcm { read: ReadPcm, next: usize },
    Mp3 { decoder: Mp3Decoder, pos: usize },
    Flac(FlacDecoder<'a>),
    OggFlac(OggFlacDecoder<'a>),
    Vorbis(VorbisDecoder<'a>),
    Aac(AdtsDecoder<'a>),
    Opus(OggOpusDecoder<'a>),
}

fn open_inner(input: &[u8], format: AudioFormat) -> Result<Inner<'_>> {
    Ok(match format {
        AudioFormat::Wav => Inner::Pcm {
            read: wav::read_frames,
            next: 0,
        },
        AudioFormat::Aiff => Inner::Pcm {
            read: aiff::read_frames,
            next: 0,
        },
        AudioFormat::Au => Inner::Pcm {
            read: au::read_frames,
            next: 0,
        },
        AudioFormat::Mp3 => Inner::Mp3 {
            decoder: Mp3Decoder::new(),
            pos: 0,
        },
        AudioFormat::Flac => Inner::Flac(FlacDecoder::new(input)?),
        AudioFormat::OggFlac => Inner::OggFlac(OggFlacDecoder::new(input)?),
        AudioFormat::OggVorbis => Inner::Vorbis(VorbisDecoder::new(input)?),
        AudioFormat::Aac => Inner::Aac(AdtsDecoder::new(input)?),
        AudioFormat::OggOpus => Inner::Opus(OggOpusDecoder::new(input)?),
    })
}

/// An encoded file being decoded in order.
pub struct AudioStream<'a> {
    input: &'a [u8],
    info: AudioInfo,
    inner: Inner<'a>,
    /// Sample frames handed out so far, counting any being skipped.
    position: u64,
    /// Decode and discard until `position` reaches this.
    skip_to: u64,
}

impl<'a> AudioStream<'a> {
    /// Opens a stream, or says why it can't play.
    pub fn open(input: &'a [u8]) -> Result<Self> {
        let info = crate::info(input)?;
        let inner = open_inner(input, info.format)?;
        Ok(Self {
            input,
            info,
            inner,
            position: 0,
            skip_to: 0,
        })
    }

    /// The stream's properties.
    pub fn info(&self) -> AudioInfo {
        self.info
    }

    /// Sample frames handed out so far.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// How many interleaved samples [`next_chunk`](Self::next_chunk) needs room
    /// for. FLAC streams state their largest block, and only a lying file asks
    /// for more. That comes back as [`AudioError::BufferTooSmall`].
    pub fn max_chunk_samples(&self) -> usize {
        match &self.inner {
            Inner::Pcm { .. } => PCM_CHUNK_FRAMES * self.info.channels as usize,
            Inner::Mp3 { .. } => MP3_CHUNK_SAMPLES,
            Inner::Flac(decoder) => {
                let stream = decoder.stream_info();
                let samples = stream.max_block_size as usize * stream.channels as usize;
                if samples == 0 {
                    MAX_FLAC_SAMPLES
                } else {
                    samples
                }
            }
            Inner::OggFlac(decoder) => {
                let stream = decoder.stream_info();
                let samples = stream.max_block_size as usize * stream.channels as usize;
                if samples == 0 {
                    MAX_FLAC_SAMPLES
                } else {
                    samples
                }
            }
            Inner::Vorbis(decoder) => decoder.max_packet_samples(),
            Inner::Aac(decoder) => decoder.max_frame_samples(),
            Inner::Opus(decoder) => decoder.max_chunk_samples(),
        }
    }

    /// Decodes one packet into `out` and returns the sample frames written, or
    /// `None` at the end of the stream.
    ///
    /// `Some(0)` is normal: some packets make nothing, and frames before a seek
    /// target aren't handed out. After a decoding error the stream has moved past
    /// the damage, so the next call may succeed.
    pub fn next_chunk(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        let channels = self.info.channels.max(1) as usize;
        let frames = match &mut self.inner {
            Inner::Pcm { read, next } => {
                let count = read(self.input, *next, out)?;
                if count == 0 {
                    return Ok(None);
                }
                *next += count;
                count
            }
            Inner::Mp3 { decoder, pos } => {
                let rest = self.input.get(*pos..).unwrap_or(&[]);
                let mut written = 0;
                let consumed = match decoder.decode_frame(rest, out, &mut written) {
                    Ok(consumed) => consumed,
                    Err(error @ AudioError::BufferTooSmall { .. }) => return Err(error),
                    // Layers this decoder doesn't do: there's no point going on.
                    Err(error) => {
                        *pos = self.input.len();
                        return Err(error);
                    }
                };
                if consumed == 0 {
                    return Ok(None);
                }
                *pos += consumed;
                // A file that changes layout partway: skip what doesn't match
                // what the caller was told.
                if decoder.channels() != channels {
                    return Ok(Some(0));
                }
                written / channels
            }
            Inner::Flac(decoder) => match decoder.next_frame(out)? {
                Some(frames) => frames,
                None => return Ok(None),
            },
            Inner::OggFlac(decoder) => match decoder.next_frame(out)? {
                Some(frames) => frames,
                None => return Ok(None),
            },
            Inner::Vorbis(decoder) => match decoder.next_packet(out)? {
                Some(frames) => frames,
                None => return Ok(None),
            },
            Inner::Aac(decoder) => match decoder.next_frame(out)? {
                Some(frames) => frames,
                None => return Ok(None),
            },
            Inner::Opus(decoder) => match decoder.next_chunk(out)? {
                Some(frames) => frames,
                None => return Ok(None),
            },
        };
        let start = self.position;
        self.position += frames as u64;
        // An Opus stream states its end, cutting off the last packet's padding.
        let mut frames = frames;
        if matches!(self.inner, Inner::Opus(_)) && self.position > self.info.frames {
            frames = frames.saturating_sub((self.position - self.info.frames) as usize);
            self.position = self.info.frames;
        }
        if self.position <= self.skip_to {
            return Ok(Some(0));
        }
        // A packet straddling the target: keep the part after it.
        let drop_frames = self.skip_to.saturating_sub(start) as usize;
        if drop_frames > 0 {
            out.copy_within(drop_frames * channels..frames * channels, 0);
        }
        Ok(Some(frames - drop_frames))
    }

    /// Makes `frame` the next sample frame [`next_chunk`](Self::next_chunk)
    /// gives. Going forward continues from here; going back starts the decoder
    /// again, which is where a compressed file's decoding state comes from. PCM
    /// just moves. Either way this returns at once, and the frames in between
    /// get decoded by the next calls, one packet each.
    pub fn seek(&mut self, frame: u64) -> Result<()> {
        let frame = if self.info.frames > 0 {
            frame.min(self.info.frames)
        } else {
            frame
        };
        if let Inner::Pcm { next, .. } = &mut self.inner {
            *next = frame as usize;
            self.position = frame;
            self.skip_to = frame;
            return Ok(());
        }
        if frame < self.position {
            self.inner = open_inner(self.input, self.info.format)?;
            self.position = 0;
        }
        self.skip_to = frame;
        Ok(())
    }
}
