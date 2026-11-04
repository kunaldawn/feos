//! Animated GIF decoding (GIF87a/GIF89a).
//!
//! GIF frames are composited onto an RGBA canvas. The caller supplies that
//! canvas in `out` and a scratch buffer large enough for the LZW index plane
//! plus one saved canvas (used by disposal method 3).

use alloc::vec::Vec;

use crate::error::{need, require_rgba, Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};

const MAX_CODES: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub(crate) struct GifInfo {
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
    pub delay_ms: u32,
}

struct Header<'a> {
    width: u32,
    height: u32,
    pos: usize,
    global: &'a [u8],
    background: [u8; 4],
}

fn header(input: &[u8]) -> Result<Header<'_>> {
    need(input.len(), 0, 13)?;
    if &input[..6] != b"GIF87a" && &input[..6] != b"GIF89a" {
        return Err(VideoError::UnknownFormat);
    }
    let width = u16::from_le_bytes([input[6], input[7]]) as u32;
    let height = u16::from_le_bytes([input[8], input[9]]) as u32;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(VideoError::InvalidData("GIF dimensions"));
    }
    let packed = input[10];
    let bg_index = input[11];
    let mut pos = 13;
    let mut global = &input[0..0];
    let mut background = [0, 0, 0, 0];
    if packed & 0x80 != 0 {
        let colors = 1usize << ((packed & 0x07) as usize + 1);
        let len = colors * 3;
        need(input.len(), pos, len)?;
        global = &input[pos..pos + len];
        pos += len;
        if (bg_index as usize) < colors {
            let i = bg_index as usize * 3;
            background = [global[i], global[i + 1], global[i + 2], 255];
        }
    }
    Ok(Header {
        width,
        height,
        pos,
        global,
        background,
    })
}

fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Result<usize> {
    loop {
        let n = *data.get(pos).ok_or(VideoError::UnexpectedEnd)? as usize;
        pos += 1;
        if n == 0 {
            return Ok(pos);
        }
        need(data.len(), pos, n)?;
        pos += n;
    }
}

/// One frame's headers, read from its graphic control extension and image
/// descriptor.
#[derive(Clone, Copy, Debug)]
struct FrameHeader {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    /// Local colour table, as offsets into the input.
    palette: Option<(usize, usize)>,
    interlaced: bool,
    min_code: u8,
    /// Start of the LZW sub-blocks.
    data: usize,
    /// Where the next block starts.
    next: usize,
    disposal: u8,
    transparent: Option<u8>,
    delay_ms: u32,
}

/// Reads blocks from `pos` up to and including the next frame. `None` means the
/// stream is over: at a trailer, or at the end of the file for one without
/// a trailer, which browsers and most players also accept.
fn next_frame(input: &[u8], mut pos: usize) -> Result<Option<FrameHeader>> {
    let mut disposal = 0u8;
    let mut transparent = None;
    let mut delay_ms = 0u32;
    loop {
        let Some(&block) = input.get(pos) else {
            return Ok(None);
        };
        pos += 1;
        match block {
            0x21 => {
                let Some(&label) = input.get(pos) else {
                    return Ok(None);
                };
                pos += 1;
                if label == 0xf9 {
                    let Some(&size) = input.get(pos) else {
                        return Ok(None);
                    };
                    let size = size as usize;
                    if need(input.len(), pos + 1, size).is_err() {
                        return Ok(None);
                    }
                    if size >= 4 {
                        let packed = input[pos + 1];
                        delay_ms = u16::from_le_bytes([input[pos + 2], input[pos + 3]]) as u32 * 10;
                        disposal = (packed >> 2) & 0x07;
                        transparent = if packed & 0x01 != 0 {
                            Some(input[pos + 4])
                        } else {
                            None
                        };
                    }
                    pos += 1 + size;
                }
                let Ok(next) = skip_sub_blocks(input, pos) else {
                    return Ok(None);
                };
                pos = next;
            }
            0x2c => {
                if need(input.len(), pos, 9).is_err() {
                    return Ok(None);
                }
                let left = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
                let top = u16::from_le_bytes([input[pos + 2], input[pos + 3]]) as usize;
                let width = u16::from_le_bytes([input[pos + 4], input[pos + 5]]) as usize;
                let height = u16::from_le_bytes([input[pos + 6], input[pos + 7]]) as usize;
                let packed = input[pos + 8];
                pos += 9;
                let mut palette = None;
                if packed & 0x80 != 0 {
                    let len = (1usize << ((packed & 0x07) as usize + 1)) * 3;
                    if need(input.len(), pos, len).is_err() {
                        return Ok(None);
                    }
                    palette = Some((pos, pos + len));
                    pos += len;
                }
                let Some(&min_code) = input.get(pos) else {
                    return Ok(None);
                };
                if min_code > 8 {
                    return Err(VideoError::InvalidData("GIF LZW code size"));
                }
                pos += 1;
                // A frame cut off by the end of the file shows what it has.
                let next = skip_sub_blocks(input, pos).unwrap_or(input.len());
                return Ok(Some(FrameHeader {
                    left,
                    top,
                    width,
                    height,
                    palette,
                    interlaced: packed & 0x40 != 0,
                    min_code,
                    data: pos,
                    next,
                    disposal,
                    transparent,
                    delay_ms,
                }));
            }
            0x3b => return Ok(None),
            _ => return Err(VideoError::InvalidData("GIF block")),
        }
    }
}

