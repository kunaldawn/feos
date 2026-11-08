//! Matroska / WebM (EBML) demuxing.
//!
//! Supports the elements needed to find a video track, read its dimensions and
//! codec id, count frames, and walk SimpleBlock/BlockGroup samples. Lacing is
//! expanded so one block may yield several video frames. Live-muxed files with
//! unknown-size clusters and files cut off inside a cluster are both handled.

use alloc::vec::Vec;

use feos_source::Window;

use crate::audio::{push_packet, AudioCodec, AudioTrack};
use crate::codec::Codec;
use crate::error::{Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};
use crate::{DisplayMetadata, StreamColour};

const ID_EBML: u32 = 0x1a45_dfa3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_a966;
const ID_TRACKS: u32 = 0x1654_ae6b;
const ID_CLUSTER: u32 = 0x1f43_b675;
const ID_CUES: u32 = 0x1c53_bb6b;
const ID_SEEK_HEAD: u32 = 0x114d_9b74;
const ID_TAGS: u32 = 0x1254_c367;
const ID_CHAPTERS: u32 = 0x1043_a770;
const ID_ATTACHMENTS: u32 = 0x1941_a469;
const ID_TRACK_ENTRY: u32 = 0xae;
const ID_TRACK_NUMBER: u32 = 0xd7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63a2;
const ID_DEFAULT_DURATION: u32 = 0x0023_e383;
const ID_VIDEO: u32 = 0xe0;
const ID_PIXEL_WIDTH: u32 = 0xb0;
const ID_PIXEL_HEIGHT: u32 = 0xba;
const ID_DISPLAY_WIDTH: u32 = 0x54b0;
const ID_DISPLAY_HEIGHT: u32 = 0x54ba;
const ID_DISPLAY_UNIT: u32 = 0x54b2;
const ID_COLOUR_SPACE: u32 = 0x002e_b524;
const ID_COLOUR: u32 = 0x55b0;
const ID_MATRIX_COEFFICIENTS: u32 = 0x55b1;
const ID_RANGE: u32 = 0x55b9;
const ID_TRANSFER: u32 = 0x55ba;
const ID_PRIMARIES: u32 = 0x55bb;
const ID_PROJECTION: u32 = 0x7670;
const ID_POSE_ROLL: u32 = 0x7675;
const ID_CONTENT_ENCODINGS: u32 = 0x6d80;
const ID_CONTENT_ENCODING: u32 = 0x6240;
const ID_CONTENT_COMPRESSION: u32 = 0x5034;
const ID_COMPRESSION_ALGO: u32 = 0x4254;
const ID_COMPRESSION_SETTINGS: u32 = 0x4255;
const ID_TIMESTAMP_SCALE: u32 = 0x002a_d7b1;
const ID_CLUSTER_TIMESTAMP: u32 = 0xe7;
const ID_SIMPLE_BLOCK: u32 = 0xa3;
const ID_BLOCK_GROUP: u32 = 0xa0;
const ID_BLOCK: u32 = 0xa1;
const ID_REFERENCE_BLOCK: u32 = 0xfb;
const ID_CUE_POINT: u32 = 0xbb;
const ID_CUE_TIME: u32 = 0xb3;

#[derive(Clone, Copy, Debug)]
pub(crate) struct MatroskaInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
    pub display: DisplayMetadata,
    pub colour: StreamColour,
}

/// How a track's samples are stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Compression {
    None,
    /// These leading bytes were stripped from every frame.
    HeaderStrip(core::ops::Range<usize>),
    Zlib,
}

#[derive(Clone, Copy)]
struct Elem {
    id: u32,
    payload: usize,
    end: usize,
    next: usize,
    unknown_size: bool,
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
    if len > 4 {
        return Err(VideoError::InvalidData("EBML id"));
    }
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

/// IDs that can only appear directly inside the segment or at the top level.
/// Any of them ends an open element of unknown size.
fn ends_unknown_size(id: u32) -> bool {
    matches!(
        id,
        ID_CLUSTER
            | ID_INFO
            | ID_TRACKS
            | ID_CUES
            | ID_SEEK_HEAD
            | ID_TAGS
            | ID_CHAPTERS
            | ID_ATTACHMENTS
            | ID_SEGMENT
            | ID_EBML
    )
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
        let size = usize::try_from(size).map_err(|_| VideoError::Overflow)?;
        payload.checked_add(size).ok_or(VideoError::Overflow)?
    };
    if elem_end > end || payload > elem_end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok(Some(Elem {
        id,
        payload,
        end: elem_end,
        next: elem_end,
        unknown_size: unknown,
    }))
}

