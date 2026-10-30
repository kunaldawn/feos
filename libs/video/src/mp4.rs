//! ISO Base Media (MP4/MOV) demuxing.
//!
//! The parser walks the box tree without allocating. Sample tables are
//! re-walked for each requested frame, which keeps the crate allocation-free
//! at the cost of O(n) seeking.

use crate::bitstream::{fourcc_at, u16_at, u32_at, u64_at};
use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::h264::{AvccDecoder, H264Info};
use crate::{h264, mjpeg, raw};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Mp4Info {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
}

struct Track<'a> {
    width: u32,
    height: u32,
    codec: Codec,
    frame_count: u32,
    frame_rate: Option<(u32, u32)>,
    config: &'a [u8],
    stbl_start: usize,
    stbl_end: usize,
}

#[derive(Clone, Copy)]
struct BoxHeader {
    kind: [u8; 4],
    payload: usize,
    end: usize,
    next: usize,
}

fn read_box(data: &[u8], pos: usize, end: usize) -> Result<Option<BoxHeader>> {
    if pos + 8 > end {
        return Ok(None);
    }
    let size32 = u32_at(data, pos)? as usize;
    let kind = fourcc_at(data, pos + 4)?;
    let (header, size) = if size32 == 1 {
        if pos + 16 > end {
            return Err(VideoError::UnexpectedEnd);
        }
        (16usize, u64_at(data, pos + 8)? as usize)
    } else if size32 == 0 {
        (8usize, end - pos)
    } else {
        (8usize, size32)
    };
    if size < header {
        return Err(VideoError::InvalidData("MP4 box size"));
    }
    let next = pos.checked_add(size).ok_or(VideoError::Overflow)?;
    if next > end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok(Some(BoxHeader {
        kind,
        payload: pos + header,
        end: next,
        next,
    }))
}

fn find_box<'a>(
    data: &'a [u8],
    start: usize,
    end: usize,
    want: &[u8; 4],
) -> Result<Option<BoxHeader>> {
    let mut pos = start;
    while let Some(header) = read_box(data, pos, end)? {
        if &header.kind == want {
            return Ok(Some(header));
        }
        pos = header.next;
    }
    Ok(None)
}

fn required_box(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Result<BoxHeader> {
    find_box(data, start, end, want)?.ok_or(VideoError::InvalidData("MP4 missing box"))
}

fn children(data: &[u8], header: BoxHeader, want: &[u8; 4]) -> Result<Option<BoxHeader>> {
    find_box(data, header.payload, header.end, want)
}

fn mvhd_timescale(data: &[u8], header: BoxHeader) -> Result<u32> {
    if header.payload + 20 > header.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let version = data[header.payload];
    if version == 1 {
        u32_at(data, header.payload + 20)
    } else {
        u32_at(data, header.payload + 12)
    }
}

fn mdhd_timescale(data: &[u8], header: BoxHeader) -> Result<(u32, u64)> {
    if header.payload + 20 > header.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let version = data[header.payload];
    if version == 1 {
        let timescale = u32_at(data, header.payload + 20)?;
        let duration = u64_at(data, header.payload + 24)?;
        Ok((timescale, duration))
    } else {
        let timescale = u32_at(data, header.payload + 12)?;
        let duration = u32_at(data, header.payload + 16)? as u64;
        Ok((timescale, duration))
    }
}

fn tkhd_dimensions(data: &[u8], header: BoxHeader) -> Result<(u32, u32)> {
    let version = data[header.payload];
    let offset = if version == 1 { 88 } else { 76 };
    if header.payload + offset + 8 > header.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let w = u32_at(data, header.payload + offset)? >> 16;
    let h = u32_at(data, header.payload + offset + 4)? >> 16;
    Ok((w, h))
}

fn parse_stbl<'a>(data: &'a [u8], stbl: BoxHeader) -> Result<Track<'a>> {
    let stsd = required_box(data, stbl.payload, stbl.end, b"stsd")?;
    if stsd.payload + 12 > stsd.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let entries = u32_at(data, stsd.payload + 4)? as usize;
    if entries == 0 {
        return Err(VideoError::InvalidData("MP4 empty stsd"));
    }
    let entry = read_box(data, stsd.payload + 8, stsd.end)?
        .ok_or(VideoError::InvalidData("MP4 sample entry"))?;
    let codec = Codec::from_mp4(entry.kind);
    if entry.payload + 28 > entry.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let entry_width = u16_at(data, entry.payload + 24)? as u32;
    let entry_height = u16_at(data, entry.payload + 26)? as u32;

    // VisualSampleEntry is 78 bytes of fixed fields, followed by codec
    // configuration boxes.
    let child_start = entry.payload.saturating_add(78).min(entry.end);
    let config = if child_start < entry.end {
        find_box(data, child_start, entry.end, b"avcC")?
            .map(|b| &data[b.payload..b.end])
            .or_else(|| {
                find_box(data, child_start, entry.end, b"hvcC")
                    .ok()
                    .flatten()
                    .map(|b| &data[b.payload..b.end])
            })
            .unwrap_or(&data[0..0])
    } else {
        &data[0..0]
    };

    let frame_count = if let Some(stsz) = find_box(data, stbl.payload, stbl.end, b"stsz")? {
        if stsz.payload + 12 > stsz.end {
            return Err(VideoError::UnexpectedEnd);
        }
        u32_at(data, stsz.payload + 8)?
    } else {
        0
    };

    Ok(Track {
        width: entry_width,
        height: entry_height,
        codec,
        frame_count,
        frame_rate: None,
        config,
        stbl_start: stbl.payload,
        stbl_end: stbl.end,
    })
}

