//! Raw DEFLATE (RFC 1951) compression.
//!
//! The compressor emits a single fixed-Huffman block and finds matches with a
//! greedy LZ77 search over a caller-owned hash table. It is intentionally small
//! and fast rather than optimal; for typical text it still compresses well.

use crate::error::{CompressError, Result};

const HASH_SIZE: usize = 4096;
const HASH_MASK: usize = HASH_SIZE - 1;
const WINDOW: usize = 32 * 1024;
const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const EMPTY: u32 = u32::MAX;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Returns an upper bound on the size of a fixed-Huffman DEFLATE stream for an
/// input of `len` bytes.
pub const fn bound(len: usize) -> usize {
    // 9 bits per literal worst case, plus block header and end-of-block.
    len + len / 8 + 16
}

/// Reusable compressor state.
///
/// The structure owns a 16 KiB match table. It can live on the stack, but for
/// deeply nested call paths prefer placing it in caller-managed storage.
pub struct Deflater {
    head: [u32; HASH_SIZE],
}

impl Default for Deflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Deflater {
    /// Creates an empty compressor.
    pub const fn new() -> Self {
        Self {
            head: [EMPTY; HASH_SIZE],
        }
    }
}

fn reverse_bits(mut code: u32, len: u8) -> u32 {
    let mut reversed = 0;
    let mut i = 0;
    while i < len {
        reversed = (reversed << 1) | (code & 1);
        code >>= 1;
        i += 1;
    }
    reversed
}

struct BitWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
    bits: u32,
    count: u8,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Self {
            out,
            pos: 0,
            bits: 0,
            count: 0,
        }
    }

    fn bits(&mut self, value: u32, len: u8) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let mask = (1u32 << len) - 1;
        self.bits |= (value & mask) << self.count;
        self.count += len;
        while self.count >= 8 {
            if self.pos >= self.out.len() {
                return Err(CompressError::BufferTooSmall {
                    required: self.pos + 1,
                    available: self.out.len(),
                });
            }
            self.out[self.pos] = (self.bits & 0xff) as u8;
            self.pos += 1;
            self.bits >>= 8;
            self.count -= 8;
        }
        Ok(())
    }

    fn code(&mut self, code: u32, len: u8) -> Result<()> {
        self.bits(reverse_bits(code, len), len)
    }

    fn finish(mut self) -> Result<usize> {
        if self.count > 0 {
            if self.pos >= self.out.len() {
                return Err(CompressError::BufferTooSmall {
                    required: self.pos + 1,
                    available: self.out.len(),
                });
            }
            self.out[self.pos] = (self.bits & 0xff) as u8;
            self.pos += 1;
        }
        Ok(self.pos)
    }
}

fn literal_code(symbol: u16) -> (u32, u8) {
    match symbol {
        0..=143 => (0x30 + symbol as u32, 8),
        144..=255 => (0x190 + (symbol as u32 - 144), 9),
        256..=279 => (symbol as u32 - 256, 7),
        _ => (0xc0 + (symbol as u32 - 280), 8),
    }
}

fn length_symbol(length: usize) -> (u16, u8, u32) {
    let mut index = 0;
    while index + 1 < LENGTH_BASE.len() && length >= LENGTH_BASE[index + 1] as usize {
        index += 1;
    }
    let base = LENGTH_BASE[index] as usize;
    (
        257 + index as u16,
        LENGTH_EXTRA[index],
        (length - base) as u32,
    )
}

fn distance_symbol(distance: usize) -> (u16, u8, u32) {
    let mut index = 0;
    while index + 1 < DIST_BASE.len() && distance >= DIST_BASE[index + 1] as usize {
        index += 1;
    }
    let base = DIST_BASE[index] as usize;
    (index as u16, DIST_EXTRA[index], (distance - base) as u32)
}

fn hash(input: &[u8], pos: usize) -> usize {
    let value = ((input[pos] as usize) << 10)
        ^ ((input[pos + 1] as usize) << 5)
        ^ (input[pos + 2] as usize);
    value & HASH_MASK
}

fn emit_literal(writer: &mut BitWriter<'_>, byte: u8) -> Result<()> {
    let (code, len) = literal_code(byte as u16);
    writer.code(code, len)
}

fn emit_match(writer: &mut BitWriter<'_>, length: usize, distance: usize) -> Result<()> {
    let (symbol, extra_bits, extra) = length_symbol(length);
    let (code, len) = literal_code(symbol);
    writer.code(code, len)?;
    writer.bits(extra, extra_bits)?;

    let (symbol, extra_bits, extra) = distance_symbol(distance);
    writer.code(symbol as u32, 5)?;
    writer.bits(extra, extra_bits)
}

/// Compresses `input` as a raw DEFLATE stream into `out`, returning the number
/// of bytes written.
pub fn compress(input: &[u8], out: &mut [u8]) -> Result<usize> {
    compress_with(&mut Deflater::new(), input, out)
}

/// Compresses with a reusable compressor state.
pub fn compress_with(deflater: &mut Deflater, input: &[u8], out: &mut [u8]) -> Result<usize> {
    deflater.head.fill(EMPTY);

    let mut writer = BitWriter::new(out);
    // Final block, fixed Huffman codes.
    writer.bits(1, 1)?;
    writer.bits(1, 2)?;

    let mut pos = 0;
    while pos < input.len() {
        if pos + MIN_MATCH <= input.len() {
            let slot = hash(input, pos);
            let candidate = deflater.head[slot];
            deflater.head[slot] = pos as u32;

            if candidate != EMPTY {
                let candidate = candidate as usize;
                let distance = pos.wrapping_sub(candidate);
                if distance > 0 && distance <= WINDOW {
                    let max = (input.len() - pos).min(MAX_MATCH);
                    let mut length = 0;
                    while length < max && input[candidate + length] == input[pos + length] {
                        length += 1;
                    }
                    if length >= MIN_MATCH {
                        emit_match(&mut writer, length, distance)?;
                        // Register the skipped positions so later matches can
                        // still find them.
                        let mut i = 1;
                        while i < length && pos + i + MIN_MATCH <= input.len() {
                            let slot = hash(input, pos + i);
                            deflater.head[slot] = (pos + i) as u32;
                            i += 1;
                        }
                        pos += length;
                        continue;
                    }
                }
            }
        }

        emit_literal(&mut writer, input[pos])?;
        pos += 1;
    }

    let (code, len) = literal_code(256);
    writer.code(code, len)?;
    writer.finish()
}
