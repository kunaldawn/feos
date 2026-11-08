//! ISO Base Media (MP4/MOV) demuxing.
//!
//! `info` walks the box tree without allocating. The sample table is built once
//! by [`build_index`], from `stbl` or from the `moof` fragments of a fragmented
//! file.

use alloc::vec::Vec;

use crate::audio::{push_packet, AudioCodec, AudioPacket, AudioTrack};
use crate::bitstream::{fourcc_at, u16_at, u32_at, u64_at};
use crate::codec::Codec;
use crate::error::{Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};
use crate::{DisplayMetadata, StreamColour};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Mp4Info {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
    pub display: DisplayMetadata,
    pub colour: StreamColour,
}

struct Track<'a> {
    id: u32,
    width: u32,
    height: u32,
    codec: Codec,
    frame_count: u32,
    frame_rate: Option<(u32, u32)>,
    timescale: u32,
    config: &'a [u8],
    display: DisplayMetadata,
    colour: StreamColour,
    stbl_start: usize,
    stbl_end: usize,
    /// Media time of the first displayed sample, in the media timescale.
    edit_start: i64,
    /// Empty time before the first sample, in milliseconds.
    edit_delay_ms: i64,
    fragmented: bool,
}

#[derive(Clone, Copy)]
struct BoxHeader {
    kind: [u8; 4],
    payload: usize,
    end: usize,
    next: usize,
}

