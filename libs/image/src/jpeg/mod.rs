//! JPEG decoding: baseline, extended sequential and progressive.
//!
//! The decoder handles 8-bit Huffman-coded files: restart intervals, any
//! sampling factors the format allows, one or many scans, grayscale, YCbCr,
//! RGB, CMYK and YCCK. Arithmetic coding, lossless and hierarchical processes,
//! and 12-bit samples are rejected with [`ImageError::UnsupportedFeature`].
//!
//! Output is always RGBA8888, with chroma upsampled by replication.
//!
//! # Memory
//!
//! A sequential file with a single interleaved scan, which covers nearly every
//! camera file and all Motion JPEG, decodes straight into the output buffer
//! with no allocation. Every other file (progressive, separate scans per
//! component, or an MCU over the 10 blocks T.81 allows) has to keep its
//! coefficients until the last scan. That storage comes from the heap, through
//! `try_reserve_exact`, bounded by [`MAX_COEFFICIENT_BYTES`]. Being over the
//! limit, or the heap being unable to serve the request, is reported as
//! [`ImageError::UnsupportedFeature`]; it never panics or aborts.
//!
//! Decoding uses under 16 KiB of stack in a release build, mostly Huffman tables.

mod idct;
mod tables;

use alloc::vec::Vec;

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

use idct::{dequantize, idct};

const MAX_COMPONENTS: usize = 4;
/// The most blocks the allocation-free path handles in one MCU (T.81 B.2.3).
const MAX_FAST_BLOCKS: usize = 10;
/// Limit on scans in one file. Legitimate files use tens at most. Each scan can
/// cost a pass over every block for a few bytes of input, so without a limit a
/// small file could stall the decoder.
const MAX_SCANS: usize = 512;
const FAST_BITS: u32 = 8;

/// Most heap the decoder will use for coefficient storage, in bytes.
///
/// Files that need it use two bytes per coefficient: 6 bytes per pixel for
/// 4:4:4 and 3 for 4:2:0, so this allows about 5 and 11 megapixels. The kernel
/// heap is 64 MiB, and the caller's RGBA output must fit there as well.
pub const MAX_COEFFICIENT_BYTES: usize = 32 * 1024 * 1024;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone, Copy)]
struct Quant {
    values: [u16; 64],
    defined: bool,
}

impl Quant {
    const EMPTY: Self = Self {
        values: [0; 64],
        defined: false,
    };
}

#[derive(Clone, Copy)]
struct Huffman {
    symbols: [u8; 256],
    /// Indexed by the next `FAST_BITS` bits: `(length << 8) | symbol`, or 0
    /// when the code is longer than that.
    fast: [u16; 1 << FAST_BITS],
    /// Largest code of each length, or -1 when the length has none.
    maxcode: [i32; 17],
    /// Added to a code of each length to get its index in `symbols`.
    delta: [i32; 17],
    defined: bool,
}

impl Huffman {
    const EMPTY: Self = Self {
        symbols: [0; 256],
        fast: [0; 1 << FAST_BITS],
        maxcode: [-1; 17],
        delta: [0; 17],
        defined: false,
    };

    fn build(&mut self, counts: &[u8], symbols: &[u8]) -> Result<()> {
        if counts.len() != 16 || symbols.len() > 256 {
            return Err(ImageError::InvalidData("bad DHT segment"));
        }
        *self = Self::EMPTY;
        self.symbols[..symbols.len()].copy_from_slice(symbols);
        let mut code = 0i32;
        let mut index = 0i32;
        for length in 1..=16usize {
            let count = counts[length - 1] as i32;
            if code + count > 1 << length {
                return Err(ImageError::InvalidData("oversubscribed Huffman table"));
            }
            if count != 0 {
                self.delta[length] = index - code;
                if length as u32 <= FAST_BITS {
                    let shift = FAST_BITS - length as u32;
                    for i in 0..count {
                        let symbol = *symbols
                            .get((index + i) as usize)
                            .ok_or(ImageError::InvalidData("bad DHT segment"))?;
                        let first = ((code + i) as usize) << shift;
                        let entry = ((length as u16) << 8) | symbol as u16;
                        self.fast[first..first + (1 << shift)].fill(entry);
                    }
                }
                index += count;
                code += count;
                self.maxcode[length] = code - 1;
            }
            code <<= 1;
        }
        self.defined = true;
        Ok(())
    }

    #[inline]
    fn decode(&self, br: &mut BitReader<'_>) -> Result<u8> {
        let look = br.peek16();
        let entry = self.fast[(look >> (16 - FAST_BITS)) as usize];
        if entry != 0 {
            br.consume((entry >> 8) as u32)?;
            return Ok(entry as u8);
        }
        for length in (FAST_BITS as usize + 1)..=16 {
            let code = (look >> (16 - length)) as i32;
            if code <= self.maxcode[length] {
                br.consume(length as u32)?;
                return self
                    .symbols
                    .get((code + self.delta[length]) as usize)
                    .copied()
                    .ok_or(ImageError::InvalidData("bad Huffman symbol"));
            }
        }
        Err(ImageError::InvalidData("invalid Huffman code"))
    }
}

#[derive(Clone, Copy)]
struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
}

impl Component {
    const EMPTY: Self = Self {
        id: 0,
        h: 1,
        v: 1,
        tq: 0,
    };
}

struct Frame {
    /// The SOF marker's code, which names the coding process.
    process: u8,
    width: usize,
    height: usize,
    components: [Component; MAX_COMPONENTS],
    count: usize,
}