fn parse_info(input: &[u8]) -> Result<GifInfo> {
    let head = header(input)?;
    let mut pos = head.pos;
    let mut frames = 0u32;
    let mut first_delay = 0u32;
    loop {
        match next_frame(input, pos) {
            Ok(Some(frame)) => {
                if frames == 0 {
                    first_delay = frame.delay_ms;
                }
                frames = frames.saturating_add(1);
                pos = frame.next;
            }
            Ok(None) => break,
            // Junk after real frames ends the animation early.
            Err(_) if frames > 0 => break,
            Err(error) => return Err(error),
        }
    }
    if frames == 0 {
        return Err(VideoError::InvalidData("GIF has no frames"));
    }
    Ok(GifInfo {
        width: head.width,
        height: head.height,
        frame_count: frames,
        delay_ms: first_delay,
    })
}

pub(crate) fn info(input: &[u8]) -> Result<GifInfo> {
    parse_info(input)
}

pub(crate) fn scratch_len(input: &[u8]) -> Result<usize> {
    let info = parse_info(input)?;
    let pixels = (info.width as usize)
        .checked_mul(info.height as usize)
        .ok_or(VideoError::Overflow)?;
    pixels.checked_mul(5).ok_or(VideoError::Overflow)
}

struct SubBlocks<'a> {
    data: &'a [u8],
    pos: usize,
    left: usize,
}

impl<'a> SubBlocks<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos, left: 0 }
    }

    /// The next data byte. `None` at the terminator, or where a truncated file
    /// ends.
    fn next_byte(&mut self) -> Option<u8> {
        if self.left == 0 {
            let n = *self.data.get(self.pos)? as usize;
            self.pos += 1;
            if n == 0 {
                return None;
            }
            self.left = n;
        }
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        self.left -= 1;
        Some(byte)
    }
}

/// Decodes LZW data, passing each run of indices to `sink`. A stream that runs
/// out without an end code is treated as complete: the frame shows what it has.
fn lzw_decode(
    reader: &mut SubBlocks<'_>,
    min_code_size: u8,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<()> {
    if !(2..=8).contains(&min_code_size) {
        return Err(VideoError::InvalidData("GIF LZW minimum code size"));
    }
    let clear = 1usize << min_code_size;
    let end = clear + 1;
    let mut prefix = [0u16; MAX_CODES];
    let mut suffix = [0u8; MAX_CODES];
    let mut stack = [0u8; MAX_CODES];
    for i in 0..clear {
        suffix[i] = i as u8;
    }

    let mut code_size = min_code_size as u32 + 1;
    let mut next = end + 1;
    let mut previous: Option<usize> = None;
    let mut bits = 0u32;
    let mut bit_count = 0u32;

    loop {
        while bit_count < code_size {
            let Some(byte) = reader.next_byte() else {
                return Ok(());
            };
            bits |= (byte as u32) << bit_count;
            bit_count += 8;
        }
        let mask = (1u32 << code_size) - 1;
        let code = (bits & mask) as usize;
        bits >>= code_size;
        bit_count -= code_size;

        if code == clear {
            code_size = min_code_size as u32 + 1;
            next = end + 1;
            previous = None;
            continue;
        }
        if code == end {
            break;
        }

        let mut top = MAX_CODES;
        let first;
        if let Some(prev) = previous {
            let mut cursor;
            if code < next {
                // Ordinary case: expand the string for `code`.
                cursor = code;
                while cursor >= clear {
                    if top == 0 {
                        return Err(VideoError::InvalidData("GIF LZW chain"));
                    }
                    top -= 1;
                    stack[top] = suffix[cursor];
                    cursor = prefix[cursor] as usize;
                }
                first = suffix[cursor];
                if top == 0 {
                    return Err(VideoError::InvalidData("GIF LZW chain"));
                }
                top -= 1;
                stack[top] = first;
            } else if code == next {
                // KwKwK case: the new string is prev + first(prev).
                let mut root = prev;
                while root >= clear {
                    root = prefix[root] as usize;
                }
                first = suffix[root];
                top = MAX_CODES - 1;
                stack[MAX_CODES - 1] = first;
                cursor = prev;
                while cursor >= clear {
                    if top == 0 {
                        return Err(VideoError::InvalidData("GIF LZW chain"));
                    }
                    top -= 1;
                    stack[top] = suffix[cursor];
                    cursor = prefix[cursor] as usize;
                }
                if top == 0 {
                    return Err(VideoError::InvalidData("GIF LZW chain"));
                }
                top -= 1;
                stack[top] = suffix[cursor];
            } else {
                return Err(VideoError::InvalidData("GIF LZW code"));
            }
        } else {
            if code >= clear {
                return Err(VideoError::InvalidData("GIF first LZW code"));
            }
            first = suffix[code];
            top -= 1;
            stack[top] = first;
        }

        sink(&stack[top..]);

        if let Some(prev) = previous {
            if next < MAX_CODES {
                prefix[next] = prev as u16;
                suffix[next] = first;
                next += 1;
                if next == (1usize << code_size) && code_size < 12 {
                    code_size += 1;
                }
            }
        }
        previous = Some(code);
    }

    Ok(())
}

#[inline]
fn interlaced_row(index: usize, height: usize) -> usize {
    // GIF interlace passes start at rows 0, 4, 2 and 1.
    let mut remaining = index;
    for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
        let count = if height > start {
            (height - start + step - 1) / step
        } else {
            0
        };
        if remaining < count {
            return start + remaining * step;
        }
        remaining -= count;
    }
    index.min(height.saturating_sub(1))
}

