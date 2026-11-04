//! FLV demuxing (Sorenson H.263, VP6, AVC and HEVC tags).

use alloc::vec::Vec;

use crate::bitstream::{u24_be_at, u32_at};
use crate::codec::Codec;
use crate::error::{Result, VideoError};
use crate::h264;
use crate::index::{try_push, DecoderLimits, Index, FLAG_KEY};
use crate::{DisplayMetadata, StreamColour};

/// Sequence headers kept per file. One is normal; each costs a parse on a seek.
const MAX_SEQUENCE_HEADERS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct FlvInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
    pub display: DisplayMetadata,
    pub colour: StreamColour,
}

struct VideoTag<'a> {
    codec: Codec,
    frame_type: u8,
    packet_type: u8,
    /// AVC composition time offset in milliseconds.
    composition_ms: i32,
    body: &'a [u8],
}

/// Parses a video tag. A tag too short to hold its headers carries no video,
/// which is not a reason to refuse the file.
fn video_tag(data: &[u8]) -> Option<VideoTag<'_>> {
    let first = *data.first()?;
    let frame_type = first >> 4;
    let codec = Codec::from_flv(first & 0x0f);
    if codec == Codec::H264 || codec == Codec::Hevc {
        let header = data.get(1..5)?;
        let raw = u32::from_be_bytes([0, header[1], header[2], header[3]]);
        // Sign-extend 24 bits.
        let composition_ms = ((raw << 8) as i32) >> 8;
        Some(VideoTag {
            codec,
            frame_type,
            packet_type: header[0],
            composition_ms,
            body: &data[5..],
        })
    } else {
        Some(VideoTag {
            codec,
            frame_type,
            packet_type: 1,
            composition_ms: 0,
            body: &data[1..],
        })
    }
}

