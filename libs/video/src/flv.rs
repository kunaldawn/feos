//! FLV demuxing (Sorenson H.263, VP6, AVC and HEVC tags).

use crate::bitstream::{u24_be_at, u32_at};
use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::h264::{AnnexBDecoder, AvccDecoder};
use crate::{h264, mjpeg, raw};

#[derive(Clone, Copy, Debug)]
pub(crate) struct FlvInfo {
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    pub frame_count: u32,
    pub frame_rate: Option<(u32, u32)>,
}

struct VideoTag<'a> {
    codec: Codec,
    frame_type: u8,
    packet_type: u8,
    body: &'a [u8],
}

fn video_tag(data: &[u8]) -> Result<VideoTag<'_>> {
    if data.is_empty() {
        return Err(VideoError::UnexpectedEnd);
    }
    let frame_type = data[0] >> 4;
    let codec_id = data[0] & 0x0f;
    let codec = Codec::from_flv(codec_id);
    if codec == Codec::H264 || codec == Codec::Hevc {
        if data.len() < 5 {
            return Err(VideoError::UnexpectedEnd);
        }
        Ok(VideoTag {
            codec,
            frame_type,
            packet_type: data[1],
            body: &data[5..],
        })
    } else {
        Ok(VideoTag {
            codec,
            frame_type,
            packet_type: 1,
            body: &data[1..],
        })
    }
}

