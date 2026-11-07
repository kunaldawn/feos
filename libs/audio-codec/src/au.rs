//! Sun/NeXT AU (`.snd`) decoding.

use crate::error::{need, AudioError, Result};
use crate::pcm;
use crate::{AudioFormat, AudioInfo, DecodedAudio};

#[derive(Clone, Copy)]
struct Format {
    encoding: u32,
    channels: u16,
    sample_rate: u32,
    frames: usize,
    data_offset: usize,
    data_len: usize,
}

fn be_u32(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_be_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn parse(input: &[u8]) -> Result<Format> {
    parse_sized(input, input.len())
}

/// See [`crate::wav::layout`].
pub(crate) fn layout(head: &[u8], file_len: usize) -> Result<(AudioInfo, usize, usize, usize)> {
    let format = parse_sized(head, file_len)?;
    let align = format.data_len.checked_div(format.frames).unwrap_or(1).max(1);
    Ok((
        AudioInfo {
            sample_rate: format.sample_rate,
            channels: format.channels,
            frames: format.frames as u64,
            format: AudioFormat::Au,
        },
        format.data_offset,
        format.data_len,
        align,
    ))
}

fn parse_sized(input: &[u8], file_len: usize) -> Result<Format> {
    if input.len() < 24 || &input[..4] != b".snd" {
        return Err(AudioError::UnknownFormat);
    }
    let data_offset = be_u32(input, 4)? as usize;
    let data_size = be_u32(input, 8)?;
    let encoding = be_u32(input, 12)?;
    let sample_rate = be_u32(input, 16)?;
    let channels = be_u32(input, 20)?;
    if channels == 0 || channels > 32 || sample_rate == 0 {
        return Err(AudioError::InvalidData("bad AU header"));
    }
    if data_offset < 24 || data_offset > input.len() {
        return Err(AudioError::InvalidData("bad AU data offset"));
    }
    let available = file_len - data_offset;
    let data_len = if data_size == u32::MAX {
        available
    } else {
        (data_size as usize).min(available)
    };
    let bytes = match encoding {
        1 | 2 | 27 => 1,
        3 => 2,
        4 => 3,
        5 | 6 => 4,
        7 => 8,
        _ => return Err(AudioError::UnsupportedFeature("AU encoding")),
    };
    let align = bytes * channels as usize;
    let frames = data_len / align;
    Ok(Format {
        encoding,
        channels: channels as u16,
        sample_rate,
        frames,
        data_offset,
        data_len,
    })
}

/// Returns the properties of an AU file.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let format = parse(input)?;
    Ok(AudioInfo {
        sample_rate: format.sample_rate,
        channels: format.channels,
        frames: format.frames as u64,
        format: AudioFormat::Au,
    })
}

/// AU decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes an AU file to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let format = parse(input)?;
    let channels = format.channels as usize;
    let samples = format
        .frames
        .checked_mul(channels)
        .ok_or(AudioError::Overflow)?;
    if out.len() < samples {
        return Err(AudioError::BufferTooSmall {
            required: samples,
            available: out.len(),
        });
    }
    read_frames(input, 0, out)?;
    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: format.sample_rate,
            channels: format.channels,
            frames: format.frames as u64,
            format: AudioFormat::Au,
        },
        frames: format.frames,
    })
}

/// Decodes as many frames as fit in `out`, from frame `start`, and returns how
/// many that was.
pub(crate) fn read_frames(input: &[u8], start: usize, out: &mut [i16]) -> Result<usize> {
    let format = parse(input)?;
    let channels = format.channels as usize;
    let frame_count = format.frames.saturating_sub(start).min(out.len() / channels);
    let bytes = match format.encoding {
        1 | 2 | 27 => 1,
        3 => 2,
        4 => 3,
        5 | 6 => 4,
        _ => 8,
    };
    let align = bytes * channels;
    let data = &input[format.data_offset..format.data_offset + format.data_len];

    for frame in 0..frame_count {
        for channel in 0..channels {
            let index = (start + frame) * align + channel * bytes;
            let sample = &data[index..index + bytes];
            let value = match format.encoding {
                1 => pcm::mulaw_to_i16(sample[0]),
                2 => ((sample[0] as i8) as i16) << 8,
                3 => i16::from_be_bytes([sample[0], sample[1]]),
                4 => pcm::i24_to_i16([sample[0], sample[1], sample[2]], true),
                5 => pcm::i32_to_i16(i32::from_be_bytes([
                    sample[0], sample[1], sample[2], sample[3],
                ])),
                6 => pcm::f32_to_i16(f32::from_bits(u32::from_be_bytes([
                    sample[0], sample[1], sample[2], sample[3],
                ]))),
                7 => pcm::f64_to_i16(f64::from_bits(u64::from_be_bytes([
                    sample[0], sample[1], sample[2], sample[3], sample[4], sample[5], sample[6],
                    sample[7],
                ]))),
                _ => pcm::alaw_to_i16(sample[0]),
            };
            out[frame * channels + channel] = value;
        }
    }

    Ok(frame_count)
}