/// Decodes one frame's pixels onto `canvas`, clipping to its bounds, so a frame
/// larger than the canvas is fine. Pixels a short stream doesn't reach keep what
/// was there.
fn draw_frame(input: &[u8], head: &Header<'_>, frame: &FrameHeader, canvas: &mut [u8]) -> Result<()> {
    if frame.width == 0 || frame.height == 0 {
        return Err(VideoError::InvalidData("GIF frame dimensions"));
    }
    let palette = frame.palette.map_or(head.global, |(a, b)| &input[a..b]);
    let canvas_width = head.width as usize;
    let canvas_height = head.height as usize;
    let mut position = 0usize;
    let total = frame.width.saturating_mul(frame.height);
    let mut reader = SubBlocks::new(input, frame.data);
    lzw_decode(&mut reader, frame.min_code, &mut |indices| {
        for &index in indices {
            if position >= total {
                return;
            }
            let src_y = position / frame.width;
            let src_x = position % frame.width;
            position += 1;
            let row = if frame.interlaced {
                interlaced_row(src_y, frame.height)
            } else {
                src_y
            };
            let y = frame.top + row;
            let x = frame.left + src_x;
            if x >= canvas_width || y >= canvas_height || frame.transparent == Some(index) {
                continue;
            }
            let pi = index as usize * 3;
            if let Some(rgb) = palette.get(pi..pi + 3) {
                let o = (y * canvas_width + x) * 4;
                canvas[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
    })
}

/// Applies a frame's disposal method once it has been shown. Disposal 2 restores
/// the frame's rectangle to transparent, which is what browsers do, rather than
/// painting the opaque background colour.
fn dispose(head: &Header<'_>, frame: &FrameHeader, canvas: &mut [u8], saved: &[u8]) {
    let canvas_width = head.width as usize;
    match frame.disposal {
        2 => {
            let bottom = (frame.top + frame.height).min(head.height as usize);
            let right = (frame.left + frame.width).min(canvas_width);
            for y in frame.top..bottom {
                for x in frame.left..right {
                    let o = (y * canvas_width + x) * 4;
                    canvas[o..o + 4].copy_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        3 => canvas.copy_from_slice(saved),
        _ => {}
    }
}

/// Decodes the frame at `index` into `out`, returning its delay in ms.
pub(crate) fn decode_frame(
    input: &[u8],
    index: u32,
    out: &mut [u8],
    scratch: &mut [u8],
) -> Result<u32> {
    let head = header(input)?;
    let canvas_len = require_rgba(out, head.width, head.height)?;
    let pixels = (head.width as usize)
        .checked_mul(head.height as usize)
        .ok_or(VideoError::Overflow)?;
    let scratch_needed = pixels.checked_mul(5).ok_or(VideoError::Overflow)?;
    if scratch.len() < scratch_needed {
        return Err(VideoError::BufferTooSmall {
            required: scratch_needed,
            available: scratch.len(),
        });
    }
    let saved = &mut scratch[pixels..pixels + canvas_len];
    let canvas = &mut out[..canvas_len];
    canvas.fill(0);

    let mut pos = head.pos;
    let mut frame_index = 0u32;
    while let Some(frame) = next_frame(input, pos)? {
        if frame.disposal == 3 {
            saved.copy_from_slice(canvas);
        }
        draw_frame(input, &head, &frame, canvas)?;
        if frame_index == index {
            return Ok(frame.delay_ms);
        }
        frame_index += 1;
        dispose(&head, &frame, canvas, saved);
        pos = frame.next;
    }
    Err(VideoError::FrameOutOfRange {
        index,
        count: frame_index,
    })
}

/// Browsers treat a delay under 20 ms as 100 ms, and animations are authored for
/// that, so an unset delay doesn't mean as fast as possible.
fn playable_delay(delay_ms: u32) -> u32 {
    if delay_ms < 20 {
        100
    } else {
        delay_ms
    }
}

/// Builds the frame table: where each frame's blocks start, when it's shown, and
/// which frames repaint the whole canvas and so need nothing before them.
pub(crate) fn build_index(input: &[u8], limits: &DecoderLimits) -> Result<Index> {
    let head = header(input)?;
    let mut index = Index::default();
    let mut pos = head.pos;
    let mut time = 0i64;
    let mut last_delay = 100;
    loop {
        let frame = match next_frame(input, pos) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) if !index.samples.is_empty() => break,
            Err(error) => return Err(error),
        };
        let covers = frame.left == 0
            && frame.top == 0
            && frame.width >= head.width as usize
            && frame.height >= head.height as usize
            && frame.transparent.is_none();
        let flags = if covers || index.samples.is_empty() {
            FLAG_KEY
        } else {
            0
        };
        index.push(limits, input.len(), pos, frame.next.max(pos) - pos, time, flags)?;
        last_delay = playable_delay(frame.delay_ms);
        time = time.saturating_add(last_delay as i64);
        pos = frame.next;
    }
    index.last_delay_ms = last_delay;
    Ok(index)
}

/// The composited animation state behind the stateful decoder.
pub(crate) struct GifStream {
    canvas: Vec<u8>,
    saved: Vec<u8>,
    /// The frame now composited on the canvas and waiting for disposal.
    shown: Option<(u32, FrameHeader)>,
}

impl GifStream {
    pub(crate) fn new(width: u32, height: u32) -> Result<Self> {
        let len = crate::pixel::rgba_len(width, height)?;
        Ok(Self {
            canvas: zeroed(len)?,
            saved: zeroed(len)?,
            shown: None,
        })
    }

    /// Composites up to `budget` frames towards `target`. Returns true when
    /// `out` holds it.
    pub(crate) fn advance(
        &mut self,
        input: &[u8],
        index: &Index,
        target: u32,
        budget: u32,
        out: &mut [u8],
    ) -> Result<bool> {
        let head = header(input)?;
        let len = self.canvas.len();
        require_rgba(out, head.width, head.height)?;
        let key = crate::decoder::key_at_or_before(index, target);
        let restart = match self.shown {
            Some((shown, _)) => target < shown || key as u32 > shown,
            None => true,
        };
        let mut next = match self.shown {
            Some((shown, _)) if !restart => shown,
            _ => key as u32,
        };
        if restart {
            self.canvas.fill(0);
            self.shown = None;
        }
        let mut fed = 0;
        loop {
            if let Some((shown, frame)) = self.shown {
                if shown == target {
                    out[..len].copy_from_slice(&self.canvas);
                    return Ok(true);
                }
                dispose(&head, &frame, &mut self.canvas, &self.saved);
                next = shown + 1;
                self.shown = None;
            }
            if fed >= budget {
                // Remember the position by re-entering from the last frame.
                return self.park(next, input, index);
            }
            fed += 1;
            let sample = index.samples.get(next as usize).copied().ok_or(
                VideoError::FrameOutOfRange {
                    index: target,
                    count: index.samples.len() as u32,
                },
            )?;
            let frame = next_frame(input, sample.offset as usize)?
                .ok_or(VideoError::InvalidData("GIF frame table"))?;
            if frame.disposal == 3 {
                self.saved.copy_from_slice(&self.canvas);
            }
            // A damaged frame leaves the canvas as it was, and the animation
            // carries on.
            let _ = draw_frame(input, &head, &frame, &mut self.canvas);
            self.shown = Some((next, frame));
        }
    }

    /// Stops between frames: frame `next` is composited on the following call.
    fn park(&mut self, next: u32, input: &[u8], index: &Index) -> Result<bool> {
        // Nothing to dispose of: `shown` of `next - 1` was disposed already, so
        // record that with a frame whose disposal does nothing.
        if next > 0 {
            let sample = index.samples[(next - 1) as usize];
            if let Some(mut frame) = next_frame(input, sample.offset as usize)? {
                frame.disposal = 0;
                self.shown = Some((next - 1, frame));
            }
        }
        Ok(false)
    }
}

fn zeroed(len: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| VideoError::OutOfMemory)?;
    bytes.resize(len, 0);
    Ok(bytes)
}
