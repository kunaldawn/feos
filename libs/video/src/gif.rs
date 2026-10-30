//! Animated GIF decoding (GIF87a/GIF89a).
//!
//! GIF frames are composited onto an RGBA canvas. The caller supplies that
//! canvas in `out` and a scratch buffer large enough for the LZW index plane
//! plus one saved canvas (used by disposal method 3).

use crate::error::{need, require_rgba, Result, VideoError};

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

fn parse_info(input: &[u8]) -> Result<GifInfo> {
    let head = header(input)?;
    let mut pos = head.pos;
    let mut frames = 0u32;
    let mut first_delay = 0u32;
    let mut delay = 0u32;
    loop {
        let block = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
        pos += 1;
        match block {
            0x21 => {
                let label = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
                pos += 1;
                if label == 0xf9 {
                    let size = *input.get(pos).ok_or(VideoError::UnexpectedEnd)? as usize;
                    need(input.len(), pos + 1, size)?;
                    if size >= 4 {
                        delay = u16::from_le_bytes([input[pos + 2], input[pos + 3]]) as u32 * 10;
                    }
                    pos = skip_sub_blocks(input, pos + 1 + size)?;
                } else {
                    pos = skip_sub_blocks(input, pos)?;
                }
            }
            0x2c => {
                need(input.len(), pos, 9)?;
                let packed = input[pos + 8];
                pos += 9;
                if packed & 0x80 != 0 {
                    let colors = 1usize << ((packed & 0x07) as usize + 1);
                    let len = colors * 3;
                    need(input.len(), pos, len)?;
                    pos += len;
                }
                let min_code = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
                if min_code > 8 {
                    return Err(VideoError::InvalidData("GIF LZW code size"));
                }
                pos += 1;
                if frames == 0 {
                    first_delay = delay;
                }
                frames = frames.saturating_add(1);
                pos = skip_sub_blocks(input, pos)?;
            }
            0x3b => break,
            _ => return Err(VideoError::InvalidData("GIF block")),
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

    fn next_byte(&mut self) -> Result<Option<u8>> {
        if self.left == 0 {
            let n = *self.data.get(self.pos).ok_or(VideoError::UnexpectedEnd)? as usize;
            self.pos += 1;
            if n == 0 {
                return Ok(None);
            }
            self.left = n;
        }
        need(self.data.len(), self.pos, 1)?;
        let byte = self.data[self.pos];
        self.pos += 1;
        self.left -= 1;
        Ok(Some(byte))
    }

    fn finish(mut self) -> Result<usize> {
        if self.left != 0 {
            need(self.data.len(), self.pos, self.left)?;
            self.pos += self.left;
        }
        skip_sub_blocks(self.data, self.pos)
    }
}

fn lzw_decode(reader: &mut SubBlocks<'_>, min_code_size: u8, out: &mut [u8]) -> Result<usize> {
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
    let mut written = 0usize;

    loop {
        while bit_count < code_size {
            let byte = reader.next_byte()?.ok_or(VideoError::UnexpectedEnd)?;
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

        let count = MAX_CODES - top;
        if written + count > out.len() {
            return Err(VideoError::BufferTooSmall {
                required: written + count,
                available: out.len(),
            });
        }
        out[written..written + count].copy_from_slice(&stack[top..]);
        written += count;

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

    Ok(written)
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

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    indices: &[u8],
    width: usize,
    height: usize,
    left: usize,
    top: usize,
    canvas_width: usize,
    canvas_height: usize,
    palette: &[u8],
    transparent: Option<u8>,
    interlaced: bool,
    out: &mut [u8],
) {
    for src_y in 0..height {
        let y = if interlaced {
            interlaced_row(src_y, height)
        } else {
            src_y
        };
        let y = top + y;
        if y >= canvas_height {
            continue;
        }
        for x in 0..width {
            let dx = left + x;
            if dx >= canvas_width {
                continue;
            }
            let index = indices[src_y * width + x];
            if transparent == Some(index) {
                continue;
            }
            let pi = index as usize * 3;
            if pi + 2 >= palette.len() {
                continue;
            }
            let o = (y * canvas_width + dx) * 4;
            out[o] = palette[pi];
            out[o + 1] = palette[pi + 1];
            out[o + 2] = palette[pi + 2];
            out[o + 3] = 255;
        }
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
    let (indices, saved) = scratch.split_at_mut(pixels);

    for px in out[..canvas_len].chunks_exact_mut(4) {
        px.copy_from_slice(&[0, 0, 0, 0]);
    }

    let mut pos = head.pos;
    let mut frame_index = 0u32;
    let mut disposal = 0u8;
    let mut transparent = None;
    let mut gce_delay = 0u32;

    loop {
        let block = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
        pos += 1;
        match block {
            0x21 => {
                let label = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
                pos += 1;
                if label == 0xf9 {
                    let size = *input.get(pos).ok_or(VideoError::UnexpectedEnd)? as usize;
                    need(input.len(), pos + 1, size)?;
                    if size >= 4 {
                        let packed = input[pos + 1];
                        gce_delay =
                            u16::from_le_bytes([input[pos + 2], input[pos + 3]]) as u32 * 10;
                        disposal = (packed >> 2) & 0x07;
                        transparent = if packed & 0x01 != 0 {
                            Some(input[pos + 4])
                        } else {
                            None
                        };
                    }
                    pos = skip_sub_blocks(input, pos + 1 + size)?;
                } else {
                    pos = skip_sub_blocks(input, pos)?;
                }
            }
            0x2c => {
                need(input.len(), pos, 9)?;
                let left = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
                let top = u16::from_le_bytes([input[pos + 2], input[pos + 3]]) as usize;
                let width = u16::from_le_bytes([input[pos + 4], input[pos + 5]]) as usize;
                let height = u16::from_le_bytes([input[pos + 6], input[pos + 7]]) as usize;
                let packed = input[pos + 8];
                pos += 9;
                if width == 0 || height == 0 {
                    return Err(VideoError::InvalidData("GIF frame dimensions"));
                }
                let local;
                if packed & 0x80 != 0 {
                    let colors = 1usize << ((packed & 0x07) as usize + 1);
                    let len = colors * 3;
                    need(input.len(), pos, len)?;
                    local = &input[pos..pos + len];
                    pos += len;
                } else {
                    local = head.global;
                }
                let interlaced = packed & 0x40 != 0;
                let min_code = *input.get(pos).ok_or(VideoError::UnexpectedEnd)?;
                if min_code > 8 {
                    return Err(VideoError::InvalidData("GIF LZW code size"));
                }
                pos += 1;
                let mut reader = SubBlocks::new(input, pos);
                let index_count = width.checked_mul(height).ok_or(VideoError::Overflow)?;
                if index_count > indices.len() {
                    return Err(VideoError::BufferTooSmall {
                        required: index_count,
                        available: indices.len(),
                    });
                }
                let decoded = lzw_decode(&mut reader, min_code, &mut indices[..index_count])?;
                if decoded < index_count {
                    // Some encoders end the LZW stream early; pad with the
                    // last index so a short frame still has defined pixels.
                    let fill = indices.get(decoded.saturating_sub(1)).copied().unwrap_or(0);
                    for item in &mut indices[decoded..index_count] {
                        *item = fill;
                    }
                }
                pos = reader.finish()?;

                if disposal == 3 {
                    saved.copy_from_slice(&out[..canvas_len]);
                }
                draw_frame(
                    &indices[..index_count],
                    width,
                    height,
                    left,
                    top,
                    head.width as usize,
                    head.height as usize,
                    local,
                    transparent,
                    interlaced,
                    out,
                );
                if frame_index == index {
                    return Ok(gce_delay);
                }
                frame_index += 1;

                match disposal {
                    2 => {
                        // Restore the frame rectangle to the background.
                        for y in top..(top + height).min(head.height as usize) {
                            for x in left..(left + width).min(head.width as usize) {
                                let o = (y * head.width as usize + x) * 4;
                                out[o..o + 4].copy_from_slice(&head.background);
                            }
                        }
                    }
                    3 => out[..canvas_len].copy_from_slice(saved),
                    _ => {}
                }
                disposal = 0;
                transparent = None;
                gce_delay = 0;
            }
            0x3b => {
                return Err(VideoError::FrameOutOfRange {
                    index,
                    count: frame_index,
                });
            }
            _ => return Err(VideoError::InvalidData("GIF block")),
        }
    }
}