fn find_video_track(input: &[u8]) -> Result<Track<'_>> {
    let moov = find_box(input, 0, input.len(), b"moov")?
        .ok_or(VideoError::InvalidData("MP4 has no moov box"))?;
    let mut pos = moov.payload;
    while let Some(trak) = read_box(input, pos, moov.end)? {
        pos = trak.next;
        if &trak.kind != b"trak" {
            continue;
        }
        let mdia = match children(input, trak, b"mdia")? {
            Some(b) => b,
            None => continue,
        };
        let hdlr = match children(input, mdia, b"hdlr")? {
            Some(b) => b,
            None => continue,
        };
        if hdlr.payload + 12 > hdlr.end || &input[hdlr.payload + 8..hdlr.payload + 12] != b"vide" {
            continue;
        }
        let minf = required_box(input, mdia.payload, mdia.end, b"minf")?;
        let stbl = required_box(input, minf.payload, minf.end, b"stbl")?;
        let mut track = parse_stbl(input, stbl)?;

        if let Some(tkhd) = children(input, trak, b"tkhd")? {
            if let Ok((w, h)) = tkhd_dimensions(input, tkhd) {
                if w != 0 && h != 0 {
                    track.width = w;
                    track.height = h;
                }
            }
        }
        if let Some(mdhd) = children(input, mdia, b"mdhd")? {
            let (timescale, _duration) = mdhd_timescale(input, mdhd)?;
            if let Some(stts) = find_box(input, stbl.payload, stbl.end, b"stts")? {
                if stts.payload + 16 <= stts.end {
                    let delta = u32_at(input, stts.payload + 12)?;
                    if timescale != 0 && delta != 0 {
                        track.frame_rate = crate::reduce_fraction(timescale, delta);
                    }
                }
            }
        }
        if track.frame_count == 0 {
            if let Some(stts) = find_box(input, stbl.payload, stbl.end, b"stts")? {
                if stts.payload + 8 <= stts.end {
                    let entries = u32_at(input, stts.payload + 4)? as usize;
                    if entries > 0 && stts.payload + 16 <= stts.end {
                        track.frame_count = u32_at(input, stts.payload + 8)?;
                    }
                }
            }
        }
        return Ok(track);
    }
    Err(VideoError::InvalidData("MP4 has no video track"))
}

pub(crate) fn info(input: &[u8]) -> Result<Mp4Info> {
    let track = find_video_track(input)?;
    Ok(Mp4Info {
        width: track.width,
        height: track.height,
        codec: track.codec,
        frame_count: track.frame_count,
        frame_rate: track.frame_rate,
    })
}

fn sample_size(data: &[u8], stsz: BoxHeader, index: u32) -> Result<usize> {
    if stsz.payload + 12 > stsz.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let fixed = u32_at(data, stsz.payload + 4)? as usize;
    if fixed != 0 {
        return Ok(fixed);
    }
    let count = u32_at(data, stsz.payload + 8)?;
    if index >= count {
        return Err(VideoError::FrameOutOfRange { index, count });
    }
    let pos = stsz.payload + 12 + index as usize * 4;
    Ok(u32_at(data, pos)? as usize)
}

fn chunk_offset(data: &[u8], stco: BoxHeader, chunk: usize) -> Result<u64> {
    if &stco.kind == b"co64" {
        let pos = stco.payload + 8 + chunk * 8;
        u64_at(data, pos)
    } else {
        let pos = stco.payload + 8 + chunk * 4;
        Ok(u32_at(data, pos)? as u64)
    }
}

fn chunk_count(data: &[u8], stco: BoxHeader) -> Result<usize> {
    if stco.payload + 8 > stco.end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok(u32_at(data, stco.payload + 4)? as usize)
}

fn stsc_entry(data: &[u8], stsc: BoxHeader, entry: usize) -> Result<(u32, u32)> {
    let pos = stsc.payload + 8 + entry * 12;
    if pos + 12 > stsc.end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok((u32_at(data, pos)?, u32_at(data, pos + 4)?))
}

