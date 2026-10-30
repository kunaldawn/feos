//! Raw DEFLATE (RFC 1951) decompression.
//!
//! The decompressor writes into a caller-provided output buffer and reuses the
//! already-written bytes as the LZ77 window, so the whole stream must fit in
//! `out`.

use crate::error::{CompressError, Result};

const MAX_BITS: usize = 15;
const MAX_SYMBOLS: usize = 288;

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
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Result of a successful inflate operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InflateResult {
    /// Number of output bytes written.
    pub written: usize,
    /// Number of input bytes consumed.
    pub consumed: usize,
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u32,
    count: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bits: 0,
            count: 0,
        }
    }

    fn bit(&mut self) -> Result<u32> {
        if self.count == 0 {
            let byte = *self
                .data
                .get(self.pos)
                .ok_or(CompressError::UnexpectedEnd)?;
            self.pos += 1;
            self.bits = byte as u32;
            self.count = 8;
        }
        let value = self.bits & 1;
        self.bits >>= 1;
        self.count -= 1;
        Ok(value)
    }

    fn read(&mut self, n: u8) -> Result<u32> {
        let mut value = 0;
        for (shift, _) in (0..n).enumerate() {
            value |= self.bit()? << shift;
        }
        Ok(value)
    }

    fn align(&mut self) {
        self.bits = 0;
        self.count = 0;
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        debug_assert_eq!(self.count, 0);
        let low = *self
            .data
            .get(self.pos)
            .ok_or(CompressError::UnexpectedEnd)? as u16;
        let high = *self
            .data
            .get(self.pos + 1)
            .ok_or(CompressError::UnexpectedEnd)? as u16;
        self.pos += 2;
        Ok(low | (high << 8))
    }

    fn read_u8(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or(CompressError::UnexpectedEnd)?;
        self.pos += 1;
        Ok(byte)
    }
}

struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; MAX_SYMBOLS],
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; MAX_BITS + 1];
        for &length in lengths {
            if length as usize > MAX_BITS {
                return Err(CompressError::InvalidData("huffman code too long"));
            }
            count[length as usize] += 1;
        }
        if count[0] as usize == lengths.len() {
            return Ok(Self {
                count,
                symbol: [0; MAX_SYMBOLS],
            });
        }

        let mut left: i32 = 1;
        for &symbols in count.iter().skip(1) {
            left <<= 1;
            left -= symbols as i32;
            if left < 0 {
                return Err(CompressError::InvalidData("over-subscribed huffman tree"));
            }
        }

        let mut offsets = [0u16; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }

        let mut symbol = [0u16; MAX_SYMBOLS];
        for (value, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let slot = offsets[length as usize] as usize;
                if slot >= MAX_SYMBOLS {
                    return Err(CompressError::InvalidData("too many huffman symbols"));
                }
                symbol[slot] = value as u16;
                offsets[length as usize] += 1;
            }
        }

        Ok(Self { count, symbol })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= reader.bit()? as i32;
            let count = self.count[len] as i32;
            if code - first < count {
                let slot = index + (code - first);
                return self
                    .symbol
                    .get(slot as usize)
                    .copied()
                    .ok_or(CompressError::InvalidData("invalid huffman code"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(CompressError::InvalidData("invalid huffman code"))
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman)> {
    let mut literal_lengths = [0u8; 288];
    for (i, length) in literal_lengths.iter_mut().enumerate() {
        *length = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let distance_lengths = [5u8; 32];
    Ok((
        Huffman::from_lengths(&literal_lengths)?,
        Huffman::from_lengths(&distance_lengths)?,
    ))
}

fn dynamic_tables(reader: &mut BitReader<'_>) -> Result<(Huffman, Huffman)> {
    let hlit = reader.read(5)? as usize + 257;
    let hdist = reader.read(5)? as usize + 1;
    let hclen = reader.read(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(CompressError::InvalidData("bad dynamic huffman counts"));
    }

    let mut code_lengths = [0u8; 19];
    for &index in CODE_LENGTH_ORDER.iter().take(hclen) {
        code_lengths[index] = reader.read(3)? as u8;
    }
    let code_table = Huffman::from_lengths(&code_lengths)?;

    let mut lengths = [0u8; 286 + 30];
    let mut i = 0;
    while i < hlit + hdist {
        let symbol = code_table.decode(reader)?;
        match symbol {
            0..=15 => {
                lengths[i] = symbol as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(CompressError::InvalidData("repeat with no previous length"));
                }
                let previous = lengths[i - 1];
                let repeat = reader.read(2)? as usize + 3;
                if i + repeat > hlit + hdist {
                    return Err(CompressError::InvalidData("code length repeat overflow"));
                }
                for _ in 0..repeat {
                    lengths[i] = previous;
                    i += 1;
                }
            }
            17 => {
                let repeat = reader.read(3)? as usize + 3;
                if i + repeat > hlit + hdist {
                    return Err(CompressError::InvalidData("code length repeat overflow"));
                }
                i += repeat;
            }
            18 => {
                let repeat = reader.read(7)? as usize + 11;
                if i + repeat > hlit + hdist {
                    return Err(CompressError::InvalidData("code length repeat overflow"));
                }
                i += repeat;
            }
            _ => return Err(CompressError::InvalidData("bad code length symbol")),
        }
    }

    let literal = Huffman::from_lengths(&lengths[..hlit])?;
    let distance = Huffman::from_lengths(&lengths[hlit..hlit + hdist])?;
    Ok((literal, distance))
}

/// Decompresses a raw DEFLATE stream into `out`, returning the number of bytes
/// written.
pub fn inflate(input: &[u8], out: &mut [u8]) -> Result<usize> {
    Ok(inflate_with_consumed(input, out)?.written)
}

/// Decompresses a raw DEFLATE stream and reports how much input it used.
///
/// This is useful when a compressed stream is followed by a trailer, as with
/// gzip and zlib.
pub fn inflate_with_consumed(input: &[u8], out: &mut [u8]) -> Result<InflateResult> {
    let mut reader = BitReader::new(input);
    let mut written = 0usize;

    loop {
        let final_block = reader.read(1)? != 0;
        let block_type = reader.read(2)?;
        match block_type {
            0 => {
                reader.align();
                let length = reader.read_u16_le()? as usize;
                let complement = reader.read_u16_le()?;
                if length as u16 != !complement {
                    return Err(CompressError::InvalidData("stored block length mismatch"));
                }
                let end = written.checked_add(length).ok_or(CompressError::Overflow)?;
                if end > out.len() {
                    return Err(CompressError::BufferTooSmall {
                        required: end,
                        available: out.len(),
                    });
                }
                for _ in 0..length {
                    out[written] = reader.read_u8()?;
                    written += 1;
                }
            }
            1 | 2 => {
                let (literal, distance) = if block_type == 1 {
                    fixed_tables()?
                } else {
                    dynamic_tables(&mut reader)?
                };
                loop {
                    let symbol = literal.decode(&mut reader)?;
                    match symbol {
                        0..=255 => {
                            if written >= out.len() {
                                return Err(CompressError::BufferTooSmall {
                                    required: written + 1,
                                    available: out.len(),
                                });
                            }
                            out[written] = symbol as u8;
                            written += 1;
                        }
                        256 => break,
                        257..=285 => {
                            let index = symbol as usize - 257;
                            let length = LENGTH_BASE[index] as usize
                                + reader.read(LENGTH_EXTRA[index])? as usize;
                            let dist_symbol = distance.decode(&mut reader)? as usize;
                            if dist_symbol >= 30 {
                                return Err(CompressError::InvalidData("invalid distance symbol"));
                            }
                            let dist = DIST_BASE[dist_symbol] as usize
                                + reader.read(DIST_EXTRA[dist_symbol])? as usize;
                            if dist == 0 || dist > written {
                                return Err(CompressError::InvalidData("distance before output"));
                            }
                            let end = written.checked_add(length).ok_or(CompressError::Overflow)?;
                            if end > out.len() {
                                return Err(CompressError::BufferTooSmall {
                                    required: end,
                                    available: out.len(),
                                });
                            }
                            for _ in 0..length {
                                out[written] = out[written - dist];
                                written += 1;
                            }
                        }
                        _ => return Err(CompressError::InvalidData("invalid literal symbol")),
                    }
                }
            }
            _ => return Err(CompressError::InvalidData("invalid deflate block type")),
        }
        if final_block {
            break;
        }
    }

    Ok(InflateResult {
        written,
        consumed: reader.pos,
    })
}