/// Like [`next_elem`], for walking a segment or cluster. A cluster running past
/// the data keeps its complete blocks, and damage inside ends the walk instead
/// of refusing the file.
fn next_lenient(data: &[u8], pos: usize, end: usize) -> Option<Elem> {
    match next_elem(data, pos, end) {
        Ok(elem) => elem,
        Err(_) => {
            let (id, id_len) = read_id(data, pos, end).ok()?;
            let (_, size_len, _) = read_size(data, pos + id_len, end).ok()?;
            if id == ID_CLUSTER || id == ID_SEGMENT {
                Some(Elem {
                    id,
                    payload: pos + id_len + size_len,
                    end,
                    next: end,
                    unknown_size: false,
                })
            } else {
                None
            }
        }
    }
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

/// Finds a direct child of the segment, stepping over unknown-size clusters.
fn find_in_segment(data: &[u8], seg: Elem, id: u32) -> Option<Elem> {
    let mut pos = seg.payload;
    while let Some(elem) = next_lenient(data, pos, seg.end) {
        if elem.id == id {
            return Some(elem);
        }
        pos = if elem.id == ID_CLUSTER && elem.unknown_size {
            unknown_cluster_end(data, elem)
        } else {
            elem.next
        };
    }
    None
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

fn read_str(data: &[u8], elem: Elem) -> &[u8] {
    let mut slice = &data[elem.payload..elem.end];
    while let [rest @ .., 0] = slice {
        slice = rest;
    }
    slice
}

fn segment(input: &[u8]) -> Result<Elem> {
    let mut pos = 0usize;
    while let Some(elem) = next_lenient(input, pos, input.len()) {
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
    display: DisplayMetadata,
    colour: StreamColour,
    compression: Compression,
    /// A DIB-style sample (`V_MS/VFW/FOURCC`), with this bottom-up flag.
    dib: Option<bool>,
}

fn video_track<'a>(input: &'a [u8], seg: Elem) -> Result<Track<'a>> {
    let tracks =
        find_in_segment(input, seg, ID_TRACKS).ok_or(VideoError::InvalidData("Matroska tracks"))?;
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
        let mut dib = None;
        if codec_id == b"V_MS/VFW/FOURCC" {
            if let Ok(bih) = crate::avi::parse_bitmap_info(config) {
                codec = bih.codec;
                dib = bih.dib.then_some(bih.bottom_up);
            }
        }
        let mut width = 0u32;
        let mut height = 0u32;
        let mut display = DisplayMetadata::default();
        let mut colour = StreamColour::default();
        if let Some(video) = find_elem(input, entry.payload, entry.end, ID_VIDEO)? {
            let uint = |id: u32| -> Result<Option<u64>> {
                find_elem(input, video.payload, video.end, id)?
                    .map(|e| read_uint(input, e))
                    .transpose()
            };
            let clamp = |value: u64| value.min(u32::MAX as u64) as u32;
            width = uint(ID_PIXEL_WIDTH)?.map_or(0, clamp);
            height = uint(ID_PIXEL_HEIGHT)?.map_or(0, clamp);
            let display_unit = uint(ID_DISPLAY_UNIT)?.unwrap_or(0);
            if let (Some(w), Some(h), 0 | 3) = (
                uint(ID_DISPLAY_WIDTH)?,
                uint(ID_DISPLAY_HEIGHT)?,
                display_unit,
            ) {
                // DisplayWidth/Height give the shape; turn that into a sample
                // aspect ratio relative to the coded size.
                if w != 0 && h != 0 && width != 0 && height != 0 {
                    let (sar_w, sar_h) = (w * height as u64, h * width as u64);
                    if sar_w != sar_h {
                        display.sar = (clamp(sar_w), clamp(sar_h));
                    }
                }
            }
            if codec_id == b"V_UNCOMPRESSED" {
                if let Some(space) = find_elem(input, video.payload, video.end, ID_COLOUR_SPACE)? {
                    if let [a, b, c, d] = input[space.payload..space.end] {
                        codec = Codec::from_colour_space([a, b, c, d]);
                    }
                }
            }
            if let Some(element) = find_elem(input, video.payload, video.end, ID_COLOUR)? {
                let value = |id: u32| -> Option<u64> {
                    find_elem(input, element.payload, element.end, id)
                        .ok()?
                        .and_then(|e| read_uint(input, e).ok())
                };
                if let Some(matrix) = value(ID_MATRIX_COEFFICIENTS) {
                    colour.matrix = matrix.min(255) as u8;
                }
                if let Some(primaries) = value(ID_PRIMARIES) {
                    colour.primaries = primaries.min(255) as u8;
                }
                if let Some(transfer) = value(ID_TRANSFER) {
                    colour.transfer = transfer.min(255) as u8;
                }
                colour.full_range = value(ID_RANGE) == Some(2);
            }
            if let Some(projection) = find_elem(input, video.payload, video.end, ID_PROJECTION)? {
                if let Some(roll) =
                    find_elem(input, projection.payload, projection.end, ID_POSE_ROLL)?
                {
                    // A float of 4 or 8 bytes. Compare exact bit patterns: only
                    // quarter turns are meaningful, and this avoids soft-float.
                    let roll = &input[roll.payload..roll.end];
                    let degrees = match *roll {
                        [a, b, c, d] => match u32::from_be_bytes([a, b, c, d]) {
                            0x42b4_0000 => 90,
                            0xc2b4_0000 | 0x4387_0000 => 270,
                            0x4334_0000 | 0xc334_0000 => 180,
                            _ => 0,
                        },
                        [a, b, c, d, e, f, g, h] => {
                            match u64::from_be_bytes([a, b, c, d, e, f, g, h]) {
                                0x4056_8000_0000_0000 => 90,
                                0xc056_8000_0000_0000 | 0x4070_e000_0000_0000 => 270,
                                0x4066_8000_0000_0000 | 0xc066_8000_0000_0000 => 180,
                                _ => 0,
                            }
                        }
                        _ => 0,
                    };
                    // The roll counts counter-clockwise.
                    display.rotation = (360 - degrees) % 360;
                }
            }
        }
        let compression = parse_compression(input, entry)?;
        return Ok(Track {
            number,
            width,
            height,
            codec,
            config,
            default_duration,
            display,
            colour,
            compression,
            dib,
        });
    }
    Err(VideoError::InvalidData("Matroska has no video track"))
}