fn read_box(data: &[u8], pos: usize, end: usize) -> Result<Option<BoxHeader>> {
    if pos.checked_add(8).is_none_or(|header_end| header_end > end) {
        return Ok(None);
    }
    let size32 = u32_at(data, pos)? as u64;
    let kind = fourcc_at(data, pos + 4)?;
    let (header, size) = if size32 == 1 {
        if pos + 16 > end {
            return Err(VideoError::UnexpectedEnd);
        }
        (16usize, u64_at(data, pos + 8)?)
    } else if size32 == 0 {
        (8usize, (end - pos) as u64)
    } else {
        (8usize, size32)
    };
    if size < header as u64 {
        return Err(VideoError::InvalidData("MP4 box size"));
    }
    let size = usize::try_from(size).map_err(|_| VideoError::Overflow)?;
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

/// Reads a box at the top level, where a cut-off `mdat` shouldn't hide a `moov`
/// before it. A box running past the file is treated as ending with the file.
fn read_top_box(data: &[u8], pos: usize) -> Option<BoxHeader> {
    match read_box(data, pos, data.len()) {
        Ok(found) => found,
        Err(_) => {
            let kind = fourcc_at(data, pos + 4).ok()?;
            let long = u32_at(data, pos).ok()? == 1;
            Some(BoxHeader {
                kind,
                payload: (pos + if long { 16 } else { 8 }).min(data.len()),
                end: data.len(),
                next: data.len(),
            })
        }
    }
}

fn find_box(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Result<Option<BoxHeader>> {
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

struct Tkhd {
    id: u32,
    width: u32,
    height: u32,
    rotation: u16,
}

fn parse_tkhd(data: &[u8], header: BoxHeader) -> Result<Tkhd> {
    let version = *data.get(header.payload).ok_or(VideoError::UnexpectedEnd)?;
    let (id_at, matrix_at, size_at) = if version == 1 {
        (20, 52, 88)
    } else {
        (12, 40, 76)
    };
    if header.payload + size_at + 8 > header.end {
        return Err(VideoError::UnexpectedEnd);
    }
    let id = u32_at(data, header.payload + id_at)?;
    let matrix = header.payload + matrix_at;
    let a = u32_at(data, matrix)? as i32;
    let b = u32_at(data, matrix + 4)? as i32;
    let c = u32_at(data, matrix + 12)? as i32;
    let d = u32_at(data, matrix + 16)? as i32;
    const ONE: i32 = 0x1_0000;
    // The matrix maps stored pixels to the display, clockwise for positive b.
    let rotation = match (a, b, c, d) {
        (0, ONE, c, 0) if c == -ONE => 90,
        (a, 0, 0, d) if a == -ONE && d == -ONE => 180,
        (0, b, ONE, 0) if b == -ONE => 270,
        _ => 0,
    };
    Ok(Tkhd {
        id,
        width: u32_at(data, header.payload + size_at)? >> 16,
        height: u32_at(data, header.payload + size_at + 4)? >> 16,
        rotation,
    })
}

/// Parses `stsd`. VisualSampleEntry is 78 bytes of fixed fields, followed by
/// codec configuration, aspect ratio and colour boxes.
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

    let mut config: &[u8] = &[];
    let mut display = DisplayMetadata::default();
    let mut colour = StreamColour::default();
    let mut pos = entry.payload.saturating_add(78).min(entry.end);
    // Trailing bytes after the last child are common in MOV sample entries, so
    // a malformed child only ends the search.
    while let Ok(Some(child)) = read_box(data, pos, entry.end) {
        pos = child.next;
        match &child.kind {
            b"avcC" | b"hvcC" => config = &data[child.payload..child.end],
            b"pasp" if child.payload + 8 <= child.end => {
                display.sar = (
                    u32_at(data, child.payload)?,
                    u32_at(data, child.payload + 4)?,
                );
            }
            b"colr" if child.payload + 10 <= child.end
                && (&data[child.payload..child.payload + 4] == b"nclx"
                    || &data[child.payload..child.payload + 4] == b"nclc")
                => {
                    colour.primaries = data[child.payload + 5];
                    colour.transfer = data[child.payload + 7];
                    colour.matrix = data[child.payload + 9];
                    if &data[child.payload..child.payload + 4] == b"nclx"
                        && child.payload + 11 <= child.end
                    {
                        colour.full_range = data[child.payload + 10] & 0x80 != 0;
                    }
                }
            _ => {}
        }
    }

    let frame_count = if let Some(stsz) = find_box(data, stbl.payload, stbl.end, b"stsz")? {
        if stsz.payload + 12 > stsz.end {
            return Err(VideoError::UnexpectedEnd);
        }
        // A count is only as good as what could hold it, as it goes straight into the
        // duration: the table's entries, or for a fixed size, what the file has room for.
        let fixed = u32_at(data, stsz.payload + 4)? as usize;
        let room = match data.len().checked_div(fixed) {
            Some(room) => room.max(1),
            None => (stsz.end - stsz.payload - 12) / 4,
        };
        u32_at(data, stsz.payload + 8)?.min(room.min(u32::MAX as usize) as u32)
    } else if let Some(stz2) = find_box(data, stbl.payload, stbl.end, b"stz2")? {
        if stz2.payload + 12 > stz2.end {
            return Err(VideoError::UnexpectedEnd);
        }
        let bits = (u32_at(data, stz2.payload + 4)? & 0xff).max(1) as usize;
        let room = (stz2.end - stz2.payload - 12).saturating_mul(8) / bits;
        u32_at(data, stz2.payload + 8)?.min(room.min(u32::MAX as usize) as u32)
    } else {
        0
    };

    Ok(Track {
        id: 0,
        width: entry_width,
        height: entry_height,
        codec,
        frame_count,
        frame_rate: None,
        timescale: 0,
        config,
        display,
        colour,
        stbl_start: stbl.payload,
        stbl_end: stbl.end,
        edit_start: 0,
        edit_delay_ms: 0,
        fragmented: false,
    })
}

/// Reads `elst`: an optional leading empty edit, then where media starts.
fn parse_edit_list(input: &[u8], trak: BoxHeader, track: &mut Track<'_>, movie_timescale: u32) {
    let parse = || -> Result<Option<(i64, i64)>> {
        let Some(edts) = children(input, trak, b"edts")? else {
            return Ok(None);
        };
        let Some(elst) = children(input, edts, b"elst")? else {
            return Ok(None);
        };
        let version = *input.get(elst.payload).ok_or(VideoError::UnexpectedEnd)?;
        let count = u32_at(input, elst.payload + 4)?;
        let mut pos = elst.payload + 8;
        let mut delay = 0i64;
        for _ in 0..count.min(8) {
            let (duration, media_time, size) = if version == 1 {
                (
                    u64_at(input, pos)? as i64,
                    u64_at(input, pos + 8)? as i64,
                    20,
                )
            } else {
                (
                    u32_at(input, pos)? as i64,
                    u32_at(input, pos + 4)? as i32 as i64,
                    12,
                )
            };
            if pos + size > elst.end {
                return Err(VideoError::UnexpectedEnd);
            }
            if media_time >= 0 {
                return Ok(Some((media_time, delay)));
            }
            delay = delay.saturating_add(duration);
            pos += size;
        }
        Ok(None)
    };
    if let Ok(Some((media_time, delay))) = parse() {
        track.edit_start = media_time;
        if movie_timescale != 0 {
            track.edit_delay_ms = delay.saturating_mul(1000) / movie_timescale as i64;
        }
    }
}

/// Reads a typical frame rate from `stts`: the only rate if there's one, the
/// average if the track has variable timing.
fn stts_frame_rate(input: &[u8], stts: BoxHeader, timescale: u32) -> Result<Option<(u32, u32)>> {
    if stts.payload + 8 > stts.end || timescale == 0 {
        return Ok(None);
    }
    let entries = u32_at(input, stts.payload + 4)? as usize;
    let available = stts.end.saturating_sub(stts.payload + 8) / 8;
    let mut total_ticks = 0u64;
    let mut total_samples = 0u64;
    let mut first_delta = None;
    let mut uniform = true;
    for i in 0..entries.min(available) {
        let count = u32_at(input, stts.payload + 8 + i * 8)?;
        let delta = u32_at(input, stts.payload + 12 + i * 8)?;
        total_ticks = total_ticks.saturating_add(count as u64 * delta as u64);
        total_samples = total_samples.saturating_add(count as u64);
        if delta != 0 {
            if first_delta.is_some_and(|d| d != delta) {
                uniform = false;
            }
            first_delta.get_or_insert(delta);
        }
    }
    let Some(delta) = first_delta else {
        return Ok(None);
    };
    if uniform || total_ticks == 0 {
        return Ok(crate::reduce_fraction(timescale, delta));
    }
    // frames / seconds, keeping three decimals of the average.
    let per_kilosecond = (total_samples as u128 * timescale as u128 * 1000) / total_ticks as u128;
    Ok(crate::reduce_fraction(
        per_kilosecond.min(u32::MAX as u128) as u32,
        1000,
    ))
}

fn find_video_track(input: &[u8]) -> Result<Track<'_>> {
    let mut moov = None;
    let mut pos = 0;
    while let Some(header) = read_top_box(input, pos) {
        if &header.kind == b"moov" {
            moov = Some(header);
            break;
        }
        pos = header.next;
    }
    let moov = moov.ok_or(VideoError::InvalidData("MP4 has no moov box"))?;
    let movie_timescale = children(input, moov, b"mvhd")?
        .and_then(|mvhd| {
            let at = if input.get(mvhd.payload) == Some(&1) {
                20
            } else {
                12
            };
            u32_at(input, mvhd.payload + at).ok()
        })
        .unwrap_or(0);
    let fragmented = children(input, moov, b"mvex")?.is_some();
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
        track.fragmented = fragmented;

        // The sample entry holds the stored pixel size. tkhd holds a display
        // size that may be scaled and rotated, so it's only a fallback.
        if let Some(tkhd) = children(input, trak, b"tkhd")? {
            if let Ok(tkhd) = parse_tkhd(input, tkhd) {
                track.id = tkhd.id;
                track.display.rotation = tkhd.rotation;
                if track.width == 0 || track.height == 0 {
                    (track.width, track.height) = (tkhd.width, tkhd.height);
                }
                if track.display.sar == (0, 0) && tkhd.width != 0 && tkhd.height != 0 {
                    // Non-square pixels without a pasp box. tkhd is unrotated.
                    if track.width != 0 && track.height != 0 {
                        let (w, h) = (tkhd.width as u64, tkhd.height as u64);
                        if w * track.height as u64 != h * track.width as u64 {
                            track.display.sar = (
                                (w * track.height as u64).min(u32::MAX as u64) as u32,
                                (h * track.width as u64).min(u32::MAX as u64) as u32,
                            );
                        }
                    }
                }
            }
        }
        if let Some(mdhd) = children(input, mdia, b"mdhd")? {
            let (timescale, _duration) = mdhd_timescale(input, mdhd)?;
            track.timescale = timescale;
            if let Some(stts) = find_box(input, stbl.payload, stbl.end, b"stts")? {
                track.frame_rate = stts_frame_rate(input, stts, timescale)?;
            }
        }
        parse_edit_list(input, trak, &mut track, movie_timescale);
        return Ok(track);
    }
    Err(VideoError::InvalidData("MP4 has no video track"))
}

