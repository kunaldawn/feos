//! Baseline and extended-sequential JPEG decoding.
//!
//! The decoder handles 8-bit Huffman-coded JPEG files, including restart
//! intervals, arbitrary chroma subsampling and grayscale images. Progressive
//! JPEG, arithmetic coding, 12-bit samples and CMYK/YCCK colour are rejected
//! with [`ImageError::UnsupportedFeature`].
//!
//! Output is always RGBA8888.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

const MAX_COMPONENTS: usize = 4;
const MAX_BLOCKS: usize = MAX_COMPONENTS * 4;
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// 16384 * C(u) * cos((2x + 1)u*pi/16)
const COS: [[i32; 8]; 8] = [
    [11585, 11585, 11585, 11585, 11585, 11585, 11585, 11585],
    [16069, 13623, 9102, 3196, -3196, -9102, -13623, -16069],
    [15137, 6270, -6270, -15137, -15137, -6270, 6270, 15137],
    [13623, -3196, -16069, -9102, 9102, 16069, 3196, -13623],
    [11585, -11585, -11585, 11585, 11585, -11585, -11585, 11585],
    [9102, -16069, 3196, 13623, -13623, -3196, 16069, -9102],
    [6270, -15137, 15137, -6270, -6270, 15137, -15137, 6270],
    [3196, -9102, 13623, -16069, 16069, -13623, 9102, -3196],
];
const IDCT_SCALE: i64 = 16384;
const IDCT_DIV: i64 = 4 * IDCT_SCALE * IDCT_SCALE;

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
    count: [u8; 17],
    symbols: [u8; 256],
    defined: bool,
}

impl Huffman {
    const EMPTY: Self = Self {
        count: [0; 17],
        symbols: [0; 256],
        defined: false,
    };

