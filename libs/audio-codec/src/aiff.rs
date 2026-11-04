//! AIFF / AIFF-C PCM decoding and PCM16 encoding.

use crate::error::{need, AudioError, Result};
use crate::pcm;
use crate::{AudioFormat, AudioInfo, DecodedAudio};

#[derive(Clone, Copy)]
enum Endian {
    Big,
    Little,
}

struct Format {
    channels: u16,
    sample_rate: u32,
    bits: u16,
    frames: usize,
    data_offset: usize,
    data_len: usize,
    endian: Endian,
}

fn be_u16(input: &[u8], offset: usize) -> Result<u16> {
    need(input.len(), offset, 2)?;
    Ok(u16::from_be_bytes([input[offset], input[offset + 1]]))
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

fn extended_rate(bytes: &[u8]) -> u32 {
    if bytes.len() < 10 {
        return 0;
    }
    let exponent = u16::from_be_bytes([bytes[0], bytes[1]]) & 0x7fff;
    let mantissa = u64::from_be_bytes([
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
    ]);
    if exponent == 0 || mantissa == 0 {
        return 0;
    }
    let shift = exponent as i32 - 16383 - 63;
    let value = if shift >= 0 {
        if shift >= 63 {
            u64::MAX
        } else {
            mantissa << shift
        }
    } else {
        let right = (-shift) as u32;
        if right >= 64 {
            0
        } else {
            mantissa >> right
        }
    };
    value.min(u32::MAX as u64) as u32
}

fn parse(input: &[u8]) -> Result<Format> {
    if input.len() < 12 || &input[..4] != b"FORM" {
        return Err(AudioError::UnknownFormat);
    }
    let kind = &input[8..12];
    if kind != b"AIFF" && kind != b"AIFC" {
        return Err(AudioError::UnknownFormat);
    }
    let aifc = kind == b"AIFC";
    let mut comm: Option<(u16, u32, u16, u32)> = None;
    let mut endian = Endian::Big;
    let mut ssnd: Option<(usize, usize)> = None;

    let mut pos = 12usize;
    while pos + 8 <= input.len() {
        let id = &input[pos..pos + 4];
        let size = be_u32(input, pos + 4)? as usize;
        let start = pos + 8;
        let end = start
            .checked_add(size)
            .unwrap_or(input.len())
            .min(input.len());
        if id == b"COMM" {
            need(input.len(), start, 18)?;
            let channels = be_u16(input, start)?;
            let frames = be_u32(input, start + 2)?;
            let bits = be_u16(input, start + 6)?;
            let rate = extended_rate(&input[start + 8..start + 18]);
            if aifc {
                if size < 22 {
                    return Err(AudioError::InvalidData("bad AIFC COMM chunk"));
                }
                // `size` is only what the file declares; a truncated file
                // can end inside the compression type.
                need(input.len(), start, 22)?;
                let compression = &input[start + 18..start + 22];
                match compression {
                    b"NONE" | b"twos" | b"in16" => endian = Endian::Big,
                    b"sowt" => endian = Endian::Little,
                    _ => return Err(AudioError::UnsupportedFeature("AIFC compression")),
                }
            }
            comm = Some((channels, frames, bits, rate));
        } else if id == b"SSND" {
            need(input.len(), start, 8)?;
            let offset = be_u32(input, start)? as usize;
            let data_start = start
                .checked_add(8)
                .and_then(|v| v.checked_add(offset))
                .ok_or(AudioError::Overflow)?;
            if data_start > input.len() {
                return Err(AudioError::UnexpectedEnd);
            }
            let data_len = size
                .saturating_sub(8 + offset)
                .min(input.len() - data_start);
            ssnd = Some((data_start, data_len));
        }
        pos = end + (size & 1);
    }

    let (channels, declared_frames, bits, sample_rate) =
        comm.ok_or(AudioError::InvalidData("AIFF file has no COMM chunk"))?;
    let (data_offset, data_len) =
        ssnd.ok_or(AudioError::InvalidData("AIFF file has no SSND chunk"))?;
    if channels == 0 || sample_rate == 0 || !matches!(bits, 8 | 16 | 24 | 32) {
        return Err(AudioError::UnsupportedFeature("AIFF sample format"));
    }
    let bytes = ((bits as usize) + 7) / 8;
    let align = bytes * channels as usize;
    let available = data_len / align;
    let frames = (declared_frames as usize).min(available);

    Ok(Format {
        channels,
        sample_rate,
        bits,
        frames,
        data_offset,
        data_len,
        endian,
    })
}

/// Returns the properties of an AIFF file.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let format = parse(input)?;
    Ok(AudioInfo {
        sample_rate: format.sample_rate,
        channels: format.channels,
        frames: format.frames as u64,
        format: AudioFormat::Aiff,
    })
}