pub(crate) fn info(input: &[u8]) -> Result<Mp4Info> {
    let mut track = find_video_track(input)?;
    if track.fragmented && track.frame_count == 0 {
        let mut fragments = FragmentStats::default();
        // Damaged fragments only cut the frame count short.
        let _ = walk_fragments(input, track.id, |_, _, _, _| Ok(()), &mut fragments);
        track.frame_count = fragments.samples;
        if track.frame_rate.is_none() && fragments.samples > 1 && fragments.ticks > 0 {
            let per_kilosecond = (fragments.samples as u128 * track.timescale as u128 * 1000)
                / fragments.ticks as u128;
            track.frame_rate =
                crate::reduce_fraction(per_kilosecond.min(u32::MAX as u128) as u32, 1000);
        }
    }
    let mut width = track.width;
    let mut height = track.height;
    let mut colour = track.colour;
    // The codec's own headers say what the decoder will produce.
    match track.codec {
        Codec::H264 => {
            let sps = if track.config.is_empty() {
                first_sample(input, &track).and_then(|s| crate::h264::info_from_sample(s, None))
            } else {
                crate::h264::info_from_avcc(track.config).ok()
            };
            if let Some(sps) = sps {
                width = sps.width;
                height = sps.height;
                crate::merge_sps_metadata(&mut track.display, &mut colour, &sps);
            }
        }
        Codec::Mjpeg => {
            if let Some(image) = first_sample(input, &track)
                .and_then(|sample| feos_image::jpeg::info(sample).ok())
            {
                width = image.width;
                height = image.height;
            }
        }
        _ => {}
    }
    Ok(Mp4Info {
        width,
        height,
        codec: track.codec,
        frame_count: track.frame_count,
        frame_rate: track.frame_rate,
        display: track.display,
        colour,
    })
}

fn stsc_entry(data: &[u8], stsc: BoxHeader, entry: usize) -> Result<(u32, u32)> {
    let pos = stsc.payload + 8 + entry * 12;
    if pos + 12 > stsc.end {
        return Err(VideoError::UnexpectedEnd);
    }
    Ok((u32_at(data, pos)?, u32_at(data, pos + 4)?))
}

fn first_sample<'a>(input: &'a [u8], track: &Track<'_>) -> Option<&'a [u8]> {
    if track.fragmented && track.frame_count == 0 {
        return None;
    }
    let stbl = (track.stbl_start, track.stbl_end);
    let stsz = find_box(input, stbl.0, stbl.1, b"stsz").ok()??;
    let stco = find_box(input, stbl.0, stbl.1, b"stco").ok()?;
    let co64 = find_box(input, stbl.0, stbl.1, b"co64").ok()?;
    let offset = match (stco, co64) {
        (Some(stco), _) => u32_at(input, stco.payload + 8).ok()? as usize,
        (None, Some(co64)) => usize::try_from(u64_at(input, co64.payload + 8).ok()?).ok()?,
        _ => return None,
    };
    let fixed = u32_at(input, stsz.payload + 4).ok()? as usize;
    let size = if fixed != 0 {
        fixed
    } else {
        u32_at(input, stsz.payload + 12).ok()? as usize
    };
    input.get(offset..offset.checked_add(size)?)
}

/// Walks a table of `(count, value)` runs, such as `stts` and `ctts`.
struct RunCursor {
    pos: usize,
    end: usize,
    left: u32,
    value: u32,
}

impl RunCursor {
    fn new(header: Option<BoxHeader>) -> Self {
        match header {
            Some(header) => Self {
                pos: header.payload + 8,
                end: header.end,
                left: 0,
                value: 0,
            },
            None => Self {
                pos: 0,
                end: 0,
                left: 0,
                value: 0,
            },
        }
    }

    /// The value for the next sample, or zero past the end of the table.
    fn next(&mut self, data: &[u8]) -> u32 {
        while self.left == 0 {
            if self.pos + 8 > self.end {
                return 0;
            }
            self.left = u32_at(data, self.pos).unwrap_or(0);
            self.value = u32_at(data, self.pos + 4).unwrap_or(0);
            self.pos += 8;
        }
        self.left -= 1;
        self.value
    }
}

/// Converts media ticks to milliseconds.
fn ticks_to_ms(ticks: i64, timescale: u32) -> i64 {
    if timescale == 0 {
        return 0;
    }
    // Clamped so that the multiplication can't overflow: ticks of a 4 GiB file
    // are far below this at any real timescale.
    ticks.clamp(-(1 << 52), 1 << 52) * 1000 / timescale as i64
}

/// Builds the sample table of the first video track.
pub(crate) fn build_index(
    input: &[u8],
    limits: &DecoderLimits,
) -> Result<(Index, core::ops::Range<usize>)> {
    build_index_sized(input, input.len(), limits)
}