fn parse_compression(input: &[u8], entry: Elem) -> Result<Compression> {
    let Some(encodings) = find_elem(input, entry.payload, entry.end, ID_CONTENT_ENCODINGS)? else {
        return Ok(Compression::None);
    };
    let Some(encoding) = find_elem(input, encodings.payload, encodings.end, ID_CONTENT_ENCODING)?
    else {
        return Ok(Compression::None);
    };
    let Some(compression) =
        find_elem(input, encoding.payload, encoding.end, ID_CONTENT_COMPRESSION)?
    else {
        return Err(VideoError::UnsupportedFeature("Matroska content encryption"));
    };
    let algo = find_elem(input, compression.payload, compression.end, ID_COMPRESSION_ALGO)?
        .map(|e| read_uint(input, e))
        .transpose()?
        .unwrap_or(0);
    match algo {
        0 => Ok(Compression::Zlib),
        3 => {
            let settings = find_elem(
                input,
                compression.payload,
                compression.end,
                ID_COMPRESSION_SETTINGS,
            )?
            .map_or(0..0, |e| e.payload..e.end);
            Ok(Compression::HeaderStrip(settings))
        }
        _ => Err(VideoError::UnsupportedFeature("Matroska compression")),
    }
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

/// A block's relevant fields, passed to the visitor.
#[derive(Clone, Copy)]
struct BlockFrame {
    offset: usize,
    size: usize,
    /// Timecode relative to the cluster, and this frame's place in its lace.
    relative_time: i16,
    lace_index: u32,
    key: bool,
}

/// Calls `f` for each frame of the block. Returns `false` if `f` asked to stop.
fn visit_block<F>(
    data: &[u8],
    block: Elem,
    track: u64,
    block_group_key: Option<bool>,
    f: &mut F,
) -> Result<bool>
where
    F: FnMut(BlockFrame) -> Result<bool>,
{
    let mut pos = block.payload;
    let (block_track, track_len, _) = read_size(data, pos, block.end)?;
    pos += track_len;
    if block_track != track {
        return Ok(true);
    }
    if pos + 3 > block.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let relative_time = i16::from_be_bytes([data[pos], data[pos + 1]]);
    pos += 2;
    let flags = data[pos];
    pos += 1;
    // SimpleBlock has a key flag. BlockGroup is a key frame unless it refers to
    // another block.
    let key = block_group_key.unwrap_or(flags & 0x80 != 0);
    let lacing = (flags >> 1) & 0x03;
    if lacing == 0 {
        if pos < block.end {
            return f(BlockFrame {
                offset: pos,
                size: block.end - pos,
                relative_time,
                lace_index: 0,
                key,
            });
        }
        return Ok(true);
    }

    let frames = data.get(pos).copied().ok_or(VideoError::UnexpectedEnd)? as usize + 1;
    pos += 1;
    let mut sizes = [0usize; 256];
    
    let total_fixed = match lacing {
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
            total
        }
        2 => {
            let remaining = block
                .end
                .checked_sub(pos)
                .ok_or(VideoError::UnexpectedEnd)?;
            if remaining % frames != 0 {
                return Err(VideoError::InvalidData("Matroska fixed lacing"));
            }
            for size in sizes.iter_mut().take(frames - 1) {
                *size = remaining / frames;
            }
            remaining / frames * (frames - 1)
        }
        _ => {
            // EBML lacing: the first size, then signed differences. The last
            // frame's size is whatever remains, and isn't coded.
            let mut total = 0usize;
            if frames > 1 {
                let (first, len, _) = read_size(data, pos, block.end)?;
                pos += len;
                let first = usize::try_from(first).map_err(|_| VideoError::Overflow)?;
                let mut previous = first;
                sizes[0] = previous;
                total = previous;
                for size in sizes.iter_mut().take(frames - 1).skip(1) {
                    let (diff, len) = lacing_signed(data, pos, block.end)?;
                    pos += len;
                    let next = (previous as i64)
                        .checked_add(diff)
                        .filter(|&next| next >= 0)
                        .ok_or(VideoError::InvalidData("Matroska EBML lacing"))?;
                    previous = next as usize;
                    *size = previous;
                    total = total.checked_add(previous).ok_or(VideoError::Overflow)?;
                }
            }
            total
        }
    };
    let remaining = block
        .end
        .checked_sub(pos)
        .ok_or(VideoError::UnexpectedEnd)?;
    sizes[frames - 1] = remaining
        .checked_sub(total_fixed)
        .ok_or(VideoError::InvalidData("Matroska lacing size"))?;

    for (lace_index, &size) in sizes.iter().take(frames).enumerate() {
        let end = pos.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > block.end {
            return Err(VideoError::UnexpectedEnd);
        }
        let more = f(BlockFrame {
            offset: pos,
            size,
            relative_time,
            lace_index: lace_index as u32,
            key: key && lace_index == 0,
        })?;
        if !more {
            return Ok(false);
        }
        pos = end;
    }
    Ok(true)
}

