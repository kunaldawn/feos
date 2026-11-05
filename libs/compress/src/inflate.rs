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

/// Where Huffman decoding gets its bits: all at once, or as they arrive.
trait Bits {
    /// Tops the window up, as far as input allows.
    fn refill(&mut self);
    /// The window, lowest bits next.
    fn window(&self) -> u64;
    /// Valid bits in the window.
    fn available(&self) -> u32;
    /// Drops bits already used.
    fn consume(&mut self, n: u32);

    fn read(&mut self, n: u8) -> Result<u32> {
        let n = n as u32;
        if self.available() < n {
            self.refill();
            if self.available() < n {
                return Err(CompressError::UnexpectedEnd);
            }
        }
        let value = (self.window() & ((1u64 << n) - 1)) as u32;
        self.consume(n);
        Ok(value)
    }
}

/// LSB-first bit reader over a 64-bit window.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u64,
    count: u32,
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

    /// Drops to the next byte boundary and returns whole unread bytes to the
    /// input, so stored blocks can be copied directly.
    fn align(&mut self) {
        self.pos -= (self.count / 8) as usize;
        self.bits = 0;
        self.count = 0;
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        debug_assert_eq!(self.count, 0);
        let bytes = self
            .data
            .get(self.pos..self.pos + 2)
            .ok_or(CompressError::UnexpectedEnd)?;
        let value = u16::from_le_bytes([bytes[0], bytes[1]]);
        self.pos += 2;
        Ok(value)
    }

    /// Input bytes used so far, counting a partly read byte as used.
    fn consumed(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }
}

impl Bits for BitReader<'_> {
    /// Tops the window up to at least 56 bits while input remains.
    fn refill(&mut self) {
        if let Some(chunk) = self.data.get(self.pos..self.pos + 8) {
            let word = u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
            self.bits |= word << self.count;
            let taken = (63 - self.count) >> 3;
            self.pos += taken as usize;
            self.count += taken * 8;
            return;
        }
        while self.count <= 56 {
            let Some(&byte) = self.data.get(self.pos) else {
                return;
            };
            self.bits |= (byte as u64) << self.count;
            self.pos += 1;
            self.count += 8;
        }
    }

    fn window(&self) -> u64 {
        self.bits
    }

    fn available(&self) -> u32 {
        self.count
    }

    fn consume(&mut self, n: u32) {
        self.bits >>= n;
        self.count -= n;
    }
}

/// Bits resolved by the fast table. Longer codes take the canonical walk.
const LOOKUP_BITS: u32 = 9;
const LOOKUP_SIZE: usize = 1 << LOOKUP_BITS;
const LOOKUP_LEN_BITS: u32 = 4;

struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; MAX_SYMBOLS],
    /// `symbol << 4 | length` for each code of at most [`LOOKUP_BITS`] bits,
    /// indexed by the next input bits. Zero means a longer or unassigned code.
    lookup: [u16; LOOKUP_SIZE],
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
                lookup: [0; LOOKUP_SIZE],
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

        // Assign canonical codes in `symbol` order, and fill every table slot
        // whose low bits are the (bit-reversed) code.
        let mut lookup = [0u16; LOOKUP_SIZE];
        let mut code = 0u32;
        let mut slot = 0usize;
        for length in 1..=LOOKUP_BITS {
            for _ in 0..count[length as usize] {
                let reversed = code.reverse_bits() >> (32 - length);
                let entry = (symbol[slot] << LOOKUP_LEN_BITS) | length as u16;
                let mut index = reversed as usize;
                while index < LOOKUP_SIZE {
                    lookup[index] = entry;
                    index += 1 << length;
                }
                code += 1;
                slot += 1;
            }
            code <<= 1;
        }

        Ok(Self {
            count,
            symbol,
            lookup,
        })
    }

    #[inline]
    fn decode<B: Bits>(&self, reader: &mut B) -> Result<u16> {
        if reader.available() < MAX_BITS as u32 {
            reader.refill();
        }
        let entry = self.lookup[reader.window() as usize & (LOOKUP_SIZE - 1)];
        if entry != 0 {
            let length = (entry & ((1 << LOOKUP_LEN_BITS) - 1)) as u32;
            if length > reader.available() {
                return Err(CompressError::UnexpectedEnd);
            }
            reader.consume(length);
            return Ok(entry >> LOOKUP_LEN_BITS);
        }
        self.decode_long(reader)
    }

    /// Canonical bit-by-bit walk, for codes the table doesn't resolve.
    fn decode_long<B: Bits>(&self, reader: &mut B) -> Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            if len as u32 > reader.available() {
                return Err(CompressError::UnexpectedEnd);
            }
            code |= ((reader.window() >> (len - 1)) & 1) as i32;
            let count = self.count[len] as i32;
            if code - first < count {
                let slot = index + (code - first);
                reader.consume(len as u32);
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

fn dynamic_tables<B: Bits>(reader: &mut B) -> Result<(Huffman, Huffman)> {
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
                let source = input
                    .get(reader.pos..reader.pos + length)
                    .ok_or(CompressError::UnexpectedEnd)?;
                out[written..end].copy_from_slice(source);
                reader.pos += length;
                written = end;
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
                            if dist >= length {
                                out.copy_within(written - dist..written - dist + length, written);
                            } else {
                                // An overlapping match repeats its own output.
                                for index in written..end {
                                    out[index] = out[index - dist];
                                }
                            }
                            written = end;
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
        consumed: reader.consumed(),
    })
}

