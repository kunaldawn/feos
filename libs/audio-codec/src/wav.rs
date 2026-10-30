//! RIFF/WAVE decoding and PCM16 encoding.

use crate::error::{need, AudioError, Result};
use crate::pcm;
use crate::{AudioFormat, AudioInfo, DecodedAudio};

#[derive(Clone, Copy)]
struct Format {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    block_align: usize,
    bits: u16,
    data_offset: usize,
    data_len: usize,
}

fn le_u16(input: &[u8], offset: usize) -> Result<u16> {
    need(input.len(), offset, 2)?;
    Ok(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn le_u32(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn parse(input: &[u8]) -> Result<Format> {
    if input.len() < 12 || &input[..4] != b"RIFF" || &input[8..12] != b"WAVE" {
        return Err(AudioError::UnknownFormat);
    }
    let mut format: Option<(u16, u16, u32, usize, u16)> = None;
    let mut data = None;
    let mut pos = 12usize;
    while pos + 8 <= input.len() {
        let id = &input[pos..pos + 4];
        let size = le_u32(input, pos + 4)? as usize;
        let start = pos + 8;
        let end = start
            .checked_add(size)
            .unwrap_or(input.len())
            .min(input.len());
        if id == b"fmt " {
            need(input.len(), start, 16)?;
            let mut tag = le_u16(input, start)?;
            let channels = le_u16(input, start + 2)?;
            let sample_rate = le_u32(input, start + 4)?;
            let block_align = le_u16(input, start + 12)? as usize;
            let bits = le_u16(input, start + 14)?;
            if tag == 0xfffe && size >= 40 {
                need(input.len(), start, 26)?;
                tag = le_u16(input, start + 24)?;
            }
            format = Some((tag, channels, sample_rate, block_align, bits));
        } else if id == b"data" {
            data = Some((start, size.min(input.len().saturating_sub(start))));
        }
        pos = end + (size & 1);
    }

    let (tag, channels, sample_rate, block_align, bits) =
        format.ok_or(AudioError::InvalidData("WAVE file has no fmt chunk"))?;
    let (data_offset, data_len) =
        data.ok_or(AudioError::InvalidData("WAVE file has no data chunk"))?;
    if channels == 0 || sample_rate == 0 || block_align == 0 {
        return Err(AudioError::InvalidData("bad WAVE format header"));
    }
    if !matches!(tag, 1 | 3 | 6 | 7) {
        return Err(AudioError::UnsupportedFeature("WAVE audio format"));
    }
    let bytes = ((bits as usize) + 7) / 8;
    if bytes == 0 || bytes * channels as usize != block_align {
        return Err(AudioError::InvalidData("bad WAVE block alignment"));
    }
    if data_len / block_align > u32::MAX as usize {
        return Err(AudioError::Overflow);
    }

    Ok(Format {
        tag,
        channels,
        sample_rate,
        block_align,
        bits,
        data_offset,
        data_len,
    })
}

fn frames(format: &Format) -> usize {
    format.data_len / format.block_align
}

/// Returns the properties of a WAVE file.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let format = parse(input)?;
    Ok(AudioInfo {
        sample_rate: format.sample_rate,
        channels: format.channels,
        frames: frames(&format) as u64,
        format: AudioFormat::Wav,
    })
}

/// WAVE decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes a WAVE file to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let format = parse(input)?;
    let frame_count = frames(&format);
    let samples = frame_count
        .checked_mul(format.channels as usize)
        .ok_or(AudioError::Overflow)?;
    if out.len() < samples {
        return Err(AudioError::BufferTooSmall {
            required: samples,
            available: out.len(),
        });
    }
    let data = &input[format.data_offset..format.data_offset + format.data_len];
    let channels = format.channels as usize;
    let bytes = format.block_align / channels;

    for frame in 0..frame_count {
        for channel in 0..channels {
            let index = frame * format.block_align + channel * bytes;
            let sample = &data[index..index + bytes];
            let value = match format.tag {
                1 => match format.bits {
                    8 => pcm::u8_to_i16(sample[0]),
                    16 => i16::from_le_bytes([sample[0], sample[1]]),
                    24 => pcm::i24_to_i16([sample[0], sample[1], sample[2]], false),
                    32 => pcm::i32_to_i16(i32::from_le_bytes([
                        sample[0], sample[1], sample[2], sample[3],
                    ])),
                    _ => return Err(AudioError::UnsupportedFeature("WAVE PCM bit depth")),
                },
                3 => match format.bits {
                    32 => pcm::f32_to_i16(pcm::f32_from_be([
                        sample[3], sample[2], sample[1], sample[0],
                    ])),
                    64 => pcm::f64_to_i16(f64::from_bits(u64::from_le_bytes([
                        sample[0], sample[1], sample[2], sample[3], sample[4], sample[5],
                        sample[6], sample[7],
                    ]))),
                    _ => return Err(AudioError::UnsupportedFeature("WAVE float bit depth")),
                },
                6 => pcm::alaw_to_i16(sample[0]),
                _ => pcm::mulaw_to_i16(sample[0]),
            };
            out[frame * channels + channel] = value;
        }
    }

    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: format.sample_rate,
            channels: format.channels,
            frames: frame_count as u64,
            format: AudioFormat::Wav,
        },
        frames: frame_count,
    })
}

/// Encodes interleaved signed 16-bit samples as a PCM WAVE file.
pub fn encode_i16(
    sample_rate: u32,
    channels: u16,
    samples: &[i16],
    out: &mut [u8],
) -> Result<usize> {
    if sample_rate == 0 || channels == 0 || channels > 32 {
        return Err(AudioError::InvalidData("bad WAVE parameters"));
    }
    let channels = channels as usize;
    if samples.len() % channels != 0 {
        return Err(AudioError::InvalidData("sample count is not frame aligned"));
    }
    let data_len = samples.len().checked_mul(2).ok_or(AudioError::Overflow)?;
    let total = 44usize
        .checked_add(data_len)
        .and_then(|n| n.checked_add(data_len & 1))
        .ok_or(AudioError::Overflow)?;
    if out.len() < total {
        return Err(AudioError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }

    out[..4].copy_from_slice(b"RIFF");
    out[4..8].copy_from_slice(&((total - 8) as u32).to_le_bytes());
    out[8..12].copy_from_slice(b"WAVE");
    out[12..16].copy_from_slice(b"fmt ");
    out[16..20].copy_from_slice(&16u32.to_le_bytes());
    out[20..22].copy_from_slice(&1u16.to_le_bytes());
    out[22..24].copy_from_slice(&(channels as u16).to_le_bytes());
    out[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate as usize * channels * 2;
    out[28..32].copy_from_slice(&(byte_rate as u32).to_le_bytes());
    out[32..34].copy_from_slice(&((channels * 2) as u16).to_le_bytes());
    out[34..36].copy_from_slice(&16u16.to_le_bytes());
    out[36..40].copy_from_slice(b"data");
    out[40..44].copy_from_slice(&(data_len as u32).to_le_bytes());
    for (chunk, sample) in out[44..44 + data_len].chunks_exact_mut(2).zip(samples) {
        chunk.copy_from_slice(&sample.to_le_bytes());
    }
    if data_len & 1 != 0 {
        out[44 + data_len] = 0;
    }
    Ok(total)
}
