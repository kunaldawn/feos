//! H.264 / AVC glue around the vendored `feos-h264` decoder.
//!
//! The decoder itself works on NAL units and returns planar 4:2:0 frames. This
//! module reads stream properties for `info` and adapts Annex-B and AVCC
//! (length-prefixed) samples into RGBA frames.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::error::{require_rgba, Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_CHECKED, FLAG_KEY, FLAG_PARAMS};
use crate::pixel::{self, YuvCoefficients};

use feos_h264::{
    annex_b_ranges, annex_b_units, avcc_ranges, avcc_units, parse_avcc_config, parse_nal_bytes,
    sps_info, DecodeError, Frame, FrameBoundary, Limits, NalUnit, NalUnitType, OrderedDecoder,
    SpsInfo,
};

/// Reads the properties of the first SPS of an `avcC` record.
pub(crate) fn info_from_avcc(config: &[u8]) -> Result<SpsInfo> {
    let avcc = parse_avcc_config(config).map_err(VideoError::InvalidData)?;
    let sps = avcc
        .sps_nals
        .iter()
        .find(|nal| nal.nal_unit_type == NalUnitType::Sps)
        .ok_or(VideoError::InvalidData("avcC SPS count"))?;
    Ok(sps_info(&sps.rbsp)?)
}

/// Reads the properties of the first SPS of an Annex-B stream.
pub(crate) fn info_from_annex_b(input: &[u8]) -> Result<SpsInfo> {
    for nal in annex_b_units(input) {
        if nal.nal_unit_type == NalUnitType::Sps {
            return Ok(sps_info(&nal.rbsp)?);
        }
    }
    Err(VideoError::InvalidData("H.264 stream has no SPS"))
}

/// Reads the properties of the first SPS inside a container sample.
pub(crate) fn info_from_sample(sample: &[u8], length_size: Option<usize>) -> Option<SpsInfo> {
    match length_size {
        Some(length_size) => avcc_units(sample, length_size)
            .find(|nal| nal.nal_unit_type == NalUnitType::Sps)
            .and_then(|nal| sps_info(&nal.rbsp).ok()),
        None => annex_b_units(sample)
            .find(|nal| nal.nal_unit_type == NalUnitType::Sps)
            .and_then(|nal| sps_info(&nal.rbsp).ok()),
    }
}

