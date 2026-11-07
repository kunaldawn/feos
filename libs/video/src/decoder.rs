//! The stateful decoder: [`VideoDecoder`] opens a file once, keeps a sample
//! table and codec state, and decodes each next frame in constant time instead
//! of restarting from frame 0.

use alloc::boxed::Box;
use alloc::vec::Vec;

use feos_source::{Source, Window};

use crate::codec::Codec;
use crate::error::{require_rgba, Result, VideoError};
use crate::gif::GifStream;
use crate::h264::{self, Framing, H264Stream};
use crate::index::{DecoderLimits, Index, FLAG_CHECKED, FLAG_KEY};
use crate::matroska::Compression;
use crate::pixel::{matrix_for, YuvCoefficients};
use crate::vp8::Vp8Stream;
use crate::{avi, flv, gif, ivf, matroska, mjpeg, mp4, mpeg1, raw, rle};
use crate::{detect, info, ContainerFormat, DecodedFrame, VideoInfo};

/// What a walk through a file on disk reads at a time. Most steps are a few bytes, close
/// to the last.
pub(crate) const STREAM_WINDOW: usize = 16 * 1024;
/// Frame rate assumed for streams with no timing at all.
const DEFAULT_FPS: (u32, u32) = (25, 1);
/// Largest decompressed frame accepted from a zlib-compressed Matroska track.
const MAX_INFLATED_SAMPLE: usize = 16 * 1024 * 1024;

/// The last frame at or before `target` that decoding can start from.
pub(crate) fn key_at_or_before(index: &Index, target: u32) -> usize {
    let end = (target as usize).saturating_add(1).min(index.samples.len());
    index.samples[..end]
        .iter()
        .rposition(|sample| sample.is_key())
        .unwrap_or(0)
}

/// How to turn a sample into pixels.
enum State {
    /// Every frame stands alone: Motion JPEG and uncompressed video.
    Intra,
    H264(H264Stream),
    Gif(GifStream),
    Rle(RleStream),
    Mpeg1(mpeg1::Mpeg1Stream),
    Vp8(Vp8Stream),
    /// No decoder for this codec. The file can still be inspected.
    None,
}

/// The canvas of a delta-coded RLE stream.
struct RleStream {
    canvas: Vec<u8>,
    /// Frames `..next` have been applied to the canvas.
    next: u32,
}

/// A video opened for decoding. Decoding the next frame costs one frame, and a
/// seek costs the frames since the closest key frame. Generic over who owns the
/// input, so it can borrow a slice or own a buffer: `VideoDecoder<&[u8]>`,
/// `VideoDecoder<Vec<u8>>`.
///
/// ```
/// # use feos_video::VideoDecoder;
/// # let gif: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\x00\x00\x00\x00\x00\
/// #     \x21\xf9\x04\x00\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\
/// #     \x02\x02\x44\x01\x00\x3b";
/// let mut decoder = VideoDecoder::open(gif).unwrap();
/// let mut rgba = vec![0u8; decoder.info().rgba_len().unwrap()];
/// for index in 0..decoder.frame_count() {
///     let frame = decoder.decode(index, &mut rgba).unwrap();
///     println!("{} ms: {}x{}", frame.pts_ms, frame.width, frame.height);
/// }
/// ```
pub struct VideoDecoder<S> {
    /// The file, or for a [`from_source`](VideoDecoder::from_source) decoder, the
    /// part of it that holds metadata.
    source: S,
    /// Where samples come from when they aren't in `source`.
    file: Option<Box<dyn Source>>,
    info: VideoInfo,
    index: Index,
    state: State,
    compression: Compression,
    /// DIB row layout and palette for AVI-style uncompressed samples.
    dib: Option<bool>,
    palette: core::ops::Range<usize>,
}

