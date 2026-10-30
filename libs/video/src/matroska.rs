//! Matroska / WebM (EBML) demuxing.
//!
//! Supports the elements needed to find a video track, read its dimensions and
//! codec id, count frames, and walk SimpleBlock/BlockGroup samples. Lacing is
//! expanded so one block may yield several video frames.

use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::h264::{AnnexBDecoder, AvccDecoder};
use crate::{mjpeg, raw};

const ID_SEGMENT: u32 = 0x1853_8067;
const ID_TRACKS: u32 = 0x1654_ae6b;
const ID_CLUSTER: u32 = 0x1f43_b675;
const ID_TRACK_ENTRY: u32 = 0xae;
const ID_TRACK_NUMBER: u32 = 0xd7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63a2;
const ID_DEFAULT_DURATION: u32 = 0x23e3_83;
const ID_VIDEO: u32 = 0xe0;
const ID_PIXEL_WIDTH: u32 = 0xb0;
const ID_PIXEL_HEIGHT: u32 = 0xba;
const ID_TIMESTAMP_SCALE: u32 = 0x2ad7_b1;
const ID_SIMPLE_BLOCK: u32 = 0xa3;
const ID_BLOCK_GROUP: u32 = 0xa0;
const ID_BLOCK: u32 = 0xa1;

#[derive(Clone, Copy, Debug)]
pub(crate) struct MatroskaInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
}

#[derive(Clone, Copy)]
struct Elem {
    id: u32,
    payload: usize,
    end: usize,
    next: usize,
}

fn id_len(first: u8) -> Option<usize> {
    if first == 0 {
        return None;
    }
    Some(first.leading_zeros() as usize + 1)
}

fn read_id(data: &[u8], pos: usize, end: usize) -> Result<(u32, usize)> {
    let first = *data.get(pos).ok_or(VideoError::UnexpectedEnd)?;
    let len = id_len(first).ok_or(VideoError::InvalidData("EBML id"))?;
    if pos + len > end {
        return Err(VideoError::UnexpectedEnd);
    }
    let mut value = 0u32;
    for i in 0..len {
        value = (value << 8) | data[pos + i] as u32;
    }
    Ok((value, len))
}

fn read_size(data: &[u8], pos: usize, end: usize) -> Result<(u64, usize, bool)> {
    let first = *data.get(pos).ok_or(VideoError::UnexpectedEnd)?;
    let len = id_len(first).ok_or(VideoError::InvalidData("EBML size"))?;
    if pos + len > end {
        return Err(VideoError::UnexpectedEnd);
    }
    let mask = 0xffu16 >> len;
    let mut value = (first as u16 & mask) as u64;
    let mut unknown = (first as u16 & mask) == mask;
    for i in 1..len {
        value = (value << 8) | data[pos + i] as u64;
        if data[pos + i] != 0xff {
            unknown = false;
        }
    }
    Ok((value, len, unknown))
}

fn next_elem(data: &[u8], pos: usize, end: usize) -> Result<Option<Elem>> {
    if pos >= end {
        return Ok(None);
    }
    let (id, id_len) = read_id(data, pos, end)?;
    let (size, size_len, unknown) = read_size(data, pos + id_len, end)?;
    let payload = pos + id_len + size_len;
    let elem_end = if unknown {
        end
    } else {
        payload
            .checked_add(size as usize)
            .ok_or(VideoError::Overflow)?
    };
    if elem_end > end || payload > elem_end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok(Some(Elem {
        id,
        payload,
        end: elem_end,
        next: elem_end,
    }))
}

fn find_elem(data: &[u8], start: usize, end: usize, id: u32) -> Result<Option<Elem>> {
    let mut pos = start;
    while let Some(elem) = next_elem(data, pos, end)? {
        if elem.id == id {
            return Ok(Some(elem));
        }
        pos = elem.next;
    }
    Ok(None)
}

fn read_uint(data: &[u8], elem: Elem) -> Result<u64> {
    let len = elem.end - elem.payload;
    if len > 8 {
        return Err(VideoError::InvalidData("EBML integer"));
    }
    let mut value = 0u64;
    for i in 0..len {
        value = (value << 8) | data[elem.payload + i] as u64;
    }
    Ok(value)
}

fn read_str<'a>(data: &'a [u8], elem: Elem) -> &'a [u8] {
    let mut slice = &data[elem.payload..elem.end];
    while let [rest @ .., 0] = slice {
        slice = rest;
    }
    slice
}

fn segment(input: &[u8]) -> Result<Elem> {
    let mut pos = 0usize;
    while let Some(elem) = next_elem(input, pos, input.len())? {
        if elem.id == ID_SEGMENT {
            return Ok(elem);
        }
        pos = elem.next;
    }
    Err(VideoError::UnknownFormat)
}

struct Track<'a> {
    number: u64,
    width: u32,
    height: u32,
    codec: Codec,
    config: &'a [u8],
    default_duration: u64,
}