/// Counts frames in an Annex-B stream, two field pictures making one.
pub(crate) fn annex_b_frame_count(input: &[u8]) -> u32 {
    let mut count = 0u32;
    let mut frames = FrameBoundary::new();
    for range in annex_b_ranges(input) {
        if frames.starts_frame(&input[range]) {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Splits an Annex-B stream into samples, one per frame, whose two fields stay together.
/// Parameter sets and SEI before a picture belong to that picture's sample.
pub(crate) fn build_annex_b_index(input: &[u8], limits: &DecoderLimits) -> Result<Index> {
    let mut index = Index::default();
    // Where the current access unit started, and where the next one may start.
    let mut unit_start = None;
    let mut next_start = 0usize;
    let mut has_picture = false;
    let mut frames = FrameBoundary::new();
    for range in annex_b_ranges(input) {
        let nal = &input[range.clone()];
        let vcl = matches!(nal.first().map(|h| h & 0x1f), Some(1 | 5));
        if frames.starts_frame(nal) {
            if has_picture {
                if let Some(start) = unit_start {
                    index.push(limits, input.len(), start, next_start - start, 0, FLAG_KEY)?;
                }
                unit_start = Some(next_start);
            }
            has_picture = true;
        }
        if unit_start.is_none() {
            // Keep the start code: a sample is itself an Annex-B stream.
            unit_start = Some(range.start.saturating_sub(3));
        }
        // Non-VCL NALs after a picture belong to the next access unit, so the
        // unit boundary stays just after the last slice.
        if vcl || !has_picture {
            next_start = range.end;
        }
    }
    if has_picture {
        if let Some(start) = unit_start {
            index.push(limits, input.len(), start, input.len() - start, 0, FLAG_KEY)?;
        }
    }
    Ok(index)
}

/// Bytes of a NAL unit kept while walking a file: more than any parameter set here, and what a
/// slice has to say about where it belongs comes first.
const NAL_PREFIX: usize = 4096;
/// How much of a raw stream is read at a time when walking it.
const WALK_CHUNK: usize = 1 << 20;
/// Most parameter sets a raw stream may have, in bytes.
const MAX_PARAMETER_BYTES: usize = 1 << 20;

/// What walking a raw stream on disk found.
pub(crate) struct WalkedStream {
    pub index: Index,
    /// The first of the file, with every different parameter set after it.
    pub head: Vec<u8>,
    pub configs: Vec<(u32, core::ops::Range<usize>)>,
}

/// The same as [`build_annex_b_index`] for a stream that stays on disk. It reads the whole file
/// once. Key frames are known exactly, and parameter sets are kept, for seeking past them.
pub(crate) fn walk_annex_b(
    file: &dyn feos_source::Source,
    len: usize,
    limits: &DecoderLimits,
    head: Vec<u8>,
) -> Result<WalkedStream> {
    let mut walk = Walk {
        limits,
        len,
        index: Index::default(),
        head,
        configs: Vec::new(),
        seen: Vec::new(),
        frames: FrameBoundary::new(),
        unit_start: None,
        next_start: 0,
        has_picture: false,
        first_vcl: None,
        head_base: 0,
    };
    walk.head_base = walk.head.len();
    let mut chunk = feos_source::zeroed(WALK_CHUNK).ok_or(VideoError::OutOfMemory)?;
    let mut prefix = [0u8; NAL_PREFIX];
    let mut prefix_len = 0;
    let mut current: Option<usize> = None;
    let mut pos = 0usize;
    while pos < len {
        let read = file.read_full(pos as u64, &mut chunk);
        if read == 0 {
            break;
        }
        let bytes = &chunk[..read];
        let last = pos + read >= len;
        // A start code can straddle chunks, so the last two bytes are looked at again.
        let limit = if last { read } else { read.saturating_sub(2) };
        let mut i = 0;
        while i + 3 <= read.min(limit + 2) {
            if bytes[i + 2] > 1 {
                i += 3;
                continue;
            }
            if bytes[i] != 0 || bytes[i + 1] != 0 || bytes[i + 2] != 1 {
                i += 1;
                continue;
            }
            let at = pos + i;
            if let Some(start) = current {
                // A four-byte start code's extra zero isn't part of this NAL.
                let end = if at > start && i > 0 && bytes[i - 1] == 0 { at - 1 } else { at };
                walk.nal(start, end, &prefix[..prefix_len])?;
            }
            let start = at + 3;
            current = Some(start);
            let from = i + 3;
            let wanted = NAL_PREFIX.min(len - start);
            if from + wanted <= read {
                prefix[..wanted].copy_from_slice(&bytes[from..from + wanted]);
                prefix_len = wanted;
            } else {
                prefix_len = file.read_full(start as u64, &mut prefix[..wanted]);
            }
            i += 3;
        }
        if read <= 2 {
            break;
        }
        pos += if last { read } else { read - 2 };
    }
    if let Some(start) = current {
        walk.nal(start, len, &prefix[..prefix_len])?;
    }
    walk.finish()
}

/// State of [`walk_annex_b`], which follows [`build_annex_b_index`].
struct Walk<'a> {
    limits: &'a DecoderLimits,
    len: usize,
    index: Index,
    head: Vec<u8>,
    configs: Vec<(u32, core::ops::Range<usize>)>,
    /// Hashes of the parameter sets kept so far.
    seen: Vec<u64>,
    frames: FrameBoundary,
    unit_start: Option<usize>,
    next_start: usize,
    has_picture: bool,
    first_vcl: Option<u8>,
    head_base: usize,
}

impl Walk<'_> {
    fn nal(&mut self, start: usize, end: usize, prefix: &[u8]) -> Result<()> {
        let kind = prefix.first().map(|header| header & 0x1f);
        let vcl = matches!(kind, Some(1 | 5));
        if self.frames.starts_frame(prefix) {
            if self.has_picture {
                if let Some(unit) = self.unit_start {
                    self.push_unit(unit, self.next_start)?;
                }
                self.unit_start = Some(self.next_start);
            }
            self.has_picture = true;
        }
        if self.unit_start.is_none() {
            self.unit_start = Some(start.saturating_sub(3));
        }
        if vcl || !self.has_picture {
            self.next_start = end;
        }
        if vcl && self.first_vcl.is_none() {
            self.first_vcl = kind;
        }
        if matches!(kind, Some(7 | 8)) {
            self.keep_parameters(prefix.get(..end - start).unwrap_or(prefix))?;
        }
        Ok(())
    }

    fn push_unit(&mut self, start: usize, end: usize) -> Result<()> {
        let flags = FLAG_CHECKED | if self.first_vcl == Some(5) { FLAG_KEY } else { 0 };
        self.index
            .push(self.limits, self.len, start, end - start, 0, flags)?;
        self.first_vcl = None;
        Ok(())
    }

    fn keep_parameters(&mut self, nal: &[u8]) -> Result<()> {
        let hash = nal
            .iter()
            .fold(0xcbf29ce484222325u64, |value, &byte| (value ^ byte as u64).wrapping_mul(0x100000001b3));
        if self.seen.contains(&hash) {
            return Ok(());
        }
        if self.head.len() - self.head_base + nal.len() > MAX_PARAMETER_BYTES {
            return Err(VideoError::LimitExceeded("too many parameter sets"));
        }
        self.seen.try_reserve(1).map_err(|_| VideoError::OutOfMemory)?;
        self.seen.push(hash);
        self.head
            .try_reserve(nal.len() + 3)
            .map_err(|_| VideoError::OutOfMemory)?;
        let from = self.head.len();
        self.head.extend_from_slice(&[0, 0, 1]);
        self.head.extend_from_slice(nal);
        self.configs
            .try_reserve(1)
            .map_err(|_| VideoError::OutOfMemory)?;
        // In force from the frame being put together.
        self.configs
            .push((self.index.samples.len() as u32, from..self.head.len()));
        Ok(())
    }

    fn finish(mut self) -> Result<WalkedStream> {
        if self.has_picture {
            if let Some(unit) = self.unit_start {
                let end = self.len;
                self.push_unit(unit, end)?;
            }
        }
        Ok(WalkedStream {
            index: self.index,
            head: self.head,
            configs: self.configs,
        })
    }
}

/// How NAL units are framed inside the samples of a stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Framing {
    AnnexB,
    /// Length prefixes of this many bytes.
    Length(usize),
}