impl VideoDecoder<Vec<u8>> {
    /// Opens a video that stays where it is, with only its metadata and sample table in
    /// memory. A sample is read when it's decoded.
    ///
    /// This walks the container, so it takes as long as the disk does. MP4, fragmented or not,
    /// Matroska and WebM, AVI, FLV and MPEG program streams can do this. Anything else that has
    /// to be searched for its frames can't, and comes back as [`VideoError::LimitExceeded`].
    pub fn from_source(file: Box<dyn Source>, limits: DecoderLimits) -> Result<Self> {
        let mut compression = Compression::None;
        let mut dib = None;
        let mut palette = 0..0;
        let mut configs = Vec::new();
        let mut mpeg1_stream = None;
        let len = file.len();
        if len > usize::MAX as u64 {
            return Err(VideoError::Overflow);
        }
        let mut window = Window::new(&*file, STREAM_WINDOW).ok_or(VideoError::OutOfMemory)?;
        let format = detect(window.bytes(0, 4096)).ok_or(VideoError::UnknownFormat)?;
        let (head, mut index) = match format {
            ContainerFormat::Mp4 => {
                let head = mp4::source_head(&mut window)?;
                let (index, config) = mp4::build_index_sized(&head, len as usize, &limits)?;
                push_config(&mut configs, config)?;
                (head, index)
            }
            ContainerFormat::Matroska => {
                let (head, segment_end) = matroska::source_head(&mut window)?;
                let built = matroska::build_index_source(&head, &mut window, segment_end, &limits)?;
                compression = built.compression;
                dib = built.dib;
                palette = built.palette;
                push_config(&mut configs, built.config)?;
                (head, built.index)
            }
            ContainerFormat::Avi => {
                let head = avi::source_head(&mut window)?;
                let built = avi::build_index_source(&head, &mut window, &limits)?;
                dib = built.dib;
                palette = built.extra.clone();
                push_config(&mut configs, built.extra)?;
                (head, built.index)
            }
            ContainerFormat::Flv => {
                let mut head = flv::source_head(&mut window)?;
                let built = flv::build_index_source(&mut head, &mut window, &limits)?;
                configs = built.configs;
                (head, built.index)
            }
            ContainerFormat::Ivf => {
                let (head, mut index) = ivf::build_index_source(&mut window, &limits)?;
                let rate = ivf::info(&head)?.frame_rate;
                index.set_uniform_timing(Some(rate.unwrap_or(DEFAULT_FPS)));
                (head, index)
            }
            ContainerFormat::Mpeg1 => {
                if window.bytes(0, 4).get(3) != Some(&mpeg1::PACK) {
                    return Err(VideoError::LimitExceeded("this kind of file can't be streamed"));
                }
                let (stream, head) = mpeg1::Mpeg1Stream::open_source(&*file, &limits)?;
                let mut index = stream.build_index(len as usize, &limits)?;
                index.set_uniform_timing(Some(mpeg1::info(&head)?.frame_rate));
                mpeg1_stream = Some(stream);
                (head, index)
            }
            _ => return Err(VideoError::LimitExceeded("this kind of file can't be streamed")),
        };
        drop(window);
        let mut info = info(&head)?;
        if info.width > limits.max_width || info.height > limits.max_height {
            return Err(VideoError::LimitExceeded("frame size"));
        }
        index.finish_timing(delay_for(info.frame_rate.unwrap_or(DEFAULT_FPS)))?;
        info.frame_count = index.samples.len() as u32;
        Self::assemble(
            head,
            Some(file),
            format,
            info,
            index,
            configs,
            compression,
            dib,
            palette,
            mpeg1_stream,
            &limits,
        )
    }
}

impl<S: AsRef<[u8]>> VideoDecoder<S> {
    /// A key frame of a file on disk is only what the container calls one, and H.264
    /// decoding can only start from an IDR picture. Reads the one before `target`
    /// to see, and looks further back if it isn't.
    fn check_key(&mut self, target: u32) {
        let Some(file) = self.file.as_deref() else {
            return;
        };
        let State::H264(stream) = &self.state else {
            return;
        };
        let framing = stream.framing();
        loop {
            let at = key_at_or_before(&self.index, target);
            let Some(sample) = self.index.samples.get(at).copied() else {
                return;
            };
            if at == 0 || sample.flags & FLAG_CHECKED != 0 || !sample.is_key() {
                return;
            }
            let Ok(bytes) = read_sample(file, &self.index, at) else {
                return;
            };
            let idr = h264::first_picture_is_idr(&bytes, framing);
            let entry = &mut self.index.samples[at];
            entry.flags |= FLAG_CHECKED;
            if idr {
                return;
            }
            entry.flags &= !FLAG_KEY;
        }
    }

    /// Opens a video with the default limits.
    pub fn open(source: S) -> Result<Self> {
        Self::with_limits(source, DecoderLimits::default())
    }

