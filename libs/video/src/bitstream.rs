//! Small byte and bit readers shared by the container and codec parsers.

use crate::error::{need, Result, VideoError};

#[inline]
pub(crate) fn u8_at(data: &[u8], pos: usize) -> Result<u8> {
    data.get(pos).copied().ok_or(VideoError::UnexpectedEnd)
}

#[inline]
pub(crate) fn u16_at(data: &[u8], pos: usize) -> Result<u16> {
    need(data.len(), pos, 2)?;
    Ok(u16::from_be_bytes([data[pos], data[pos + 1]]))
}

#[inline]
pub(crate) fn u16_le_at(data: &[u8], pos: usize) -> Result<u16> {
    need(data.len(), pos, 2)?;
    Ok(u16::from_le_bytes([data[pos], data[pos + 1]]))
}

#[inline]
pub(crate) fn u24_le_at(data: &[u8], pos: usize) -> Result<u32> {
    need(data.len(), pos, 3)?;
    Ok(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        0,
    ]))
}

#[inline]
pub(crate) fn u24_be_at(data: &[u8], pos: usize) -> Result<u32> {
    need(data.len(), pos, 3)?;
    Ok(u32::from_be_bytes([
        0,
        data[pos],
        data[pos + 1],
        data[pos + 2],
    ]))
}

#[inline]
pub(crate) fn u32_at(data: &[u8], pos: usize) -> Result<u32> {
    need(data.len(), pos, 4)?;
    Ok(u32::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

#[inline]
pub(crate) fn u32_le_at(data: &[u8], pos: usize) -> Result<u32> {
    need(data.len(), pos, 4)?;
    Ok(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

#[inline]
pub(crate) fn u64_at(data: &[u8], pos: usize) -> Result<u64> {
    need(data.len(), pos, 8)?;
    Ok(u64::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]))
}

#[inline]
pub(crate) fn u64_le_at(data: &[u8], pos: usize) -> Result<u64> {
    need(data.len(), pos, 8)?;
    Ok(u64::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]))
}

/// Reads a four-character code as bytes.
#[inline]
pub(crate) fn fourcc_at(data: &[u8], pos: usize) -> Result<[u8; 4]> {
    need(data.len(), pos, 4)?;
    Ok([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
}

/// MSB-first bit reader over a byte slice.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u8,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit: 0,
        }
    }

    /// Reads one bit, or `None` at the end of the slice.
    pub(crate) fn read_bit(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(VideoError::UnexpectedEnd);
        }
        let value = (self.data[self.pos] >> (7 - self.bit)) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(value)
    }

    /// Reads up to 32 bits, MSB first.
    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u32> {
        if count > 32 {
            return Err(VideoError::InvalidData("bit count"));
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.read_bit()? as u32;
        }
        Ok(value)
    }

    /// Reads an unsigned Exp-Golomb code (H.264/H.265 style).
    pub(crate) fn read_ue(&mut self) -> Result<u32> {
        let mut leading = 0u32;
        while self.read_bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(VideoError::InvalidData("exp-golomb code"));
            }
        }
        let suffix = if leading == 0 {
            0
        } else {
            self.read_bits(leading)?
        };
        Ok(((1u32 << leading) - 1) + suffix)
    }

    /// Reads a signed Exp-Golomb code.
    pub(crate) fn read_se(&mut self) -> Result<i32> {
        let value = self.read_ue()?;
        if value & 1 == 1 {
            Ok(((value + 1) / 2) as i32)
        } else {
            Ok(-((value / 2) as i32))
        }
    }

    /// Discards bits until the next byte boundary.
    pub(crate) fn byte_align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}
