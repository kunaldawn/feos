//! Vorbis I decoding from Ogg, in fixed-point integer arithmetic.
//!
//! Nothing here uses floating point. Residues are Q16 in 64 bits, the floor is
//! Q31, and the time domain is Q24 (full scale is 1 << 24), which leaves 7 bits
//! of headroom in the `i32` transform. Output differs from a float decoder by
//! rounding only: within a few 16-bit steps, as `tests/audio.rs` measures.
//!
//! Limits, each reported as an error rather than attempted:
//!
//! - Floor type 0 is not implemented: no encoder has emitted it since 2002, so
//!   nothing here could test it. Such a stream returns `UnsupportedFeature`.
//! - At most 8 channels, and at most [`MAX_BOOK_ENTRIES`] codebook entries and
//!   [`MAX_VQ_VALUES`] expanded VQ values across a stream's codebooks. Real files
//!   take about a megabyte. These caps keep a hostile setup header under about
//!   30 MiB and its cost constant, since a packed length list can claim millions
//!   of entries in a few bytes.
//!
//! The CPU cost of a packet is proportional to its output, and output is
//! bounded by the caller's buffer: a packet is only decoded once it is known to
//! fit. Output channels are in WAVE order. A corrupt audio packet decodes as far as it goes, which the format
//! defines, so damage is audible rather than an error. Bad headers are errors.

use alloc::vec::Vec;

use crate::bits::{try_push, try_vec, LsbReader};
use crate::error::{AudioError, Result};
use crate::mdct::{self, sin_turn, Imdct};
use crate::ogg::{self, PacketReader, Page};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// Codebook entries allowed across one stream.
pub const MAX_BOOK_ENTRIES: usize = 1 << 20;
/// Expanded VQ values allowed across one stream.
pub const MAX_VQ_VALUES: usize = 1 << 21;
const MAX_CHANNELS: usize = 8;
const FAST_BITS: u32 = 10;
/// Fractional bits of a VQ value and of the residue.
const RESIDUE_FRACTION: u32 = 16;
/// Fractional bits of the time domain.
const TIME_FRACTION: u32 = 24;

/// For each channel count, the Vorbis channel at each output position. Output
/// uses WAVE order, as FLAC and the other decoders here do.
static OUTPUT_ORDER: [&[u8]; MAX_CHANNELS] = [
    &[0],
    &[0, 1],
    &[0, 2, 1],
    &[0, 1, 2, 3],
    &[0, 2, 1, 3, 4],
    &[0, 2, 1, 5, 3, 4],
    &[0, 2, 1, 6, 5, 3, 4],
    &[0, 2, 1, 7, 5, 6, 3, 4],
];

/// `floor1_inverse_dB_table` from the specification, in Q31.
static FLOOR_DB: [i32; 256] = include!("vorbis_floor.in");

fn ilog(value: u32) -> u32 {
    32 - value.leading_zeros()
}

fn get(reader: &mut LsbReader<'_>, bits: u32) -> Result<u32> {
    reader.read(bits).ok_or(AudioError::UnexpectedEnd)
}

fn get_flag(reader: &mut LsbReader<'_>) -> Result<bool> {
    Ok(get(reader, 1)? != 0)
}

/// Vorbis's float format, scaled to Q32 and saturated.
fn float32_to_q32(packed: u32) -> i64 {
    let mantissa = (packed & 0x1f_ffff) as i64;
    let exponent = ((packed >> 21) & 0x3ff) as i32;
    // value = mantissa * 2^(exponent - 788), times 2^32.
    let shift = exponent - 788 + 32;
    let value = if shift >= 42 {
        if mantissa == 0 {
            0
        } else {
            i64::MAX / 2
        }
    } else if shift >= 0 {
        mantissa << shift
    } else if shift > -40 {
        mantissa >> -shift
    } else {
        0
    };
    if packed & 0x8000_0000 != 0 {
        -value
    } else {
        value
    }
}