    /// Opens a video, refusing one beyond `limits`.
    pub fn with_limits(source: S, limits: DecoderLimits) -> Result<Self> {
        let input = source.as_ref();
        let mut info = info(input)?;
        let format = detect(input).ok_or(VideoError::UnknownFormat)?;
        if info.width > limits.max_width || info.height > limits.max_height {
            return Err(VideoError::LimitExceeded("frame size"));
        }
        let fallback_delay = delay_for(info.frame_rate.unwrap_or(DEFAULT_FPS));

        let mut compression = Compression::None;
        let mut dib = None;
        let mut palette = 0..0;
        let mut configs = Vec::new();
        let mut uniform = false;
        let mut mpeg1_stream = None;
        let mut index = match format {
            ContainerFormat::Mpeg1 => {
                uniform = true;
                let stream = mpeg1::Mpeg1Stream::open(input, &limits)?;
                let index = stream.build_index(input.len(), &limits)?;
                mpeg1_stream = Some(stream);
                index
            }
            ContainerFormat::Gif => gif::build_index(input, &limits)?,
            ContainerFormat::Mp4 => {
                let (index, config) = mp4::build_index(input, &limits)?;
                push_config(&mut configs, config)?;
                index
            }
            ContainerFormat::Matroska => {
                let built = matroska::build_index(input, &limits)?;
                compression = built.compression;
                dib = built.dib;
                palette = built.palette;
                push_config(&mut configs, built.config)?;
                built.index
            }
            ContainerFormat::Avi => {
                let built = avi::build_index(input, &limits)?;
                dib = built.dib;
                palette = built.extra.clone();
                push_config(&mut configs, built.extra)?;
                built.index
            }
            ContainerFormat::Flv => {
                let built = flv::build_index(input, &limits)?;
                configs = built.configs;
                built.index
            }
            ContainerFormat::H264 => {
                uniform = true;
                h264::build_annex_b_index(input, &limits)?
            }
            ContainerFormat::MotionJpeg => {
                uniform = true;
                mjpeg::build_index(input, &limits)?
            }
            ContainerFormat::Ivf if info.codec == Codec::Vp8 => {
                uniform = true;
                ivf::build_index(input, &limits)?
            }
            ContainerFormat::Ivf => Index::default(),
        };
        if uniform {
            index.set_uniform_timing(Some(info.frame_rate.unwrap_or(DEFAULT_FPS)));
        }
        let gif_last_delay = index.last_delay_ms;
        index.finish_timing(fallback_delay)?;
        if format == ContainerFormat::Gif {
            index.last_delay_ms = gif_last_delay;
        }
        if format != ContainerFormat::Ivf || !index.samples.is_empty() {
            info.frame_count = index.samples.len() as u32;
        }

        Self::assemble(
            source,
            None,
            format,
            info,
            index,
            configs,
            compression,
            dib,
            palette,
            mpeg1_stream,
            &limits,
        )
    }