/// Where an unknown-size cluster ends: at the next segment-level element.
fn unknown_cluster_end(data: &[u8], cluster: Elem) -> usize {
    let mut pos = cluster.payload;
    while pos < cluster.end {
        let Ok((id, _)) = read_id(data, pos, cluster.end) else {
            return cluster.end;
        };
        if ends_unknown_size(id) {
            return pos;
        }
        match next_elem(data, pos, cluster.end) {
            Ok(Some(elem)) => pos = elem.next,
            _ => return cluster.end,
        }
    }
    cluster.end
}

/// Walks clusters, calling `f(cluster_timestamp, frame)` for each frame of
/// `track` until it returns `false`. Damage or truncation ends the walk with
/// what was read so far.
fn visit_clusters<F>(input: &[u8], seg: Elem, track: u64, mut f: F) -> Result<()>
where
    F: FnMut(u64, BlockFrame) -> Result<bool>,
{
    let mut pos = seg.payload;
    while let Some(elem) = next_lenient(input, pos, seg.end) {
        let is_unknown = elem.unknown_size;
        let id = elem.id;
        if id != ID_CLUSTER {
            // An unknown-size element other than a cluster can't be skipped.
            if is_unknown {
                return Ok(());
            }
            pos = elem.next;
            continue;
        }
        let cluster_end = if is_unknown {
            unknown_cluster_end(input, elem)
        } else {
            elem.end
        };
        let mut cluster_time = 0u64;
        let mut cpos = elem.payload;
        while let Some(child) = next_lenient(input, cpos, cluster_end) {
            cpos = child.next;
            let more = match child.id {
                ID_CLUSTER_TIMESTAMP => {
                    cluster_time = read_uint(input, child).unwrap_or(cluster_time);
                    true
                }
                ID_SIMPLE_BLOCK => {
                    let mut visit = |frame| f(cluster_time, frame);
                    match visit_block(input, child, track, None, &mut visit) {
                        Ok(more) => more,
                        Err(error) if error.is_fatal() => return Err(error),
                        Err(_) => true,
                    }
                }
                ID_BLOCK_GROUP => {
                    let key = !matches!(
                        find_elem(input, child.payload, child.end, ID_REFERENCE_BLOCK),
                        Ok(Some(_))
                    );
                    match find_elem(input, child.payload, child.end, ID_BLOCK) {
                        Ok(Some(block)) => {
                            let mut visit = |frame| f(cluster_time, frame);
                            match visit_block(input, block, track, Some(key), &mut visit) {
                                Ok(more) => more,
                                Err(error) if error.is_fatal() => return Err(error),
                                Err(_) => true,
                            }
                        }
                        _ => true,
                    }
                }
                _ => true,
            };
            if !more {
                return Ok(());
            }
        }
        pos = cluster_end;
    }
    Ok(())
}

fn first_frame<'a>(input: &'a [u8], seg: Elem, track: &Track<'_>) -> Option<&'a [u8]> {
    let mut found = None;
    let _ = visit_clusters(input, seg, track.number, |_, frame| {
        found = Some(frame);
        Ok(false)
    });
    let frame = found?;
    input.get(frame.offset..frame.offset + frame.size)
}