fn be_u16(input: &[u8], offset: usize) -> Result<u16> {
    need(input.len(), offset, 2)?;
    Ok(u16::from_be_bytes([input[offset], input[offset + 1]]))
}

fn next_marker(input: &[u8], mut pos: usize) -> Result<(u8, usize)> {
    while pos < input.len() && input[pos] != 0xff {
        pos += 1;
    }
    while pos < input.len() && input[pos] == 0xff {
        pos += 1;
    }
    if pos >= input.len() {
        return Err(ImageError::UnexpectedEnd);
    }
    Ok((input[pos], pos + 1))
}

fn parse_frame(process: u8, data: &[u8]) -> Result<Frame> {
    if data.len() < 6 {
        return Err(ImageError::UnexpectedEnd);
    }
    let precision = data[0];
    if precision != 8 {
        return Err(ImageError::UnsupportedFeature("JPEG sample precision"));
    }
    let height = u16::from_be_bytes([data[1], data[2]]) as usize;
    let width = u16::from_be_bytes([data[3], data[4]]) as usize;
    let count = data[5] as usize;
    if width == 0 || height == 0 || count == 0 || count > MAX_COMPONENTS {
        return Err(ImageError::InvalidData("bad JPEG frame header"));
    }
    if data.len() < 6 + count * 3 {
        return Err(ImageError::UnexpectedEnd);
    }
    let mut components = [Component::EMPTY; MAX_COMPONENTS];
    for i in 0..count {
        let base = 6 + i * 3;
        let sampling = data[base + 1];
        let h = (sampling >> 4) as usize;
        let v = (sampling & 15) as usize;
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
            return Err(ImageError::InvalidData("bad JPEG sampling factor"));
        }
        let tq = data[base + 2] as usize;
        if tq >= 4 {
            return Err(ImageError::InvalidData("bad JPEG quantization table"));
        }
        components[i] = Component {
            id: data[base],
            h,
            v,
            tq,
        };
    }
    if count == 1 {
        // A lone component's scan is non-interleaved, so its MCU is one block
        // whatever the header's sampling factors say (T.81 A.2.2).
        components[0].h = 1;
        components[0].v = 1;
    }
    Ok(Frame {
        process,
        width,
        height,
        components,
        count,
    })
}

fn find_frame(input: &[u8]) -> Result<Frame> {
    if input.len() < 2 || input[0] != 0xff || input[1] != 0xd8 {
        return Err(ImageError::UnknownFormat);
    }
    let mut pos = 2;
    loop {
        let (code, after) = next_marker(input, pos)?;
        if code == 0xd9 || code == 0xda {
            return Err(ImageError::InvalidData("JPEG has no frame header"));
        }
        if is_standalone(code) {
            pos = after;
            continue;
        }
        let length = be_u16(input, after)? as usize;
        if length < 2 {
            return Err(ImageError::InvalidData("bad JPEG segment length"));
        }
        let start = after + 2;
        let end = after.checked_add(length).ok_or(ImageError::Overflow)?;
        need(input.len(), start, length - 2)?;
        if is_frame_marker(code) {
            return parse_frame(code, &input[start..end]);
        }
        pos = end;
    }
}

/// True for codes that carry no segment length, including a stuffed zero seen
/// while resynchronising and a stray SOI.
fn is_standalone(code: u8) -> bool {
    matches!(code, 0x00 | 0x01 | 0xd0..=0xd8)
}

fn is_frame_marker(code: u8) -> bool {
    matches!(code, 0xc0..=0xcf) && !matches!(code, 0xc4 | 0xc8 | 0xcc)
}

/// Reads bits from entropy-coded data, removing byte stuffing.
///
/// `fill` stops at any marker without consuming it, so `pos` never passes the
/// end of the entropy data. Bits beyond that point read as zero for lookahead,
/// and consuming them is an error.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u64,
    count: u32,
    stopped: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            bits: 0,
            count: 0,
            stopped: false,
        }
    }

    fn fill(&mut self) {
        while self.count <= 48 && !self.stopped {
            let Some(&byte) = self.data.get(self.pos) else {
                self.stopped = true;
                return;
            };
            if byte == 0xff {
                if self.data.get(self.pos + 1) != Some(&0x00) {
                    self.stopped = true;
                    return;
                }
                self.pos += 2;
            } else {
                self.pos += 1;
            }
            self.bits = (self.bits << 8) | byte as u64;
            self.count += 8;
        }
    }

    /// The next 16 bits, padded with zeros at the end of the data.
    #[inline]
    fn peek16(&mut self) -> u32 {
        if self.count < 16 {
            self.fill();
        }
        if self.count >= 16 {
            ((self.bits >> (self.count - 16)) & 0xffff) as u32
        } else {
            ((self.bits << (16 - self.count)) & 0xffff) as u32
        }
    }

    #[inline]
    fn consume(&mut self, n: u32) -> Result<()> {
        if n > self.count {
            return Err(if self.pos >= self.data.len() {
                ImageError::UnexpectedEnd
            } else {
                ImageError::InvalidData("marker inside JPEG entropy data")
            });
        }
        self.count -= n;
        Ok(())
    }

    /// Reads `n` bits, with `n` at most 16.
    #[inline]
    fn read(&mut self, n: u32) -> Result<u32> {
        let value = self.peek16() >> (16 - n);
        self.consume(n)?;
        Ok(value)
    }

    fn receive_extend(&mut self, n: u8) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 16 {
            return Err(ImageError::InvalidData("bad JPEG coefficient size"));
        }
        let value = self.read(n as u32)? as i32;
        let threshold = 1 << (n - 1);
        if value < threshold {
            Ok(value - (1 << n) + 1)
        } else {
            Ok(value)
        }
    }

    /// Drops buffered bits and moves past the next restart marker.
    fn restart(&mut self) -> Result<()> {
        self.bits = 0;
        self.count = 0;
        self.stopped = false;
        let marker = entropy_marker(self.data, self.pos);
        let code = *self.data.get(marker + 1).ok_or(ImageError::UnexpectedEnd)?;
        if !(0xd0..=0xd7).contains(&code) {
            return Err(ImageError::InvalidData("missing JPEG restart marker"));
        }
        self.pos = marker + 2;
        Ok(())
    }
}