/// Walks samples `0..=target`, calling `f(sample_index, sample)`.
fn for_each_sample<'a, F>(data: &'a [u8], track: &Track<'a>, target: u32, mut f: F) -> Result<()>
where
    F: FnMut(u32, &'a [u8]) -> Result<bool>,
{
    let stbl_start = track.stbl_start;
    let stbl_end = track.stbl_end;
    let stsz = required_box(data, stbl_start, stbl_end, b"stsz")?;
    let stsc = required_box(data, stbl_start, stbl_end, b"stsc")?;
    let stco = find_box(data, stbl_start, stbl_end, b"stco")?
        .or(find_box(data, stbl_start, stbl_end, b"co64")?)
        .ok_or(VideoError::InvalidData("MP4 has no chunk offsets"))?;
    let chunks = chunk_count(data, stco)?;
    let stsc_entries = if stsc.payload + 8 <= stsc.end {
        u32_at(data, stsc.payload + 4)? as usize
    } else {
        0
    };
    if stsc_entries == 0 {
        return Err(VideoError::InvalidData("MP4 empty stsc"));
    }
    let mut entry_index = 0usize;
    let mut sample_index = 0u32;
    let mut chunk = 0usize;
    while chunk < chunks {
        while entry_index + 1 < stsc_entries {
            let (next_chunk, _) = stsc_entry(data, stsc, entry_index + 1)?;
            if next_chunk as usize <= chunk + 1 {
                entry_index += 1;
            } else {
                break;
            }
        }
        let (_, per_chunk) = stsc_entry(data, stsc, entry_index)?;
        let mut offset = chunk_offset(data, stco, chunk)? as usize;
        for _ in 0..per_chunk {
            let size = sample_size(data, stsz, sample_index)?;
            if sample_index <= target {
                let end = offset.checked_add(size).ok_or(VideoError::Overflow)?;
                if end > data.len() {
                    return Err(VideoError::UnexpectedEnd);
                }
                if f(sample_index, &data[offset..end])? {
                    return Ok(());
                }
            }
            offset += size;
            sample_index = sample_index.saturating_add(1);
            if sample_index > target {
                return Ok(());
            }
        }
        chunk += 1;
    }
    Ok(())
}

fn avcc_info(config: &[u8]) -> Result<H264Info> {
    h264::info_from_avcc(config)
}

fn avc_config_start<'a>(track: &Track<'a>) -> &'a [u8] {
    track.config
}

pub(crate) fn decode_frame(input: &[u8], index: u32, out: &mut [u8]) -> Result<u32> {
    let track = find_video_track(input)?;
    if index >= track.frame_count {
        return Err(VideoError::FrameOutOfRange {
            index,
            count: track.frame_count,
        });
    }
    // Some files leave tkhd dimensions at zero; fall back to the sample entry.
    let (width, height) = if track.width != 0 && track.height != 0 {
        (track.width, track.height)
    } else if track.codec == Codec::H264 && !track.config.is_empty() {
        let info = avcc_info(track.config)?;
        (info.width, info.height)
    } else {
        (track.width, track.height)
    };
    require_rgba(out, width, height)?;

    match track.codec {
        Codec::H264 => {
            let config = avc_config_start(&track);
            if config.is_empty() {
                return Err(VideoError::InvalidData("MP4 avcC missing"));
            }
            let mut decoder = AvccDecoder::new(config, index)?;
            let mut done = false;
            for_each_sample(input, &track, index, |_, sample| {
                if decoder.push(sample, out)? {
                    done = true;
                    return Ok(true);
                }
                Ok(false)
            })?;
            if !done && decoder.finish(out)? {
                done = true;
            }
            if !done {
                return Err(VideoError::FrameOutOfRange {
                    index,
                    count: track.frame_count,
                });
            }
            Ok(0)
        }
        Codec::Mjpeg => {
            let mut sample = None;
            for_each_sample(input, &track, index, |i, data| {
                if i == index {
                    sample = Some(data);
                    return Ok(true);
                }
                Ok(false)
            })?;
            mjpeg::decode_sample(sample.ok_or(VideoError::UnexpectedEnd)?, out)?;
            Ok(0)
        }
        codec if codec.pixel_format().is_some() => {
            let mut sample = None;
            for_each_sample(input, &track, index, |i, data| {
                if i == index {
                    sample = Some(data);
                    return Ok(true);
                }
                Ok(false)
            })?;
            raw::decode(
                sample.ok_or(VideoError::UnexpectedEnd)?,
                codec,
                width,
                height,
                false,
                out,
            )?;
            Ok(0)
        }
        codec => Err(VideoError::UnsupportedCodec(codec.name())),
    }
}