pub(crate) fn info(input: &[u8]) -> Result<MatroskaInfo> {
    let seg = segment(input)?;
    let mut track = video_track(input, seg)?;
    let mut frame_count = 0u32;
    visit_clusters(input, seg, track.number, |_, _| {
        frame_count = frame_count.saturating_add(1);
        Ok(true)
    })?;
    let frame_rate = if track.default_duration != 0 && track.default_duration <= u32::MAX as u64 {
        crate::reduce_fraction(1_000_000_000, track.default_duration as u32)
    } else {
        None
    };
    let mut width = track.width;
    let mut height = track.height;
    let mut colour = track.colour;
    if track.compression == Compression::None {
        match track.codec {
            Codec::H264 => {
                let sps = if track.config.first() == Some(&1) {
                    crate::h264::info_from_avcc(track.config).ok()
                } else {
                    first_frame(input, seg, &track)
                        .and_then(|sample| crate::h264::info_from_sample(sample, None))
                };
                if let Some(sps) = sps {
                    width = sps.width;
                    height = sps.height;
                    crate::merge_sps_metadata(&mut track.display, &mut colour, &sps);
                }
            }
            Codec::Mjpeg => {
                if let Some(image) = first_frame(input, seg, &track)
                    .and_then(|sample| feos_image::jpeg::info(sample).ok())
                {
                    width = image.width;
                    height = image.height;
                }
            }
            _ => {}
        }
    }
    Ok(MatroskaInfo {
        width,
        height,
        codec: track.codec,
        frame_count,
        frame_rate,
        display: track.display,
        colour,
    })
}

/// A sample table and what's needed to read the samples.
pub(crate) struct MatroskaIndex {
    pub index: Index,
    pub config: core::ops::Range<usize>,
    pub compression: Compression,
    pub dib: Option<bool>,
    pub palette: core::ops::Range<usize>,
}

/// Marks samples at the times listed in the Cues element as seek points.
fn apply_cues(input: &[u8], seg: Elem, scale_ns: u64, index: &mut Index) {
    let Some(cues) = find_in_segment(input, seg, ID_CUES) else {
        return;
    };
    let mut pos = cues.payload;
    while let Ok(Some(point)) = next_elem(input, pos, cues.end) {
        pos = point.next;
        if point.id != ID_CUE_POINT {
            continue;
        }
        let Ok(Some(time)) = find_elem(input, point.payload, point.end, ID_CUE_TIME) else {
            continue;
        };
        let Ok(ticks) = read_uint(input, time) else {
            continue;
        };
        let ms = (ticks as u128 * scale_ns as u128 / 1_000_000).min(i32::MAX as u128) as i32;
        // Decode order is nearly sorted by time, so look around the insertion
        // point instead of scanning. Cue times are in the file's own units.
        let at = index.samples.partition_point(|s| s.pts_ms < ms);
        let window = at.saturating_sub(8)..(at + 8).min(index.samples.len());
        for sample in &mut index.samples[window] {
            if sample.pts_ms == ms {
                sample.flags |= FLAG_KEY;
            }
        }
    }
}

/// Builds the sample table of the first video track.
pub(crate) fn build_index(input: &[u8], limits: &DecoderLimits) -> Result<MatroskaIndex> {
    let seg = segment(input)?;
    let track = video_track(input, seg)?;
    let scale_ns = find_in_segment(input, seg, ID_INFO)
        .and_then(|element| {
            find_elem(input, element.payload, element.end, ID_TIMESTAMP_SCALE)
                .ok()
                .flatten()
                .and_then(|e| read_uint(input, e).ok())
        })
        .filter(|&scale| scale != 0)
        .unwrap_or(1_000_000);
    let frame_step_ns = track.default_duration;
    let mut index = Index::default();
    visit_clusters(input, seg, track.number, |cluster_time, frame| {
        let ticks = cluster_time as i128 + frame.relative_time as i128;
        let ns = ticks * scale_ns as i128 + (frame.lace_index as i128 * frame_step_ns as i128);
        let ms = (ns / 1_000_000).clamp(i32::MIN as i128, i32::MAX as i128) as i64;
        let flags = if frame.key { FLAG_KEY } else { 0 };
        index.push(limits, input.len(), frame.offset, frame.size, ms, flags)?;
        Ok(true)
    })?;
    apply_cues(input, seg, scale_ns, &mut index);
    let offset_of = |slice: &[u8]| slice.as_ptr() as usize - input.as_ptr() as usize;
    let config = if track.config.is_empty() {
        0..0
    } else {
        let start = offset_of(track.config);
        start..start + track.config.len()
    };
    let palette = if track.dib.is_some() {
        match crate::avi::parse_bitmap_info(track.config) {
            Ok(bih) => {
                let start = config.start + bih.palette.start;
                start..config.start + bih.palette.end
            }
            Err(_) => 0..0,
        }
    } else {
        0..0
    };
    Ok(MatroskaIndex {
        index,
        config,
        compression: track.compression,
        dib: track.dib,
        palette,
    })
}

