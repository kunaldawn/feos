//! PNG (Portable Network Graphics) decoder and encoder.
//!
//! The decoder supports every legal combination of colour type and bit depth,
//! palette images with `tRNS` transparency, and both non-interlaced and Adam7
//! interlaced files. Output is always RGBA8888.
//!
//! The encoder writes 8-bit RGBA using uncompressed zlib "stored" blocks. This
//! keeps the encoder small while still producing standards-compliant PNG files.

use crate::error::{need, ImageError, Result};
use crate::{ImageFormat, ImageInfo};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const MAX_DIMENSION: u32 = 1 << 24;

#[derive(Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: bool,
}

impl Header {
    fn channels(self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 0,
        }
    }

    fn bits_per_pixel(self) -> usize {
        self.channels() * self.bit_depth as usize
    }

    fn bytes_per_pixel(self) -> usize {
        (self.bits_per_pixel() + 7) / 8
    }

    fn filter_bpp(self) -> usize {
        self.bytes_per_pixel().max(1)
    }

    fn row_bytes(self, width: usize) -> usize {
        (width * self.bits_per_pixel() + 7) / 8
    }
}

fn be_u32(input: &[u8], offset: usize) -> Result<u32> {
    need(input.len(), offset, 4)?;
    Ok(u32::from_be_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn parse_header(input: &[u8]) -> Result<(Header, usize)> {
    if input.len() < SIGNATURE.len() || input[..8] != SIGNATURE {
        return Err(ImageError::UnknownFormat);
    }
    let (kind, data, data_offset) = next_chunk(input, SIGNATURE.len())?;
    if kind != *b"IHDR" || data.len() != 13 {
        return Err(ImageError::InvalidData("missing IHDR chunk"));
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let bit_depth = data[8];
    let color_type = data[9];
    let compression = data[10];
    let filter = data[11];
    let interlace = match data[12] {
        0 => false,
        1 => true,
        _ => return Err(ImageError::InvalidData("bad interlace method")),
    };
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(ImageError::InvalidData("bad image dimensions"));
    }
    if compression != 0 || filter != 0 {
        return Err(ImageError::UnsupportedFeature(
            "png compression or filter method",
        ));
    }
    let header = Header {
        width,
        height,
        bit_depth,
        color_type,
        interlace,
    };
    let valid_depth = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if !valid_depth {
        return Err(ImageError::InvalidData("invalid colour type / bit depth"));
    }
    Ok((header, data_offset + 13 + 4))
}

fn next_chunk<'a>(input: &'a [u8], pos: usize) -> Result<([u8; 4], &'a [u8], usize)> {
    need(input.len(), pos, 8)?;
    let length = be_u32(input, pos)? as usize;
    let kind = [
        input[pos + 4],
        input[pos + 5],
        input[pos + 6],
        input[pos + 7],
    ];
    let data_offset = pos + 8;
    need(
        input.len(),
        data_offset,
        length.checked_add(4).ok_or(ImageError::Overflow)?,
    )?;
    let data = &input[data_offset..data_offset + length];
    let expected = be_u32(input, data_offset + length)?;
    let mut crc = Crc32::new();
    crc.update(&kind);
    crc.update(data);
    if crc.finish() != expected {
        return Err(ImageError::Checksum);
    }
    Ok((kind, data, data_offset))
}

const ADAM7_X_START: [usize; 7] = [0, 4, 0, 2, 0, 1, 0];
const ADAM7_Y_START: [usize; 7] = [0, 0, 4, 0, 2, 0, 1];
const ADAM7_X_STEP: [usize; 7] = [8, 8, 4, 4, 2, 2, 1];
const ADAM7_Y_STEP: [usize; 7] = [8, 8, 8, 4, 4, 2, 2];

fn pass_size(header: Header, pass: usize) -> (usize, usize) {
    let width = header.width as usize;
    let height = header.height as usize;
    let pw = if width > ADAM7_X_START[pass] {
        (width - ADAM7_X_START[pass] + ADAM7_X_STEP[pass] - 1) / ADAM7_X_STEP[pass]
    } else {
        0
    };
    let ph = if height > ADAM7_Y_START[pass] {
        (height - ADAM7_Y_START[pass] + ADAM7_Y_STEP[pass] - 1) / ADAM7_Y_STEP[pass]
    } else {
        0
    };
    (pw, ph)
}

fn filtered_size(header: Header) -> Result<usize> {
    let mut total = 0usize;
    if header.interlace {
        for pass in 0..7 {
            let (width, height) = pass_size(header, pass);
            if width == 0 || height == 0 {
                continue;
            }
            let row = 1 + header.row_bytes(width);
            total = total
                .checked_add(row.checked_mul(height).ok_or(ImageError::Overflow)?)
                .ok_or(ImageError::Overflow)?;
        }
    } else {
        let row = 1 + header.row_bytes(header.width as usize);
        total = row
            .checked_mul(header.height as usize)
            .ok_or(ImageError::Overflow)?;
    }
    Ok(total)
}

struct IdatInfo {
    first_data: usize,
    end: usize,
    total: usize,
    contiguous: bool,
}

fn find_idat(input: &[u8], mut pos: usize) -> Result<IdatInfo> {
    let mut info = IdatInfo {
        first_data: 0,
        end: 0,
        total: 0,
        contiguous: true,
    };
    let mut seen = false;
    loop {
        let (kind, data, data_offset) = next_chunk(input, pos)?;
        if kind == *b"IDAT" {
            if !seen {
                info.first_data = data_offset;
                seen = true;
            } else if data_offset != info.end {
                info.contiguous = false;
            }
            info.end = data_offset + data.len();
            info.total = info
                .total
                .checked_add(data.len())
                .ok_or(ImageError::Overflow)?;
        } else if kind == *b"IEND" {
            break;
        }
        pos = data_offset + data.len() + 4;
        if pos > input.len() {
            return Err(ImageError::UnexpectedEnd);
        }
    }
    if !seen {
        return Err(ImageError::InvalidData("missing IDAT chunk"));
    }
    Ok(info)
}

fn copy_idat(input: &[u8], mut pos: usize, out: &mut [u8]) -> Result<usize> {
    let mut written = 0usize;
    loop {
        let (kind, data, data_offset) = next_chunk(input, pos)?;
        if kind == *b"IDAT" {
            let end = written
                .checked_add(data.len())
                .ok_or(ImageError::Overflow)?;
            if end > out.len() {
                return Err(ImageError::BufferTooSmall {
                    required: end,
                    available: out.len(),
                });
            }
            out[written..end].copy_from_slice(data);
            written = end;
        } else if kind == *b"IEND" {
            return Ok(written);
        }
        pos = data_offset + data.len() + 4;
    }
}

/// Returns the dimensions of a PNG image.
pub fn info(input: &[u8]) -> Result<ImageInfo> {
    let (header, _) = parse_header(input)?;
    Ok(ImageInfo {
        width: header.width,
        height: header.height,
        format: ImageFormat::Png,
    })
}

/// Returns the scratch space needed to decode this PNG.
pub fn scratch_len(input: &[u8]) -> Result<usize> {
    let (header, pos) = parse_header(input)?;
    let raw = filtered_size(header)?;
    let idat = find_idat(input, pos)?;
    if idat.contiguous {
        Ok(raw)
    } else {
        raw.checked_add(idat.total).ok_or(ImageError::Overflow)
    }
}

/// Decodes a PNG into an RGBA8888 buffer.
pub fn decode(input: &[u8], out: &mut [u8], scratch: &mut [u8]) -> Result<ImageInfo> {
    let (header, pos) = parse_header(input)?;
    let info = ImageInfo {
        width: header.width,
        height: header.height,
        format: ImageFormat::Png,
    };
    let out_len = info.rgba_len()?;
    if out.len() < out_len {
        return Err(ImageError::BufferTooSmall {
            required: out_len,
            available: out.len(),
        });
    }

    // Palette and transparency live in ancillary chunks before the image data.
    let mut palette = [0u8; 768];
    let mut palette_alpha = [255u8; 256];
    let mut has_palette = false;
    let mut trns_gray: Option<u16> = None;
    let mut trns_rgb: Option<(u16, u16, u16)> = None;
    let mut scan = pos;
    loop {
        let (kind, data, data_offset) = next_chunk(input, scan)?;
        match &kind {
            b"PLTE" => {
                if data.len() % 3 != 0 || data.len() > 768 {
                    return Err(ImageError::InvalidData("bad PLTE chunk"));
                }
                palette[..data.len()].copy_from_slice(data);
                has_palette = true;
            }
            b"tRNS" => match header.color_type {
                3 => {
                    if data.len() > 256 {
                        return Err(ImageError::InvalidData("bad tRNS chunk"));
                    }
                    for (alpha, &value) in palette_alpha.iter_mut().zip(data) {
                        *alpha = value;
                    }
                }
                0 => {
                    if data.len() != 2 {
                        return Err(ImageError::InvalidData("bad tRNS chunk"));
                    }
                    trns_gray = Some(u16::from_be_bytes([data[0], data[1]]));
                }
                2 => {
                    if data.len() != 6 {
                        return Err(ImageError::InvalidData("bad tRNS chunk"));
                    }
                    trns_rgb = Some((
                        u16::from_be_bytes([data[0], data[1]]),
                        u16::from_be_bytes([data[2], data[3]]),
                        u16::from_be_bytes([data[4], data[5]]),
                    ));
                }
                _ => {}
            },
            b"IDAT" | b"IEND" => break,
            _ => {}
        }
        scan = data_offset + data.len() + 4;
    }
    if header.color_type == 3 && !has_palette {
        return Err(ImageError::InvalidData("palette image without PLTE"));
    }

    let raw_len = filtered_size(header)?;
    let idat = find_idat(input, pos)?;
    if idat.contiguous {
        if scratch.len() < raw_len {
            return Err(ImageError::BufferTooSmall {
                required: raw_len,
                available: scratch.len(),
            });
        }
        inflate_exact(&input[idat.first_data..idat.end], &mut scratch[..raw_len])?;
    } else {
        let required = raw_len
            .checked_add(idat.total)
            .ok_or(ImageError::Overflow)?;
        if scratch.len() < required {
            return Err(ImageError::BufferTooSmall {
                required,
                available: scratch.len(),
            });
        }
        let (raw, compressed) = scratch.split_at_mut(raw_len);
        let written = copy_idat(input, pos, compressed)?;
        inflate_exact(&compressed[..written], raw)?;
    }

    unfilter(header, &mut scratch[..raw_len])?;
    expand(
        header,
        &scratch[..raw_len],
        out,
        &palette,
        &palette_alpha,
        trns_gray,
        trns_rgb,
    );
    Ok(info)
}

/// Inflates the image data, which must fill `raw` exactly.
fn inflate_exact(compressed: &[u8], raw: &mut [u8]) -> Result<()> {
    match feos_compress::zlib::decompress(compressed, raw) {
        Ok(written) if written == raw.len() => Ok(()),
        Ok(_) => Err(ImageError::InvalidData("PNG image data is too short")),
        // `raw` is sized from IHDR, so this is the file's fault, not the caller's.
        Err(feos_compress::CompressError::BufferTooSmall { .. }) => {
            Err(ImageError::InvalidData("PNG image data is too long"))
        }
        Err(error) => Err(error.into()),
    }
}

fn unfilter(header: Header, raw: &mut [u8]) -> Result<()> {
    let bpp = header.filter_bpp();
    let mut offset = 0usize;

    if header.interlace {
        for pass in 0..7 {
            let (width, height) = pass_size(header, pass);
            if width == 0 || height == 0 {
                continue;
            }
            unfilter_pass(
                raw,
                &mut offset,
                width,
                height,
                header.row_bytes(width),
                bpp,
            )?;
        }
    } else {
        unfilter_pass(
            raw,
            &mut offset,
            header.width as usize,
            header.height as usize,
            header.row_bytes(header.width as usize),
            bpp,
        )?;
    }

    if offset != raw.len() {
        return Err(ImageError::InvalidData("trailing image data"));
    }
    Ok(())
}

fn unfilter_pass(
    raw: &mut [u8],
    offset: &mut usize,
    _width: usize,
    height: usize,
    row_bytes: usize,
    bpp: usize,
) -> Result<()> {
    let row_len = 1 + row_bytes;
    let mut previous: Option<usize> = None;
    for _ in 0..height {
        let start = *offset;
        let end = start.checked_add(row_len).ok_or(ImageError::Overflow)?;
        if end > raw.len() {
            return Err(ImageError::UnexpectedEnd);
        }
        let filter = raw[start];
        for i in 0..row_bytes {
            let value = raw[start + 1 + i];
            let left = if i >= bpp {
                raw[start + 1 + i - bpp]
            } else {
                0
            };
            let up = previous.map_or(0, |p| raw[p + 1 + i]);
            let up_left = if i >= bpp {
                previous.map_or(0, |p| raw[p + 1 + i - bpp])
            } else {
                0
            };
            let reconstructed = match filter {
                0 => value,
                1 => value.wrapping_add(left),
                2 => value.wrapping_add(up),
                3 => value.wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => value.wrapping_add(paeth(left, up, up_left)),
                _ => return Err(ImageError::InvalidData("unknown PNG filter")),
            };
            raw[start + 1 + i] = reconstructed;
        }
        previous = Some(start);
        *offset = end;
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i16;
    let b = b as i16;
    let c = c as i16;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

fn pass_geometry(header: Header, pass: usize) -> (usize, usize, usize, usize, usize, usize) {
    if !header.interlace {
        return (0, 0, 1, 1, header.width as usize, header.height as usize);
    }
    let (width, height) = pass_size(header, pass);
    (
        ADAM7_X_START[pass],
        ADAM7_Y_START[pass],
        ADAM7_X_STEP[pass],
        ADAM7_Y_STEP[pass],
        width,
        height,
    )
}

fn expand(
    header: Header,
    raw: &[u8],
    out: &mut [u8],
    palette: &[u8; 768],
    palette_alpha: &[u8; 256],
    trns_gray: Option<u16>,
    trns_rgb: Option<(u16, u16, u16)>,
) {
    let width = header.width as usize;
    let mut offset = 0usize;

    let mut emit_pass = |pass: usize, offset: &mut usize| {
        let (x_start, y_start, x_step, y_step, pass_width, pass_height) =
            pass_geometry(header, pass);
        if pass_width == 0 || pass_height == 0 {
            return;
        }
        let row_bytes = header.row_bytes(pass_width);
        for row in 0..pass_height {
            let data = &raw[*offset + 1..*offset + 1 + row_bytes];
            let y = y_start + row * y_step;
            for x in 0..pass_width {
                let px = x_start + x * x_step;
                let (r, g, b, a) =
                    pixel(header, data, x, palette, palette_alpha, trns_gray, trns_rgb);
                let index = (y * width + px) * 4;
                out[index] = r;
                out[index + 1] = g;
                out[index + 2] = b;
                out[index + 3] = a;
            }
            *offset += 1 + row_bytes;
        }
    };

    if header.interlace {
        for pass in 0..7 {
            emit_pass(pass, &mut offset);
        }
    } else {
        emit_pass(0, &mut offset);
    }
}

fn sample(data: &[u8], bit_depth: u8, index: usize) -> u16 {
    match bit_depth {
        8 => data[index] as u16,
        16 => u16::from_be_bytes([data[index * 2], data[index * 2 + 1]]),
        depth => {
            let bit = index * depth as usize;
            let byte = data[bit / 8];
            let shift = 8 - depth as usize - (bit % 8);
            ((byte >> shift) & ((1 << depth) - 1)) as u16
        }
    }
}

fn scale(value: u16, bit_depth: u8) -> u8 {
    match bit_depth {
        8 => value as u8,
        16 => (value >> 8) as u8,
        depth => {
            let max = (1u16 << depth) - 1;
            ((value as u32 * 255 + max as u32 / 2) / max as u32) as u8
        }
    }
}

fn pixel(
    header: Header,
    data: &[u8],
    x: usize,
    palette: &[u8; 768],
    palette_alpha: &[u8; 256],
    trns_gray: Option<u16>,
    trns_rgb: Option<(u16, u16, u16)>,
) -> (u8, u8, u8, u8) {
    let depth = header.bit_depth;
    match header.color_type {
        0 => {
            let value = sample(data, depth, x);
            let gray = scale(value, depth);
            let alpha = if trns_gray == Some(value) { 0 } else { 255 };
            (gray, gray, gray, alpha)
        }
        2 => {
            let r = sample(data, depth, x * 3);
            let g = sample(data, depth, x * 3 + 1);
            let b = sample(data, depth, x * 3 + 2);
            let alpha = if trns_rgb == Some((r, g, b)) { 0 } else { 255 };
            (scale(r, depth), scale(g, depth), scale(b, depth), alpha)
        }
        3 => {
            let index = sample(data, depth, x) as usize;
            let base = index * 3;
            (
                palette[base],
                palette[base + 1],
                palette[base + 2],
                palette_alpha[index],
            )
        }
        4 => {
            let value = sample(data, depth, x * 2);
            let alpha = sample(data, depth, x * 2 + 1);
            let gray = scale(value, depth);
            (gray, gray, gray, scale(alpha, depth))
        }
        _ => {
            let r = sample(data, depth, x * 4);
            let g = sample(data, depth, x * 4 + 1);
            let b = sample(data, depth, x * 4 + 2);
            let a = sample(data, depth, x * 4 + 3);
            (
                scale(r, depth),
                scale(g, depth),
                scale(b, depth),
                scale(a, depth),
            )
        }
    }
}

const CRC_POLY: u32 = 0xedb8_8320;

const fn make_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                CRC_POLY ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = make_crc_table();

struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    fn update(&mut self, data: &[u8]) {
        let mut value = self.value;
        for &byte in data {
            let index = ((value ^ byte as u32) & 0xff) as usize;
            value = CRC_TABLE[index] ^ (value >> 8);
        }
        self.value = value;
    }

    fn finish(self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

/// Encodes an RGBA8888 image as a PNG.
///
/// Returns the number of bytes written to `out`.
pub fn encode_rgba(width: u32, height: u32, pixels: &[u8], out: &mut [u8]) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(ImageError::InvalidData("image has no pixels"));
    }
    let pixel_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(ImageError::Overflow)?;
    if pixels.len() < pixel_len {
        return Err(ImageError::BufferTooSmall {
            required: pixel_len,
            available: pixels.len(),
        });
    }

    let mut writer = Writer { out, pos: 0 };
    writer.bytes(&SIGNATURE)?;

    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    writer.chunk(b"IHDR", &ihdr)?;

    let stride = width as usize * 4;
    let row_len = 1 + stride;
    let raw_len = row_len
        .checked_mul(height as usize)
        .ok_or(ImageError::Overflow)?;
    let blocks = (raw_len + 65534) / 65535;
    let idat_len = 2 + blocks * 5 + raw_len + 4;

    let start = writer.chunk_begin(b"IDAT", idat_len)?;
    writer.bytes(&[0x78, 0x01])?;
    let mut adler = Adler::new();

    let mut row = 0usize;
    let mut row_pos = 0usize;
    let mut remaining = raw_len;
    while remaining > 0 {
        let block = remaining.min(65535);
        let final_block = block == remaining;
        writer.byte(if final_block { 1 } else { 0 })?;
        let length = block as u16;
        writer.bytes(&length.to_le_bytes())?;
        writer.bytes(&(!length).to_le_bytes())?;

        let mut left = block;
        while left > 0 {
            if row_pos == 0 {
                writer.byte(0)?;
                adler.update(&[0]);
                row_pos = 1;
                left -= 1;
                if left == 0 {
                    break;
                }
            }
            let available = row_len - row_pos;
            let take = available.min(left);
            let pixel_offset = row * stride + (row_pos - 1);
            let piece = &pixels[pixel_offset..pixel_offset + take];
            writer.bytes(piece)?;
            adler.update(piece);
            row_pos += take;
            left -= take;
            if row_pos == row_len {
                row += 1;
                row_pos = 0;
            }
        }
        remaining -= block;
    }

    writer.bytes(&adler.finish().to_be_bytes())?;
    writer.chunk_end(start)?;
    writer.chunk(b"IEND", &[])?;
    Ok(writer.pos)
}

struct Adler {
    a: u32,
    b: u32,
}

impl Adler {
    fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    fn update(&mut self, data: &[u8]) {
        const MOD: u32 = 65521;
        for &byte in data {
            self.a = (self.a + byte as u32) % MOD;
            self.b = (self.b + self.a) % MOD;
        }
    }

    fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn byte(&mut self, value: u8) -> Result<()> {
        if self.pos >= self.out.len() {
            return Err(ImageError::BufferTooSmall {
                required: self.pos + 1,
                available: self.out.len(),
            });
        }
        self.out[self.pos] = value;
        self.pos += 1;
        Ok(())
    }

    fn bytes(&mut self, data: &[u8]) -> Result<()> {
        let end = self
            .pos
            .checked_add(data.len())
            .ok_or(ImageError::Overflow)?;
        if end > self.out.len() {
            return Err(ImageError::BufferTooSmall {
                required: end,
                available: self.out.len(),
            });
        }
        self.out[self.pos..end].copy_from_slice(data);
        self.pos = end;
        Ok(())
    }

    fn chunk(&mut self, kind: &[u8; 4], data: &[u8]) -> Result<()> {
        let start = self.chunk_begin(kind, data.len())?;
        self.bytes(data)?;
        self.chunk_end(start)
    }

    fn chunk_begin(&mut self, kind: &[u8; 4], len: usize) -> Result<usize> {
        self.bytes(&(len as u32).to_be_bytes())?;
        let start = self.pos;
        self.bytes(kind)?;
        Ok(start)
    }

    fn chunk_end(&mut self, start: usize) -> Result<()> {
        let mut crc = Crc32::new();
        crc.update(&self.out[start..self.pos]);
        let value = crc.finish();
        self.bytes(&value.to_be_bytes())
    }
}