fn video_track<'a>(input: &'a [u8], seg: Elem) -> Result<Track<'a>> {
    let tracks = find_elem(input, seg.payload, seg.end, ID_TRACKS)?
        .ok_or(VideoError::InvalidData("Matroska tracks"))?;
    let mut pos = tracks.payload;
    while let Some(entry) = next_elem(input, pos, tracks.end)? {
        pos = entry.next;
        if entry.id != ID_TRACK_ENTRY {
            continue;
        }
        let track_type = find_elem(input, entry.payload, entry.end, ID_TRACK_TYPE)?
            .map(|e| read_uint(input, e))
            .transpose()?
            .unwrap_or(0);
        if track_type != 1 {
            continue;
        }
        let number = find_elem(input, entry.payload, entry.end, ID_TRACK_NUMBER)?
            .map(|e| read_uint(input, e))
            .transpose()?
            .unwrap_or(0);
        let codec_id = find_elem(input, entry.payload, entry.end, ID_CODEC_ID)?
            .map(|e| read_str(input, e))
            .unwrap_or(&[]);
        let config = find_elem(input, entry.payload, entry.end, ID_CODEC_PRIVATE)?
            .map(|e| &input[e.payload..e.end])
            .unwrap_or(&[]);
        let default_duration = find_elem(input, entry.payload, entry.end, ID_DEFAULT_DURATION)?
            .map(|e| read_uint(input, e))
            .transpose()?
            .unwrap_or(0);
        let mut codec = Codec::from_matroska(codec_id);
        if matches!(codec, Codec::Unknown(_))
            && codec_id == b"V_MS/VFW/FOURCC"
            && config.len() >= 20
        {
            let compression = [config[16], config[17], config[18], config[19]];
            codec = Codec::from_avi(compression);
        }
        let mut width = 0u32;
        let mut height = 0u32;
        if let Some(video) = find_elem(input, entry.payload, entry.end, ID_VIDEO)? {
            if let Some(w) = find_elem(input, video.payload, video.end, ID_PIXEL_WIDTH)? {
                width = read_uint(input, w)? as u32;
            }
            if let Some(h) = find_elem(input, video.payload, video.end, ID_PIXEL_HEIGHT)? {
                height = read_uint(input, h)? as u32;
            }
        }
        return Ok(Track {
            number,
            width,
            height,
            codec,
            config,
            default_duration,
        });
    }
    Err(VideoError::InvalidData("Matroska has no video track"))
}

fn lacing_signed(data: &[u8], pos: usize, end: usize) -> Result<(i64, usize)> {
    let first = *data.get(pos).ok_or(VideoError::UnexpectedEnd)?;
    let len = id_len(first).ok_or(VideoError::InvalidData("EBML lacing"))?;
    if pos + len > end {
        return Err(VideoError::UnexpectedEnd);
    }
    let mask = 0xffu16 >> len;
    let mut value = (first as u16 & mask) as u64;
    for i in 1..len {
        value = (value << 8) | data[pos + i] as u64;
    }
    let bias = (1i64 << (7 * len as i32 - 1)) - 1;
    Ok((value as i64 - bias, len))
}