/// Iterates FLV tags, calling `f(tag_type, body)`.
fn for_each_tag<'a, F>(input: &'a [u8], mut f: F) -> Result<()>
where
    F: FnMut(u8, &'a [u8]) -> Result<bool>,
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
    while pos + 11 <= input.len() {
        let tag_type = input[pos];
        let size = u24_be_at(input, pos + 1)? as usize;
        let data_start = pos + 11;
        let end = data_start.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > input.len() {
            return Err(VideoError::UnexpectedEnd);
        }
        if f(tag_type, &input[data_start..end])? {
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
        b"width" => meta.width = Some(value.max(0.0) as u32),
        b"height" => meta.height = Some(value.max(0.0) as u32),
        b"framerate" => meta.frame_rate = fps_fraction(value),
        _ => {}
    }
}

fn skip_amf_value(amf: &mut Amf<'_>, meta: &mut Metadata) -> Option<()> {
    match amf.byte()? {
        0x00 => {
            amf.f64be()?;
        }
        0x01 => amf.skip(1)?,
        0x02 => {
            let len = amf.u16be()? as usize;
            amf.skip(len)?;
        }
        0x03 => amf_property_list(amf, meta)?,
        0x05 | 0x06 => {}
        0x07 => amf.skip(2)?,
        0x08 => {
            amf.u32be()?;
            amf_property_list(amf, meta)?;
        }
        0x0a => {
            let count = amf.u32be()?;
            for _ in 0..count {
                skip_amf_value(amf, meta)?;
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

fn amf_property_list(amf: &mut Amf<'_>, meta: &mut Metadata) -> Option<()> {
    loop {
        if amf.data.get(amf.pos..).and_then(|s| s.get(..3))? == [0, 0, 0x09] {
            amf.pos += 3;
            return Some(());
        }
        let len = amf.u16be()? as usize;
        let name = amf.bytes(len)?;
        match amf.byte()? {
            0x00 => {
                let value = amf.f64be()?;
                update_metadata(meta, name, value);
            }
            0x01 => amf.skip(1)?,
            0x02 => {
                let len = amf.u16be()? as usize;
                amf.skip(len)?;
            }
            0x03 => amf_property_list(amf, meta)?,
            0x05 | 0x06 => {}
            0x07 => amf.skip(2)?,
            0x08 => {
                amf.u32be()?;
                amf_property_list(amf, meta)?;
            }
            0x09 => return Some(()),
            0x0a => {
                let count = amf.u32be()?;
                for _ in 0..count {
                    skip_amf_value(amf, meta)?;
                }
            }
            0x0b => amf.skip(10)?,
            0x0c => {
                let len = amf.u32be()? as usize;
                amf.skip(len)?;
            }
            _ => return None,
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
            amf_property_list(&mut amf, &mut meta)?;
        }
        0x03 => amf_property_list(&mut amf, &mut meta)?,
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
    let mut dimensions = None;
    let mut frame_rate = None;
    let mut config: &[u8] = &[];
    for_each_tag(input, |tag_type, body| {
        match tag_type {
            9 => {
                let tag = video_tag(body)?;
                codec = tag.codec;
                if tag.codec == Codec::H264 && tag.packet_type == 0 {
                    config = tag.body;
                    if let Ok(info) = h264::info_from_avcc(tag.body) {
                        dimensions = Some((info.width, info.height));
                    }
                } else if tag.packet_type == 1 {
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
    if codec == Codec::H264 && dimensions.is_none() && !config.is_empty() {
        let info = h264::info_from_avcc(config)?;
        dimensions = Some((info.width, info.height));
    }
    let (width, height) = dimensions.unwrap_or((0, 0));
    Ok(FlvInfo {
        width,
        height,
        codec,
        frame_count,
        frame_rate,
    })
}

pub(crate) fn decode_frame(input: &[u8], index: u32, out: &mut [u8]) -> Result<u32> {
    let info = info(input)?;
    if info.codec != Codec::H264 {
        return Err(VideoError::UnsupportedCodec(info.codec.name()));
    }
    if index >= info.frame_count {
        return Err(VideoError::FrameOutOfRange {
            index,
            count: info.frame_count,
        });
    }
    require_rgba(out, info.width, info.height)?;

    let mut config: &[u8] = &[];
    for_each_tag(input, |tag_type, body| {
        if tag_type == 9 {
            let tag = video_tag(body)?;
            if tag.codec == Codec::H264 && tag.packet_type == 0 {
                config = tag.body;
                return Ok(true);
            }
        }
        Ok(false)
    })?;

    if !config.is_empty() {
        let mut decoder = AvccDecoder::new(config, index)?;
        let mut done = false;
        for_each_tag(input, |tag_type, body| {
            if tag_type != 9 {
                return Ok(false);
            }
            let tag = video_tag(body)?;
            if tag.codec != Codec::H264 || tag.packet_type != 1 {
                return Ok(false);
            }
            if decoder.push(tag.body, out)? {
                done = true;
                return Ok(true);
            }
            Ok(false)
        })?;
        if !done && decoder.finish(out)? {
            done = true;
        }
        if done {
            return Ok(0);
        }
    } else {
        let mut decoder = AnnexBDecoder::new(index);
        let mut done = false;
        for_each_tag(input, |tag_type, body| {
            if tag_type != 9 {
                return Ok(false);
            }
            let tag = video_tag(body)?;
            if tag.codec != Codec::H264 || tag.packet_type != 1 {
                return Ok(false);
            }
            if decoder.push(tag.body, out)? {
                done = true;
                return Ok(true);
            }
            Ok(false)
        })?;
        if !done && decoder.finish(out)? {
            done = true;
        }
        if done {
            return Ok(0);
        }
    }
    Err(VideoError::FrameOutOfRange {
        index,
        count: info.frame_count,
    })
}

// These helpers keep the module symmetric with the other containers; FLV
// samples for the codecs we can decode in future are exposed through
// `video_tag`.
#[allow(dead_code)]
fn _reserved(tag: VideoTag<'_>) -> (u8, u8, &[u8]) {
    (tag.frame_type, tag.packet_type, tag.body)
}

#[allow(dead_code)]
fn _decode_mjpeg(sample: &[u8], out: &mut [u8]) -> Result<()> {
    mjpeg::decode_sample(sample, out)
}

#[allow(dead_code)]
fn _decode_raw(sample: &[u8], codec: Codec, w: u32, h: u32, out: &mut [u8]) -> Result<()> {
    raw::decode(sample, codec, w, h, false, out)
}