/// The largest `r` with `r^dims <= entries`.
fn lookup1_values(entries: u32, dims: u32) -> u32 {
    let fits = |r: u32| -> bool {
        let mut product = 1u64;
        for _ in 0..dims {
            product *= r as u64;
            if product > entries as u64 {
                return false;
            }
        }
        true
    };
    if dims == 0 || entries == 0 {
        return 0;
    }
    let (mut low, mut high) = (1u32, entries + 1);
    if dims > 32 {
        // 2^33 is past any entry count, so only 1 can fit.
        return 1;
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if fits(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

/// A codebook: a canonical Huffman code and optional VQ values.
struct Book {
    dims: usize,
    /// `entry << 8 | length` by the next [`FAST_BITS`] bits; 0 means look further.
    fast: Vec<u32>,
    /// Codewords, bit reversed and left aligned, in ascending order.
    codes: Vec<u32>,
    lengths: Vec<u8>,
    values: Vec<u32>,
    /// `entries * dims` values in Q16; empty without a value mapping.
    vq: Vec<i32>,
}

impl Book {
    fn parse(reader: &mut LsbReader<'_>, budget: &mut Budget) -> Result<Self> {
        if get(reader, 24)? != 0x56_4342 {
            return Err(AudioError::InvalidData("bad Vorbis codebook sync"));
        }
        let dims = get(reader, 16)? as usize;
        let entries = get(reader, 24)? as usize;
        budget.take_entries(entries)?;
        let mut lengths = try_vec(entries, 0u8)?;
        if get_flag(reader)? {
            // Ordered: runs of ascending lengths.
            let mut length = get(reader, 5)? + 1;
            let mut entry = 0usize;
            while entry < entries {
                let count = get(reader, ilog((entries - entry) as u32))? as usize;
                let run = lengths
                    .get_mut(entry..entry + count)
                    .ok_or(AudioError::InvalidData("bad Vorbis codebook run"))?;
                if length > 32 {
                    return Err(AudioError::InvalidData("bad Vorbis codeword length"));
                }
                run.fill(length as u8);
                entry += count;
                length += 1;
            }
        } else {
            let sparse = get_flag(reader)?;
            for slot in lengths.iter_mut() {
                if !sparse || get_flag(reader)? {
                    *slot = get(reader, 5)? as u8 + 1;
                }
            }
        }

        let lookup = get(reader, 4)?;
        let mut vq = Vec::new();
        match lookup {
            0 => {}
            1 | 2 => {
                if dims == 0 || entries == 0 {
                    return Err(AudioError::InvalidData("empty Vorbis VQ codebook"));
                }
                let minimum = float32_to_q32(get(reader, 32)?);
                let delta = float32_to_q32(get(reader, 32)?);
                let value_bits = get(reader, 4)? + 1;
                let sequence = get_flag(reader)?;
                let count = if lookup == 1 {
                    lookup1_values(entries as u32, dims.min(u32::MAX as usize) as u32) as usize
                } else {
                    entries.saturating_mul(dims)
                };
                let total = entries.saturating_mul(dims);
                budget.take_values(total)?;
                let mut multiplicands = try_vec(count, 0u32)?;
                for slot in multiplicands.iter_mut() {
                    *slot = get(reader, value_bits)?;
                }
                vq = try_vec(total, 0i32)?;
                for (entry, vector) in vq.chunks_exact_mut(dims).enumerate() {
                    let mut last = 0i64;
                    let mut divisor = 1u64;
                    for (dim, slot) in vector.iter_mut().enumerate() {
                        let index = if lookup == 1 {
                            let index = (entry as u64 / divisor) % count.max(1) as u64;
                            divisor = divisor.saturating_mul(count as u64);
                            index as usize
                        } else {
                            entry * dims + dim
                        };
                        let multiplicand = multiplicands.get(index).copied().unwrap_or(0) as i64;
                        let value = multiplicand
                            .saturating_mul(delta)
                            .saturating_add(minimum)
                            .saturating_add(last);
                        if sequence {
                            last = value;
                        }
                        let q16 = value.saturating_add(1 << 15) >> (32 - RESIDUE_FRACTION);
                        *slot = q16.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    }
                }
            }
            _ => return Err(AudioError::InvalidData("bad Vorbis lookup type")),
        }

        let mut book = Self {
            dims,
            fast: Vec::new(),
            codes: Vec::new(),
            lengths: Vec::new(),
            values: Vec::new(),
            vq,
        };
        book.build_codes(&lengths)?;
        Ok(book)
    }

    /// Assigns codewords the way the specification's tree walk does, and builds
    /// the decode tables.
    fn build_codes(&mut self, lengths: &[u8]) -> Result<()> {
        let used = lengths.iter().filter(|&&length| length != 0).count();
        // (code, length, entry), code left aligned with the first-read bit on top.
        let mut codes = try_vec(used, (0u32, 0u8, 0u32))?;
        // The next free node at each depth, as a left-aligned bit pattern.
        let mut available = [0u32; 33];
        let mut next = 0usize;
        let mut first = true;
        for (entry, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let length = length as usize;
            if length > 32 {
                return Err(AudioError::InvalidData("bad Vorbis codeword length"));
            }
            let code = if first {
                first = false;
                for (depth, slot) in available.iter_mut().enumerate().take(length + 1).skip(1) {
                    *slot = if depth == 32 { 1 } else { 1u32 << (32 - depth) };
                }
                0
            } else {
                let mut depth = length;
                while depth > 0 && available[depth] == 0 {
                    depth -= 1;
                }
                if depth == 0 {
                    return Err(AudioError::InvalidData("overspecified Vorbis codebook"));
                }
                let code = available[depth];
                available[depth] = 0;
                for lower in (depth + 1..=length).rev() {
                    available[lower] = code | if lower == 32 { 1 } else { 1u32 << (32 - lower) };
                }
                code
            };
            codes[next] = (code, length as u8, entry as u32);
            next += 1;
        }
        codes.sort_unstable();

        self.fast = try_vec(1 << FAST_BITS, 0u32)?;
        self.codes = try_vec(used, 0u32)?;
        self.lengths = try_vec(used, 0u8)?;
        self.values = try_vec(used, 0u32)?;
        for (index, &(code, length, entry)) in codes.iter().enumerate() {
            self.codes[index] = code;
            self.lengths[index] = length;
            self.values[index] = entry;
            let length = length as u32;
            if length <= FAST_BITS {
                // The tree assigned this prefix to one codeword only, so these
                // writes total at most the table's size.
                let prefix = (code >> (32 - FAST_BITS)) as usize;
                let span = 1usize << (FAST_BITS - length);
                for slot in prefix..prefix + span {
                    self.fast[slot.reverse_bits() >> (usize::BITS - FAST_BITS)] =
                        (entry << 8) | length;
                }
            }
        }
        Ok(())
    }

    /// Decodes one entry, or ends the packet.
    #[inline]
    fn decode(&self, reader: &mut LsbReader<'_>) -> Option<usize> {
        let (bits, available) = reader.peek(32);
        // Entry 0 with a nonzero length is encoded with a nonzero slot.
        if let Some(&slot) = self.fast.get((bits & ((1 << FAST_BITS) - 1)) as usize) {
            let length = slot & 0xff;
            if length != 0 && length <= available {
                reader.consume(length);
                return Some((slot >> 8) as usize);
            }
        }
        self.decode_slow(reader, bits, available)
    }

    fn decode_slow(&self, reader: &mut LsbReader<'_>, bits: u32, available: u32) -> Option<usize> {
        let target = bits.reverse_bits();
        let index = self.codes.partition_point(|&code| code <= target);
        let found = index.checked_sub(1);
        let hit = found.and_then(|index| {
            let length = *self.lengths.get(index)? as u32;
            let code = *self.codes.get(index)?;
            let shift = 32 - length;
            // A tree of one entry has just that codeword, whatever the bits say.
            let matches = self.codes.len() == 1 || (code >> shift) == (target >> shift);
            if matches && length <= available {
                reader.consume(length);
                self.values.get(index).map(|&entry| entry as usize)
            } else {
                None
            }
        });
        if hit.is_none() {
            reader.end();
        }
        hit
    }
}

/// Running totals against the stream-wide caps.
#[derive(Default)]
struct Budget {
    entries: usize,
    values: usize,
}

impl Budget {
    fn take_entries(&mut self, count: usize) -> Result<()> {
        self.entries += count;
        if self.entries > MAX_BOOK_ENTRIES {
            return Err(AudioError::UnsupportedFeature(
                "Vorbis codebooks exceed the entry limit",
            ));
        }
        Ok(())
    }

    fn take_values(&mut self, count: usize) -> Result<()> {
        self.values = self.values.saturating_add(count);
        if self.values > MAX_VQ_VALUES {
            return Err(AudioError::UnsupportedFeature(
                "Vorbis codebooks exceed the value limit",
            ));
        }
        Ok(())
    }
}

const MAX_FLOOR_VALUES: usize = 65;

#[derive(Clone)]
struct Floor {
    partitions: usize,
    partition_class: [u8; 31],
    class_dims: [u8; 16],
    class_subclasses: [u8; 16],
    class_master: [u8; 16],
    class_books: [[i16; 8]; 16],
    multiplier: i32,
    values: usize,
    x: [u16; MAX_FLOOR_VALUES],
    /// Indices into `x` in ascending order of x.
    order: [u8; MAX_FLOOR_VALUES],
    low: [u8; MAX_FLOOR_VALUES],
    high: [u8; MAX_FLOOR_VALUES],
}

impl Floor {
    fn parse(reader: &mut LsbReader<'_>, books: usize) -> Result<Self> {
        let mut floor = Self {
            partitions: get(reader, 5)? as usize,
            partition_class: [0; 31],
            class_dims: [0; 16],
            class_subclasses: [0; 16],
            class_master: [0; 16],
            class_books: [[-1; 8]; 16],
            multiplier: 0,
            values: 2,
            x: [0; MAX_FLOOR_VALUES],
            order: [0; MAX_FLOOR_VALUES],
            low: [0; MAX_FLOOR_VALUES],
            high: [0; MAX_FLOOR_VALUES],
        };
        let mut classes = 0usize;
        for slot in floor.partition_class.iter_mut().take(floor.partitions) {
            *slot = get(reader, 4)? as u8;
            classes = classes.max(*slot as usize + 1);
        }
        for class in 0..classes {
            floor.class_dims[class] = get(reader, 3)? as u8 + 1;
            let subclasses = get(reader, 2)?;
            floor.class_subclasses[class] = subclasses as u8;
            if subclasses != 0 {
                let master = get(reader, 8)?;
                if master as usize >= books {
                    return Err(AudioError::InvalidData("bad Vorbis floor book"));
                }
                floor.class_master[class] = master as u8;
            }
            for slot in floor.class_books[class].iter_mut().take(1 << subclasses) {
                let book = get(reader, 8)? as i16 - 1;
                if book >= books as i16 {
                    return Err(AudioError::InvalidData("bad Vorbis floor book"));
                }
                *slot = book;
            }
        }
        floor.multiplier = get(reader, 2)? as i32 + 1;
        let range_bits = get(reader, 4)?;
        floor.x[1] = 1u16 << range_bits;
        for partition in 0..floor.partitions {
            let class = floor.partition_class[partition] as usize;
            for _ in 0..floor.class_dims[class] {
                if floor.values >= MAX_FLOOR_VALUES {
                    return Err(AudioError::InvalidData("too many Vorbis floor points"));
                }
                floor.x[floor.values] = get(reader, range_bits)? as u16;
                floor.values += 1;
            }
        }

        let values = floor.values;
        let x = floor.x;
        for i in 0..values {
            if x[..i].contains(&x[i]) {
                return Err(AudioError::InvalidData("repeated Vorbis floor point"));
            }
        }
        let mut order = [0u8; MAX_FLOOR_VALUES];
        for (i, slot) in order.iter_mut().enumerate().take(values) {
            *slot = i as u8;
        }
        order[..values].sort_unstable_by_key(|&i| x[i as usize]);
        floor.order = order;
        for i in 2..values {
            let mut low = 0;
            let mut high = 1;
            for j in 0..i {
                if x[j] < x[i] && x[j] > x[low] {
                    low = j;
                }
                if x[j] > x[i] && x[j] < x[high] {
                    high = j;
                }
            }
            floor.low[i] = low as u8;
            floor.high[i] = high as u8;
        }
        Ok(floor)
    }

    /// Reads a channel's floor and renders it into `curve`. Returns `false`
    /// for an unused floor, in which case the channel is silent.
    fn decode(&self, reader: &mut LsbReader<'_>, books: &[Book], curve: &mut [i32]) -> bool {
        if reader.read(1) != Some(1) {
            return false;
        }
        let mut y = [0i32; MAX_FLOOR_VALUES];
        let (range, bits) = match self.multiplier {
            1 => (256, 8),
            2 => (128, 7),
            3 => (86, 7),
            _ => (64, 6),
        };
        let (Some(y0), Some(y1)) = (reader.read(bits), reader.read(bits)) else {
            return false;
        };
        y[0] = y0 as i32;
        y[1] = y1 as i32;
        let mut offset = 2;
        for partition in 0..self.partitions {
            let class = self.partition_class[partition] as usize;
            let dims = self.class_dims[class] as usize;
            let sub_bits = self.class_subclasses[class] as u32;
            let mask = (1usize << sub_bits) - 1;
            let mut value = 0usize;
            if sub_bits != 0 {
                let Some(book) = books.get(self.class_master[class] as usize) else {
                    return false;
                };
                let Some(entry) = book.decode(reader) else {
                    return false;
                };
                value = entry;
            }
            for slot in 0..dims {
                let book = self.class_books[class][value & mask];
                value >>= sub_bits;
                let mut decoded = 0;
                if book >= 0 {
                    let Some(entry) = books
                        .get(book as usize)
                        .and_then(|book| book.decode(reader))
                    else {
                        return false;
                    };
                    decoded = entry.min(1 << 16) as i32;
                }
                // Parsing bounded `offset` by the number of floor values.
                if let Some(target) = y.get_mut(offset + slot) {
                    *target = decoded;
                }
            }
            offset += dims;
        }

        // Undo the prediction of each point from its two neighbours.
        let mut used = [false; MAX_FLOOR_VALUES];
        used[0] = true;
        used[1] = true;
        let mut final_y = [0i32; MAX_FLOOR_VALUES];
        final_y[0] = y[0];
        final_y[1] = y[1];
        for i in 2..self.values {
            let (low, high) = (self.low[i] as usize, self.high[i] as usize);
            let predicted = render_point(
                self.x[low] as i32,
                final_y[low],
                self.x[high] as i32,
                final_y[high],
                self.x[i] as i32,
            );
            let value = y[i];
            let high_room = range - predicted;
            let low_room = predicted;
            let room = high_room.min(low_room) * 2;
            if value != 0 {
                used[low] = true;
                used[high] = true;
                used[i] = true;
                final_y[i] = if value >= room {
                    if high_room > low_room {
                        value - low_room + predicted
                    } else {
                        predicted - value + high_room - 1
                    }
                } else if value & 1 != 0 {
                    predicted - (value + 1) / 2
                } else {
                    predicted + value / 2
                };
                final_y[i] = final_y[i].clamp(0, range - 1);
            } else {
                final_y[i] = predicted;
            }
        }

        // Render line segments between the used points.
        let n = curve.len();
        let (mut lx, mut ly) = (0i32, final_y[0].clamp(0, range - 1) * self.multiplier);
        for &index in &self.order[1..self.values] {
            let index = index as usize;
            if used[index] {
                let hy = final_y[index].clamp(0, range - 1) * self.multiplier;
                let hx = self.x[index] as i32;
                render_line(lx, ly, hx, hy, curve);
                lx = hx;
                ly = hy;
            }
        }
        if (lx as usize) < n {
            for slot in &mut curve[lx as usize..] {
                *slot = floor_value(ly);
            }
        }
        true
    }
}

#[inline]
fn floor_value(y: i32) -> i32 {
    FLOOR_DB[(y as usize).min(255)]
}

fn render_point(x0: i32, y0: i32, x1: i32, y1: i32, x: i32) -> i32 {
    let dy = y1 - y0;
    let adx = (x1 - x0).max(1);
    let offset = dy.abs() * (x - x0) / adx;
    if dy < 0 {
        y0 - offset
    } else {
        y0 + offset
    }
}

fn render_line(x0: i32, y0: i32, x1: i32, y1: i32, curve: &mut [i32]) {
    let n = curve.len() as i32;
    let dy = y1 - y0;
    let adx = x1 - x0;
    if adx <= 0 {
        return;
    }
    let base = dy / adx;
    let step = if dy < 0 { base - 1 } else { base + 1 };
    let ady = dy.abs() - base.abs() * adx;
    let mut y = y0;
    let mut error = 0;
    if x0 < n {
        curve[x0 as usize] = floor_value(y);
    }
    for x in x0 + 1..x1.min(n) {
        error += ady;
        if error >= adx {
            error -= adx;
            y += step;
        } else {
            y += base;
        }
        curve[x as usize] = floor_value(y);
    }
}

#[derive(Clone)]
struct Residue {
    kind: u8,
    begin: usize,
    end: usize,
    partition_size: usize,
    classifications: usize,
    class_book: usize,
    /// By classification and pass; -1 means the pass adds nothing.
    books: [[i16; 8]; 64],
}

impl Residue {
    fn parse(reader: &mut LsbReader<'_>, kind: u8, books: &[Book]) -> Result<Self> {
        let mut residue = Self {
            kind,
            begin: get(reader, 24)? as usize,
            end: get(reader, 24)? as usize,
            partition_size: get(reader, 24)? as usize + 1,
            classifications: get(reader, 6)? as usize + 1,
            class_book: get(reader, 8)? as usize,
            books: [[-1; 8]; 64],
        };
        let class_book = books
            .get(residue.class_book)
            .ok_or(AudioError::InvalidData("bad Vorbis residue book"))?;
        if class_book.dims == 0 {
            return Err(AudioError::InvalidData("bad Vorbis residue book"));
        }
        let mut cascade = [0u32; 64];
        for slot in cascade.iter_mut().take(residue.classifications) {
            let low = get(reader, 3)?;
            let high = if get_flag(reader)? { get(reader, 5)? } else { 0 };
            *slot = high * 8 + low;
        }
        for class in 0..residue.classifications {
            for pass in 0..8 {
                if cascade[class] & (1 << pass) != 0 {
                    let book = get(reader, 8)? as usize;
                    let target = books
                        .get(book)
                        .ok_or(AudioError::InvalidData("bad Vorbis residue book"))?;
                    if target.vq.is_empty() {
                        return Err(AudioError::InvalidData("Vorbis residue book has no VQ"));
                    }
                    residue.books[class][pass] = book as i16;
                }
            }
        }
        Ok(residue)
    }

    /// Adds this residue to `channels` vectors, each `size` long, laid out
    /// consecutively in `vectors`. Stops quietly at the end of the packet.
    fn decode(
        &self,
        reader: &mut LsbReader<'_>,
        books: &[Book],
        vectors: &mut [i64],
        size: usize,
        skip: &[bool],
        classes: &mut [u8],
    ) {
        let begin = self.begin.min(size);
        let end = self.end.min(size);
        if size == 0 || end <= begin {
            return;
        }
        let partitions = (end - begin) / self.partition_size;
        let Some(class_book) = books.get(self.class_book) else {
            return;
        };
        let classwords = class_book.dims;
        let stride = partitions;
        if classwords == 0 || partitions == 0 || classes.len() < skip.len() * stride {
            return;
        }
        let classifications = self.classifications;
        for pass in 0..8 {
            let mut partition = 0;
            while partition < partitions {
                if pass == 0 {
                    for (channel, _) in skip.iter().enumerate().filter(|(_, &skip)| !skip) {
                        let Some(mut value) = class_book.decode(reader) else {
                            return;
                        };
                        // Only the partitions that exist are worth computing.
                        let word = classwords.min(partitions - partition);
                        for _ in word..classwords {
                            value /= classifications;
                        }
                        for slot in (0..word).rev() {
                            classes[channel * stride + partition + slot] =
                                (value % classifications) as u8;
                            value /= classifications;
                        }
                    }
                }
                for _ in 0..classwords {
                    if partition >= partitions {
                        break;
                    }
                    for (channel, _) in skip.iter().enumerate().filter(|(_, &skip)| !skip) {
                        let class = classes[channel * stride + partition] as usize;
                        let book = self.books[class.min(63)][pass];
                        if book < 0 {
                            continue;
                        }
                        let Some(book) = books.get(book as usize) else {
                            continue;
                        };
                        let offset = begin + partition * self.partition_size;
                        let vector = channel * size + offset;
                        let Some(vector) =
                            vectors.get_mut(vector..vector + self.partition_size)
                        else {
                            return;
                        };
                        if !self.decode_partition(reader, book, vector) {
                            return;
                        }
                    }
                    partition += 1;
                }
            }
        }
    }

    fn decode_partition(
        &self,
        reader: &mut LsbReader<'_>,
        book: &Book,
        vector: &mut [i64],
    ) -> bool {
        let dims = book.dims;
        if self.kind == 0 {
            // Interleaved by the vector's dimension.
            let step = vector.len() / dims;
            for i in 0..step {
                let Some(entry) = book.decode(reader) else {
                    return false;
                };
                let Some(values) = book.vq.get(entry * dims..(entry + 1) * dims) else {
                    return false;
                };
                for (j, &value) in values.iter().enumerate() {
                    if let Some(slot) = vector.get_mut(i + j * step) {
                        *slot = slot.saturating_add(value as i64);
                    }
                }
            }
        } else {
            for chunk in vector.chunks_mut(dims) {
                let Some(entry) = book.decode(reader) else {
                    return false;
                };
                let Some(values) = book.vq.get(entry * dims..(entry + 1) * dims) else {
                    return false;
                };
                for (slot, &value) in chunk.iter_mut().zip(values) {
                    *slot = slot.saturating_add(value as i64);
                }
            }
        }
        true
    }
}

#[derive(Clone, Default)]
struct Mapping {
    coupling: Vec<(u8, u8)>,
    mux: [u8; MAX_CHANNELS],
    /// (floor, residue) per submap.
    submaps: Vec<(u8, u8)>,
}

#[derive(Clone, Copy)]
struct Mode {
    long_block: bool,
    mapping: u8,
}

/// The identification header.
#[derive(Clone, Copy)]
struct Ident {
    channels: usize,
    sample_rate: u32,
    block_sizes: [usize; 2],
}

fn parse_ident(packet: &[u8]) -> Result<Ident> {
    if packet.len() < 30 || packet[0] != 1 || &packet[1..7] != b"vorbis" {
        return Err(AudioError::InvalidData("bad Vorbis identification header"));
    }
    if packet[7..11] != [0; 4] {
        return Err(AudioError::UnsupportedFeature("Vorbis version"));
    }
    let channels = packet[11] as usize;
    let sample_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    let exponents = packet[28];
    let (small, large) = ((exponents & 15) as u32, (exponents >> 4) as u32);
    if channels == 0
        || sample_rate == 0
        || !(6..=13).contains(&small)
        || !(6..=13).contains(&large)
        || small > large
        || packet[29] & 1 == 0
    {
        return Err(AudioError::InvalidData("bad Vorbis identification header"));
    }
    if channels > MAX_CHANNELS {
        return Err(AudioError::UnsupportedFeature("Vorbis channel count"));
    }
    Ok(Ident {
        channels,
        sample_rate,
        block_sizes: [1 << small, 1 << large],
    })
}

/// The setup header.
struct Setup {
    books: Vec<Book>,
    floors: Vec<Floor>,
    residues: Vec<Residue>,
    mappings: Vec<Mapping>,
    modes: Vec<Mode>,
}

fn parse_setup(packet: &[u8], channels: usize) -> Result<Setup> {
    if packet.len() < 7 || packet[0] != 5 || &packet[1..7] != b"vorbis" {
        return Err(AudioError::InvalidData("bad Vorbis setup header"));
    }
    let mut reader = LsbReader::new(&packet[7..]);
    let reader = &mut reader;

    let mut budget = Budget::default();
    let count = get(reader, 8)? as usize + 1;
    let mut books = Vec::new();
    for _ in 0..count {
        let book = Book::parse(reader, &mut budget)?;
        try_push(&mut books, book)?;
    }

    for _ in 0..get(reader, 6)? + 1 {
        if get(reader, 16)? != 0 {
            return Err(AudioError::InvalidData("bad Vorbis time transform"));
        }
    }

    let mut floors = Vec::new();
    for _ in 0..get(reader, 6)? + 1 {
        match get(reader, 16)? {
            1 => try_push(&mut floors, Floor::parse(reader, books.len())?)?,
            0 => return Err(AudioError::UnsupportedFeature("Vorbis floor type 0")),
            _ => return Err(AudioError::InvalidData("bad Vorbis floor type")),
        }
    }

    let mut residues = Vec::new();
    for _ in 0..get(reader, 6)? + 1 {
        let kind = get(reader, 16)?;
        if kind > 2 {
            return Err(AudioError::InvalidData("bad Vorbis residue type"));
        }
        try_push(&mut residues, Residue::parse(reader, kind as u8, &books)?)?;
    }

    let mut mappings = Vec::new();
    let channel_bits = ilog(channels as u32 - 1);
    for _ in 0..get(reader, 6)? + 1 {
        if get(reader, 16)? != 0 {
            return Err(AudioError::InvalidData("bad Vorbis mapping type"));
        }
        let mut mapping = Mapping::default();
        let submaps = if get_flag(reader)? {
            get(reader, 4)? as usize + 1
        } else {
            1
        };
        if get_flag(reader)? {
            for _ in 0..get(reader, 8)? + 1 {
                let magnitude = get(reader, channel_bits)? as usize;
                let angle = get(reader, channel_bits)? as usize;
                if magnitude >= channels || angle >= channels || magnitude == angle {
                    return Err(AudioError::InvalidData("bad Vorbis channel coupling"));
                }
                try_push(&mut mapping.coupling, (magnitude as u8, angle as u8))?;
            }
        }
        if get(reader, 2)? != 0 {
            return Err(AudioError::InvalidData("bad Vorbis mapping"));
        }
        if submaps > 1 {
            for slot in mapping.mux.iter_mut().take(channels) {
                *slot = get(reader, 4)? as u8;
                if *slot as usize >= submaps {
                    return Err(AudioError::InvalidData("bad Vorbis channel mux"));
                }
            }
        }
        for _ in 0..submaps {
            get(reader, 8)?;
            let floor = get(reader, 8)? as usize;
            let residue = get(reader, 8)? as usize;
            if floor >= floors.len() || residue >= residues.len() {
                return Err(AudioError::InvalidData("bad Vorbis submap"));
            }
            try_push(&mut mapping.submaps, (floor as u8, residue as u8))?;
        }
        try_push(&mut mappings, mapping)?;
    }

    let mut modes = Vec::new();
    for _ in 0..get(reader, 6)? + 1 {
        let long_block = get_flag(reader)?;
        if get(reader, 16)? != 0 || get(reader, 16)? != 0 {
            return Err(AudioError::InvalidData("bad Vorbis mode"));
        }
        let mapping = get(reader, 8)? as usize;
        if mapping >= mappings.len() {
            return Err(AudioError::InvalidData("bad Vorbis mode"));
        }
        try_push(
            &mut modes,
            Mode {
                long_block,
                mapping: mapping as u8,
            },
        )?;
    }
    if !get_flag(reader)? {
        return Err(AudioError::InvalidData("bad Vorbis setup framing"));
    }
    Ok(Setup {
        books,
        floors,
        residues,
        mappings,
        modes,
    })
}

/// The rising half of the Vorbis power-complementary window, `len` values in
/// Q31: `sin(pi/2 sin^2((i + 1/2) / len * pi/2))`.
fn make_window(len: usize) -> Result<Vec<i32>> {
    let mut window = try_vec(len, 0i32)?;
    let bits = len.trailing_zeros();
    for (i, slot) in window.iter_mut().enumerate() {
        let phase = ((2 * i as u64 + 1) as u128) << (64 - 3 - bits);
        let sine = sin_turn(phase as u64);
        // sin^2 in Q62 is, read as a phase, sin^2 quarter turns.
        let square = (sine as i128 * sine as i128) >> 62;
        *slot = mdct::q62_to_q31(sin_turn(square as u64));
    }
    Ok(window)
}

/// A stateful Ogg Vorbis decoder, one packet at a time.
pub struct VorbisDecoder<'a> {
    packets: PacketReader<'a>,
    ident: Ident,
    setup: Setup,
    imdct: [Imdct; 2],
    windows: [Vec<i32>; 2],
    /// Per channel, `block / 2` long: residue, floor, windowed tail of the
    /// previous block.
    residue: Vec<i64>,
    floor: Vec<i32>,
    overlap: Vec<i32>,
    /// One channel's working vectors.
    interleaved: Vec<i64>,
    spectrum: Vec<i32>,
    time: Vec<i32>,
    classes: Vec<u8>,
    /// The previous block's size, 0 before the first.
    previous: usize,
    /// Sample frames still allowed by the end-of-stream granule position.
    remaining: u64,
}

impl<'a> VorbisDecoder<'a> {
    /// Reads the three header packets of the first Vorbis stream in `input`.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let page = match ogg::find_stream(input)? {
            Some((AudioFormat::OggVorbis, page)) => page,
            _ => return Err(AudioError::InvalidData("not an Ogg Vorbis stream")),
        };
        let mut packets = PacketReader::new(input, page.serial);
        let ident = parse_ident(packets.next_packet()?.ok_or(AudioError::UnexpectedEnd)?)?;
        let comment = packets.next_packet()?.ok_or(AudioError::UnexpectedEnd)?;
        if comment.len() < 7 || comment[0] != 3 || &comment[1..7] != b"vorbis" {
            return Err(AudioError::InvalidData("bad Vorbis comment header"));
        }
        let setup = parse_setup(
            packets.next_packet()?.ok_or(AudioError::UnexpectedEnd)?,
            ident.channels,
        )?;

        let [small, large] = ident.block_sizes;
        let half = large / 2;
        let channels = ident.channels;
        let granule = ogg::final_granule(input, page.serial);
        Ok(Self {
            packets,
            ident,
            setup,
            imdct: [Imdct::new(small)?, Imdct::new(large)?],
            windows: [make_window(small / 2)?, make_window(large / 2)?],
            residue: try_vec(half * channels, 0)?,
            floor: try_vec(half * channels, 0)?,
            overlap: try_vec(half * channels, 0)?,
            interleaved: try_vec(half * channels, 0)?,
            spectrum: try_vec(half, 0)?,
            time: try_vec(large, 0)?,
            classes: try_vec(half * channels, 0)?,
            previous: 0,
            remaining: granule.unwrap_or(u64::MAX),
        })
    }

    /// Number of interleaved channels.
    pub fn channels(&self) -> usize {
        self.ident.channels
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.ident.sample_rate
    }

    /// The most samples, counting every channel, a single packet can produce.
    pub fn max_packet_samples(&self) -> usize {
        self.ident.block_sizes[1] / 2 * self.ident.channels
    }

    /// Decodes the next packet to `out`, which must hold
    /// [`Self::max_packet_samples`] or at least this packet's output.
    ///
    /// Returns the sample frames written, or `None` at the end of the stream.
    /// A packet can legitimately produce nothing: the first always does.
    pub fn next_packet(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        // Past the stream's stated end, packets would cost CPU for no output.
        if self.remaining == 0 {
            return Ok(None);
        }
        let Some(packet) = self.packets.next_packet()? else {
            return Ok(None);
        };
        let channels = self.ident.channels;
        let mut reader = LsbReader::new(packet);
        let reader = &mut reader;
        // Not an audio packet: ignored, as the specification says.
        if reader.read(1) != Some(0) {
            return Ok(Some(0));
        }
        let mode_bits = ilog(self.setup.modes.len() as u32 - 1);
        let Some(mode) = reader
            .read(mode_bits)
            .and_then(|mode| self.setup.modes.get(mode as usize))
            .copied()
        else {
            return Ok(Some(0));
        };
        let [small, large] = self.ident.block_sizes;
        let n = if mode.long_block { large } else { small };
        let half = n / 2;
        let (mut previous_long, mut next_long) = (false, false);
        if mode.long_block {
            let (Some(a), Some(b)) = (reader.read(1), reader.read(1)) else {
                return Ok(Some(0));
            };
            previous_long = a != 0;
            next_long = b != 0;
        }

        // Check the output before any state changes.
        let count = if self.previous == 0 {
            0
        } else {
            self.previous / 4 + n / 4
        };
        let frames = (count as u64).min(self.remaining) as usize;
        if out.len() < frames * channels {
            return Err(AudioError::BufferTooSmall {
                required: frames * channels,
                available: out.len(),
            });
        }

        let Self {
            setup,
            residue,
            floor,
            interleaved,
            classes,
            spectrum,
            time,
            imdct,
            windows,
            overlap,
            ..
        } = self;
        let Some(mapping) = setup.mappings.get(mode.mapping as usize) else {
            return Ok(Some(0));
        };
        let books = &setup.books[..];

        // Floors.
        let mut silent = [true; MAX_CHANNELS];
        for channel in 0..channels {
            let submap = if mapping.submaps.len() > 1 {
                mapping.mux[channel] as usize
            } else {
                0
            };
            let curve = &mut floor[channel * half..(channel + 1) * half];
            if let Some(&(index, _)) = mapping.submaps.get(submap) {
                if let Some(definition) = setup.floors.get(index as usize) {
                    silent[channel] = !definition.decode(reader, books, curve);
                }
            }
        }

        // Coupled channels have residue if either side has a floor.
        let mut no_residue = silent;
        for &(magnitude, angle) in &mapping.coupling {
            let (magnitude, angle) = (magnitude as usize, angle as usize);
            if !no_residue[magnitude] || !no_residue[angle] {
                no_residue[magnitude] = false;
                no_residue[angle] = false;
            }
        }

        // Residues, one bundle of channels per submap.
        residue[..half * channels].fill(0);
        for (index, &(_, residue_index)) in mapping.submaps.iter().enumerate() {
            let Some(definition) = setup.residues.get(residue_index as usize) else {
                continue;
            };
            let mut members = [0usize; MAX_CHANNELS];
            let mut skip = [false; MAX_CHANNELS];
            let mut bundle = 0;
            for channel in 0..channels {
                let submap = if mapping.submaps.len() > 1 {
                    mapping.mux[channel] as usize
                } else {
                    0
                };
                if submap == index {
                    members[bundle] = channel;
                    skip[bundle] = no_residue[channel];
                    bundle += 1;
                }
            }
            if bundle == 0 {
                continue;
            }
            let vectors = &mut interleaved[..half * bundle];
            vectors.fill(0);
            if definition.kind == 2 {
                if skip[..bundle].iter().all(|&skip| skip) {
                    continue;
                }
                definition.decode(
                    reader,
                    books,
                    vectors,
                    half * bundle,
                    &[false],
                    classes,
                );
                for (member, &channel) in members[..bundle].iter().enumerate() {
                    let target = &mut residue[channel * half..(channel + 1) * half];
                    for (i, slot) in target.iter_mut().enumerate() {
                        *slot = vectors[i * bundle + member];
                    }
                }
            } else {
                definition.decode(reader, books, vectors, half, &skip[..bundle], classes);
                for (member, &channel) in members[..bundle].iter().enumerate() {
                    residue[channel * half..(channel + 1) * half]
                        .copy_from_slice(&vectors[member * half..(member + 1) * half]);
                }
            }
        }

        // Inverse channel coupling, last step first.
        for &(magnitude, angle) in mapping.coupling.iter().rev() {
            let (magnitude, angle) = (magnitude as usize, angle as usize);
            for k in 0..half {
                let m = residue[magnitude * half + k];
                let a = residue[angle * half + k];
                let (new_m, new_a) = if m > 0 {
                    if a > 0 {
                        (m, m.saturating_sub(a))
                    } else {
                        (m.saturating_add(a), m)
                    }
                } else if a > 0 {
                    (m, m.saturating_add(a))
                } else {
                    (m.saturating_sub(a), m)
                };
                residue[magnitude * half + k] = new_m;
                residue[angle * half + k] = new_a;
            }
        }

        // The window for this block.
        let long = mode.long_block as usize;
        let (left_start, left_end, left_window) = if mode.long_block && !previous_long {
            (n / 4 - small / 4, n / 4 + small / 4, &windows[0])
        } else {
            (0, half, &windows[long])
        };
        let (right_start, right_end, right_window) = if mode.long_block && !next_long {
            (n * 3 / 4 - small / 4, n * 3 / 4 + small / 4, &windows[0])
        } else {
            (half, n, &windows[long])
        };

        // Floor times residue, transform, window, overlap, output.
        let shift = 31 + RESIDUE_FRACTION - TIME_FRACTION;
        let order = OUTPUT_ORDER[channels - 1];
        for (position, &channel) in order.iter().enumerate() {
            let channel = channel as usize;
            let time = &mut time[..n];
            if silent[channel] {
                time.fill(0);
            } else {
                let curve = &floor[channel * half..(channel + 1) * half];
                let vector = &residue[channel * half..(channel + 1) * half];
                for ((slot, &f), &r) in spectrum[..half].iter_mut().zip(curve).zip(vector) {
                    let product = (f as i64).saturating_mul(r);
                    *slot = (product >> shift).clamp(-(1 << 30), 1 << 30) as i32;
                }
                imdct[long].run(&spectrum[..half], time);
                time[..left_start].fill(0);
                for (slot, &weight) in time[left_start..left_end].iter_mut().zip(left_window) {
                    *slot = mdct::window(*slot, weight);
                }
                let falling = right_window.iter().rev();
                for (slot, &weight) in time[right_start..right_end].iter_mut().zip(falling) {
                    *slot = mdct::window(*slot, weight);
                }
                time[right_end..].fill(0);
            }

            let tail = &mut overlap[channel * (large / 2)..(channel + 1) * (large / 2)];
            if frames != 0 {
                // The previous block's 3/4 point meets this block's 1/4 point.
                let shift = (n / 4) as isize - (self.previous / 4) as isize;
                for i in 0..frames {
                    let old = tail.get(i).copied().unwrap_or(0) as i64;
                    let index = i as isize + shift;
                    let new = if index >= 0 {
                        time.get(index as usize).copied().unwrap_or(0) as i64
                    } else {
                        0
                    };
                    // A tail shorter than the overlap has already ended.
                    let old = if i < self.previous / 2 { old } else { 0 };
                    let new = if index >= 0 && (index as usize) < half {
                        new
                    } else {
                        0
                    };
                    let sum = old + new;
                    let rounding = 1i64 << (TIME_FRACTION - 16);
                    let sample = (sum + rounding) >> (TIME_FRACTION - 15);
                    out[i * channels + position] =
                        sample.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                }
            }
            tail[..half].copy_from_slice(&time[half..n]);
        }

        self.previous = n;
        self.remaining -= (frames as u64).min(self.remaining);
        Ok(Some(frames))
    }
}