fn visit_block<'a, F>(data: &'a [u8], block: Elem, track: u64, mut f: F) -> Result<()>
where
    F: FnMut(&'a [u8]) -> Result<bool>,
{
    let mut pos = block.payload;
    let (block_track, track_len, _) = read_size(data, pos, block.end)?;
    pos += track_len;
    if block_track != track {
        return Ok(());
    }
    if pos + 3 > block.end {
        return Err(VideoError::UnexpectedEnd);
    }
    pos += 2; // timecode
    let flags = data[pos];
    pos += 1;
    let lacing = (flags >> 1) & 0x03;
    if lacing == 0 {
        if pos < block.end {
            f(&data[pos..block.end])?;
        }
        return Ok(());
    }

    let frames = data.get(pos).copied().ok_or(VideoError::UnexpectedEnd)? as usize + 1;
    pos += 1;
    if frames > 256 {
        return Err(VideoError::InvalidData("Matroska lacing count"));
    }
    let mut sizes = [0usize; 256];
    match lacing {
        1 => {
            // Xiph lacing: each size is a sequence of 255-continuation bytes.
            let mut total = 0usize;
            for size in sizes.iter_mut().take(frames - 1) {
                let mut value = 0usize;
                loop {
                    let byte = data.get(pos).copied().ok_or(VideoError::UnexpectedEnd)?;
                    pos += 1;
                    value = value
                        .checked_add(byte as usize)
                        .ok_or(VideoError::Overflow)?;
                    if byte != 255 {
                        break;
                    }
                }
                *size = value;
                total = total.checked_add(value).ok_or(VideoError::Overflow)?;
            }
            let remaining = block
                .end
                .checked_sub(pos)
                .ok_or(VideoError::UnexpectedEnd)?;
            if total > remaining {
                return Err(VideoError::InvalidData("Matroska lacing size"));
            }
            sizes[frames - 1] = remaining - total;
        }
        2 => {
            let remaining = block
                .end
                .checked_sub(pos)
                .ok_or(VideoError::UnexpectedEnd)?;
            if remaining % frames != 0 {
                return Err(VideoError::InvalidData("Matroska fixed lacing"));
            }
            for size in sizes.iter_mut().take(frames) {
                *size = remaining / frames;
            }
        }
        3 => {
            let (first, len, _) = read_size(data, pos, block.end)?;
            pos += len;
            let mut previous = first as usize;
            sizes[0] = previous;
            for size in sizes.iter_mut().take(frames).skip(1) {
                let (diff, len) = lacing_signed(data, pos, block.end)?;
                pos += len;
                let next = previous as i64 + diff;
                if next < 0 {
                    return Err(VideoError::InvalidData("Matroska EBML lacing"));
                }
                previous = next as usize;
                *size = previous;
            }
        }
        _ => unreachable!(),
    }

    for &size in sizes.iter().take(frames) {
        let end = pos.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > block.end {
            return Err(VideoError::UnexpectedEnd);
        }
        if f(&data[pos..end])? {
            return Ok(());
        }
        pos = end;
    }
    Ok(())
}

fn visit_cluster_blocks<'a, F>(input: &'a [u8], seg: Elem, track: u64, mut f: F) -> Result<()>
where
    F: FnMut(&'a [u8]) -> Result<bool>,
{
    let mut pos = seg.payload;
    while let Some(elem) = next_elem(input, pos, seg.end)? {
        pos = elem.next;
        if elem.id != ID_CLUSTER {
            continue;
        }
        let mut cpos = elem.payload;
        while let Some(child) = next_elem(input, cpos, elem.end)? {
            cpos = child.next;
            match child.id {
                ID_SIMPLE_BLOCK => {
                    let mut hit = false;
                    visit_block(input, child, track, |sample| {
                        if f(sample)? {
                            hit = true;
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    })?;
                    if hit {
                        return Ok(());
                    }
                }
                ID_BLOCK_GROUP => {
                    if let Some(block) = find_elem(input, child.payload, child.end, ID_BLOCK)? {
                        let mut hit = false;
                        visit_block(input, block, track, |sample| {
                            if f(sample)? {
                                hit = true;
                                Ok(true)
                            } else {
                                Ok(false)
                            }
                        })?;
                        if hit {
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn count_frames(input: &[u8], seg: Elem, track: u64) -> Result<u32> {
    let mut count = 0u32;
    visit_cluster_blocks(input, seg, track, |_| {
        count = count.saturating_add(1);
        Ok(false)
    })?;
    Ok(count)
}

pub(crate) fn info(input: &[u8]) -> Result<MatroskaInfo> {
    let seg = segment(input)?;
    let track = video_track(input, seg)?;
    let frame_count = count_frames(input, seg, track.number)?;
    let frame_rate = if track.default_duration != 0 && track.default_duration <= u32::MAX as u64 {
        crate::reduce_fraction(1_000_000_000, track.default_duration as u32)
    } else {
        None
    };
    Ok(MatroskaInfo {
        width: track.width,
        height: track.height,
        codec: track.codec,
        frame_count,
        frame_rate,
    })
}

pub(crate) fn decode_frame(input: &[u8], index: u32, out: &mut [u8]) -> Result<u32> {
    let seg = segment(input)?;
    let track = video_track(input, seg)?;
    let count = count_frames(input, seg, track.number)?;
    if index >= count {
        return Err(VideoError::FrameOutOfRange { index, count });
    }
    require_rgba(out, track.width, track.height)?;

    match track.codec {
        Codec::H264 => {
            let mut avcc = if track.config.len() > 7 && track.config[0] == 1 {
                Some(AvccDecoder::new(track.config, index)?)
            } else {
                None
            };
            let mut annex_b = AnnexBDecoder::new(index);
            let mut done = false;
            visit_cluster_blocks(input, seg, track.number, |sample| {
                let hit = if let Some(decoder) = avcc.as_mut() {
                    decoder.push(sample, out)?
                } else {
                    annex_b.push(sample, out)?
                };
                if hit {
                    done = true;
                }
                Ok(hit)
            })?;
            if !done {
                done = if let Some(decoder) = avcc.as_mut() {
                    decoder.finish(out)?
                } else {
                    annex_b.finish(out)?
                };
            }
            if !done {
                return Err(VideoError::FrameOutOfRange { index, count });
            }
            Ok(0)
        }
        Codec::Mjpeg
        | Codec::RawRgb
        | Codec::RawBgr
        | Codec::RawBgra
        | Codec::RawRgba
        | Codec::RawGray
        | Codec::RawYuv420
        | Codec::RawYuv422
        | Codec::RawYuv444
        | Codec::RawYuy2
        | Codec::RawUyvy => {
            let mut sample = None;
            let mut seen = 0u32;
            visit_cluster_blocks(input, seg, track.number, |data| {
                if seen == index {
                    sample = Some(data);
                    return Ok(true);
                }
                seen = seen.saturating_add(1);
                Ok(false)
            })?;
            let sample = sample.ok_or(VideoError::UnexpectedEnd)?;
            match track.codec {
                Codec::Mjpeg => mjpeg::decode_sample(sample, out)?,
                codec => raw::decode(sample, codec, track.width, track.height, false, out)?,
            }
            Ok(0)
        }
        codec => Err(VideoError::UnsupportedCodec(codec.name())),
    }
}