/// AIFF decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes an AIFF file to interleaved signed 16-bit samples.
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
    let bytes = ((format.bits as usize) + 7) / 8;
    let align = bytes * channels;
    let data = &input[format.data_offset..format.data_offset + format.data_len];

    for frame in 0..format.frames {
        for channel in 0..channels {
            let index = frame * align + channel * bytes;
            let sample = &data[index..index + bytes];
            let value = match (format.bits, format.endian) {
                (8, _) => ((sample[0] as i8) as i16) << 8,
                (16, Endian::Big) => i16::from_be_bytes([sample[0], sample[1]]),
                (16, Endian::Little) => i16::from_le_bytes([sample[0], sample[1]]),
                (24, Endian::Big) => pcm::i24_to_i16([sample[0], sample[1], sample[2]], true),
                (24, Endian::Little) => pcm::i24_to_i16([sample[0], sample[1], sample[2]], false),
                (32, Endian::Big) => pcm::i32_to_i16(i32::from_be_bytes([
                    sample[0], sample[1], sample[2], sample[3],
                ])),
                (32, Endian::Little) => pcm::i32_to_i16(i32::from_le_bytes([
                    sample[0], sample[1], sample[2], sample[3],
                ])),
                _ => return Err(AudioError::UnsupportedFeature("AIFF bit depth")),
            };
            out[frame * channels + channel] = value;
        }
    }

    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: format.sample_rate,
            channels: format.channels,
            frames: format.frames as u64,
            format: AudioFormat::Aiff,
        },
        frames: format.frames,
    })
}

fn write_extended_rate(rate: u32, out: &mut [u8]) {
    if rate == 0 {
        out[..10].fill(0);
        return;
    }
    let highest = 31 - rate.leading_zeros();
    let exponent = (16383 + highest) as u16;
    let mantissa = (rate as u64) << (63 - highest);
    out[0..2].copy_from_slice(&exponent.to_be_bytes());
    out[2..10].copy_from_slice(&mantissa.to_be_bytes());
}

/// Encodes interleaved signed 16-bit samples as an AIFF file.
pub fn encode_i16(
    sample_rate: u32,
    channels: u16,
    samples: &[i16],
    out: &mut [u8],
) -> Result<usize> {
    if sample_rate == 0 || channels == 0 || channels > 32 {
        return Err(AudioError::InvalidData("bad AIFF parameters"));
    }
    let channels = channels as usize;
    if samples.len() % channels != 0 {
        return Err(AudioError::InvalidData("sample count is not frame aligned"));
    }
    let data_len = samples.len().checked_mul(2).ok_or(AudioError::Overflow)?;
    let total = 54usize
        .checked_add(data_len)
        .and_then(|n| n.checked_add(data_len & 1))
        .ok_or(AudioError::Overflow)?;
    if out.len() < total {
        return Err(AudioError::BufferTooSmall {
            required: total,
            available: out.len(),
        });
    }
    let frames = samples.len() / channels;

    out[..4].copy_from_slice(b"FORM");
    out[4..8].copy_from_slice(&((total - 8) as u32).to_be_bytes());
    out[8..12].copy_from_slice(b"AIFF");
    out[12..16].copy_from_slice(b"COMM");
    out[16..20].copy_from_slice(&18u32.to_be_bytes());
    out[20..22].copy_from_slice(&(channels as u16).to_be_bytes());
    out[22..26].copy_from_slice(&(frames as u32).to_be_bytes());
    out[26..28].copy_from_slice(&16u16.to_be_bytes());
    write_extended_rate(sample_rate, &mut out[28..38]);
    out[38..42].copy_from_slice(b"SSND");
    out[42..46].copy_from_slice(&((data_len + 8) as u32).to_be_bytes());
    out[46..50].copy_from_slice(&0u32.to_be_bytes());
    out[50..54].copy_from_slice(&0u32.to_be_bytes());
    for (chunk, sample) in out[54..54 + data_len].chunks_exact_mut(2).zip(samples) {
        chunk.copy_from_slice(&sample.to_be_bytes());
    }
    if data_len & 1 != 0 {
        out[54 + data_len] = 0;
    }
    Ok(total)
}
