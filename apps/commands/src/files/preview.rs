//! Reading ahead for the explorer's viewer, and decoding its pictures.

use alloc::vec::Vec;

use feos_fs::{FileSystem, NodeId};

/// Largest image file the explorer will load.
const FILE_MAX: usize = 4 * 1024 * 1024;
/// Largest decoded RGBA buffer the explorer will ask the heap for.
const DECODED_MAX: usize = 32 * 1024 * 1024;
/// Bytes read at a time while looking for line breaks.
const CHUNK: usize = 1024;

/// A picture ready to blit: packed XRGB8888, `width * height` pixels.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// The size of `width` by `height` shrunk to fit a box, keeping its shape.
pub fn fit_size(width: u32, height: u32, box_width: u32, box_height: u32) -> (u32, u32) {
    if width <= box_width && height <= box_height {
        return (width.max(1), height.max(1));
    }
    let across = box_width as u64 * height as u64;
    let down = box_height as u64 * width as u64;
    if across <= down {
        let fitted = box_width as u64 * height as u64 / width.max(1) as u64;
        (box_width.max(1), (fitted as u32).max(1))
    } else {
        let fitted = box_height as u64 * width as u64 / height.max(1) as u64;
        ((fitted as u32).max(1), box_height.max(1))
    }
}

/// Decodes `node` to fit a box, over white where it is transparent.
///
/// Returns `None` for anything that is not a supported picture, is too big, is
/// damaged, or does not fit in the heap: the caller shows bytes or an icon.
pub fn decode_fit(
    fs: &dyn FileSystem,
    node: NodeId,
    box_width: u32,
    box_height: u32,
) -> Option<Decoded> {
    let size = fs.size(node);
    if size == 0 || size > FILE_MAX {
        return None;
    }
    let mut head = [0u8; 16];
    let read = fs.read(node, 0, &mut head);
    feos_image::detect(&head[..read])?;

    let input = fs.read_to_vec(node).ok()?;
    let info = feos_image::info(&input).ok()?;
    let length = info.rgba_len().ok()?;
    if length == 0 || length > DECODED_MAX {
        return None;
    }
    let mut output = zeroed(length)?;
    let mut scratch = zeroed(feos_image::scratch_len(&input).ok()?)?;
    let info = feos_image::decode(&input, &mut output, &mut scratch).ok()?;
    drop(scratch);
    drop(input);

    let (width, height) = fit_size(info.width, info.height, box_width, box_height);
    let count = (width as usize).checked_mul(height as usize)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(count).ok()?;
    for y in 0..height as u64 {
        let source_y = y * info.height as u64 / height as u64;
        for x in 0..width as u64 {
            let source_x = x * info.width as u64 / width as u64;
            let index = (source_y * info.width as u64 + source_x) as usize * 4;
            let pixel = output.get(index..index + 4)?;
            pixels.push(over_white(pixel));
        }
    }
    Some(Decoded {
        width,
        height,
        pixels,
    })
}

fn zeroed(length: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).ok()?;
    bytes.resize(length, 0);
    Some(bytes)
}

fn over_white(rgba: &[u8]) -> u32 {
    let alpha = rgba[3] as u32;
    let mix = |channel: u8| (channel as u32 * alpha + 255 * (255 - alpha)) / 255;
    (mix(rgba[0]) << 16) | (mix(rgba[1]) << 8) | mix(rgba[2])
}

/// The offset `count` lines after the line starting at `offset`. Stops at the
/// start of the file's last line.
pub fn lines_after(fs: &dyn FileSystem, node: NodeId, offset: usize, count: usize) -> usize {
    let mut chunk = [0u8; CHUNK];
    let mut position = offset;
    let mut line_start = offset;
    let mut left = count;
    while left > 0 {
        let read = fs.read(node, position, &mut chunk);
        if read == 0 {
            break;
        }
        for (index, byte) in chunk[..read].iter().enumerate() {
            if *byte == b'\n' {
                let next = position + index + 1;
                // A final newline does not start another line.
                if next >= fs.size(node) {
                    return line_start;
                }
                line_start = next;
                left -= 1;
                if left == 0 {
                    return line_start;
                }
            }
        }
        position += read;
    }
    line_start
}

/// The offset of the line `count` lines before the line starting at `offset`.
///
/// `offset` may be the end of the file, and then the file's last line, whether
/// or not a newline closes it, is the first one counted.
pub fn lines_before(fs: &dyn FileSystem, node: NodeId, offset: usize, count: usize) -> usize {
    let offset = offset.min(fs.size(node));
    if count == 0 {
        return offset;
    }
    let mut chunk = [0u8; CHUNK];
    let mut end = offset;
    // Each line back crosses one newline; the last one crossed is the one just
    // before the target line.
    let mut left = count;
    let mut first = true;
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let read = fs.read(node, start, &mut chunk[..end - start]);
        if read == 0 {
            return 0;
        }
        for index in (0..read).rev() {
            if chunk[index] != b'\n' {
                first = false;
                continue;
            }
            // A newline right at `offset` only ends the line being left.
            if first && start + index + 1 == offset {
                first = false;
                continue;
            }
            first = false;
            left -= 1;
            if left == 0 {
                return start + index + 1;
            }
        }
        end = start;
    }
    0
}