/// [`build_index`] with only the metadata in memory: `input` is what
/// [`source_head`] made of a file of `file_len` bytes.
pub(crate) fn build_index_sized(
    input: &[u8],
    file_len: usize,
    limits: &DecoderLimits,
) -> Result<(Index, core::ops::Range<usize>)> {
    let track = find_video_track(input)?;
    let mut index = Index::default();
    if track.frame_count > 0 && !(track.fragmented && track.stbl_samples_absent(input)) {
        index_stbl(input, file_len, &track, limits, &mut index)?;
    }
    if track.fragmented {
        let mut stats = FragmentStats::default();
        let mut dts = 0i64;
        let timescale = track.timescale;
        let edit = track.edit_start;
        let delay = track.edit_delay_ms;
        // Damaged trailing fragments keep the frames before them.
        let _ = walk_fragments(
            input,
            track.id,
            |offset, size, timing, flags| {
                let pts = dts + timing.1 as i64 - edit;
                dts = dts.saturating_add(timing.0 as i64);
                index.push(
                    limits,
                    file_len,
                    offset,
                    size,
                    ticks_to_ms(pts, timescale) + delay,
                    flags,
                )
            },
            &mut stats,
        );
    }
    // An absent config is a literal empty slice, outside the input.
    let config = if track.config.is_empty() {
        0..0
    } else {
        let start = track.config.as_ptr() as usize - input.as_ptr() as usize;
        start..start + track.config.len()
    };
    Ok((index, config))
}

impl Track<'_> {
    /// True if a fragmented file's `stbl` lists no samples.
    fn stbl_samples_absent(&self, input: &[u8]) -> bool {
        find_box(input, self.stbl_start, self.stbl_end, b"stsz")
            .ok()
            .flatten()
            .and_then(|stsz| u32_at(input, stsz.payload + 8).ok())
            .is_none_or(|count| count == 0)
    }
}

fn index_stbl(
    input: &[u8],
    file_len: usize,
    track: &Track<'_>,
    limits: &DecoderLimits,
    index: &mut Index,
) -> Result<()> {
    let (start, end) = (track.stbl_start, track.stbl_end);
    let stsz = find_box(input, start, end, b"stsz")?;
    let stz2 = find_box(input, start, end, b"stz2")?;
    let stsc = required_box(input, start, end, b"stsc")?;
    let stco = find_box(input, start, end, b"stco")?;
    let co64 = find_box(input, start, end, b"co64")?;
    let (chunk_box, chunk_size) = match (stco, co64) {
        (Some(stco), _) => (stco, 4),
        (None, Some(co64)) => (co64, 8),
        _ => return Err(VideoError::InvalidData("MP4 has no chunk offsets")),
    };
    if chunk_box.payload + 8 > chunk_box.end || stsc.payload + 8 > stsc.end {
        return Err(VideoError::UnexpectedEnd);
    }
    // Tables can claim more entries than they hold; believe the box sizes.
    let chunks = (u32_at(input, chunk_box.payload + 4)? as usize)
        .min(chunk_box.end.saturating_sub(chunk_box.payload + 8) / chunk_size);
    let stsc_entries =
        (u32_at(input, stsc.payload + 4)? as usize).min(stsc.end.saturating_sub(stsc.payload + 8) / 12);
    if stsc_entries == 0 {
        return Err(VideoError::InvalidData("MP4 empty stsc"));
    }

    // Sample sizes: a fixed size, or a table of 32, 16, 8 or 4-bit entries.
    let (fixed, size_bits, size_table, total) = match (stsz, stz2) {
        (Some(stsz), _) => {
            let fixed = u32_at(input, stsz.payload + 4)? as usize;
            let claimed = u32_at(input, stsz.payload + 8)? as usize;
            let total = if fixed != 0 {
                claimed
            } else {
                claimed.min(stsz.end.saturating_sub(stsz.payload + 12) / 4)
            };
            (fixed, 32usize, stsz.payload + 12, total)
        }
        (None, Some(stz2)) => {
            let bits = *input
                .get(stz2.payload + 7)
                .ok_or(VideoError::UnexpectedEnd)? as usize;
            if !matches!(bits, 4 | 8 | 16) {
                return Err(VideoError::InvalidData("MP4 stz2 field size"));
            }
            let claimed = u32_at(input, stz2.payload + 8)? as usize;
            let total = claimed.min(stz2.end.saturating_sub(stz2.payload + 12).saturating_mul(8) / bits);
            (0, bits, stz2.payload + 12, total)
        }
        _ => return Err(VideoError::InvalidData("MP4 has no sample sizes")),
    };
    let sample_size = |at: usize| -> Result<usize> {
        if fixed != 0 {
            return Ok(fixed);
        }
        Ok(match size_bits {
            32 => u32_at(input, size_table + at * 4)? as usize,
            16 => u16_at(input, size_table + at * 2)? as usize,
            8 => *input.get(size_table + at).ok_or(VideoError::UnexpectedEnd)? as usize,
            _ => {
                let byte = *input
                    .get(size_table + at / 2)
                    .ok_or(VideoError::UnexpectedEnd)?;
                (if at.is_multiple_of(2) { byte >> 4 } else { byte & 15 }) as usize
            }
        })
    };

    let mut stts = RunCursor::new(find_box(input, start, end, b"stts")?);
    let mut ctts = RunCursor::new(find_box(input, start, end, b"ctts")?);
    let stss = find_box(input, start, end, b"stss")?;
    let mut sync_at = 0usize;
    let sync_count = stss.map_or(0, |stss| {
        (u32_at(input, stss.payload + 4).unwrap_or(0) as usize)
            .min(stss.end.saturating_sub(stss.payload + 8) / 4)
    });

    let mut stsc_at = 0usize;
    let mut sample = 0usize;
    let mut dts = 0i64;
    'chunks: for chunk in 0..chunks {
        while stsc_at + 1 < stsc_entries
            && stsc_entry(input, stsc, stsc_at + 1)?.0 as usize <= chunk + 1
        {
            stsc_at += 1;
        }
        let per_chunk = stsc_entry(input, stsc, stsc_at)?.1;
        let mut offset = if chunk_size == 8 {
            usize::try_from(u64_at(input, chunk_box.payload + 8 + chunk * 8)?)
                .map_err(|_| VideoError::Overflow)?
        } else {
            u32_at(input, chunk_box.payload + 8 + chunk * 4)? as usize
        };
        for _ in 0..per_chunk {
            if sample >= total {
                break 'chunks;
            }
            let size = sample_size(sample)?;
            // Tolerate a file cut off inside its media data.
            if offset.checked_add(size).is_none_or(|e| e > file_len) {
                break 'chunks;
            }
            let delta = stts.next(input);
            // Composition offsets are signed in version 1, and in practice
            // version 0 files use the same encoding.
            let composition = ctts.next(input) as i32 as i64;
            let pts = dts + composition - track.edit_start;
            dts = dts.saturating_add(delta as i64);

            let mut flags = 0;
            if let Some(stss) = stss {
                while sync_at < sync_count {
                    let number = u32_at(input, stss.payload + 8 + sync_at * 4)? as usize;
                    if number > sample + 1 {
                        break;
                    }
                    sync_at += 1;
                    if number == sample + 1 {
                        flags = FLAG_KEY;
                    }
                }
            } else {
                flags = FLAG_KEY;
            }
            index.push(
                limits,
                file_len,
                offset,
                size,
                ticks_to_ms(pts, track.timescale) + track.edit_delay_ms,
                flags,
            )?;
            offset += size;
            sample += 1;
        }
    }
    Ok(())
}