    /// Everything that doesn't care how the index was found.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        source: S,
        file: Option<Box<dyn Source>>,
        format: ContainerFormat,
        info: VideoInfo,
        mut index: Index,
        configs: Vec<(u32, core::ops::Range<usize>)>,
        compression: Compression,
        dib: Option<bool>,
        palette: core::ops::Range<usize>,
        mpeg1_stream: Option<mpeg1::Mpeg1Stream>,
        limits: &DecoderLimits,
    ) -> Result<Self> {
        let input = source.as_ref();
        let state = match info.codec {
            Codec::Mpeg1 if mpeg1_stream.is_some() => {
                mpeg1_stream.map_or(State::None, State::Mpeg1)
            }
            Codec::Gif => State::Gif(GifStream::new(info.width, info.height)?),
            Codec::H264 => {
                let config = configs
                    .first()
                    .map_or(&[][..], |(_, range): &(u32, core::ops::Range<usize>)| {
                        &input[range.clone()]
                    });
                let framing = if format == ContainerFormat::H264 {
                    Framing::AnnexB
                } else {
                    h264::framing(config)?
                };
                // That reads every sample, so a file on disk checks its key frames as
                // it gets to them, in `check_key`.
                if compression == Compression::None && file.is_none() {
                    h264::mark_samples(&mut index, input, framing);
                }
                let matrix = matrix_for(info.colour.matrix, info.height);
                let coefficients = YuvCoefficients::new(matrix, info.colour.full_range);
                State::H264(H264Stream::new(framing, configs, coefficients, limits))
            }
            Codec::Vp8 => {
                let matrix = matrix_for(info.colour.matrix, info.height);
                let coefficients = YuvCoefficients::new(matrix, info.colour.full_range);
                State::Vp8(Vp8Stream::new(limits.max_memory, coefficients))
            }
            Codec::Rle8 | Codec::Rle4 => {
                let len = info.rgba_len()?;
                let mut canvas = Vec::new();
                canvas
                    .try_reserve_exact(len)
                    .map_err(|_| VideoError::OutOfMemory)?;
                canvas.resize(len, 0);
                State::Rle(RleStream { canvas, next: 0 })
            }
            codec if codec.can_decode() => {
                for sample in &mut index.samples {
                    sample.flags |= FLAG_KEY;
                }
                State::Intra
            }
            _ => State::None,
        };
        Ok(Self {
            source,
            file,
            info,
            index,
            state,
            compression,
            dib,
            palette,
        })
    }

    /// The properties of the video. `frame_count` is what the index found,
    /// which is what can actually be played.
    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    /// The input this decoder reads.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Takes the input back.
    pub fn into_source(self) -> S {
        self.source
    }

    /// Number of frames.
    pub fn frame_count(&self) -> u32 {
        self.index.samples.len() as u32
    }

    /// Total running time in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.index.duration_ms()
    }

    /// When a frame (numbered in display order) is shown.
    pub fn frame_time_ms(&self, index: u32) -> Option<u64> {
        self.index.time_ms(index).map(u64::from)
    }

    /// How long a frame stays on screen.
    pub fn frame_delay_ms(&self, index: u32) -> u32 {
        self.index.delay_ms(index)
    }

    /// The frame on screen at a time.
    pub fn frame_at_time(&self, ms: u64) -> u32 {
        self.index.frame_at_time(ms)
    }

    /// The closest frame at or before `index` that decoding can start from.
    pub fn key_frame_at_or_before(&self, index: u32) -> u32 {
        key_at_or_before(&self.index, index) as u32
    }

    /// The next frame after `index` that decoding can start from, if any.
    pub fn key_frame_after(&self, index: u32) -> Option<u32> {
        let from = (index as usize).saturating_add(1);
        let tail = self.index.samples.get(from..)?;
        let at = tail.iter().position(|sample| sample.is_key())?;
        Some((from + at) as u32)
    }

    /// Decodes the frame at `index` (in display order) into an RGBA8888 buffer.
    ///
    /// A damaged sample doesn't end decoding: H.264 skips the picture it held
    /// and returns the next good one, and errors that concern only one frame
    /// don't affect the next call. See [`VideoError::is_fatal`].
    pub fn decode(&mut self, index: u32, out: &mut [u8]) -> Result<DecodedFrame> {
        loop {
            if let Some(frame) = self.advance(index, out, u32::MAX)? {
                return Ok(frame);
            }
        }
    }

    /// Like [`decode`](Self::decode), but decodes at most `budget` frames before
    /// returning `Ok(None)`. Reaching a far-off frame can then be spread over
    /// several calls, so a caller on the compositor thread can keep drawing.
    pub fn advance(
        &mut self,
        index: u32,
        out: &mut [u8],
        budget: u32,
    ) -> Result<Option<DecodedFrame>> {
        let count = self.frame_count();
        self.check_key(index);
        let input = self.source.as_ref();
        let file = self.file.as_deref();
        let codec = self.info.codec;
        if matches!(self.state, State::None) {
            return Err(VideoError::UnsupportedCodec(codec.name()));
        }
        if index >= count {
            return Err(VideoError::FrameOutOfRange { index, count });
        }
        let (mut width, mut height) = (self.info.width, self.info.height);
        let mut shown = index;
        let compression = self.compression.clone();
        let table = &self.index;
        match &mut self.state {
            State::None => unreachable!(),
            State::Intra => {
                // A zero-length sample means "repeat the previous frame".
                let at = table.samples[..=index as usize]
                    .iter()
                    .rposition(|sample| sample.size > 0)
                    .ok_or(VideoError::InvalidData("video starts with an empty frame"))?;
                let expanded = expand_sample(input, file, table, at, &compression)?;
                let sample = expanded.as_deref().unwrap_or(table.bytes(input, at));
                if codec == Codec::Mjpeg {
                    (width, height) = mjpeg::decode_sample(sample, out)?;
                } else if let Some(bottom_up) = self.dib {
                    raw::decode_dib(
                        sample,
                        codec,
                        &input[self.palette.clone()],
                        width,
                        height,
                        bottom_up,
                        out,
                    )?;
                } else {
                    raw::decode(sample, codec, width, height, out)?;
                }
            }
            State::H264(stream) => {
                let mut expand = |at: usize| expand_sample(input, file, table, at, &compression);
                match stream.advance(input, table, &mut expand, index, budget, out)? {
                    Some(size) => (width, height, shown) = size,
                    None => return Ok(None),
                }
            }
            State::Mpeg1(stream) => {
                if !stream.advance(input, file, index, budget, out)? {
                    return Ok(None);
                }
            }
            State::Vp8(stream) => {
                let mut expand = |at: usize| expand_sample(input, file, table, at, &compression);
                match stream.advance(table, &mut expand, input, index, budget, out)? {
                    Some(size) => (width, height) = size,
                    None => return Ok(None),
                }
            }
            State::Gif(stream) => {
                if !stream.advance(input, table, index, budget, out)? {
                    return Ok(None);
                }
            }
            State::Rle(stream) => {
                require_rgba(out, width, height)?;
                let key = key_at_or_before(table, index) as u32;
                // Frame `next - 1` is on the canvas.
                if index + 1 < stream.next || key >= stream.next {
                    stream.next = key;
                    for pixel in stream.canvas.chunks_exact_mut(4) {
                        pixel.copy_from_slice(&[0, 0, 0, 255]);
                    }
                }
                let mut fed = 0;
                while stream.next <= index {
                    if fed >= budget {
                        return Ok(None);
                    }
                    fed += 1;
                    let at = stream.next as usize;
                    stream.next += 1;
                    let expanded = expand_sample(input, file, table, at, &compression)?;
                    let sample = expanded.as_deref().unwrap_or(table.bytes(input, at));
                    let palette = &input[self.palette.clone()];
                    // A damaged delta only loses its own changes; the next
                    // frame paints over the same canvas.
                    let _ = if codec == Codec::Rle8 {
                        rle::decode_rle8(sample, width, height, palette, &mut stream.canvas)
                    } else {
                        rle::decode_rle4(sample, width, height, palette, &mut stream.canvas)
                    };
                }
                out[..stream.canvas.len()].copy_from_slice(&stream.canvas);
            }
        }
        Ok(Some(DecodedFrame {
            width,
            height,
            delay_ms: table.delay_ms(shown),
            pts_ms: table.time_ms(shown).unwrap_or(0) as u64,
            index: shown,
        }))
    }
}

