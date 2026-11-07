//! AVI (RIFF) demuxing, including OpenDML files and `rec ` lists.

use alloc::vec::Vec;

use feos_source::Window;

use crate::audio::{push_packet, AudioCodec, AudioTrack};
use crate::bitstream::{fourcc_at, u16_le_at, u32_le_at};
use crate::codec::Codec;
use crate::error::{Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};
use crate::{DisplayMetadata, StreamColour};

#[derive(Clone, Copy, Debug)]
pub(crate) struct AviInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
    pub display: DisplayMetadata,
    pub colour: StreamColour,
}

/// A BITMAPINFOHEADER: the part of an AVI `strf` that describes video, also
/// used by Matroska's `V_MS/VFW/FOURCC`.
#[derive(Clone, Debug)]
pub(crate) struct BitmapInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    /// Stored as a true DIB: packed rows padded to 4 bytes. Fourcc raw formats
    /// such as Y800 and YV12 are tightly packed, top-down.
    pub dib: bool,
    pub bottom_up: bool,
    /// Palette or codec data, relative to the start of the header.
    pub palette: core::ops::Range<usize>,
}

/// Parses a BITMAPINFOHEADER and what follows it.
pub(crate) fn parse_bitmap_info(data: &[u8]) -> Result<BitmapInfo> {
    if data.len() < 40 {
        return Err(VideoError::InvalidData("AVI BITMAPINFOHEADER"));
    }
    let bi_size = u32_le_at(data, 0)? as usize;
    let width = u32_le_at(data, 4)? as i32;
    let height = u32_le_at(data, 8)? as i32;
    let bit_count = u16_le_at(data, 14)?;
    let compression = fourcc_at(data, 16)?;
    let extra_start = bi_size.max(40).min(data.len());
    let codec = match compression {
        [0, 0, 0, 0] => match bit_count {
            32 => Codec::RawBgra,
            24 => Codec::RawBgr,
            16 => Codec::RawBgr555,
            8 => Codec::RawPal8,
            _ => Codec::Unknown(u32::from_be_bytes(compression)),
        },
        [3, 0, 0, 0] => match bit_count {
            32 => Codec::RawBgra,
            16 => {
                // BI_BITFIELDS: the green mask decides 5:5:5 or 5:6:5.
                let green = u32_le_at(data, extra_start + 4).unwrap_or(0);
                if green == 0x07e0 {
                    Codec::RawBgr565
                } else {
                    Codec::RawBgr555
                }
            }
            _ => Codec::Unknown(u32::from_be_bytes(compression)),
        },
        [1, 0, 0, 0] => Codec::Rle8,
        [2, 0, 0, 0] => Codec::Rle4,
        // `\0\0\0\0`-style fourccs aside, Y800 and friends are also gray.
        _ => Codec::from_avi(compression),
    };
    // A negative height marks a top-down DIB, and only DIBs have a direction:
    // YUV and compressed frames are always stored top-down.
    let bottom_up = height > 0;
    let dib = matches!(compression, [0, 0, 0, 0] | [3, 0, 0, 0]);
    Ok(BitmapInfo {
        dib,
        width: width.unsigned_abs(),
        height: height.unsigned_abs(),
        codec,
        bottom_up,
        palette: extra_start..data.len(),
    })
}