/// Reads the NAL framing from an `avcC` record, if the track has one.
pub(crate) fn framing(config: &[u8]) -> Result<Framing> {
    if config.first() == Some(&1) {
        let avcc = parse_avcc_config(config).map_err(VideoError::InvalidData)?;
        if avcc.sps_nals.is_empty() || avcc.pps_nals.is_empty() {
            return Err(VideoError::InvalidData("avcC parameter sets"));
        }
        Ok(Framing::Length(avcc.length_size))
    } else {
        Ok(Framing::AnnexB)
    }
}

/// Classifies samples from their NAL headers. A frame is a usable seek point
/// only if the container offered it and it starts with an IDR: a non-IDR
/// "key frame" can't be decoded without what came before it.
/// Whether a sample's first picture is an IDR picture.
pub(crate) fn first_picture_is_idr(bytes: &[u8], framing: Framing) -> bool {
    let mut first_vcl = None;
    let mut visit = |nal: &[u8]| {
        let kind = nal.first().map(|header| header & 0x1f);
        if first_vcl.is_none() && matches!(kind, Some(1 | 5)) {
            first_vcl = kind;
        }
    };
    match framing {
        Framing::AnnexB => annex_b_ranges(bytes).for_each(|r| visit(&bytes[r])),
        Framing::Length(n) => avcc_ranges(bytes, n).for_each(|r| visit(&bytes[r])),
    }
    first_vcl == Some(5)
}

pub(crate) fn mark_samples(index: &mut Index, input: &[u8], framing: Framing) {
    for i in 0..index.samples.len() {
        let bytes = index.bytes(input, i);
        let mut has_params = false;
        let mut first_vcl = None;
        let mut visit = |nal: &[u8]| {
            let kind = nal.first().map(|header| header & 0x1f);
            has_params |= matches!(kind, Some(7 | 8));
            if first_vcl.is_none() && matches!(kind, Some(1 | 5)) {
                first_vcl = kind;
            }
        };
        match framing {
            Framing::AnnexB => annex_b_ranges(bytes).for_each(|r| visit(&bytes[r])),
            Framing::Length(n) => avcc_ranges(bytes, n).for_each(|r| visit(&bytes[r])),
        }
        let sample = &mut index.samples[i];
        if first_vcl != Some(5) {
            sample.flags &= !FLAG_KEY;
        }
        if has_params {
            sample.flags |= FLAG_PARAMS;
        }
    }
}