/// Input for [`inflate_stream`]: fills the buffer and returns how much it wrote,
/// with zero at the end of the input.
pub type StreamInput<'a> = &'a mut dyn FnMut(&mut [u8]) -> Result<usize>;
/// Output for [`inflate_stream`]: takes each piece as it's ready.
pub type StreamOutput<'a> = &'a mut dyn FnMut(&[u8]) -> Result<()>;

/// Bytes of history a DEFLATE stream can refer back to.
pub const WINDOW_SIZE: usize = 32 * 1024;

/// Totals from a streaming decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamResult {
    /// Bytes handed to the output.
    pub written: u64,
    /// Bytes of input the stream used.
    pub consumed: u64,
}

/// Bits drawn from a callback, a buffer at a time.
pub(crate) struct StreamBits<'a> {
    input: StreamInput<'a>,
    buffer: [u8; 4096],
    len: usize,
    pos: usize,
    bits: u64,
    count: u32,
    total_read: u64,
    /// An input error, returned once decoding stops for lack of bits.
    failure: Option<CompressError>,
}

impl<'a> StreamBits<'a> {
    pub(crate) fn new(input: StreamInput<'a>) -> Self {
        Self {
            input,
            buffer: [0; 4096],
            len: 0,
            pos: 0,
            bits: 0,
            count: 0,
            total_read: 0,
            failure: None,
        }
    }

    fn next_byte(&mut self) -> Option<u8> {
        if self.pos == self.len {
            if self.failure.is_some() {
                return None;
            }
            match (self.input)(&mut self.buffer) {
                Ok(0) => return None,
                Ok(count) => {
                    self.len = count.min(self.buffer.len());
                    self.pos = 0;
                    self.total_read += self.len as u64;
                }
                Err(error) => {
                    self.failure = Some(error);
                    return None;
                }
            }
        }
        let byte = self.buffer[self.pos];
        self.pos += 1;
        Some(byte)
    }

    /// Reads a byte on a byte boundary, or `None` at the end of the input.
    pub(crate) fn byte(&mut self) -> Option<u8> {
        self.read(8).ok().map(|value| value as u8)
    }

    /// Drops to the next byte boundary. Whole bytes stay in the window.
    pub(crate) fn align(&mut self) {
        self.consume(self.count % 8);
    }

    /// Input used so far.
    pub(crate) fn consumed(&self) -> u64 {
        self.total_read - (self.len - self.pos) as u64 - (self.count / 8) as u64
    }

    /// What stopped the input, or a plain unexpected end.
    fn stop_reason(&mut self, error: CompressError) -> CompressError {
        match (error, self.failure.take()) {
            (CompressError::UnexpectedEnd, Some(failure)) => failure,
            (error, _) => error,
        }
    }
}

impl Bits for StreamBits<'_> {
    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(byte) = self.next_byte() else {
                return;
            };
            self.bits |= (byte as u64) << self.count;
            self.count += 8;
        }
    }

    fn window(&self) -> u64 {
        self.bits
    }

    fn available(&self) -> u32 {
        self.count
    }

    fn consume(&mut self, n: u32) {
        self.bits >>= n;
        self.count -= n;
    }
}

/// The last [`WINDOW_SIZE`] bytes, and whatever hasn't gone to the output yet.
struct Ring<'a, 'b> {
    window: &'a mut [u8],
    output: StreamOutput<'b>,
    /// Bytes ever produced. Their position in the ring is this, wrapped.
    total: u64,
    /// How many of them have gone to the output.
    flushed: u64,
}