/// Iterates FLV tags, calling `f(tag_type, timestamp_ms, body)`. A truncated
/// last tag ends the walk: what came before it still plays.
fn for_each_tag<'a, F>(input: &'a [u8], mut f: F) -> Result<()>
where
    F: FnMut(u8, u32, &'a [u8]) -> Result<bool>,
{
    if input.len() < 9 || &input[..3] != b"FLV" {
        return Err(VideoError::UnknownFormat);
    }
    let offset = u32_at(input, 5)? as usize;
    if offset < 9 || offset > input.len() {
        return Err(VideoError::InvalidData("FLV data offset"));
    }
    // The header is followed by the size of the (nonexistent) previous tag.
    let mut pos = offset.checked_add(4).ok_or(VideoError::Overflow)?;
    if pos > input.len() {
        return Err(VideoError::UnexpectedEnd);
    }
    while pos.saturating_add(11) <= input.len() {
        let tag_type = input[pos];
        let size = u24_be_at(input, pos + 1)? as usize;
        let timestamp = u24_be_at(input, pos + 4)? | ((input[pos + 7] as u32) << 24);
        let data_start = pos + 11;
        let end = data_start.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > input.len() {
            return Ok(());
        }
        if f(tag_type, timestamp, &input[data_start..end])? {
            return Ok(());
        }
        pos = end + 4;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Metadata {
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<(u32, u32)>,
}

fn fps_fraction(fps: f64) -> Option<(u32, u32)> {
    if !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    let common = [
        (23.976, (24_000, 1_001)),
        (29.97, (30_000, 1_001)),
        (47.952, (48_000, 1_001)),
        (59.94, (60_000, 1_001)),
    ];
    for (rate, fraction) in common {
        let difference = if fps > rate { fps - rate } else { rate - fps };
        if difference < 0.02 {
            return Some(fraction);
        }
    }
    if fps > 1_000_000.0 {
        return None;
    }
    let scaled = fps * 1000.0;
    let numerator = (scaled + 0.5) as u32;
    crate::reduce_fraction(numerator, 1000)
}

struct Amf<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Amf<'a> {
    fn byte(&mut self) -> Option<u8> {
        let value = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(value)
    }

    fn u16be(&mut self) -> Option<u16> {
        let bytes = self.bytes(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32be(&mut self) -> Option<u32> {
        let bytes = self.bytes(4)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn f64be(&mut self) -> Option<f64> {
        let b = self.bytes(8)?;
        Some(f64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let bytes = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        self.bytes(len).map(|_| ())
    }
}

fn update_metadata(meta: &mut Metadata, name: &[u8], value: f64) {
    match name {
        b"width" => meta.width = Some(value.clamp(0.0, 16384.0) as u32),
        b"height" => meta.height = Some(value.clamp(0.0, 16384.0) as u32),
        b"framerate" => meta.frame_rate = fps_fraction(value),
        _ => {}
    }
}

/// Deepest AMF nesting followed. Real metadata uses two or three levels, and
/// the parsers recurse, so this bounds the stack use of a hostile script tag.
const MAX_AMF_DEPTH: u32 = 16;

fn skip_amf_value(amf: &mut Amf<'_>, meta: &mut Metadata, depth: u32) -> Option<()> {
    let marker = amf.byte()?;
    skip_amf_payload(amf, marker, meta, depth)
}

fn skip_amf_payload(amf: &mut Amf<'_>, marker: u8, meta: &mut Metadata, depth: u32) -> Option<()> {
    if depth >= MAX_AMF_DEPTH {
        return None;
    }
    match marker {
        0x00 => {
            amf.f64be()?;
        }
        0x01 => amf.skip(1)?,
        0x02 => {
            let len = amf.u16be()? as usize;
            amf.skip(len)?;
        }
        0x03 => amf_property_list(amf, meta, depth + 1)?,
        0x05 | 0x06 => {}
        0x07 => amf.skip(2)?,
        0x08 => {
            amf.u32be()?;
            amf_property_list(amf, meta, depth + 1)?;
        }
        0x0a => {
            let count = amf.u32be()?;
            for _ in 0..count {
                skip_amf_value(amf, meta, depth + 1)?;
            }
        }
        0x0b => amf.skip(10)?,
        0x0c => {
            let len = amf.u32be()? as usize;
            amf.skip(len)?;
        }
        _ => return None,
    }
    Some(())
}

fn amf_property_list(amf: &mut Amf<'_>, meta: &mut Metadata, depth: u32) -> Option<()> {
    loop {
        if amf.data.get(amf.pos..).and_then(|s| s.get(..3))? == [0, 0, 0x09] {
            amf.pos += 3;
            return Some(());
        }
        let len = amf.u16be()? as usize;
        let name = amf.bytes(len)?;
        let marker = amf.byte()?;
        match marker {
            0x00 => {
                let value = amf.f64be()?;
                update_metadata(meta, name, value);
            }
            0x09 => return Some(()),
            _ => skip_amf_payload(amf, marker, meta, depth)?,
        }
    }
}

fn metadata(body: &[u8]) -> Option<Metadata> {
    let mut meta = Metadata {
        width: None,
        height: None,
        frame_rate: None,
    };
    let mut amf = Amf { data: body, pos: 0 };
    // The tag usually starts with the AMF string "onMetaData".
    if amf.byte()? != 0x02 {
        return None;
    }
    let name_len = amf.u16be()? as usize;
    amf.skip(name_len)?;
    match amf.byte()? {
        0x08 => {
            amf.u32be()?;
            amf_property_list(&mut amf, &mut meta, 0)?;
        }
        0x03 => amf_property_list(&mut amf, &mut meta, 0)?,
        _ => return None,
    }
    Some(Metadata {
        width: meta.width.filter(|&w| w != 0),
        height: meta.height.filter(|&h| h != 0),
        frame_rate: meta.frame_rate,
    })
}

pub(crate) fn info(input: &[u8]) -> Result<FlvInfo> {
    let mut codec = Codec::Unknown(0);
    let mut frame_count = 0u32;
    let mut sps_found = false;
    let mut dimensions = None;
    let mut frame_rate = None;
    let mut display = DisplayMetadata::default();
    let mut colour = StreamColour::default();
    for_each_tag(input, |tag_type, _, body| {
        match tag_type {
            9 => {
                let Some(tag) = video_tag(body) else {
                    return Ok(false);
                };
                codec = tag.codec;
                if tag.codec == Codec::H264 && tag.packet_type == 0 {
                    if !sps_found {
                        if let Ok(sps) = h264::info_from_avcc(tag.body) {
                            sps_found = true;
                            dimensions = Some((sps.width, sps.height));
                            crate::merge_sps_metadata(&mut display, &mut colour, &sps);
                        }
                    }
                } else if tag.packet_type == 1 && tag.frame_type != 5 {
                    frame_count = frame_count.saturating_add(1);
                }
            }
            18 => {
                if let Some(meta) = metadata(body) {
                    if dimensions.is_none() {
                        if let (Some(w), Some(h)) = (meta.width, meta.height) {
                            dimensions = Some((w, h));
                        }
                    }
                    if frame_rate.is_none() {
                        frame_rate = meta.frame_rate;
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    })?;
    let (width, height) = dimensions.unwrap_or((0, 0));
    Ok(FlvInfo {
        width,
        height,
        codec,
        frame_count,
        frame_rate,
        display,
        colour,
    })
}

/// A sample table and the AVC sequence headers that apply to it.
pub(crate) struct FlvIndex {
    pub index: Index,
    /// Each sequence header, and the sample it applies from.
    pub configs: Vec<(u32, core::ops::Range<usize>)>,
}

pub(crate) fn build_index(input: &[u8], limits: &DecoderLimits) -> Result<FlvIndex> {
    let mut index = Index::default();
    let mut configs = Vec::new();
    for_each_tag(input, |tag_type, timestamp, body| {
        if tag_type != 9 {
            return Ok(false);
        }
        let Some(tag) = video_tag(body) else {
            return Ok(false);
        };
        if tag.codec != Codec::H264 {
            return Ok(false);
        }
        let offset = tag.body.as_ptr() as usize - input.as_ptr() as usize;
        match tag.packet_type {
            0 => {
                if configs.len() >= MAX_SEQUENCE_HEADERS {
                    return Err(VideoError::LimitExceeded("too many AVC sequence headers"));
                }
                try_push(
                    &mut configs,
                    (
                        index.samples.len() as u32,
                        offset..offset + tag.body.len(),
                    ),
                )?;
            }
            1 if tag.frame_type != 5 => {
                let pts = timestamp as i32 as i64 + tag.composition_ms as i64;
                let flags = if tag.frame_type == 1 { FLAG_KEY } else { 0 };
                index.push(limits, input.len(), offset, tag.body.len(), pts, flags)?;
            }
            _ => {}
        }
        Ok(false)
    })?;
    Ok(FlvIndex { index, configs })
}