/// Everything marker segments define that scans refer to.
struct Tables {
    quant: [Quant; 4],
    dc: [Huffman; 4],
    ac: [Huffman; 4],
    restart_interval: usize,
    jfif: bool,
    adobe_transform: Option<u8>,
}

impl Tables {
    fn new() -> Self {
        Self {
            quant: [Quant::EMPTY; 4],
            dc: [Huffman::EMPTY; 4],
            ac: [Huffman::EMPTY; 4],
            restart_interval: 0,
            jfif: false,
            adobe_transform: None,
        }
    }

    fn segment(&mut self, code: u8, data: &[u8]) -> Result<()> {
        match code {
            0xdb => self.read_quant(data),
            0xc4 => self.read_huffman(data),
            0xdd => {
                if data.len() != 2 {
                    return Err(ImageError::InvalidData("bad JPEG DRI segment"));
                }
                self.restart_interval = u16::from_be_bytes([data[0], data[1]]) as usize;
                Ok(())
            }
            0xe0 => {
                if data.starts_with(b"JFIF\0") {
                    self.jfif = true;
                }
                Ok(())
            }
            0xee => {
                if data.len() >= 12 && data.starts_with(b"Adobe") {
                    self.adobe_transform = Some(data[11]);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn read_quant(&mut self, data: &[u8]) -> Result<()> {
        let mut i = 0;
        while i < data.len() {
            let info = data[i];
            i += 1;
            let precision = info >> 4;
            let table = (info & 15) as usize;
            if table >= 4 || precision > 1 {
                return Err(ImageError::InvalidData("bad JPEG quantization table id"));
            }
            let size = precision as usize + 1;
            if i + 64 * size > data.len() {
                return Err(ImageError::UnexpectedEnd);
            }
            for k in 0..64 {
                let value = if size == 1 {
                    data[i + k] as u16
                } else {
                    u16::from_be_bytes([data[i + k * 2], data[i + k * 2 + 1]])
                };
                self.quant[table].values[ZIGZAG[k]] = value;
            }
            self.quant[table].defined = true;
            i += 64 * size;
        }
        Ok(())
    }

    fn read_huffman(&mut self, data: &[u8]) -> Result<()> {
        let mut i = 0;
        while i < data.len() {
            let info = data[i];
            i += 1;
            let class = info >> 4;
            let table = (info & 15) as usize;
            if table >= 4 || class > 1 || i + 16 > data.len() {
                return Err(ImageError::InvalidData("bad JPEG Huffman table"));
            }
            let counts = &data[i..i + 16];
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            i += 16;
            if i + total > data.len() {
                return Err(ImageError::UnexpectedEnd);
            }
            let symbols = &data[i..i + total];
            if class == 0 {
                self.dc[table].build(counts, symbols)?;
            } else {
                self.ac[table].build(counts, symbols)?;
            }
            i += total;
        }
        Ok(())
    }

    /// Makes DC table `id` usable, falling back to the standard tables.
    fn require_dc(&mut self, id: usize) -> Result<()> {
        if !self.dc[id].defined {
            match id {
                0 => self.dc[0].build(&tables::DC_LUMA_COUNTS, &tables::DC_LUMA_SYMBOLS)?,
                1 => self.dc[1].build(&tables::DC_CHROMA_COUNTS, &tables::DC_CHROMA_SYMBOLS)?,
                _ => return Err(UNDEFINED_TABLE),
            }
        }
        Ok(())
    }

    /// Makes AC table `id` usable, falling back to the standard tables.
    fn require_ac(&mut self, id: usize) -> Result<()> {
        if !self.ac[id].defined {
            match id {
                0 => self.ac[0].build(&tables::AC_LUMA_COUNTS, &tables::AC_LUMA_SYMBOLS)?,
                1 => self.ac[1].build(&tables::AC_CHROMA_COUNTS, &tables::AC_CHROMA_SYMBOLS)?,
                _ => return Err(UNDEFINED_TABLE),
            }
        }
        Ok(())
    }
}

const UNDEFINED_TABLE: ImageError =
    ImageError::InvalidData("JPEG table referenced but not defined");

/// One SOS header. Arrays are in scan order; `order` holds frame indices.
struct Scan {
    count: usize,
    order: [usize; MAX_COMPONENTS],
    dc: [usize; MAX_COMPONENTS],
    ac: [usize; MAX_COMPONENTS],
    start: usize,
    end: usize,
    high: u32,
    low: u32,
}

fn parse_scan(data: &[u8], frame: &Frame) -> Result<Scan> {
    let count = *data.first().ok_or(ImageError::UnexpectedEnd)? as usize;
    if count == 0 || count > frame.count {
        return Err(ImageError::InvalidData("bad JPEG scan component count"));
    }
    if data.len() < 1 + count * 2 + 3 {
        return Err(ImageError::UnexpectedEnd);
    }
    let mut scan = Scan {
        count,
        order: [0; MAX_COMPONENTS],
        dc: [0; MAX_COMPONENTS],
        ac: [0; MAX_COMPONENTS],
        start: data[1 + count * 2] as usize,
        end: data[2 + count * 2] as usize,
        high: (data[3 + count * 2] >> 4) as u32,
        low: (data[3 + count * 2] & 15) as u32,
    };
    let mut seen = 0u8;
    for i in 0..count {
        let id = data[1 + i * 2];
        let tables = data[2 + i * 2];
        let index = frame.components[..frame.count]
            .iter()
            .position(|c| c.id == id)
            .ok_or(ImageError::InvalidData("unknown JPEG component"))?;
        if seen & (1 << index) != 0 {
            return Err(ImageError::InvalidData("repeated JPEG scan component"));
        }
        seen |= 1 << index;
        scan.order[i] = index;
        scan.dc[i] = (tables >> 4) as usize;
        scan.ac[i] = (tables & 15) as usize;
        if scan.dc[i] >= 4 || scan.ac[i] >= 4 {
            return Err(ImageError::InvalidData("bad JPEG Huffman table id"));
        }
    }
    if frame.process == 0xc2 {
        let bad_band = if scan.start == 0 {
            scan.end != 0
        } else {
            scan.end < scan.start || scan.end > 63 || count != 1
        };
        let bad_bits = scan.low > 13 || (scan.high != 0 && scan.low + 1 != scan.high);
        if bad_band || bad_bits {
            return Err(ImageError::InvalidData("bad JPEG progressive scan"));
        }
    } else if scan.start != 0 || scan.end != 63 || scan.high != 0 || scan.low != 0 {
        return Err(ImageError::InvalidData("bad JPEG sequential scan"));
    }
    Ok(scan)
}

/// How a frame's components become RGB.
#[derive(Clone, Copy)]
enum Color {
    Gray,
    YCbCr,
    Rgb,
    /// `inverted` is the Adobe convention, where 255 means no ink.
    Cmyk {
        inverted: bool,
    },
    Ycck,
}

/// Picks the colour transform the same way libjpeg does.
fn color_of(frame: &Frame, tables: &Tables) -> Result<Color> {
    match frame.count {
        1 => Ok(Color::Gray),
        3 => {
            let ids = [
                frame.components[0].id,
                frame.components[1].id,
                frame.components[2].id,
            ];
            Ok(if tables.jfif {
                Color::YCbCr
            } else if let Some(transform) = tables.adobe_transform {
                if transform == 0 {
                    Color::Rgb
                } else {
                    Color::YCbCr
                }
            } else if ids == *b"RGB" {
                Color::Rgb
            } else {
                Color::YCbCr
            })
        }
        4 => Ok(match tables.adobe_transform {
            None => Color::Cmyk { inverted: false },
            Some(0) => Color::Cmyk { inverted: true },
            Some(_) => Color::Ycck,
        }),
        _ => Err(ImageError::UnsupportedFeature("JPEG component count")),
    }
}

#[inline]
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    // 16.16 fixed point: 1.402, 0.34414, 0.71414 and 1.772.
    let y = y as i32;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;
    let r = y + ((91881 * cr + 32768) >> 16);
    let g = y + ((-22554 * cb - 46802 * cr + 32768) >> 16);
    let b = y + ((116130 * cb + 32768) >> 16);
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

#[inline]
fn mul255(a: u8, b: u8) -> u8 {
    ((a as u32 * b as u32 + 127) / 255) as u8
}

/// Frame-wide MCU layout. MCU sample planes sit at `plane_base`, one per
/// component, each holding `h * v` blocks of 64 samples.
struct Geometry {
    max_h: usize,
    max_v: usize,
    mcu_width: usize,
    mcu_height: usize,
    mcus_x: usize,
    mcus_y: usize,
    blocks_per_mcu: usize,
    plane_base: [usize; MAX_COMPONENTS],
    /// Per component, the offset in a plane row of each MCU pixel's sample.
    x_map: [[u16; 32]; MAX_COMPONENTS],
    /// Per component, the offset in the plane of each MCU row of samples.
    y_map: [[u16; 32]; MAX_COMPONENTS],
}

impl Geometry {
    fn new(frame: &Frame) -> Self {
        let components = &frame.components[..frame.count];
        let max_h = components.iter().map(|c| c.h).max().unwrap_or(1);
        let max_v = components.iter().map(|c| c.v).max().unwrap_or(1);
        let mcu_width = max_h * 8;
        let mcu_height = max_v * 8;
        let mut geometry = Self {
            max_h,
            max_v,
            mcu_width,
            mcu_height,
            mcus_x: frame.width.div_ceil(mcu_width),
            mcus_y: frame.height.div_ceil(mcu_height),
            blocks_per_mcu: 0,
            plane_base: [0; MAX_COMPONENTS],
            x_map: [[0; 32]; MAX_COMPONENTS],
            y_map: [[0; 32]; MAX_COMPONENTS],
        };
        for (i, c) in components.iter().enumerate() {
            geometry.plane_base[i] = geometry.blocks_per_mcu * 64;
            geometry.blocks_per_mcu += c.h * c.v;
            for px in 0..mcu_width {
                let sx = px * c.h / max_h;
                geometry.x_map[i][px] = (sx / 8 * 64 + sx % 8) as u16;
            }
            for py in 0..mcu_height {
                let sy = py * c.v / max_v;
                geometry.y_map[i][py] = (sy / 8 * c.h * 64 + sy % 8 * 8) as u16;
            }
        }
        geometry
    }

    /// Writes the visible part of one MCU's samples as RGBA pixels.
    fn emit(
        &self,
        frame: &Frame,
        color: Color,
        planes: &[u8],
        mcu_x: usize,
        mcu_y: usize,
        out: &mut [u8],
    ) {
        let base_x = mcu_x * self.mcu_width;
        let base_y = mcu_y * self.mcu_height;
        let width = self.mcu_width.min(frame.width - base_x);
        let height = self.mcu_height.min(frame.height - base_y);
        for py in 0..height {
            let start = ((base_y + py) * frame.width + base_x) * 4;
            let row = &mut out[start..start + width * 4];
            let mut rows = [0usize; MAX_COMPONENTS];
            for (i, base) in rows.iter_mut().enumerate().take(frame.count) {
                *base = self.plane_base[i] + self.y_map[i][py] as usize;
            }
            for (px, pixel) in row.chunks_exact_mut(4).enumerate() {
                let sample = |i: usize| planes[rows[i] + self.x_map[i][px] as usize];
                let (r, g, b) = match color {
                    Color::Gray => {
                        let value = sample(0);
                        (value, value, value)
                    }
                    Color::YCbCr => ycbcr_to_rgb(sample(0), sample(1), sample(2)),
                    Color::Rgb => (sample(0), sample(1), sample(2)),
                    Color::Cmyk { inverted: true } => {
                        let k = sample(3);
                        (
                            mul255(sample(0), k),
                            mul255(sample(1), k),
                            mul255(sample(2), k),
                        )
                    }
                    Color::Cmyk { inverted: false } => {
                        let k = 255 - sample(3);
                        (
                            mul255(255 - sample(0), k),
                            mul255(255 - sample(1), k),
                            mul255(255 - sample(2), k),
                        )
                    }
                    Color::Ycck => {
                        // The first three components hold inverted CMY as
                        // YCC, so they convert to the complement of the ink,
                        // and K is stored inverted.
                        let (c, m, y) = ycbcr_to_rgb(sample(0), sample(1), sample(2));
                        let k = sample(3);
                        (mul255(255 - c, k), mul255(255 - m, k), mul255(255 - y, k))
                    }
                };
                pixel[0] = r;
                pixel[1] = g;
                pixel[2] = b;
                pixel[3] = 255;
            }
        }
    }
}

/// Decodes one sequential block, dequantized, in natural order.
///
/// Returns whether the block has any nonzero AC coefficient.
fn block(
    br: &mut BitReader<'_>,
    dc: &Huffman,
    ac: &Huffman,
    quant: &Quant,
    coefficient: &mut [i32; 64],
    predictor: &mut i32,
) -> Result<bool> {
    coefficient.fill(0);
    let dc_category = dc.decode(br)?;
    let difference = br.receive_extend(dc_category)?;
    *predictor = predictor
        .checked_add(difference)
        .ok_or(ImageError::InvalidData("JPEG DC overflow"))?;
    coefficient[0] = dequantize(*predictor, quant.values[0]);

    let mut has_ac = false;
    let mut k = 1usize;
    while k < 64 {
        let symbol = ac.decode(br)?;
        let run = (symbol >> 4) as usize;
        let size = symbol & 15;
        if size == 0 {
            if run == 15 {
                k += 16;
                continue;
            }
            break;
        }
        k += run;
        if k >= 64 {
            return Err(ImageError::InvalidData("JPEG AC run overflow"));
        }
        let index = ZIGZAG[k];
        coefficient[index] = dequantize(br.receive_extend(size)?, quant.values[index]);
        has_ac = true;
        k += 1;
    }
    Ok(has_ac)
}

/// Decodes a sequential scan holding every component straight into `out`.
#[allow(clippy::too_many_arguments)]
fn decode_interleaved(
    input: &[u8],
    entropy_pos: usize,
    frame: &Frame,
    geometry: &Geometry,
    scan: &Scan,
    tables: &Tables,
    color: Color,
    out: &mut [u8],
) -> Result<()> {
    let mut planes = [0u8; MAX_FAST_BLOCKS * 64];
    let mut coefficient = [0i32; 64];
    let mut predictors = [0i32; MAX_COMPONENTS];
    let mut samples = [0u8; 64];
    let mut reader = BitReader::new(input, entropy_pos);
    let mut countdown = tables.restart_interval;

    for mcu_y in 0..geometry.mcus_y {
        for mcu_x in 0..geometry.mcus_x {
            if tables.restart_interval != 0 {
                if countdown == 0 {
                    reader.restart()?;
                    predictors = [0; MAX_COMPONENTS];
                    countdown = tables.restart_interval;
                }
                countdown -= 1;
            }
            for s in 0..scan.count {
                let index = scan.order[s];
                let c = frame.components[index];
                let base = geometry.plane_base[index];
                for b in 0..c.h * c.v {
                    let has_ac = block(
                        &mut reader,
                        &tables.dc[scan.dc[s]],
                        &tables.ac[scan.ac[s]],
                        &tables.quant[c.tq],
                        &mut coefficient,
                        &mut predictors[s],
                    )?;
                    idct(&coefficient, has_ac, &mut samples);
                    planes[base + b * 64..base + (b + 1) * 64].copy_from_slice(&samples);
                }
            }
            geometry.emit(frame, color, &planes, mcu_x, mcu_y, out);
        }
    }
    Ok(())
}

/// Quantized coefficients for a whole image, kept across scans.
struct Coefficients {
    data: Vec<i16>,
    /// Index in `data` where each component starts.
    base: [usize; MAX_COMPONENTS],
    /// Blocks per row of each component.
    stride: [usize; MAX_COMPONENTS],
}

const COEFFICIENT_MEMORY: ImageError =
    ImageError::UnsupportedFeature("JPEG coefficient storage exceeds the memory limit");

impl Coefficients {
    fn new(frame: &Frame, geometry: &Geometry) -> Result<Self> {
        let mut base = [0usize; MAX_COMPONENTS];
        let mut stride = [0usize; MAX_COMPONENTS];
        let mut total = 0usize;
        for i in 0..frame.count {
            let c = frame.components[i];
            base[i] = total;
            stride[i] = geometry.mcus_x * c.h;
            let blocks = stride[i]
                .checked_mul(geometry.mcus_y * c.v)
                .ok_or(COEFFICIENT_MEMORY)?;
            total = blocks
                .checked_mul(64)
                .and_then(|n| n.checked_add(total))
                .ok_or(COEFFICIENT_MEMORY)?;
        }
        if total > MAX_COEFFICIENT_BYTES / 2 {
            return Err(COEFFICIENT_MEMORY);
        }
        let mut data = Vec::new();
        data.try_reserve_exact(total)
            .map_err(|_| COEFFICIENT_MEMORY)?;
        data.resize(total, 0);
        Ok(Self { data, base, stride })
    }

    fn block_mut(&mut self, component: usize, x: usize, y: usize) -> &mut [i16] {
        let start = self.base[component] + (y * self.stride[component] + x) * 64;
        &mut self.data[start..start + 64]
    }
}

/// Decodes one sequential block as stored quantized coefficients.
fn sequential_block(
    br: &mut BitReader<'_>,
    dc: &Huffman,
    ac: &Huffman,
    coefficients: &mut [i16],
    predictor: &mut i32,
) -> Result<()> {
    coefficients.fill(0);
    let difference = {
        let category = dc.decode(br)?;
        br.receive_extend(category)?
    };
    *predictor = predictor
        .checked_add(difference)
        .ok_or(ImageError::InvalidData("JPEG DC overflow"))?;
    coefficients[0] = *predictor as i16;
    let mut k = 1usize;
    while k < 64 {
        let symbol = ac.decode(br)?;
        let run = (symbol >> 4) as usize;
        let size = symbol & 15;
        if size == 0 {
            if run == 15 {
                k += 16;
                continue;
            }
            break;
        }
        k += run;
        if k >= 64 {
            return Err(ImageError::InvalidData("JPEG AC run overflow"));
        }
        coefficients[ZIGZAG[k]] = br.receive_extend(size)? as i16;
        k += 1;
    }
    Ok(())
}

/// First pass over a band of AC coefficients (T.81 G.1.2.2).
fn ac_first(
    br: &mut BitReader<'_>,
    ac: &Huffman,
    coefficients: &mut [i16],
    scan: &Scan,
    eob_run: &mut u32,
) -> Result<()> {
    if *eob_run > 0 {
        *eob_run -= 1;
        return Ok(());
    }
    let mut k = scan.start;
    while k <= scan.end {
        let symbol = ac.decode(br)?;
        let run = (symbol >> 4) as usize;
        let size = symbol & 15;
        if size == 0 {
            if run < 15 {
                *eob_run = (1u32 << run) - 1;
                if run > 0 {
                    *eob_run += br.read(run as u32)?;
                }
                break;
            }
            k += 16;
            continue;
        }
        k += run;
        if k > scan.end {
            return Err(ImageError::InvalidData("JPEG AC run overflow"));
        }
        coefficients[ZIGZAG[k]] = (br.receive_extend(size)? << scan.low) as i16;
        k += 1;
    }
    Ok(())
}

/// Reads one correction bit for an already nonzero coefficient.
#[inline]
fn refine_coefficient(
    br: &mut BitReader<'_>,
    value: &mut i16,
    plus: i16,
    minus: i16,
) -> Result<()> {
    if br.read(1)? != 0 && *value & plus == 0 {
        *value = value.wrapping_add(if *value >= 0 { plus } else { minus });
    }
    Ok(())
}

/// Refinement pass over a band of AC coefficients (T.81 G.1.2.3).
fn ac_refine(
    br: &mut BitReader<'_>,
    ac: &Huffman,
    coefficients: &mut [i16],
    scan: &Scan,
    eob_run: &mut u32,
) -> Result<()> {
    let plus = 1i16 << scan.low;
    let minus = -1i16 << scan.low;
    let mut k = scan.start;
    if *eob_run == 0 {
        while k <= scan.end {
            let symbol = ac.decode(br)?;
            let mut run = (symbol >> 4) as usize;
            let size = symbol & 15;
            let mut value = 0i16;
            if size != 0 {
                if size != 1 {
                    return Err(ImageError::InvalidData("bad JPEG refinement symbol"));
                }
                value = if br.read(1)? != 0 { plus } else { minus };
            } else if run != 15 {
                *eob_run = 1u32 << run;
                if run > 0 {
                    *eob_run += br.read(run as u32)?;
                }
                break;
            }
            // Skip `run` zero-history coefficients, refining the others.
            while k <= scan.end {
                let current = &mut coefficients[ZIGZAG[k]];
                if *current != 0 {
                    refine_coefficient(br, current, plus, minus)?;
                } else {
                    if run == 0 {
                        break;
                    }
                    run -= 1;
                }
                k += 1;
            }
            if value != 0 {
                if k > scan.end {
                    return Err(ImageError::InvalidData("JPEG AC run overflow"));
                }
                coefficients[ZIGZAG[k]] = value;
            }
            k += 1;
        }
    }
    if *eob_run > 0 {
        while k <= scan.end {
            let current = &mut coefficients[ZIGZAG[k]];
            if *current != 0 {
                refine_coefficient(br, current, plus, minus)?;
            }
            k += 1;
        }
        *eob_run -= 1;
    }
    Ok(())
}

/// Decodes one scan into stored coefficients, and returns where it stopped.
fn coefficient_scan(
    input: &[u8],
    entropy_pos: usize,
    frame: &Frame,
    geometry: &Geometry,
    scan: &Scan,
    tables: &Tables,
    store: &mut Coefficients,
) -> Result<usize> {
    let progressive = frame.process == 0xc2;
    let single = scan.count == 1;
    let (units_x, units_y) = if single {
        // A non-interleaved MCU is one block, and only blocks holding image
        // samples are coded (T.81 A.2.2).
        let c = frame.components[scan.order[0]];
        let samples_x = (frame.width * c.h).div_ceil(geometry.max_h);
        let samples_y = (frame.height * c.v).div_ceil(geometry.max_v);
        (samples_x.div_ceil(8), samples_y.div_ceil(8))
    } else {
        (geometry.mcus_x, geometry.mcus_y)
    };

    let mut reader = BitReader::new(input, entropy_pos);
    let mut predictors = [0i32; MAX_COMPONENTS];
    let mut eob_run = 0u32;
    let mut countdown = tables.restart_interval;

    for unit_y in 0..units_y {
        for unit_x in 0..units_x {
            if tables.restart_interval != 0 {
                if countdown == 0 {
                    reader.restart()?;
                    predictors = [0; MAX_COMPONENTS];
                    eob_run = 0;
                    countdown = tables.restart_interval;
                }
                countdown -= 1;
            }
            for s in 0..scan.count {
                let index = scan.order[s];
                let c = frame.components[index];
                let (blocks_x, blocks_y) = if single { (1, 1) } else { (c.h, c.v) };
                for by in 0..blocks_y {
                    for bx in 0..blocks_x {
                        let (x, y) = if single {
                            (unit_x, unit_y)
                        } else {
                            (unit_x * c.h + bx, unit_y * c.v + by)
                        };
                        let coefficients = store.block_mut(index, x, y);
                        let dc = &tables.dc[scan.dc[s]];
                        let ac = &tables.ac[scan.ac[s]];
                        if !progressive {
                            sequential_block(
                                &mut reader,
                                dc,
                                ac,
                                coefficients,
                                &mut predictors[s],
                            )?;
                        } else if scan.start != 0 {
                            if scan.high == 0 {
                                ac_first(&mut reader, ac, coefficients, scan, &mut eob_run)?;
                            } else {
                                ac_refine(&mut reader, ac, coefficients, scan, &mut eob_run)?;
                            }
                        } else if scan.high == 0 {
                            let category = dc.decode(&mut reader)?;
                            let difference = reader.receive_extend(category)?;
                            predictors[s] = predictors[s]
                                .checked_add(difference)
                                .ok_or(ImageError::InvalidData("JPEG DC overflow"))?;
                            coefficients[0] = predictors[s].wrapping_shl(scan.low) as i16;
                        } else if reader.read(1)? != 0 {
                            coefficients[0] |= 1 << scan.low;
                        }
                    }
                }
            }
        }
    }
    Ok(reader.pos)
}

/// Turns stored coefficients into pixels.
fn render(
    frame: &Frame,
    geometry: &Geometry,
    store: &mut Coefficients,
    quant: &[Quant; MAX_COMPONENTS],
    color: Color,
    out: &mut [u8],
) -> Result<()> {
    let plane_len = geometry.blocks_per_mcu * 64;
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(plane_len)
        .map_err(|_| COEFFICIENT_MEMORY)?;
    planes.resize(plane_len, 0u8);

    let mut coefficient = [0i32; 64];
    let mut samples = [0u8; 64];
    for mcu_y in 0..geometry.mcus_y {
        for mcu_x in 0..geometry.mcus_x {
            for index in 0..frame.count {
                let c = frame.components[index];
                let table = &quant[index].values;
                let base = geometry.plane_base[index];
                for b in 0..c.h * c.v {
                    let (bx, by) = (b % c.h, b / c.h);
                    let stored = store.block_mut(index, mcu_x * c.h + bx, mcu_y * c.v + by);
                    let mut has_ac = false;
                    for i in 0..64 {
                        let value = stored[i];
                        coefficient[i] = dequantize(value as i32, table[i]);
                        has_ac |= i > 0 && value != 0;
                    }
                    idct(&coefficient, has_ac, &mut samples);
                    planes[base + b * 64..base + (b + 1) * 64].copy_from_slice(&samples);
                }
            }
            geometry.emit(frame, color, &planes, mcu_x, mcu_y, out);
        }
    }
    Ok(())
}

/// Returns the dimensions of a JPEG image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let frame = find_frame(input)?;
    Ok(ImageInfo {
        width: frame.width as u32,
        height: frame.height as u32,
        format: ImageFormat::Jpeg,
    })
}

/// JPEG decoding takes no caller scratch space.
///
/// Files that need working memory allocate it; see the module documentation.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes a JPEG into RGBA8888.
pub fn decode(input: &[u8], out: &mut [u8]) -> Result<ImageInfo> {
    let frame = find_frame(input)?;
    let info = ImageInfo {
        width: frame.width as u32,
        height: frame.height as u32,
        format: ImageFormat::Jpeg,
    };
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }
    if !matches!(frame.count, 1 | 3 | 4) {
        return Err(ImageError::UnsupportedFeature("JPEG component count"));
    }
    if !matches!(frame.process, 0xc0..=0xc2) {
        return Err(ImageError::UnsupportedFeature("JPEG coding process"));
    }
    let progressive = frame.process == 0xc2;
    let geometry = Geometry::new(&frame);

    let mut tables = Tables::new();
    let mut store: Option<Coefficients> = None;
    // Each component's quantizers are fixed by its first scan (T.81 G.1.1.1).
    let mut latched = [Quant::EMPTY; MAX_COMPONENTS];
    let mut started = 0u8;
    let mut scans = 0usize;
    let mut color = Color::Gray;
    let mut frames_seen = 0usize;

    let mut pos = 2;
    loop {
        let (code, after) = match next_marker(input, pos) {
            Ok(marker) => marker,
            // A multi-scan file may have lost its EOI, as a single-scan file
            // may: the fast path below never looks for it.
            Err(_) if store.is_some() => break,
            Err(error) => return Err(error),
        };
        if code == 0xd9 {
            if store.is_some() {
                break;
            }
            return Err(ImageError::InvalidData("JPEG ended before scan"));
        }
        if is_standalone(code) {
            pos = after;
            continue;
        }
        let length = be_u16(input, after)? as usize;
        if length < 2 {
            return Err(ImageError::InvalidData("bad JPEG segment length"));
        }
        let start = after + 2;
        let end = after.checked_add(length).ok_or(ImageError::Overflow)?;
        need(input.len(), start, length - 2)?;
        let data = &input[start..end];

        if is_frame_marker(code) {
            frames_seen += 1;
            if frames_seen > 1 {
                return Err(ImageError::InvalidData("multiple JPEG frames"));
            }
        } else if code != 0xda {
            tables.segment(code, data)?;
        } else {
            let scan = parse_scan(data, &frame)?;
            for s in 0..scan.count {
                if !progressive || (scan.start == 0 && scan.high == 0) {
                    tables.require_dc(scan.dc[s])?;
                }
                if !progressive || scan.start != 0 {
                    tables.require_ac(scan.ac[s])?;
                }
                if !tables.quant[frame.components[scan.order[s]].tq].defined {
                    return Err(UNDEFINED_TABLE);
                }
            }
            if store.is_none() {
                color = color_of(&frame, &tables)?;
                let complete = scan.count == frame.count;
                if !progressive && complete && geometry.blocks_per_mcu <= MAX_FAST_BLOCKS {
                    decode_interleaved(input, end, &frame, &geometry, &scan, &tables, color, out)?;
                    return Ok(info);
                }
            }
            scans += 1;
            if scans > MAX_SCANS {
                return Err(ImageError::UnsupportedFeature("JPEG scan count"));
            }
            if store.is_none() {
                store = Some(Coefficients::new(&frame, &geometry)?);
            }
            let Some(coefficients) = store.as_mut() else {
                return Err(COEFFICIENT_MEMORY);
            };
            for s in 0..scan.count {
                let index = scan.order[s];
                let bit = 1u8 << index;
                if !latched[index].defined {
                    latched[index] = tables.quant[frame.components[index].tq];
                }
                // A sequential scan is complete; a progressive component needs
                // its DC band.
                if !progressive || (scan.start == 0 && scan.high == 0) {
                    started |= bit;
                }
            }
            pos = coefficient_scan(input, end, &frame, &geometry, &scan, &tables, coefficients)?;
            continue;
        }
        pos = end;
    }

    let Some(mut coefficients) = store else {
        return Err(ImageError::InvalidData("JPEG ended before scan"));
    };
    if started != (1u8 << frame.count) - 1 {
        return Err(ImageError::InvalidData("JPEG component has no scan data"));
    }
    render(&frame, &geometry, &mut coefficients, &latched, color, out)?;
    Ok(info)
}

/// Finds the next marker at or after `pos` in entropy-coded data, skipping
/// stuffed zeros and fill bytes.
///
/// Returns the offset of the marker's 0xFF byte, or `input.len()` if there is
/// none.
fn entropy_marker(input: &[u8], mut pos: usize) -> usize {
    while pos < input.len() {
        if input[pos] != 0xff {
            pos += 1;
            continue;
        }
        match input.get(pos + 1) {
            None => return input.len(),
            Some(0x00) => pos += 2,
            Some(0xff) => pos += 1,
            Some(_) => return pos,
        }
    }
    input.len()
}

/// Returns the offset just past the EOI marker of the JPEG starting at
/// `input[0]`, or `None` if `input` does not start with SOI or no EOI is found.
///
/// The walk follows the file structure instead of searching for bytes: marker
/// segments are skipped by their length (so an EOI inside an EXIF thumbnail
/// does not end the image), entropy-coded data is scanned past byte stuffing
/// and restart markers, and every scan of a multi-scan or progressive file is
/// covered. A second SOI at marker level means this image never ended, and
/// also gives `None`.
pub fn image_end(input: &[u8]) -> Option<usize> {
    if input.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut pos = 2usize;
    loop {
        let (code, after) = next_marker(input, pos).ok()?;
        match code {
            0xd9 => return Some(after),
            0xd8 => return None,
            // Not markers, or markers without a length: keep walking.
            0x00 | 0x01 | 0xd0..=0xd7 => pos = after,
            _ => {
                let length = be_u16(input, after).ok()? as usize;
                if length < 2 {
                    return None;
                }
                let end = after.checked_add(length)?;
                if end > input.len() {
                    return None;
                }
                pos = if code == 0xda {
                    entropy_end(input, end)
                } else {
                    end
                };
            }
        }
    }
}

/// Finds the first marker after entropy-coded data that is not a restart.
fn entropy_end(input: &[u8], mut pos: usize) -> usize {
    loop {
        let marker = entropy_marker(input, pos);
        match input.get(marker + 1) {
            Some(0xd0..=0xd7) => pos = marker + 2,
            _ => return marker,
        }
    }
}