/// Returns the properties of an Ogg Vorbis stream, given its first page.
pub(crate) fn ogg_info(input: &[u8], page: &Page<'_>) -> Result<AudioInfo> {
    let ident = parse_ident(page.data)?;
    Ok(AudioInfo {
        sample_rate: ident.sample_rate,
        channels: ident.channels as u16,
        frames: ogg::last_granule(input, page.serial),
        format: AudioFormat::OggVorbis,
    })
}

/// Decodes an Ogg Vorbis stream to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let mut decoder = VorbisDecoder::new(input)?;
    let channels = decoder.channels();
    let mut written = 0usize;
    loop {
        let rest = out.get_mut(written..).unwrap_or(&mut []);
        match decoder.next_packet(rest) {
            Ok(Some(frames)) => written += frames * channels,
            Ok(None) => break,
            Err(AudioError::BufferTooSmall { required, .. }) => {
                return Err(AudioError::BufferTooSmall {
                    required: written + required,
                    available: out.len(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let frames = written / channels;
    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: decoder.sample_rate(),
            channels: channels as u16,
            frames: frames as u64,
            format: AudioFormat::OggVorbis,
        },
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_unpack() {
        // 1.0 is 2^20 * 2^-20: exponent 768, mantissa 1 << 20.
        let one = (768u32 << 21) | (1 << 20);
        assert_eq!(float32_to_q32(one), 1 << 32);
        assert_eq!(float32_to_q32(one | 0x8000_0000), -(1 << 32));
        assert_eq!(float32_to_q32(0), 0);
    }

    #[test]
    fn lookup1_is_the_integer_root() {
        assert_eq!(lookup1_values(81, 4), 3);
        assert_eq!(lookup1_values(80, 4), 2);
        assert_eq!(lookup1_values(1 << 20, 1), 1 << 20);
        assert_eq!(lookup1_values(5, 40), 1);
    }

    #[test]
    fn window_is_power_complementary() {
        let window = make_window(128).unwrap();
        for i in 0..128 {
            let (a, b) = (window[i] as i64, window[127 - i] as i64);
            assert!((a * a + b * b - (1 << 62)).abs() < 1 << 36);
        }
    }

    #[test]
    fn floor_line_matches_the_specification_walk() {
        let mut curve = [0; 8];
        render_line(0, 10, 8, 18, &mut curve);
        let expected: [i32; 8] = core::array::from_fn(|i| FLOOR_DB[10 + i]);
        assert_eq!(curve, expected);
    }
}