    fn build(&mut self, counts: &[u8], symbols: &[u8]) -> Result<()> {
        if counts.len() != 16 || symbols.len() > 256 {
            return Err(ImageError::InvalidData("bad DHT segment"));
        }
        self.count = [0; 17];
        self.count[1..].copy_from_slice(counts);
        self.symbols[..symbols.len()].copy_from_slice(symbols);
        self.defined = true;
        Ok(())
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u8> {
        if !self.defined {
            return Err(ImageError::InvalidData("undefined Huffman table"));
        }
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for length in 1..=16 {
            code |= br.bit()? as i32;
            let count = self.count[length] as i32;
            if code - first < count {
                let slot = index + (code - first);
                return self
                    .symbols
                    .get(slot as usize)
                    .copied()
                    .ok_or(ImageError::InvalidData("bad Huffman symbol"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
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
    dc: usize,
    ac: usize,
}

impl Component {
    const EMPTY: Self = Self {
        id: 0,
        h: 1,
        v: 1,
        tq: 0,
        dc: 0,
        ac: 0,
    };
}

struct Frame {
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

fn parse_frame(data: &[u8]) -> Result<Frame> {
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
    let mut blocks_per_mcu = 0usize;
    for i in 0..count {
        let base = 6 + i * 3;
        let sampling = data[base + 1];
        let h = (sampling >> 4) as usize;
        let v = (sampling & 15) as usize;
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
            return Err(ImageError::InvalidData("bad JPEG sampling factor"));
        }
        if h * v > 4 {
            return Err(ImageError::UnsupportedFeature("JPEG sampling factors"));
        }
        blocks_per_mcu += h * v;
        let tq = data[base + 2] as usize;
        if tq >= 4 {
            return Err(ImageError::InvalidData("bad JPEG quantization table"));
        }
        components[i] = Component {
            id: data[base],
            h,
            v,
            tq,
            dc: 0,
            ac: 0,
        };
    }
    if blocks_per_mcu > MAX_BLOCKS {
        return Err(ImageError::UnsupportedFeature("JPEG MCU block count"));
    }
    Ok(Frame {
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
        if (0xd0..=0xd7).contains(&code) || code == 0x01 {
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
        if let 0xc0..=0xcf = code {
            if !matches!(code, 0xc4 | 0xc8 | 0xcc) {
                return parse_frame(&input[start..end]);
            }
        }
        pos = end;
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u8,
    count: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            bits: 0,
            count: 0,
        }
    }

    fn fill(&mut self) -> Result<()> {
        let byte = *self.data.get(self.pos).ok_or(ImageError::UnexpectedEnd)?;
        self.pos += 1;
        if byte == 0xff {
            let next = *self.data.get(self.pos).ok_or(ImageError::UnexpectedEnd)?;
            if next != 0x00 {
                return Err(ImageError::InvalidData("marker inside JPEG entropy data"));
            }
            self.pos += 1;
        }
        self.bits = byte;
        self.count = 8;
        Ok(())
    }

    fn bit(&mut self) -> Result<u32> {
        if self.count == 0 {
            self.fill()?;
        }
        let value = (self.bits >> 7) as u32 & 1;
        self.bits <<= 1;
        self.count -= 1;
        Ok(value)
    }

    fn read(&mut self, n: u8) -> Result<u32> {
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn receive_extend(&mut self, n: u8) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 16 {
            return Err(ImageError::InvalidData("bad JPEG coefficient size"));
        }
        let value = self.read(n)? as i32;
        let threshold = 1 << (n - 1);
        if value < threshold {
            Ok(value - (1 << n) + 1)
        } else {
            Ok(value)
        }
    }

    fn restart(&mut self) -> Result<()> {
        self.bits = 0;
        self.count = 0;
        while self.data.get(self.pos) == Some(&0xff) && self.data.get(self.pos + 1) == Some(&0xff) {
            self.pos += 1;
        }
        let marker = *self.data.get(self.pos).ok_or(ImageError::UnexpectedEnd)?;
        let code = *self
            .data
            .get(self.pos + 1)
            .ok_or(ImageError::UnexpectedEnd)?;
        if marker != 0xff || !(0xd0..=0xd7).contains(&code) {
            return Err(ImageError::InvalidData("missing JPEG restart marker"));
        }
        self.pos += 2;
        Ok(())
    }
}

// The real block decoder lives outside the `Huffman` impl to keep the borrow
// of the bit reader explicit.
fn block(
    br: &mut BitReader<'_>,
    dc: &Huffman,
    ac: &Huffman,
    coefficient: &mut [i32; 64],
    predictor: &mut i32,
) -> Result<()> {
    coefficient.fill(0);
    let dc_category = dc.decode(br)?;
    let difference = br.receive_extend(dc_category)?;
    *predictor = predictor
        .checked_add(difference)
        .ok_or(ImageError::InvalidData("JPEG DC overflow"))?;
    coefficient[0] = *predictor;

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
        coefficient[ZIGZAG[k]] = br.receive_extend(size)?;
        k += 1;
    }
    Ok(())
}

fn idct(coefficient: &[i32; 64], quant: &Quant, out: &mut [u8]) {
    let mut temp = [0i64; 64];
    for v in 0..8 {
        for x in 0..8 {
            let mut sum = 0i64;
            for u in 0..8 {
                let index = v * 8 + u;
                sum += COS[u][x] as i64 * coefficient[index] as i64 * quant.values[index] as i64;
            }
            temp[v * 8 + x] = sum;
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0i64;
            for v in 0..8 {
                sum += COS[v][y] as i64 * temp[v * 8 + x];
            }
            let value = sum / IDCT_DIV + 128;
            out[y * 8 + x] = value.clamp(0, 255) as u8;
        }
    }
}

fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as i32;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;
    let r = y + (cr * 359) / 256;
    let g = y - (cb * 88 + cr * 183) / 256;
    let b = y + (cb * 454) / 256;
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
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

/// JPEG decoding needs no scratch space.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes a baseline JPEG into RGBA8888.
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
    if frame.count == 4 {
        return Err(ImageError::UnsupportedFeature("CMYK JPEG"));
    }
    if frame.count != 1 && frame.count != 3 {
        return Err(ImageError::UnsupportedFeature("JPEG component count"));
    }

    let mut quant = [Quant::EMPTY; 4];
    let mut dc = [Huffman::EMPTY; 4];
    let mut ac = [Huffman::EMPTY; 4];
    let mut restart_interval = 0usize;
    let mut components = frame.components;

    let mut pos = 2;
    loop {
        let (code, after) = next_marker(input, pos)?;
        if code == 0xd9 {
            return Err(ImageError::InvalidData("JPEG ended before scan"));
        }
        if (0xd0..=0xd7).contains(&code) || code == 0x01 {
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

        match code {
            0xdb => {
                let mut i = 0;
                while i < data.len() {
                    let info = data[i];
                    i += 1;
                    let precision = info >> 4;
                    let table = (info & 15) as usize;
                    if table >= 4 {
                        return Err(ImageError::InvalidData("bad JPEG quantization table id"));
                    }
                    let size = if precision == 0 { 1 } else { 2 };
                    if i + 64 * size > data.len() {
                        return Err(ImageError::UnexpectedEnd);
                    }
                    for k in 0..64 {
                        let value = if size == 1 {
                            data[i + k] as u16
                        } else {
                            u16::from_be_bytes([data[i + k * 2], data[i + k * 2 + 1]])
                        };
                        quant[table].values[ZIGZAG[k]] = value;
                    }
                    quant[table].defined = true;
                    i += 64 * size;
                }
            }
            0xc4 => {
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
                        dc[table].build(counts, symbols)?;
                    } else {
                        ac[table].build(counts, symbols)?;
                    }
                    i += total;
                }
            }
            0xdd => {
                if data.len() != 2 {
                    return Err(ImageError::InvalidData("bad JPEG DRI segment"));
                }
                restart_interval = u16::from_be_bytes([data[0], data[1]]) as usize;
            }
            0xda => {
                let count = *data.first().ok_or(ImageError::UnexpectedEnd)? as usize;
                if count != frame.count {
                    return Err(ImageError::UnsupportedFeature("non-interleaved JPEG scan"));
                }
                if data.len() < 1 + count * 2 + 3 {
                    return Err(ImageError::UnexpectedEnd);
                }
                for i in 0..count {
                    let id = data[1 + i * 2];
                    let tables = data[2 + i * 2];
                    let index = components
                        .iter()
                        .position(|c| c.id == id)
                        .ok_or(ImageError::InvalidData("unknown JPEG component"))?;
                    components[index].dc = (tables >> 4) as usize;
                    components[index].ac = (tables & 15) as usize;
                    if components[index].dc >= 4 || components[index].ac >= 4 {
                        return Err(ImageError::InvalidData("bad JPEG Huffman table id"));
                    }
                }
                let spectral_start = data[1 + count * 2];
                let spectral_end = data[2 + count * 2];
                let approximation = data[3 + count * 2];
                if spectral_start != 0 || spectral_end != 63 || approximation != 0 {
                    return Err(ImageError::UnsupportedFeature("progressive JPEG"));
                }

                let entropy_pos = end;
                decode_scan(
                    input,
                    entropy_pos,
                    &frame,
                    &components,
                    &quant,
                    &dc,
                    &ac,
                    restart_interval,
                    out,
                )?;
                return Ok(info);
            }
            0xc0 | 0xc1 => {}
            0xc2 => return Err(ImageError::UnsupportedFeature("progressive JPEG")),
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                return Err(ImageError::UnsupportedFeature("JPEG coding process"))
            }
            _ => {}
        }
        pos = end;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    input: &[u8],
    entropy_pos: usize,
    frame: &Frame,
    components: &[Component; MAX_COMPONENTS],
    quant: &[Quant; 4],
    dc: &[Huffman; 4],
    ac: &[Huffman; 4],
    restart_interval: usize,
    out: &mut [u8],
) -> Result<()> {
    for i in 0..frame.count {
        let c = components[i];
        if !quant[c.tq].defined || !dc[c.dc].defined || !ac[c.ac].defined {
            return Err(ImageError::InvalidData(
                "JPEG table referenced but not defined",
            ));
        }
    }

    let max_h = components[..frame.count]
        .iter()
        .map(|c| c.h)
        .max()
        .unwrap_or(1);
    let max_v = components[..frame.count]
        .iter()
        .map(|c| c.v)
        .max()
        .unwrap_or(1);
    let mcu_width = max_h * 8;
    let mcu_height = max_v * 8;
    let mcus_x = (frame.width + mcu_width - 1) / mcu_width;
    let mcus_y = (frame.height + mcu_height - 1) / mcu_height;
    let total_mcus = mcus_x.checked_mul(mcus_y).ok_or(ImageError::Overflow)?;

    let mut planes = [0u8; MAX_BLOCKS * 64];
    let mut coefficient = [0i32; 64];
    let mut predictors = [0i32; MAX_COMPONENTS];
    let mut reader = BitReader::new(input, entropy_pos);

    for mcu in 0..total_mcus {
        if restart_interval != 0 && mcu != 0 && mcu % restart_interval == 0 {
            reader.restart()?;
            predictors = [0; MAX_COMPONENTS];
        }
        for ci in 0..frame.count {
            let c = components[ci];
            let plane = &mut planes[ci * 256..ci * 256 + 256];
            for by in 0..c.v {
                for bx in 0..c.h {
                    block(
                        &mut reader,
                        &dc[c.dc],
                        &ac[c.ac],
                        &mut coefficient,
                        &mut predictors[ci],
                    )?;
                    let block_index = by * c.h + bx;
                    idct(
                        &coefficient,
                        &quant[c.tq],
                        &mut plane[block_index * 64..block_index * 64 + 64],
                    );
                }
            }
        }

        let mcu_x = mcu % mcus_x;
        let mcu_y = mcu / mcus_x;
        let base_x = mcu_x * mcu_width;
        let base_y = mcu_y * mcu_height;
        for py in 0..mcu_height {
            let y = base_y + py;
            if y >= frame.height {
                break;
            }
            for px in 0..mcu_width {
                let x = base_x + px;
                if x >= frame.width {
                    break;
                }
                let (r, g, b) = if frame.count == 1 {
                    let value = planes[sample_index(0, px, py, &components[0], max_h, max_v)];
                    (value, value, value)
                } else {
                    let yy = planes[sample_index(0, px, py, &components[0], max_h, max_v)];
                    let cb = planes[sample_index(1, px, py, &components[1], max_h, max_v)];
                    let cr = planes[sample_index(2, px, py, &components[2], max_h, max_v)];
                    ycbcr_to_rgb(yy, cb, cr)
                };
                let index = (y * frame.width + x) * 4;
                out[index] = r;
                out[index + 1] = g;
                out[index + 2] = b;
                out[index + 3] = 255;
            }
        }
    }
    Ok(())
}

fn sample_index(
    component: usize,
    px: usize,
    py: usize,
    c: &Component,
    max_h: usize,
    max_v: usize,
) -> usize {
    let sx = px * c.h / max_h;
    let sy = py * c.v / max_v;
    component * 256 + sy * 8 * c.h + sx
}