impl Ring<'_, '_> {
    fn flush(&mut self) -> Result<()> {
        let size = self.window.len() as u64;
        while self.flushed < self.total {
            let start = (self.flushed % size) as usize;
            let run = (self.total - self.flushed).min(size - start as u64) as usize;
            (self.output)(&self.window[start..start + run])?;
            self.flushed += run as u64;
        }
        Ok(())
    }

    fn push(&mut self, byte: u8) -> Result<()> {
        let size = self.window.len() as u64;
        if self.total - self.flushed == size {
            self.flush()?;
        }
        self.window[(self.total % size) as usize] = byte;
        self.total += 1;
        Ok(())
    }

    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        let size = self.window.len() as u64;
        if distance as u64 > self.total || distance > self.window.len() {
            return Err(CompressError::InvalidData("distance before output"));
        }
        for _ in 0..length {
            let from = ((self.total - distance as u64) % size) as usize;
            self.push(self.window[from])?;
        }
        Ok(())
    }
}

/// Decodes one raw DEFLATE stream from a source to a sink, holding only the
/// window: the output need not fit in memory, or even have a known size.
pub(crate) fn inflate_bits(
    bits: &mut StreamBits<'_>,
    window: &mut [u8],
    output: StreamOutput<'_>,
) -> Result<u64> {
    if window.len() < WINDOW_SIZE {
        return Err(CompressError::BufferTooSmall {
            required: WINDOW_SIZE,
            available: window.len(),
        });
    }
    // The ring arithmetic wraps at the exact size given.
    let window = &mut window[..WINDOW_SIZE];
    let mut ring = Ring {
        window,
        output,
        total: 0,
        flushed: 0,
    };
    let result = inflate_blocks(bits, &mut ring);
    let result = result.and_then(|()| ring.flush());
    result.map_err(|error| bits.stop_reason(error))?;
    Ok(ring.total)
}

fn inflate_blocks(bits: &mut StreamBits<'_>, ring: &mut Ring<'_, '_>) -> Result<()> {
    loop {
        let final_block = bits.read(1)? != 0;
        let block_type = bits.read(2)?;
        match block_type {
            0 => {
                bits.align();
                let length = bits.read(16)? as u16;
                let complement = bits.read(16)? as u16;
                if length != !complement {
                    return Err(CompressError::InvalidData("stored block length mismatch"));
                }
                for _ in 0..length {
                    let byte = bits.read(8)? as u8;
                    ring.push(byte)?;
                }
            }
            1 | 2 => {
                let (literal, distance) = if block_type == 1 {
                    fixed_tables()?
                } else {
                    dynamic_tables(bits)?
                };
                loop {
                    match literal.decode(bits)? {
                        symbol @ 0..=255 => ring.push(symbol as u8)?,
                        256 => break,
                        symbol @ 257..=285 => {
                            let index = symbol as usize - 257;
                            let length =
                                LENGTH_BASE[index] as usize + bits.read(LENGTH_EXTRA[index])? as usize;
                            let dist_symbol = distance.decode(bits)? as usize;
                            if dist_symbol >= 30 {
                                return Err(CompressError::InvalidData("invalid distance symbol"));
                            }
                            let dist = DIST_BASE[dist_symbol] as usize
                                + bits.read(DIST_EXTRA[dist_symbol])? as usize;
                            ring.copy_match(dist, length)?;
                        }
                        _ => return Err(CompressError::InvalidData("invalid literal symbol")),
                    }
                }
            }
            _ => return Err(CompressError::InvalidData("invalid deflate block type")),
        }
        if final_block {
            return Ok(());
        }
    }
}