#[derive(Default)]
struct FragmentStats {
    samples: u32,
    ticks: u64,
}

/// Trex defaults for the track, from `moov/mvex`.
#[derive(Clone, Copy, Default)]
struct SampleDefaults {
    duration: u32,
    size: u32,
    flags: u32,
}

fn trex_defaults(input: &[u8], track_id: u32) -> Result<SampleDefaults> {
    let mut defaults = SampleDefaults::default();
    let mut pos = 0;
    while let Some(top) = read_top_box(input, pos) {
        pos = top.next;
        if &top.kind != b"moov" {
            continue;
        }
        let Some(mvex) = children(input, top, b"mvex")? else {
            continue;
        };
        let mut at = mvex.payload;
        while let Some(child) = read_box(input, at, mvex.end)? {
            at = child.next;
            if &child.kind == b"trex" && u32_at(input, child.payload + 4)? == track_id {
                defaults = SampleDefaults {
                    duration: u32_at(input, child.payload + 12)?,
                    size: u32_at(input, child.payload + 16)?,
                    flags: u32_at(input, child.payload + 20)?,
                };
            }
        }
    }
    Ok(defaults)
}

const MAX_FRAGMENT_SAMPLES: u32 = 1 << 24;

/// A `sample_flags` value's sample_is_non_sync_sample bit.
const NON_SYNC: u32 = 0x0001_0000;

/// The name of the box [`source_head`] adds after the `moof`s it keeps: the file's length,
/// then for each `moof`, where it is in the head and where it is in the file.
const MAP_BOX: &[u8; 4] = b"feMp";

struct FragmentMap<'a> {
    file_len: usize,
    entries: &'a [u8],
}

impl FragmentMap<'_> {
    /// Where a `moof` in the head came from. They're in order, and few are looked up twice.
    fn file_offset(&self, head_offset: usize) -> Option<usize> {
        let count = self.entries.len() / 16;
        let (mut low, mut high) = (0, count);
        while low < high {
            let middle = (low + high) / 2;
            let entry = &self.entries[middle * 16..middle * 16 + 16];
            let at = u64::from_be_bytes(entry[..8].try_into().ok()?) as usize;
            match at.cmp(&head_offset) {
                core::cmp::Ordering::Equal => {
                    return usize::try_from(u64::from_be_bytes(entry[8..].try_into().ok()?)).ok();
                }
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
            }
        }
        None
    }
}

fn fragment_map(input: &[u8]) -> Option<FragmentMap<'_>> {
    let mut pos = 0;
    while let Some(top) = read_top_box(input, pos) {
        if pos == top.next {
            return None;
        }
        pos = top.next;
        if &top.kind == MAP_BOX {
            let body = input.get(top.payload..top.end)?;
            let file_len = usize::try_from(u64::from_be_bytes(body.get(..8)?.try_into().ok()?)).ok()?;
            return Some(FragmentMap { file_len, entries: body.get(8..)? });
        }
    }
    None
}