const ID_AUDIO: u32 = 0xe1;
const ID_SAMPLING_FREQUENCY: u32 = 0xb5;
const ID_CHANNELS: u32 = 0x9f;
const ID_BIT_DEPTH: u32 = 0x6264;

/// Reads a float element of 4 or 8 bytes.
fn read_float(data: &[u8], elem: Elem) -> Option<f64> {
    let bytes = data.get(elem.payload..elem.end)?;
    match bytes.len() {
        4 => Some(f32::from_be_bytes(bytes.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

/// Finds the first audio track.
pub(crate) fn audio_track(input: &[u8]) -> Option<AudioTrack> {
    let seg = segment(input).ok()?;
    let (mut track, number, scale_ns) = audio_header(input, seg)?;
    let mut full = false;
    let walked = visit_clusters(input, seg, number, |cluster_time, frame| {
        let ticks = cluster_time as i128 + frame.relative_time as i128;
        let ms = (ticks * scale_ns as i128 / 1_000_000).max(0) as u64;
        if push_packet(&mut track.packets, input.len(), frame.offset, frame.size, ms).is_none() {
            full = true;
            return Ok(false);
        }
        Ok(true)
    });
    if walked.is_err() || full {
        return None;
    }
    Some(track)
}

/// The first audio track's description, with no packets yet, and its number and the
/// file's time scale.
fn audio_header(input: &[u8], seg: Elem) -> Option<(AudioTrack, u64, u64)> {
    let tracks = find_in_segment(input, seg, ID_TRACKS)?;
    let scale_ns = find_in_segment(input, seg, ID_INFO)
        .and_then(|element| {
            find_elem(input, element.payload, element.end, ID_TIMESTAMP_SCALE)
                .ok()
                .flatten()
                .and_then(|e| read_uint(input, e).ok())
        })
        .filter(|&scale| scale != 0)
        .unwrap_or(1_000_000);
    let mut pos = tracks.payload;
    while let Some(entry) = next_elem(input, pos, tracks.end).ok()? {
        pos = entry.next;
        if entry.id != ID_TRACK_ENTRY {
            continue;
        }
        let track_type = find_elem(input, entry.payload, entry.end, ID_TRACK_TYPE)
            .ok()?
            .and_then(|e| read_uint(input, e).ok())
            .unwrap_or(0);
        if track_type != 2 {
            continue;
        }
        let number = find_elem(input, entry.payload, entry.end, ID_TRACK_NUMBER)
            .ok()?
            .and_then(|e| read_uint(input, e).ok())?;
        let codec_id = find_elem(input, entry.payload, entry.end, ID_CODEC_ID)
            .ok()?
            .map(|e| read_str(input, e))?;
        let config = find_elem(input, entry.payload, entry.end, ID_CODEC_PRIVATE)
            .ok()?
            .map_or(0..0, |e| e.payload..e.end);
        let audio = find_elem(input, entry.payload, entry.end, ID_AUDIO).ok()??;
        let rate = find_elem(input, audio.payload, audio.end, ID_SAMPLING_FREQUENCY)
            .ok()?
            .and_then(|e| read_float(input, e))
            .unwrap_or(8000.0) as u32;
        let channels = find_elem(input, audio.payload, audio.end, ID_CHANNELS)
            .ok()?
            .and_then(|e| read_uint(input, e).ok())
            .unwrap_or(1) as u16;
        let bits = find_elem(input, audio.payload, audio.end, ID_BIT_DEPTH)
            .ok()?
            .and_then(|e| read_uint(input, e).ok())
            .unwrap_or(16) as u8;
        // Stripped or compressed packets would need putting back together.
        let compressed = find_elem(input, entry.payload, entry.end, ID_CONTENT_ENCODINGS)
            .ok()?
            .is_some();
        let codec = match codec_id {
            _ if compressed => AudioCodec::Other,
            b"A_AAC" => AudioCodec::Aac,
            b"A_MPEG/L3" | b"A_MPEG/L2" | b"A_MPEG/L1" => AudioCodec::Mp3,
            b"A_VORBIS" => AudioCodec::Vorbis,
            b"A_FLAC" => AudioCodec::Flac,
            b"A_OPUS" => AudioCodec::Opus,
            b"A_PCM/INT/LIT" => AudioCodec::Pcm {
                bytes: bits.div_ceil(8),
                big_endian: false,
                float: false,
            },
            b"A_PCM/INT/BIG" => AudioCodec::Pcm {
                bytes: bits.div_ceil(8),
                big_endian: true,
                float: false,
            },
            b"A_PCM/FLOAT/IEEE" => AudioCodec::Pcm {
                bytes: bits.div_ceil(8),
                big_endian: false,
                float: true,
            },
            _ => AudioCodec::Other,
        };
        return Some((
            AudioTrack {
                codec,
                sample_rate: rate,
                channels,
                config,
                packets: Vec::new(),
            },
            number,
            scale_ns,
        ));
    }
    None
}

/// How much of a block's start to look at for its header and any lacing.
const BLOCK_PEEK: usize = 4096;
/// The most metadata to keep from before the first cluster.
const MAX_HEAD: u64 = 16 * 1024 * 1024;

/// One element in a file, as [`Elem`] is in a slice.
#[derive(Clone, Copy)]
struct FileElem {
    id: u32,
    payload: u64,
    end: u64,
    unknown_size: bool,
}

fn elem_at(window: &mut Window<'_>, pos: u64, limit: u64) -> Option<FileElem> {
    let data = window.bytes(pos, 12);
    let (id, id_len) = read_id(data, 0, data.len()).ok()?;
    let (size, size_len, unknown) = read_size(data, id_len, data.len()).ok()?;
    let payload = pos + (id_len + size_len) as u64;
    if payload > limit {
        return None;
    }
    let end = if unknown {
        limit
    } else {
        payload.saturating_add(size).min(limit)
    };
    Some(FileElem {
        id,
        payload,
        end,
        unknown_size: unknown,
    })
}

/// A file's bytes up to its first cluster, which is where the parsers above find the
/// tracks and everything else about it. Returns those, and the segment's end.
pub(crate) fn source_head(window: &mut Window<'_>) -> Result<(Vec<u8>, u64)> {
    let file_len = window.file_len();
    let mut pos = 0u64;
    let mut segment = None;
    while let Some(elem) = elem_at(window, pos, file_len) {
        if elem.id == ID_SEGMENT {
            segment = Some(elem);
            break;
        }
        if elem.unknown_size {
            break;
        }
        pos = elem.end;
    }
    let segment = segment.ok_or(VideoError::UnknownFormat)?;
    let mut pos = segment.payload;
    while let Some(elem) = elem_at(window, pos, segment.end) {
        if elem.id == ID_CLUSTER || elem.unknown_size {
            break;
        }
        pos = elem.end;
    }
    if pos > MAX_HEAD {
        return Err(VideoError::LimitExceeded("Matroska metadata is too large"));
    }
    let mut head = Vec::new();
    head.try_reserve_exact(pos as usize).map_err(|_| VideoError::OutOfMemory)?;
    while (head.len() as u64) < pos {
        let want = (pos - head.len() as u64) as usize;
        let chunk = window.bytes(head.len() as u64, want);
        if chunk.is_empty() {
            return Err(VideoError::UnexpectedEnd);
        }
        head.extend_from_slice(chunk);
    }
    Ok((head, segment.end))
}

/// [`visit_clusters`], from a file, starting where its [`source_head`] ended. Frame
/// offsets are the file's.
fn visit_file_clusters<F>(
    window: &mut Window<'_>,
    start: u64,
    segment_end: u64,
    track: u64,
    mut f: F,
) -> Result<()>
where
    F: FnMut(u64, u64, BlockFrame) -> Result<bool>,
{
    let mut pos = start;
    while let Some(elem) = elem_at(window, pos, segment_end) {
        if elem.id != ID_CLUSTER {
            if elem.unknown_size {
                return Ok(());
            }
            pos = elem.end;
            continue;
        }
        let mut cluster_time = 0u64;
        let mut child_pos = elem.payload;
        while child_pos < elem.end {
            let Some(child) = elem_at(window, child_pos, elem.end) else {
                break;
            };
            if elem.unknown_size && ends_unknown_size(child.id) {
                break;
            }
            child_pos = child.end;
            match child.id {
                ID_CLUSTER_TIMESTAMP => {
                    let len = (child.end - child.payload) as usize;
                    if len <= 8 {
                        let data = window.bytes(child.payload, len);
                        cluster_time = data.iter().fold(0, |value, &b| (value << 8) | b as u64);
                    }
                }
                ID_SIMPLE_BLOCK => {
                    if !visit_file_block(window, child, track, None, cluster_time, &mut f)? {
                        return Ok(());
                    }
                }
                ID_BLOCK_GROUP => {
                    let mut block = None;
                    let mut key = true;
                    let mut inner = child.payload;
                    while let Some(part) = elem_at(window, inner, child.end) {
                        inner = part.end;
                        match part.id {
                            ID_BLOCK => block = Some(part),
                            ID_REFERENCE_BLOCK => key = false,
                            _ => {}
                        }
                        if inner >= child.end {
                            break;
                        }
                    }
                    if let Some(block) = block {
                        if !visit_file_block(window, block, track, Some(key), cluster_time, &mut f)? {
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
        pos = if elem.unknown_size { child_pos } else { elem.end };
        if pos >= segment_end {
            break;
        }
    }
    Ok(())
}

fn visit_file_block<F>(
    window: &mut Window<'_>,
    block: FileElem,
    track: u64,
    group_key: Option<bool>,
    cluster_time: u64,
    f: &mut F,
) -> Result<bool>
where
    F: FnMut(u64, u64, BlockFrame) -> Result<bool>,
{
    let size = (block.end - block.payload) as usize;
    let peek = size.min(BLOCK_PEEK);
    let data = window.bytes(block.payload, peek);
    // Cut off by the end of the file.
    if data.len() < peek || peek < 4 {
        return Ok(true);
    }
    // Lacing tables are read from the block's start, and could run past what's here.
    if size > peek {
        let (_, track_len, _) = read_size(data, 0, peek)?;
        if data.get(track_len + 2).is_none_or(|flags| flags & 0x06 != 0) {
            return Ok(true);
        }
    }
    let base = block.payload;
    let view = Elem {
        id: block.id,
        payload: 0,
        end: size,
        next: size,
        unknown_size: false,
    };
    let mut visit = |frame: BlockFrame| f(base, cluster_time, frame);
    match visit_block(data, view, track, group_key, &mut visit) {
        Ok(more) => Ok(more),
        Err(error) if error.is_fatal() => Err(error),
        Err(_) => Ok(true),
    }
}

/// [`build_index`] for a file that stays on disk. `head` is its [`source_head`].
pub(crate) fn build_index_source(
    head: &[u8],
    window: &mut Window<'_>,
    segment_end: u64,
    limits: &DecoderLimits,
) -> Result<MatroskaIndex> {
    let seg = segment(head)?;
    let track = video_track(head, seg)?;
    let scale_ns = time_scale(head, seg);
    let frame_step_ns = track.default_duration;
    let file_len = window.file_len() as usize;
    let number = track.number;
    let mut index = Index::default();
    visit_file_clusters(window, head.len() as u64, segment_end, number, |base, cluster_time, frame| {
        let ticks = cluster_time as i128 + frame.relative_time as i128;
        let ns = ticks * scale_ns as i128 + (frame.lace_index as i128 * frame_step_ns as i128);
        let ms = (ns / 1_000_000).clamp(i32::MIN as i128, i32::MAX as i128) as i64;
        let flags = if frame.key { FLAG_KEY } else { 0 };
        index.push(limits, file_len, base as usize + frame.offset, frame.size, ms, flags)?;
        Ok(true)
    })?;
    let offset_of = |slice: &[u8]| slice.as_ptr() as usize - head.as_ptr() as usize;
    let config = if track.config.is_empty() {
        0..0
    } else {
        let start = offset_of(track.config);
        start..start + track.config.len()
    };
    let palette = if track.dib.is_some() {
        match crate::avi::parse_bitmap_info(track.config) {
            Ok(bih) => config.start + bih.palette.start..config.start + bih.palette.end,
            Err(_) => 0..0,
        }
    } else {
        0..0
    };
    Ok(MatroskaIndex {
        index,
        config,
        compression: track.compression,
        dib: track.dib,
        palette,
    })
}

fn time_scale(input: &[u8], seg: Elem) -> u64 {
    find_in_segment(input, seg, ID_INFO)
        .and_then(|element| {
            find_elem(input, element.payload, element.end, ID_TIMESTAMP_SCALE)
                .ok()
                .flatten()
                .and_then(|e| read_uint(input, e).ok())
        })
        .filter(|&scale| scale != 0)
        .unwrap_or(1_000_000)
}

/// [`audio_track`] for a file that stays on disk. The config range is in the head,
/// which comes back with it.
pub(crate) fn audio_track_source(window: &mut Window<'_>) -> Option<(Vec<u8>, AudioTrack)> {
    let (head, segment_end) = source_head(window).ok()?;
    let seg = segment(&head).ok()?;
    let (mut track, number, scale_ns) = audio_header(&head, seg)?;
    let file_len = window.file_len() as usize;
    let mut full = false;
    let walked = visit_file_clusters(window, head.len() as u64, segment_end, number, |base, cluster_time, frame| {
        let ticks = cluster_time as i128 + frame.relative_time as i128;
        let ms = (ticks * scale_ns as i128 / 1_000_000).max(0) as u64;
        if push_packet(&mut track.packets, file_len, base as usize + frame.offset, frame.size, ms).is_none() {
            full = true;
            return Ok(false);
        }
        Ok(true)
    });
    if walked.is_err() || full {
        return None;
    }
    Some((head, track))
}