/// Decompresses a raw DEFLATE stream from `input` to `output`.
///
/// `window` holds the history and must be at least [`WINDOW_SIZE`] bytes.
pub fn inflate_stream(
    input: StreamInput<'_>,
    window: &mut [u8],
    output: StreamOutput<'_>,
) -> Result<StreamResult> {
    let mut bits = StreamBits::new(input);
    let written = inflate_bits(&mut bits, window, output)?;
    Ok(StreamResult {
        written,
        consumed: bits.consumed(),
    })
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    fn pseudo_random(len: usize) -> std::vec::Vec<u8> {
        let mut state = 0x1234_5678u32;
        (0..len)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Compressible stretches, with far-back repeats.
                if index % 4096 < 2048 {
                    (index % 251) as u8
                } else {
                    (state >> 24) as u8
                }
            })
            .collect()
    }

    fn stream(compressed: &[u8], piece: usize) -> Result<(std::vec::Vec<u8>, StreamResult)> {
        let mut at = 0;
        let mut input = |buffer: &mut [u8]| -> Result<usize> {
            let take = piece.min(buffer.len()).min(compressed.len() - at);
            buffer[..take].copy_from_slice(&compressed[at..at + take]);
            at += take;
            Ok(take)
        };
        let mut out = std::vec::Vec::new();
        let mut output = |chunk: &[u8]| -> Result<()> {
            out.extend_from_slice(chunk);
            Ok(())
        };
        let mut window = std::vec![0u8; WINDOW_SIZE];
        let result = inflate_stream(&mut input, &mut window, &mut output)?;
        Ok((out, result))
    }

    fn compressed_form(data: &[u8]) -> std::vec::Vec<u8> {
        let mut out = std::vec![0u8; data.len() + data.len() / 2 + 1024];
        let written = crate::deflate_compress(data, &mut out).unwrap();
        out.truncate(written);
        out
    }

    #[test]
    fn agrees_with_the_one_shot_decoder_for_any_input_chunking() {
        let data = pseudo_random(200_000);
        let compressed = compressed_form(&data);
        for piece in [1usize, 7, 4096, usize::MAX] {
            let (out, result) = stream(&compressed, piece).unwrap();
            assert_eq!(out, data, "input in pieces of {piece}");
            assert_eq!(result.written, data.len() as u64);
            assert_eq!(result.consumed, compressed.len() as u64);
        }
    }

    #[test]
    fn output_larger_than_the_window_needs_no_more_memory() {
        // Well past the window, all zeros, so that matches run across the wrap.
        let data = std::vec![0u8; 1_000_000];
        let compressed = compressed_form(&data);
        let (out, _) = stream(&compressed, 512).unwrap();
        assert_eq!(out.len(), data.len());
        assert!(out.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn stored_blocks_stream_too() {
        // A stored block by hand: final, type 0, length 3, complement, "abc".
        let raw = [0b001u8, 3, 0, 0xfc, 0xff, b'a', b'b', b'c'];
        assert_eq!(stream(&raw, 1).unwrap().0, b"abc");
        assert_eq!(stream(&raw[..7], 3), Err(CompressError::UnexpectedEnd));
        let bad = [0b001u8, 3, 0, 0, 0, b'a', b'b', b'c'];
        assert!(stream(&bad, 3).is_err());
    }

    #[test]
    fn a_distance_before_the_start_is_refused_and_junk_never_panics() {
        // Fixed-table match at distance 1 with nothing written: symbol 257.
        assert!(stream(&[0x03, 0x02, 0x00], 3).is_err());
        let mut state = 7u32;
        for _ in 0..2000 {
            let junk: std::vec::Vec<u8> = (0..64)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1);
                    (state >> 24) as u8
                })
                .collect();
            let _ = stream(&junk, 16);
        }
    }

    #[test]
    fn errors_from_the_callbacks_come_back() {
        let data = pseudo_random(100_000);
        let compressed = compressed_form(&data);
        let mut window = std::vec![0u8; WINDOW_SIZE];

        let mut at = 0;
        let mut input = |buffer: &mut [u8]| -> Result<usize> {
            let take = buffer.len().min(compressed.len() - at);
            buffer[..take].copy_from_slice(&compressed[at..at + take]);
            at += take;
            Ok(take)
        };
        let mut refusing = |_: &[u8]| -> Result<()> { Err(CompressError::Overflow) };
        assert_eq!(
            inflate_stream(&mut input, &mut window, &mut refusing),
            Err(CompressError::Overflow)
        );

        let mut failing = |_: &mut [u8]| -> Result<usize> { Err(CompressError::Checksum) };
        let mut sink = |_: &[u8]| -> Result<()> { Ok(()) };
        assert_eq!(
            inflate_stream(&mut failing, &mut window, &mut sink),
            Err(CompressError::Checksum)
        );
        let mut tiny = [0u8; 16];
        let mut empty = |_: &mut [u8]| -> Result<usize> { Ok(0) };
        assert!(matches!(
            inflate_stream(&mut empty, &mut tiny, &mut sink),
            Err(CompressError::BufferTooSmall { .. })
        ));
    }
}