fn frame_to_rgba(frame: &Frame, coefficients: &YuvCoefficients, out: &mut [u8]) -> Result<()> {
    require_rgba(out, frame.width, frame.height)?;
    let width = frame.width as usize;
    for (row, line) in out
        .chunks_exact_mut(width * 4)
        .take(frame.height as usize)
        .enumerate()
    {
        let chroma = row / 2;
        pixel::yuv420_row(
            coefficients,
            frame.y_row(row),
            frame.u_row(chroma),
            frame.v_row(chroma),
            line,
        );
    }
    Ok(())
}

/// A stateful decoder over the samples of one H.264 track.
pub(crate) struct H264Stream {
    decoder: OrderedDecoder,
    framing: Framing,
    /// `avcC` records in the input, and the sample each one applies from.
    configs: Vec<(u32, core::ops::Range<usize>)>,
    next_sample: usize,
    /// Display index of the next frame out of the decoder.
    next_output: u32,
    ready: VecDeque<Frame>,
    flushed: bool,
    started: bool,
    /// The last frame returned, so a repeat request costs only a conversion.
    held: Option<(u32, Frame)>,
    last_error: Option<VideoError>,
    coefficients: YuvCoefficients,
}

impl H264Stream {
    pub(crate) fn new(
        framing: Framing,
        configs: Vec<(u32, core::ops::Range<usize>)>,
        coefficients: YuvCoefficients,
        limits: &DecoderLimits,
    ) -> Self {
        Self {
            decoder: OrderedDecoder::with_limits(Limits {
                max_width: limits.max_width,
                max_height: limits.max_height,
                max_memory: limits.max_memory,
            }),
            framing,
            configs,
            next_sample: 0,
            next_output: 0,
            ready: VecDeque::new(),
            flushed: false,
            started: false,
            held: None,
            last_error: None,
            coefficients,
        }
    }

    /// Decodes the frame at `target` in display order into `out`.
    ///
    /// Feeds at most `budget` samples before returning `Ok(None)`, so a caller
    /// can spread a long catch-up across several calls.
    pub(crate) fn framing(&self) -> Framing {
        self.framing
    }

    pub(crate) fn advance(
        &mut self,
        input: &[u8],
        index: &Index,
        expand: &mut dyn FnMut(usize) -> Result<Option<Vec<u8>>>,
        target: u32,
        budget: u32,
        out: &mut [u8],
    ) -> Result<Option<(u32, u32, u32)>> {
        if let Some((shown, frame)) = &self.held {
            if *shown == target {
                frame_to_rgba(frame, &self.coefficients, out)?;
                return Ok(Some((frame.width, frame.height, *shown)));
            }
        }
        self.seek_if_needed(input, index, target)?;

        let mut fed = 0;
        loop {
            while let Some(frame) = self.ready.pop_front() {
                let mut shown = self.next_output;
                // An IDR's sample number is its display index: everything
                // before it is displayed first. This resynchronises the count
                // after a picture was lost to a damaged sample. It can't go back: a
                // stream may display pictures decoded after its IDR before it.
                if frame.is_idr {
                    shown = shown.max(frame.tag.min(u32::MAX as u64) as u32);
                }
                self.next_output = shown.saturating_add(1);
                if shown >= target {
                    // Hold the frame first so that a buffer too small for it can
                    // be replaced and the frame requested again.
                    let size = (frame.width, frame.height, shown);
                    frame_to_rgba(&frame, &self.coefficients, out).map_err(|error| {
                        self.held = Some((shown, frame.clone()));
                        error
                    })?;
                    self.held = Some((shown, frame));
                    return Ok(Some(size));
                }
            }
            if self.next_sample < index.samples.len() {
                if fed >= budget {
                    return Ok(None);
                }
                fed += 1;
                self.feed_sample(input, index, expand)?;
            } else if !self.flushed {
                self.flushed = true;
                for frame in self.decoder.flush() {
                    self.push_ready(frame)?;
                }
            } else {
                return Err(self.last_error.unwrap_or(VideoError::FrameOutOfRange {
                    index: target,
                    count: self.next_output,
                }));
            }
        }
    }

    fn push_ready(&mut self, frame: Frame) -> Result<()> {
        self.ready
            .try_reserve(1)
            .map_err(|_| VideoError::OutOfMemory)?;
        self.ready.push_back(frame);
        Ok(())
    }

