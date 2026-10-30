//! AVI (RIFF) demuxing and uncompressed/RLE/MJPEG/H.264 frame decoding.

use crate::bitstream::{fourcc_at, u16_le_at, u32_le_at};
use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::h264::{AnnexBDecoder, AvccDecoder};
use crate::{mjpeg, raw, rle};

#[derive(Clone, Copy, Debug)]
pub(crate) struct AviInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
    pub bottom_up: bool,
}

struct Strf<'a> {
    codec: Codec,
    bottom_up: bool,
    palette: &'a [u8],
    extra: &'a [u8],
}

fn scan(data: &[u8], pos: usize, end: usize) -> Option<([u8; 4], usize, usize, usize)> {
    // Returns (id, payload_start, payload_len, next_chunk_start).
    if pos + 8 > end {
        return None;
    }
    let id = fourcc_at(data, pos).ok()?;
    let size = u32_le_at(data, pos + 4).ok()? as usize;
    let payload = pos + 8;
    let next = payload.checked_add(size)?.checked_add(size & 1)?;
    if next > end {
        return None;
    }
    Some((id, payload, size, next))
}

fn find_chunk(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = start;
    while let Some((id, payload, size, next)) = scan(data, pos, end) {
        if &id == want {
            return Some((payload, size));
        }
        pos = next;
    }
    None
}

fn find_list(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = start;
    while let Some((id, payload, size, next)) = scan(data, pos, end) {
        if &id == b"LIST" && size >= 4 {
            let ty = fourcc_at(data, payload).ok()?;
            if &ty == want {
                return Some((payload + 4, size - 4));
            }
        }
        pos = next;
    }
    None
}

fn parse_strf(data: &[u8], payload: usize, size: usize) -> Result<Strf<'_>> {
    if size < 40 {
        return Err(VideoError::InvalidData("AVI BITMAPINFOHEADER"));
    }
    let bi_size = u32_le_at(data, payload)? as usize;
    let compression = fourcc_at(data, payload + 16)?;
    let bit_count = u16_le_at(data, payload + 14)?;
    let raw_codec = match compression {
        [0, 0, 0, 0] | [3, 0, 0, 0] => match bit_count {
            32 => Codec::RawBgra,
            24 => Codec::RawBgr,
            8 => Codec::RawGray,
            _ => Codec::Unknown(u32::from_be_bytes(compression)),
        },
        [1, 0, 0, 0] => Codec::Rle8,
        [2, 0, 0, 0] => Codec::Rle4,
        _ => Codec::from_avi(compression),
    };
    let palette_start = payload + bi_size.max(40).min(size);
    let palette = &data[palette_start.min(payload + size)..payload + size];
    let extra_start = payload + bi_size.min(size);
    let extra = if extra_start < payload + size {
        &data[extra_start..payload + size]
    } else {
        &data[0..0]
    };
    Ok(Strf {
        codec: raw_codec,
        bottom_up: (i32::from_le_bytes([
            data[payload + 8],
            data[payload + 9],
            data[payload + 10],
            data[payload + 11],
        ])) > 0,
        palette,
        extra,
    })
}

pub(crate) fn info(input: &[u8]) -> Result<AviInfo> {
    if input.len() < 12 || &input[..4] != b"RIFF" || &input[8..12] != b"AVI " {
        return Err(VideoError::UnknownFormat);
    }
    let end = input.len().min(u32_le_at(input, 4)? as usize + 8);
    let (hdrl_start, hdrl_len) =
        find_list(input, 12, end, b"hdrl").ok_or(VideoError::InvalidData("AVI hdrl"))?;
    let hdrl_end = hdrl_start + hdrl_len;

    let (avih, avih_len) = find_chunk(input, hdrl_start, hdrl_end, b"avih")
        .ok_or(VideoError::InvalidData("AVI avih"))?;
    if avih_len < 40 {
        return Err(VideoError::InvalidData("AVI avih size"));
    }
    let total_frames = u32_le_at(input, avih + 16)?;
    let width = u32_le_at(input, avih + 32)?;
    let height = u32_le_at(input, avih + 36)?;

    let mut pos = hdrl_start;
    let mut selected = None;
    while let Some((id, payload, size, next)) = scan(input, pos, hdrl_end) {
        if &id == b"LIST" && size >= 4 && &input[payload..payload + 4] == b"strl" {
            let s_start = payload + 4;
            let s_end = payload + size;
            if let (Some((strh, _)), Some((strf, strf_len))) = (
                find_chunk(input, s_start, s_end, b"strh"),
                find_chunk(input, s_start, s_end, b"strf"),
            ) {
                if strh + 4 <= input.len() && &input[strh..strh + 4] == b"vids" {
                    let handler = fourcc_at(input, strh + 4).unwrap_or([0; 4]);
                    let scale = u32_le_at(input, strh + 20).unwrap_or(0);
                    let rate = u32_le_at(input, strh + 24).unwrap_or(0);
                    let strf = parse_strf(input, strf, strf_len)?;
                    let codec = if strf.codec == Codec::Unknown(0) {
                        Codec::from_avi(handler)
                    } else {
                        strf.codec
                    };
                    selected = Some((codec, strf.bottom_up, strf.palette, strf.extra, scale, rate));
                    break;
                }
            }
        }
        pos = next;
    }
    let (codec, bottom_up, _palette, _extra, scale, rate) =
        selected.ok_or(VideoError::InvalidData("AVI video stream"))?;

    let mut frame_count = total_frames;
    if frame_count == 0 {
        if let Some((movi_start, movi_len)) = find_list(input, 12, end, b"movi") {
            frame_count = count_video_chunks(input, movi_start, movi_start + movi_len);
        }
    }
    let frame_rate = if scale != 0 && rate != 0 {
        crate::reduce_fraction(rate, scale)
    } else {
        None
    };
    Ok(AviInfo {
        width,
        height,
        codec,
        frame_count,
        frame_rate,
        bottom_up,
    })
}