/// Walks every `moof`, calling `f(offset, size, (duration, composition), flags)`
/// for each sample of the track.
fn walk_fragments<F>(
    input: &[u8],
    track_id: u32,
    mut f: F,
    stats: &mut FragmentStats,
) -> Result<()>
where
    F: FnMut(usize, usize, (u32, i32), u8) -> Result<()>,
{
    let trex = trex_defaults(input, track_id)?;
    // With only the metadata in memory, there's a map of where it all came from.
    let map = fragment_map(input);
    let limit = map.as_ref().map_or(input.len(), |map| map.file_len);
    let mut pos = 0;
    while let Some(top) = read_top_box(input, pos) {
        pos = top.next;
        if &top.kind != b"moof" {
            continue;
        }
        let moof_start = top.payload - 8;
        let moof_in_file = match &map {
            Some(map) => map.file_offset(moof_start).ok_or(VideoError::InvalidData("MP4 moof map"))?,
            None => moof_start,
        };
        let mut at = top.payload;
        while let Some(traf) = read_box(input, at, top.end)? {
            at = traf.next;
            if &traf.kind != b"traf" {
                continue;
            }
            let tfhd = required_box(input, traf.payload, traf.end, b"tfhd")?;
            if u32_at(input, tfhd.payload + 4)? != track_id && track_id != 0 {
                continue;
            }
            let tf_flags = u32_at(input, tfhd.payload)? & 0x00ff_ffff;
            let mut field = tfhd.payload + 8;
            let mut base = moof_in_file;
            if tf_flags & 0x1 != 0 {
                base = usize::try_from(u64_at(input, field)?).map_err(|_| VideoError::Overflow)?;
                field += 8;
            }
            if tf_flags & 0x2 != 0 {
                field += 4; // sample_description_index
            }
            let mut defaults = trex;
            if tf_flags & 0x8 != 0 {
                defaults.duration = u32_at(input, field)?;
                field += 4;
            }
            if tf_flags & 0x10 != 0 {
                defaults.size = u32_at(input, field)?;
                field += 4;
            }
            if tf_flags & 0x20 != 0 {
                defaults.flags = u32_at(input, field)?;
            }
            // Runs without their own offset continue where the last one ended.
            let mut next_offset = base;
            let mut child_at = traf.payload;
            while let Some(trun) = read_box(input, child_at, traf.end)? {
                child_at = trun.next;
                if &trun.kind != b"trun" {
                    continue;
                }
                let version_flags = u32_at(input, trun.payload)?;
                let version = version_flags >> 24;
                let flags = version_flags & 0x00ff_ffff;
                let count = u32_at(input, trun.payload + 4)?;
                let mut cursor = trun.payload + 8;
                let mut offset = next_offset;
                if flags & 0x1 != 0 {
                    let delta = u32_at(input, cursor)? as i32;
                    offset = (base as i64)
                        .checked_add(delta as i64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or(VideoError::InvalidData("MP4 trun offset"))?;
                    cursor += 4;
                }
                let mut first_flags = None;
                if flags & 0x4 != 0 {
                    first_flags = Some(u32_at(input, cursor)?);
                    cursor += 4;
                }
                let entry_size = [0x100u32, 0x200, 0x400, 0x800]
                    .iter()
                    .filter(|&&bit| flags & bit != 0)
                    .count()
                    * 4;
                // Entries must fit the box unless each is implicit, and implicit
                // samples still have to fit in the file below.
                if entry_size != 0 && (count as usize).saturating_mul(entry_size) > trun.end.saturating_sub(cursor)
                {
                    return Err(VideoError::UnexpectedEnd);
                }
                for i in 0..count {
                    let mut duration = defaults.duration;
                    let mut size = defaults.size;
                    let mut sample_flags = if i == 0 {
                        first_flags.unwrap_or(defaults.flags)
                    } else {
                        defaults.flags
                    };
                    let mut composition = 0i32;
                    if flags & 0x100 != 0 {
                        duration = u32_at(input, cursor)?;
                        cursor += 4;
                    }
                    if flags & 0x200 != 0 {
                        size = u32_at(input, cursor)?;
                        cursor += 4;
                    }
                    if flags & 0x400 != 0 {
                        sample_flags = u32_at(input, cursor)?;
                        cursor += 4;
                    }
                    if flags & 0x800 != 0 {
                        let raw = u32_at(input, cursor)?;
                        composition = if version == 0 {
                            raw.min(i32::MAX as u32) as i32
                        } else {
                            raw as i32
                        };
                        cursor += 4;
                    }
                    let size = size as usize;
                    let end = offset.checked_add(size).ok_or(VideoError::Overflow)?;
                    if end > limit {
                        return Err(VideoError::UnexpectedEnd);
                    }
                    let key = if sample_flags & NON_SYNC == 0 { FLAG_KEY } else { 0 };
                    f(offset, size, (duration, composition), key)?;
                    // Zero-sized samples take no file space, so the count must
                    // be bounded for itself.
                    if stats.samples >= MAX_FRAGMENT_SAMPLES {
                        return Err(VideoError::LimitExceeded("too many frames"));
                    }
                    stats.samples += 1;
                    stats.ticks = stats.ticks.saturating_add(duration as u64);
                    offset = end;
                }
                next_offset = offset;
            }
        }
    }
    Ok(())
}

/// Reads a descriptor's tag and length, MPEG-4 style. Returns the tag, where its
/// payload starts, and where it ends.
fn read_descriptor(data: &[u8], pos: usize, end: usize) -> Option<(u8, usize, usize)> {
    let tag = *data.get(pos)?;
    let mut at = pos + 1;
    let mut len = 0usize;
    for _ in 0..4 {
        if at >= end {
            return None;
        }
        let byte = *data.get(at)?;
        at += 1;
        len = (len << 7) | (byte & 0x7f) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let stop = at.checked_add(len)?;
    (stop <= end).then_some((tag, at, stop))
}

/// Finds the object type and decoder configuration in an `esds` box.
fn parse_esds(data: &[u8], esds: BoxHeader) -> Option<(u8, core::ops::Range<usize>)> {
    let (tag, mut pos, end) = read_descriptor(data, esds.payload + 4, esds.end)?;
    if tag != 3 {
        return None;
    }
    let flags = *data.get(pos + 2)?;
    pos += 3;
    if flags & 0x80 != 0 {
        pos += 2;
    }
    if flags & 0x40 != 0 {
        pos += 1 + *data.get(pos)? as usize;
    }
    if flags & 0x20 != 0 {
        pos += 2;
    }
    let (tag, config, end) = read_descriptor(data, pos, end)?;
    if tag != 4 {
        return None;
    }
    let object = *data.get(config)?;
    let mut config_range = 0..0;
    if config + 13 < end {
        if let Some((5, start, stop)) = read_descriptor(data, config + 13, end) {
            config_range = start..stop;
        }
    }
    Some((object, config_range))
}

/// The packets of a sample table, sample by sample, or chunk by chunk when
/// `chunk_frame_bytes` says each sample is one PCM frame.
fn audio_packets(
    input: &[u8],
    file_len: usize,
    stbl: BoxHeader,
    timescale: u32,
    start_ticks: i64,
    chunk_frame_bytes: Option<usize>,
) -> Option<Vec<AudioPacket>> {
    let stsz = children(input, stbl, b"stsz").ok()??;
    let stsc = children(input, stbl, b"stsc").ok()??;
    let stts = children(input, stbl, b"stts").ok()?;
    let (stco, wide) = match children(input, stbl, b"stco").ok()? {
        Some(stco) => (stco, false),
        None => (children(input, stbl, b"co64").ok()??, true),
    };
    let fixed = u32_at(input, stsz.payload + 4).ok()? as usize;
    let samples = u32_at(input, stsz.payload + 8).ok()? as usize;
    let chunks = u32_at(input, stco.payload + 4).ok()? as usize;
    let stsc_count = u32_at(input, stsc.payload + 4).ok()? as usize;
    let mut packets = Vec::new();
    let mut times = RunCursor::new(stts);
    let mut ticks = 0i64;
    let mut sample = 0usize;
    let mut stsc_at = 0usize;
    let (_, mut per_chunk) = stsc_entry(input, stsc, 0).ok()?;
    let mut next_first = if stsc_count > 1 {
        stsc_entry(input, stsc, 1).ok()?.0 as usize
    } else {
        usize::MAX
    };
    for chunk in 0..chunks {
        if chunk + 1 >= next_first {
            stsc_at += 1;
            per_chunk = stsc_entry(input, stsc, stsc_at).ok()?.1;
            next_first = if stsc_at + 1 < stsc_count {
                stsc_entry(input, stsc, stsc_at + 1).ok()?.0 as usize
            } else {
                usize::MAX
            };
        }
        let mut offset = if wide {
            usize::try_from(u64_at(input, stco.payload + 8 + chunk * 8).ok()?).ok()?
        } else {
            u32_at(input, stco.payload + 8 + chunk * 4).ok()? as usize
        };
        let pts = |ticks: i64| ticks_to_ms(ticks - start_ticks, timescale).max(0) as u64;
        if let Some(frame_bytes) = chunk_frame_bytes {
            let count = (per_chunk as usize).min(samples.saturating_sub(sample));
            if count == 0 {
                break;
            }
            let size = count.checked_mul(frame_bytes)?;
            push_packet(&mut packets, file_len, offset, size, pts(ticks))?;
            for _ in 0..count {
                ticks = ticks.saturating_add(times.next(input) as i64);
            }
            sample += count;
            continue;
        }
        for _ in 0..per_chunk {
            if sample >= samples {
                break;
            }
            let size = if fixed != 0 {
                fixed
            } else {
                u32_at(input, stsz.payload + 12 + sample * 4).ok()? as usize
            };
            push_packet(&mut packets, file_len, offset, size, pts(ticks))?;
            offset = offset.checked_add(size)?;
            ticks = ticks.saturating_add(times.next(input) as i64);
            sample += 1;
        }
    }
    Some(packets)
}

/// Finds the first audio track.
pub(crate) fn audio_track(input: &[u8]) -> Option<AudioTrack> {
    audio_track_sized(input, input.len())
}

/// [`audio_track`] with only the metadata in memory, as [`build_index_sized`].
pub(crate) fn audio_track_sized(input: &[u8], file_len: usize) -> Option<AudioTrack> {
    let mut moov = None;
    let mut pos = 0;
    while let Some(header) = read_top_box(input, pos) {
        if &header.kind == b"moov" {
            moov = Some(header);
            break;
        }
        pos = header.next;
    }
    let moov = moov?;
    let mut pos = moov.payload;
    while let Some(trak) = read_box(input, pos, moov.end).ok()? {
        pos = trak.next;
        if &trak.kind != b"trak" {
            continue;
        }
        let Some(mdia) = children(input, trak, b"mdia").ok()? else {
            continue;
        };
        let Some(hdlr) = children(input, mdia, b"hdlr").ok()? else {
            continue;
        };
        if input.get(hdlr.payload + 8..hdlr.payload + 12) != Some(b"soun") {
            continue;
        }
        let mdhd = children(input, mdia, b"mdhd").ok()??;
        let (timescale, _) = mdhd_timescale(input, mdhd).ok()?;
        let minf = children(input, mdia, b"minf").ok()??;
        let stbl = children(input, minf, b"stbl").ok()??;
        let stsd = children(input, stbl, b"stsd").ok()??;
        let entry = read_box(input, stsd.payload + 8, stsd.end).ok()??;
        let at = entry.payload;
        let version = u16::from_be_bytes(input.get(at + 8..at + 10)?.try_into().ok()?);
        let channels = u16::from_be_bytes(input.get(at + 16..at + 18)?.try_into().ok()?);
        let bits = u16::from_be_bytes(input.get(at + 18..at + 20)?.try_into().ok()?);
        let rate = u16::from_be_bytes(input.get(at + 24..at + 26)?.try_into().ok()?) as u32;
        // QuickTime versions 1 and 2 add fields before any child boxes.
        let first_child = at
            + match version {
                1 => 28 + 16,
                2 => 28 + 36,
                _ => 28,
            };
        let esds = find_box(input, first_child, entry.end, b"esds").ok().flatten();
        let mut config = 0..0;
        let pcm = |bytes: u8, big_endian: bool, float: bool| AudioCodec::Pcm {
            bytes,
            big_endian,
            float,
        };
        let codec = match &entry.kind {
            b"mp4a" => match esds.and_then(|esds| parse_esds(input, esds)) {
                Some((0x40 | 0x66 | 0x67 | 0x68, range)) => {
                    config = range;
                    AudioCodec::Aac
                }
                Some((0x69 | 0x6b, _)) => AudioCodec::Mp3,
                _ => AudioCodec::Other,
            },
            b".mp3" => AudioCodec::Mp3,
            b"sowt" => pcm(2, false, false),
            b"twos" if bits <= 8 => pcm(1, true, false),
            b"twos" => pcm(2, true, false),
            b"in24" => pcm(3, true, false),
            b"in32" => pcm(4, true, false),
            b"fl32" => pcm(4, true, true),
            b"fl64" => pcm(8, true, true),
            b"ulaw" => AudioCodec::MuLaw,
            b"alaw" => AudioCodec::ALaw,
            b"Opus" => {
                if let Ok(Some(dops)) = find_box(input, first_child, entry.end, b"dOps") {
                    config = dops.payload..dops.end;
                }
                AudioCodec::Opus
            }
            _ => AudioCodec::Other,
        };
        // Edit lists delay or trim: an AAC encoder's priming is usually in there.
        let start_ticks = children(input, trak, b"edts")
            .ok()
            .flatten()
            .and_then(|edts| children(input, edts, b"elst").ok().flatten())
            .and_then(|elst| {
                if *input.get(elst.payload)? == 1 {
                    u64_at(input, elst.payload + 16).ok().map(|ticks| ticks as i64)
                } else {
                    u32_at(input, elst.payload + 12).ok().map(|ticks| ticks as i32 as i64)
                }
            })
            .unwrap_or(0)
            .max(0);
        let frame_bytes = match codec {
            AudioCodec::Pcm { bytes, .. } => Some(bytes as usize * channels as usize),
            AudioCodec::MuLaw | AudioCodec::ALaw => Some(channels as usize),
            _ => None,
        };
        let fragmented = children(input, moov, b"mvex").ok().flatten().is_some();
        let mut packets = match audio_packets(input, file_len, stbl, timescale, start_ticks, frame_bytes) {
            Some(packets) => packets,
            None if fragmented => Vec::new(),
            None => return None,
        };
        // Nothing in the sample table, and the movie has fragments: they're in there.
        if packets.is_empty() && fragmented {
            let id = children(input, trak, b"tkhd")
                .ok()
                .flatten()
                .and_then(|tkhd| parse_tkhd(input, tkhd).ok())
                .map(|tkhd| tkhd.id)?;
            let mut stats = FragmentStats::default();
            let mut dts = 0i64;
            let _ = walk_fragments(
                input,
                id,
                |offset, size, timing, _| {
                    let pts = (dts + timing.1 as i64 - start_ticks).max(0);
                    dts = dts.saturating_add(timing.0 as i64);
                    crate::audio::push_packet(&mut packets, file_len, offset, size, ticks_to_ms(pts, timescale).max(0) as u64)
                        .ok_or(VideoError::LimitExceeded("too many audio packets"))
                },
                &mut stats,
            );
        }
        return Some(AudioTrack {
            codec,
            sample_rate: rate,
            channels,
            config,
            packets,
        });
    }
    None
}

/// The largest `moov` read into memory. A sample table is about 10 bytes a frame.
const MAX_HEAD: u64 = 24 * 1024 * 1024;

/// A file's metadata, for one too large to read: its `ftyp` and `moov`, one after the
/// other. Offsets in the sample tables are the file's, so they don't care where this
/// `moov` ended up. Fragments keep their tables all over the file, so in a fragmented file every
/// `moof` comes along too, with a map of where it was, as a run's offsets can be from its `moof`.
pub(crate) fn source_head(window: &mut feos_source::Window<'_>) -> Result<alloc::vec::Vec<u8>> {
    let file_len = window.file_len();
    let mut head = alloc::vec::Vec::new();
    let mut found = false;
    walk_top_boxes(window, file_len, |window, kind, at, size| {
        match kind {
            b"ftyp" => read_box_into(&mut head, window, at, size)?,
            b"moov" => {
                read_box_into(&mut head, window, at, size)?;
                found = true;
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    })?;
    if !found {
        return Err(VideoError::InvalidData("MP4 file has no moov"));
    }
    if !is_fragmented(&head) {
        return Ok(head);
    }
    let mut map = alloc::vec::Vec::new();
    walk_top_boxes(window, file_len, |window, kind, at, size| {
        if kind == b"moof" {
            map.try_reserve(16).map_err(|_| VideoError::OutOfMemory)?;
            map.extend_from_slice(&(head.len() as u64).to_be_bytes());
            map.extend_from_slice(&at.to_be_bytes());
            read_box_into(&mut head, window, at, size)?;
        }
        Ok(true)
    })?;
    if (head.len() as u64).saturating_add(16 + map.len() as u64) > MAX_HEAD {
        return Err(VideoError::LimitExceeded("MP4 metadata is too large"));
    }
    let size = 8 + 8 + map.len();
    head.try_reserve_exact(size).map_err(|_| VideoError::OutOfMemory)?;
    head.extend_from_slice(&(size as u32).to_be_bytes());
    head.extend_from_slice(MAP_BOX);
    head.extend_from_slice(&file_len.to_be_bytes());
    head.extend_from_slice(&map);
    Ok(head)
}

/// Calls `f` with each top-level box's kind, offset and size, until it says to stop.
fn walk_top_boxes(
    window: &mut feos_source::Window<'_>,
    file_len: u64,
    mut f: impl FnMut(&mut feos_source::Window<'_>, &[u8; 4], u64, u64) -> Result<bool>,
) -> Result<()> {
    let mut pos = 0u64;
    while pos + 8 <= file_len {
        let header = window.bytes(pos, 16);
        if header.len() < 8 {
            break;
        }
        let short = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let kind = [header[4], header[5], header[6], header[7]];
        let size = match short {
            0 => file_len - pos,
            1 => {
                if header.len() < 16 {
                    break;
                }
                u64::from_be_bytes(header[8..16].try_into().unwrap_or([0; 8]))
            }
            size => size,
        };
        if size < 8 || pos.saturating_add(size) > file_len && &kind != b"mdat" {
            break;
        }
        if !f(window, &kind, pos, size)? {
            break;
        }
        pos = pos.saturating_add(size);
    }
    Ok(())
}

fn read_box_into(
    head: &mut alloc::vec::Vec<u8>,
    window: &mut feos_source::Window<'_>,
    at: u64,
    size: u64,
) -> Result<()> {
    if (head.len() as u64).saturating_add(size) > MAX_HEAD {
        return Err(VideoError::LimitExceeded("MP4 metadata is too large"));
    }
    let start = head.len();
    head.try_reserve_exact(size as usize).map_err(|_| VideoError::OutOfMemory)?;
    head.resize(start + size as usize, 0);
    let mut done = 0u64;
    while done < size {
        let chunk = window.bytes(at + done, (size - done) as usize);
        if chunk.is_empty() {
            return Err(VideoError::UnexpectedEnd);
        }
        let count = chunk.len();
        head[start + done as usize..start + done as usize + count].copy_from_slice(chunk);
        done += count as u64;
    }
    Ok(())
}

/// Whether a head's `moov` says the samples are in fragments.
fn is_fragmented(head: &[u8]) -> bool {
    find_video_track(head).is_ok_and(|track| track.fragmented)
}