    /// Restarts from the closest IDR if `target` is behind us, or if an IDR
    /// ahead of us makes everything before it unnecessary.
    fn seek_if_needed(&mut self, input: &[u8], index: &Index, target: u32) -> Result<()> {
        let key = crate::decoder::key_at_or_before(index, target);
        let behind = target < self.next_output;
        let skip_ahead = key > self.next_sample && (key as u32) > self.next_output;
        if self.started && !behind && !skip_ahead {
            return Ok(());
        }
        self.started = true;
        self.decoder.reset();
        self.ready.clear();
        self.flushed = false;
        self.last_error = None;
        self.held = None;

        // Parameter sets stay in force until replaced, so everything sent
        // before the seek point has to be replayed in order.
        let configs = core::mem::take(&mut self.configs);
        let mut result = Ok(());
        for (from, range) in &configs {
            if *from as usize <= key {
                result = result.and_then(|()| self.feed_config(&input[range.clone()]));
            }
        }
        self.configs = configs;
        result?;
        for i in 0..key {
            if index.samples[i].flags & FLAG_PARAMS != 0 {
                self.feed_params(index.bytes(input, i))?;
            }
        }
        self.next_sample = key;
        self.next_output = key as u32;
        Ok(())
    }

    fn feed_config(&mut self, config: &[u8]) -> Result<()> {
        if config.first() != Some(&1) {
            // Some muxers store plain Annex-B parameter sets here.
            for nal in annex_b_units(config) {
                self.decode_nal(&nal)?;
            }
            return Ok(());
        }
        let avcc = parse_avcc_config(config).map_err(VideoError::InvalidData)?;
        for nal in avcc.sps_nals.iter().chain(avcc.pps_nals.iter()) {
            self.decode_nal(nal)?;
        }
        Ok(())
    }

    fn feed_params(&mut self, sample: &[u8]) -> Result<()> {
        let visit = |this: &mut Self, nal: &[u8]| -> Result<()> {
            if matches!(nal.first().map(|header| header & 0x1f), Some(7 | 8)) {
                if let Some(nal) = parse_nal_bytes(nal) {
                    this.decode_nal(&nal)?;
                }
            }
            Ok(())
        };
        match self.framing {
            Framing::AnnexB => {
                for range in annex_b_ranges(sample) {
                    visit(self, &sample[range])?;
                }
            }
            Framing::Length(n) => {
                for range in avcc_ranges(sample, n) {
                    visit(self, &sample[range])?;
                }
            }
        }
        Ok(())
    }

    fn feed_sample(
        &mut self,
        input: &[u8],
        index: &Index,
        expand: &mut dyn FnMut(usize) -> Result<Option<Vec<u8>>>,
    ) -> Result<()> {
        let at = self.next_sample;
        self.next_sample += 1;
        // A sequence header in the middle of the stream applies from here on.
        let configs = core::mem::take(&mut self.configs);
        let mut result = Ok(());
        for (from, range) in &configs {
            if *from as usize == at && at != 0 {
                result = result.and_then(|()| self.feed_config(&input[range.clone()]));
            }
        }
        self.configs = configs;
        result?;
        self.decoder.set_tag(at as u64);
        let expanded = expand(at);
        let expanded = match expanded {
            Ok(buffer) => buffer,
            Err(error) if error.is_fatal() => return Err(error),
            Err(error) => {
                self.last_error = Some(error);
                return Ok(());
            }
        };
        let sample = expanded.as_deref().unwrap_or(index.bytes(input, at));
        match self.framing {
            Framing::AnnexB => {
                for nal in annex_b_units(sample) {
                    self.decode_nal(&nal)?;
                }
            }
            Framing::Length(n) => {
                for nal in avcc_units(sample, n) {
                    self.decode_nal(&nal)?;
                }
            }
        }
        Ok(())
    }

    /// Feeds one NAL. A damaged one only loses its picture, so decoding carries
    /// on; only errors that later data can't cure are returned.
    fn decode_nal(&mut self, nal: &NalUnit<'_>) -> Result<()> {
        match self.decoder.decode_nal(nal) {
            Ok(frames) => {
                for frame in frames {
                    self.push_ready(frame)?;
                }
                Ok(())
            }
            Err(error @ (DecodeError::Unsupported(_) | DecodeError::OutOfMemory)) => {
                Err(error.into())
            }
            Err(error) => {
                self.last_error = Some(error.into());
                Ok(())
            }
        }
    }
}