fn count_video_chunks(data: &[u8], start: usize, end: usize) -> u32 {
    let mut pos = start;
    let mut count = 0u32;
    while let Some((id, _payload, _size, next)) = scan(data, pos, end) {
        if id[2] == b'd' && (id[3] == b'c' || id[3] == b'b') {
            count = count.saturating_add(1);
        }
        pos = next;
    }
    count
}

fn for_each_video_chunk<'a, F>(data: &'a [u8], start: usize, end: usize, mut f: F) -> Result<()>
where
    F: FnMut(u32, &'a [u8]) -> Result<bool>,
{
    let mut pos = start;
    let mut index = 0u32;
    while let Some((id, payload, size, next)) = scan(data, pos, end) {
        if id[2] == b'd' && (id[3] == b'c' || id[3] == b'b') {
            let sample = &data[payload..payload + size];
            if f(index, sample)? {
                return Ok(());
            }
            index = index.saturating_add(1);
        }
        pos = next;
    }
    Ok(())
}

pub(crate) fn decode_frame(input: &[u8], index: u32, out: &mut [u8]) -> Result<u32> {
    let info = info(input)?;
    if index >= info.frame_count {
        return Err(VideoError::FrameOutOfRange {
            index,
            count: info.frame_count,
        });
    }
    let end = input.len().min(u32_le_at(input, 4)? as usize + 8);
    let (movi_start, movi_len) =
        find_list(input, 12, end, b"movi").ok_or(VideoError::InvalidData("AVI movi"))?;
    let movi_end = movi_start + movi_len;
    require_rgba(out, info.width, info.height)?;

    // Locate the selected stream's header again for palette/extradata.
    let strf = find_strf(input, end);
    let palette = strf.as_ref().map(|s| s.palette).unwrap_or(&[]);
    let extra = strf.as_ref().map(|s| s.extra).unwrap_or(&[]);

    if info.codec == Codec::H264 {
        let mut annex_b = AnnexBDecoder::new(index);
        let mut avcc = if extra.len() > 7 && extra[0] == 1 {
            Some(AvccDecoder::new(extra, index)?)
        } else {
            None
        };
        let mut found = false;
        for_each_video_chunk(input, movi_start, movi_end, |_, sample| {
            let hit = if let Some(decoder) = avcc.as_mut() {
                decoder.push(sample, out)?
            } else {
                annex_b.push(sample, out)?
            };
            if hit {
                found = true;
                return Ok(true);
            }
            Ok(false)
        })?;
        if !found {
            let hit = if let Some(decoder) = avcc.as_mut() {
                decoder.finish(out)?
            } else {
                annex_b.finish(out)?
            };
            if !hit {
                return Err(VideoError::FrameOutOfRange {
                    index,
                    count: info.frame_count,
                });
            }
        }
        return Ok(0);
    }

    let mut sample = None;
    for_each_video_chunk(input, movi_start, movi_end, |i, data| {
        if i == index {
            sample = Some(data);
            return Ok(true);
        }
        Ok(false)
    })?;
    let sample = sample.ok_or(VideoError::FrameOutOfRange {
        index,
        count: info.frame_count,
    })?;

    match info.codec {
        Codec::Mjpeg => mjpeg::decode_sample(sample, out)?,
        Codec::Rle8 => rle::decode_rle8(sample, info.width, info.height, palette, out)?,
        Codec::Rle4 => rle::decode_rle4(sample, info.width, info.height, palette, out)?,
        codec if codec.pixel_format().is_some() => {
            raw::decode(sample, codec, info.width, info.height, info.bottom_up, out)?
        }
        codec => return Err(VideoError::UnsupportedCodec(codec.name())),
    }
    Ok(0)
}

fn find_strf<'a>(data: &'a [u8], end: usize) -> Option<Strf<'a>> {
    let (hdrl_start, hdrl_len) = find_list(data, 12, end, b"hdrl")?;
    let hdrl_end = hdrl_start + hdrl_len;
    let mut pos = hdrl_start;
    while let Some((id, payload, size, next)) = scan(data, pos, hdrl_end) {
        if &id == b"LIST" && size >= 4 && &data[payload..payload + 4] == b"strl" {
            let s_start = payload + 4;
            let s_end = payload + size;
            if let (Some((strh, _)), Some((strf, strf_len))) = (
                find_chunk(data, s_start, s_end, b"strh"),
                find_chunk(data, s_start, s_end, b"strf"),
            ) {
                if strh + 4 <= data.len() && &data[strh..strh + 4] == b"vids" {
                    return parse_strf(data, strf, strf_len).ok();
                }
            }
        }
        pos = next;
    }
    None
}