/// One sample's bytes from a file on disk.
fn read_sample(file: &dyn Source, table: &Index, at: usize) -> Result<Vec<u8>> {
    let Some(sample) = table.samples.get(at) else {
        return Ok(Vec::new());
    };
    let mut bytes = feos_source::zeroed(sample.size as usize).ok_or(VideoError::OutOfMemory)?;
    let read = file.read_full(sample.offset, &mut bytes);
    if read < bytes.len() {
        return Err(VideoError::InvalidData("the file could not be read"));
    }
    Ok(bytes)
}

fn delay_for((num, den): (u32, u32)) -> u32 {
    if num == 0 {
        return 40;
    }
    ((den as u64 * 1000) / num as u64).min(u32::MAX as u64) as u32
}

fn push_config(
    configs: &mut Vec<(u32, core::ops::Range<usize>)>,
    range: core::ops::Range<usize>,
) -> Result<()> {
    if !range.is_empty() {
        crate::index::try_push(configs, (0, range))?;
    }
    Ok(())
}

/// Rebuilds a sample of a compressed Matroska track, or `None` if it's stored
/// as it is.
fn expand_sample(
    input: &[u8],
    file: Option<&dyn Source>,
    table: &Index,
    at: usize,
    compression: &Compression,
) -> Result<Option<Vec<u8>>> {
    let loaded = match file {
        Some(file) => Some(read_sample(file, table, at)?),
        None => None,
    };
    let sample = loaded.as_deref().unwrap_or_else(|| table.bytes(input, at));
    match compression {
        Compression::None => Ok(loaded),
        Compression::HeaderStrip(prefix) => {
            let prefix = &input[prefix.clone()];
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(prefix.len() + sample.len())
                .map_err(|_| VideoError::OutOfMemory)?;
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(sample);
            Ok(Some(bytes))
        }
        Compression::Zlib => {
            let mut capacity = sample.len().saturating_mul(4).clamp(4096, MAX_INFLATED_SAMPLE);
            loop {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(capacity)
                    .map_err(|_| VideoError::OutOfMemory)?;
                bytes.resize(capacity, 0);
                match feos_compress::zlib::decompress(sample, &mut bytes) {
                    Ok(len) => {
                        bytes.truncate(len);
                        return Ok(Some(bytes));
                    }
                    Err(feos_compress::CompressError::BufferTooSmall { .. })
                        if capacity < MAX_INFLATED_SAMPLE =>
                    {
                        capacity = capacity.saturating_mul(2).min(MAX_INFLATED_SAMPLE);
                    }
                    Err(_) => return Err(VideoError::InvalidData("Matroska zlib frame")),
                }
            }
        }
    }
}