fn scan(data: &[u8], pos: usize, end: usize) -> Option<([u8; 4], usize, usize, usize)> {
    // Returns (id, payload_start, payload_len, next_chunk_start).
    if pos.checked_add(8)? > end {
        return None;
    }
    let id = fourcc_at(data, pos).ok()?;
    let size = u32_le_at(data, pos + 4).ok()? as usize;
    let payload = pos + 8;
    let next = payload.checked_add(size)?.checked_add(size & 1)?;
    // The last chunk's padding byte may be missing from a file.
    if payload + size > end {
        return None;
    }
    Some((id, payload, size, next.min(end)))
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

/// The chosen video stream of a file.
struct Stream {
    dib: bool,
    number: u8,
    width: u32,
    height: u32,
    codec: Codec,
    bottom_up: bool,
    scale: u32,
    rate: u32,
    /// Palette or codec data, as offsets into the file.
    extra: core::ops::Range<usize>,
}

/// Where the first RIFF's chunks end. The RIFF size may be wrong or truncated.
fn riff_end(input: &[u8]) -> Result<usize> {
    Ok(input
        .len()
        .min((u32_le_at(input, 4)? as usize).saturating_add(8)))
}

fn video_stream(input: &[u8]) -> Result<Stream> {
    if input.len() < 12 || &input[..4] != b"RIFF" || &input[8..12] != b"AVI " {
        return Err(VideoError::UnknownFormat);
    }
    let end = riff_end(input)?;
    let (hdrl_start, hdrl_len) =
        find_list(input, 12, end, b"hdrl").ok_or(VideoError::InvalidData("AVI hdrl"))?;
    let hdrl_end = hdrl_start + hdrl_len;

    let (avih, avih_len) = find_chunk(input, hdrl_start, hdrl_end, b"avih")
        .ok_or(VideoError::InvalidData("AVI avih"))?;
    if avih_len < 40 {
        return Err(VideoError::InvalidData("AVI avih size"));
    }
    let avih_width = u32_le_at(input, avih + 32)?;
    let avih_height = u32_le_at(input, avih + 36)?;

    let mut pos = hdrl_start;
    let mut number = 0u8;
    while let Some((id, payload, size, next)) = scan(input, pos, hdrl_end) {
        pos = next;
        if &id != b"LIST" || size < 4 || &input[payload..payload + 4] != b"strl" {
            continue;
        }
        let s_start = payload + 4;
        let s_end = payload + size;
        let this = number;
        number = number.saturating_add(1);
        let (Some((strh, strh_len)), Some((strf, strf_len))) = (
            find_chunk(input, s_start, s_end, b"strh"),
            find_chunk(input, s_start, s_end, b"strf"),
        ) else {
            continue;
        };
        if strh_len < 4 || &input[strh..strh + 4] != b"vids" {
            continue;
        }
        let handler = fourcc_at(input, strh + 4).unwrap_or([0; 4]);
        let scale = u32_le_at(input, strh + 20).unwrap_or(0);
        let rate = u32_le_at(input, strh + 24).unwrap_or(0);
        let bih = parse_bitmap_info(&input[strf..strf + strf_len])?;
        let codec = if bih.codec == Codec::Unknown(0) {
            Codec::from_avi(handler)
        } else {
            bih.codec
        };
        let width = if bih.width != 0 { bih.width } else { avih_width };
        let height = if bih.height != 0 {
            bih.height
        } else {
            avih_height
        };
        return Ok(Stream {
            dib: bih.dib,
            number: this,
            width,
            height,
            codec,
            bottom_up: bih.bottom_up,
            scale,
            rate,
            extra: strf + bih.palette.start..strf + bih.palette.end,
        });
    }
    Err(VideoError::InvalidData("AVI video stream"))
}

/// Whether `id` is a data chunk of stream `number`: `##dc` or `##db`.
fn is_video_chunk(id: &[u8; 4], number: u8) -> bool {
    id[2] == b'd'
        && matches!(id[3], b'c' | b'b')
        && id[0].is_ascii_digit()
        && id[1].is_ascii_digit()
        && (id[0] - b'0') * 10 + (id[1] - b'0') == number
}

/// Walks a `movi` list, descending into `rec ` lists, until `f` returns false.
fn walk_movi<F>(data: &[u8], start: usize, end: usize, number: u8, f: &mut F) -> Result<bool>
where
    F: FnMut(usize, usize) -> Result<bool>,
{
    let mut pos = start;
    while let Some((id, payload, size, next)) = scan(data, pos, end) {
        pos = next;
        if &id == b"LIST" && size >= 4 && &data[payload..payload + 4] == b"rec " {
            let mut inner = payload + 4;
            // rec lists don't nest, so this needs no recursion.
            while let Some((id, payload, size, next)) = scan(data, inner, payload + size) {
                inner = next;
                if is_video_chunk(&id, number) && !f(payload, size)? {
                    return Ok(false);
                }
            }
        } else if is_video_chunk(&id, number) && !f(payload, size)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Visits video chunks in file order, across the `RIFF AVIX` extensions of an
/// OpenDML file, until `f` returns false.
fn for_each_video_chunk<F>(input: &[u8], number: u8, mut f: F) -> Result<()>
where
    F: FnMut(usize, usize) -> Result<bool>,
{
    let end = riff_end(input)?;
    if let Some((start, len)) = find_list(input, 12, end, b"movi") {
        if !walk_movi(input, start, start + len, number, &mut f)? {
            return Ok(());
        }
    }
    let mut pos = end + (end & 1);
    while let Some((id, payload, size, next)) = scan(input, pos, input.len()) {
        pos = next;
        if &id == b"RIFF" && size >= 4 && &input[payload..payload + 4] == b"AVIX" {
            if let Some((start, len)) = find_list(input, payload + 4, payload + size, b"movi") {
                if !walk_movi(input, start, start + len, number, &mut f)? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn info(input: &[u8]) -> Result<AviInfo> {
    let stream = video_stream(input)?;
    // avih's total is wrong for OpenDML and truncated files. The chunks are
    // what's actually playable.
    let mut frame_count = 0u32;
    let mut first = None;
    for_each_video_chunk(input, stream.number, |offset, size| {
        frame_count = frame_count.saturating_add(1);
        if first.is_none() && size > 0 {
            first = Some(offset..offset + size);
        }
        Ok(true)
    })?;
    let frame_rate = if stream.scale != 0 && stream.rate != 0 {
        crate::reduce_fraction(stream.rate, stream.scale)
    } else {
        None
    };
    let mut display = DisplayMetadata::default();
    let mut colour = StreamColour::default();
    let (mut width, mut height) = (stream.width, stream.height);
    if let Some(range) = first {
        let sample = &input[range];
        match stream.codec {
            Codec::Mjpeg => {
                if let Ok(image) = feos_image::jpeg::info(sample) {
                    width = image.width;
                    height = image.height;
                }
            }
            Codec::H264 => {
                let extra = &input[stream.extra.clone()];
                let sps = if extra.first() == Some(&1) {
                    crate::h264::info_from_avcc(extra).ok()
                } else {
                    crate::h264::info_from_sample(sample, None)
                        .or_else(|| crate::h264::info_from_annex_b(extra).ok())
                };
                if let Some(sps) = sps {
                    width = sps.width;
                    height = sps.height;
                    crate::merge_sps_metadata(&mut display, &mut colour, &sps);
                }
            }
            _ => {}
        }
    }
    Ok(AviInfo {
        width,
        height,
        codec: stream.codec,
        frame_count,
        frame_rate,
        display,
        colour,
    })
}

/// A sample table and what's needed to read the samples.
pub(crate) struct AviIndex {
    pub index: Index,
    pub extra: core::ops::Range<usize>,
    /// For a true DIB, whether it's bottom-up.
    pub dib: Option<bool>,
}

/// Applies the `idx1` key-frame flags. Entries are in file order, so the nth
/// entry for this stream describes its nth chunk, whatever base the offsets
/// use. Returns false if the file has no usable index.
fn apply_idx1(input: &[u8], number: u8, index: &mut Index) -> bool {
    let Ok(end) = riff_end(input) else {
        return false;
    };
    let Some((start, len)) = find_chunk(input, 12, end, b"idx1") else {
        return false;
    };
    apply_idx1_entries(&input[start..start + len], number, index)
}

fn apply_idx1_entries(entries: &[u8], number: u8, index: &mut Index) -> bool {
    const AVIIF_KEYFRAME: u32 = 0x10;
    let mut at = 0usize;
    for entry in entries.chunks_exact(16) {
        let id = [entry[0], entry[1], entry[2], entry[3]];
        if !is_video_chunk(&id, number) {
            continue;
        }
        let Some(sample) = index.samples.get_mut(at) else {
            break;
        };
        let flags = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        if flags & AVIIF_KEYFRAME != 0 {
            sample.flags |= FLAG_KEY;
        } else {
            sample.flags &= !FLAG_KEY;
        }
        at += 1;
    }
    at != 0
}

pub(crate) fn build_index(input: &[u8], limits: &DecoderLimits) -> Result<AviIndex> {
    let stream = video_stream(input)?;
    let mut index = Index::default();
    // Without an idx1 only the first frame of a delta codec is certain to be
    // complete. Intra codecs set every flag in the decoder.
    for_each_video_chunk(input, stream.number, |offset, size| {
        let flags = if index.samples.is_empty() { FLAG_KEY } else { 0 };
        index.push(limits, input.len(), offset, size, 0, flags)?;
        Ok(true)
    })?;
    apply_idx1(input, stream.number, &mut index);
    if let Some(first) = index.samples.first_mut() {
        first.flags |= FLAG_KEY;
    }
    let frame_rate = if stream.scale != 0 && stream.rate != 0 {
        crate::reduce_fraction(stream.rate, stream.scale)
    } else {
        None
    };
    index.set_uniform_timing(frame_rate);
    Ok(AviIndex {
        index,
        extra: stream.extra,
        dib: stream.dib.then_some(stream.bottom_up),
    })
}

/// Whether `id` is an audio chunk of stream `number`: `##wb`.
fn is_audio_chunk(id: &[u8; 4], number: u8) -> bool {
    &id[2..] == b"wb"
        && id[0].is_ascii_digit()
        && id[1].is_ascii_digit()
        && (id[0] - b'0') * 10 + (id[1] - b'0') == number
}

/// Finds the first audio stream of a file.
pub(crate) fn audio_track(input: &[u8]) -> Option<AudioTrack> {
    audio_track_in(input, input.len()).map(|(track, _)| track)
}

/// When an audio chunk starts. Constant bit rates count bytes; anything else chunks.
#[derive(Clone, Copy)]
struct AudioTiming {
    sample_size: u32,
    bytes_per_second: u32,
    stream_rate: u64,
    scale: u64,
    bytes: u64,
    count: u64,
}

impl AudioTiming {
    fn next(&mut self, size: usize) -> u64 {
        let ms = if self.sample_size != 0 && self.bytes_per_second != 0 {
            self.bytes * 1000 / self.bytes_per_second as u64
        } else if self.stream_rate != 0 {
            self.count * self.scale * 1000 / self.stream_rate
        } else {
            0
        };
        self.bytes += size as u64;
        self.count += 1;
        ms
    }
}

fn audio_track_in(
    input: &[u8],
    file_len: usize,
) -> Option<(AudioTrack, (u8, AudioTiming))> {
    let end = riff_end(input).ok()?;
    let (hdrl_start, hdrl_len) = find_list(input, 12, end, b"hdrl")?;
    let hdrl_end = hdrl_start + hdrl_len;
    let mut pos = hdrl_start;
    let mut number = 0u8;
    while let Some((id, payload, size, next)) = scan(input, pos, hdrl_end) {
        pos = next;
        if &id != b"LIST" || size < 4 || &input[payload..payload + 4] != b"strl" {
            continue;
        }
        let this = number;
        number = number.saturating_add(1);
        let (s_start, s_end) = (payload + 4, payload + size);
        let (strh, strh_len) = find_chunk(input, s_start, s_end, b"strh")?;
        if strh_len < 4 || &input[strh..strh + 4] != b"auds" {
            continue;
        }
        let (strf, strf_len) = find_chunk(input, s_start, s_end, b"strf")?;
        if strf_len < 14 {
            return None;
        }
        let tag = u16_le_at(input, strf).ok()?;
        let channels = u16_le_at(input, strf + 2).ok()?;
        let rate = u32_le_at(input, strf + 4).ok()?;
        let bytes_per_second = u32_le_at(input, strf + 8).ok()?;
        let bits = if strf_len >= 16 {
            u16_le_at(input, strf + 14).ok()?
        } else {
            8
        };
        let scale = u32_le_at(input, strh + 20).unwrap_or(0) as u64;
        let stream_rate = u32_le_at(input, strh + 24).unwrap_or(0) as u64;
        let sample_size = u32_le_at(input, strh + 44).unwrap_or(0);
        let mut config = 0..0;
        let codec = match tag {
            1 => AudioCodec::Pcm {
                bytes: (bits as u8).div_ceil(8),
                big_endian: false,
                float: false,
            },
            3 => AudioCodec::Pcm {
                bytes: (bits as u8).div_ceil(8),
                big_endian: false,
                float: true,
            },
            6 => AudioCodec::ALaw,
            7 => AudioCodec::MuLaw,
            0x50 | 0x55 => AudioCodec::Mp3,
            0xff | 0x1600 | 0x706d => {
                if strf_len >= 18 {
                    let extra = u16_le_at(input, strf + 16).ok()? as usize;
                    if extra > 0 && 18 + extra <= strf_len {
                        config = strf + 18..strf + 18 + extra;
                    }
                }
                AudioCodec::Aac
            }
            _ => AudioCodec::Other,
        };
        let mut packets = Vec::new();
        let mut timing = AudioTiming {
            sample_size,
            bytes_per_second,
            stream_rate,
            scale,
            bytes: 0,
            count: 0,
        };
        let mut full = false;
        let (movi, movi_len) = match find_list(input, 12, end, b"movi") {
            Some(found) => found,
            None if file_len != input.len() => (0, 0),
            None => return None,
        };
        let mut visit = |offset: usize, size: usize| {
            let ms = timing.next(size);
            if push_packet(&mut packets, file_len, offset, size, ms).is_none() {
                full = true;
            }
        };
        let mut pos = movi;
        while let Some((id, payload, size, next)) = scan(input, pos, movi + movi_len) {
            pos = next;
            if &id == b"LIST" && size >= 4 && &input[payload..payload + 4] == b"rec " {
                let mut inner = payload + 4;
                while let Some((id, payload, size, next)) = scan(input, inner, payload + size) {
                    inner = next;
                    if is_audio_chunk(&id, this) {
                        visit(payload, size);
                    }
                }
            } else if is_audio_chunk(&id, this) {
                visit(payload, size);
            }
        }
        if full {
            return None;
        }
        return Some((
            AudioTrack {
                codec,
                sample_rate: rate,
                channels,
                config,
                packets,
            },
            (this, timing),
        ));
    }
    None
}

/// The most header to keep from before the first `movi`.
const MAX_HEAD: u64 = 8 * 1024 * 1024;
/// The largest `idx1` read, for its key-frame flags: about 500 000 chunks.
const MAX_IDX1: u64 = 8 * 1024 * 1024;

/// A file's bytes up to its `movi` list, which is all the header there is.
pub(crate) fn source_head(window: &mut Window<'_>) -> Result<Vec<u8>> {
    let mut pos = 12u64;
    let mut stop = None;
    loop {
        let header = window.bytes(pos, 12);
        if header.len() < 12 {
            break;
        }
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
        if &header[..4] == b"LIST" && &header[8..12] == b"movi" {
            stop = Some(pos);
            break;
        }
        pos += 8 + size + (size & 1);
    }
    let stop = stop.ok_or(VideoError::InvalidData("AVI file has no movi list"))?;
    if stop > MAX_HEAD {
        return Err(VideoError::LimitExceeded("AVI header is too large"));
    }
    let mut head = Vec::new();
    head.try_reserve_exact(stop as usize).map_err(|_| VideoError::OutOfMemory)?;
    while (head.len() as u64) < stop {
        let chunk = window.bytes(head.len() as u64, (stop - head.len() as u64) as usize);
        if chunk.is_empty() {
            return Err(VideoError::UnexpectedEnd);
        }
        head.extend_from_slice(chunk);
    }
    Ok(head)
}

/// Walks a file's chunks in order, across `RIFF AVIX` extensions, without reading their
/// data. Returns where its `idx1` is, if it has one.
fn walk_file<F>(window: &mut Window<'_>, mut f: F) -> Result<Option<(u64, u64)>>
where
    F: FnMut(&[u8; 4], u64, usize) -> Result<bool>,
{
    let file_len = window.file_len();
    let mut idx1 = None;
    let mut riff = 0u64;
    while riff + 12 <= file_len {
        let header = window.bytes(riff, 12);
        if header.len() < 12 || &header[..4] != b"RIFF" {
            break;
        }
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
        let end = (riff + 8 + size).min(file_len);
        let mut pos = riff + 12;
        while pos + 8 <= end {
            let chunk = window.bytes(pos, 12);
            if chunk.len() < 8 {
                break;
            }
            let id = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
            if &id == b"LIST" && chunk.len() == 12 && &chunk[8..12] == b"movi" {
                if !walk_list(window, pos + 12, (pos + 8 + size).min(end), &mut f)? {
                    return Ok(idx1);
                }
            } else if &id == b"idx1" && idx1.is_none() {
                idx1 = Some((pos + 8, size));
            }
            pos += 8 + size + (size & 1);
        }
        riff = riff + 8 + size + (size & 1);
    }
    Ok(idx1)
}

fn walk_list<F>(window: &mut Window<'_>, start: u64, end: u64, f: &mut F) -> Result<bool>
where
    F: FnMut(&[u8; 4], u64, usize) -> Result<bool>,
{
    let mut pos = start;
    while pos + 8 <= end {
        let chunk = window.bytes(pos, 12);
        if chunk.len() < 8 {
            break;
        }
        let id = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        // Cut off by the end of the file: what came before still plays.
        if pos + 8 + size > end {
            break;
        }
        if &id == b"LIST" && chunk.len() == 12 && &chunk[8..12] == b"rec " {
            // rec lists don't nest.
            let mut inner = pos + 12;
            let inner_end = pos + 8 + size;
            while inner + 8 <= inner_end {
                let chunk = window.bytes(inner, 8);
                if chunk.len() < 8 {
                    break;
                }
                let id = [chunk[0], chunk[1], chunk[2], chunk[3]];
                let inner_size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
                if inner + 8 + inner_size > inner_end {
                    break;
                }
                if !f(&id, inner + 8, inner_size as usize)? {
                    return Ok(false);
                }
                inner += 8 + inner_size + (inner_size & 1);
            }
        } else if !f(&id, pos + 8, size as usize)? {
            return Ok(false);
        }
        pos += 8 + size + (size & 1);
    }
    Ok(true)
}

/// [`build_index`] for a file that stays on disk. `head` is its [`source_head`].
pub(crate) fn build_index_source(
    head: &[u8],
    window: &mut Window<'_>,
    limits: &DecoderLimits,
) -> Result<AviIndex> {
    let stream = video_stream(head)?;
    let file_len = window.file_len() as usize;
    let number = stream.number;
    let mut index = Index::default();
    let idx1 = walk_file(window, |id, offset, size| {
        if is_video_chunk(id, number) {
            let flags = if index.samples.is_empty() { FLAG_KEY } else { 0 };
            index.push(limits, file_len, offset as usize, size, 0, flags)?;
        }
        Ok(true)
    })?;
    if let Some((at, size)) = idx1.filter(|&(_, size)| size <= MAX_IDX1) {
        if let Some(mut entries) = feos_source::zeroed(size as usize) {
            let mut done = 0;
            while done < entries.len() {
                let chunk = window.bytes(at + done as u64, entries.len() - done);
                if chunk.is_empty() {
                    break;
                }
                entries[done..done + chunk.len()].copy_from_slice(chunk);
                done += chunk.len();
            }
            entries.truncate(done);
            apply_idx1_entries(&entries, number, &mut index);
        }
    }
    if let Some(first) = index.samples.first_mut() {
        first.flags |= FLAG_KEY;
    }
    let frame_rate = if stream.scale != 0 && stream.rate != 0 {
        crate::reduce_fraction(stream.rate, stream.scale)
    } else {
        None
    };
    index.set_uniform_timing(frame_rate);
    Ok(AviIndex {
        index,
        extra: stream.extra,
        dib: stream.dib.then_some(stream.bottom_up),
    })
}

/// [`audio_track`] for a file that stays on disk. The config range is in the head.
pub(crate) fn audio_track_source(window: &mut Window<'_>) -> Option<(Vec<u8>, AudioTrack)> {
    let head = source_head(window).ok()?;
    let file_len = window.file_len() as usize;
    let (mut track, (number, mut timing)) = audio_track_in(&head, file_len)?;
    let mut full = false;
    walk_file(window, |id, offset, size| {
        if is_audio_chunk(id, number) {
            let ms = timing.next(size);
            if push_packet(&mut track.packets, file_len, offset as usize, size, ms).is_none() {
                full = true;
                return Ok(false);
            }
        }
        Ok(true)
    })
    .ok()?;
    if full {
        return None;
    }
    Some((head, track))
}
