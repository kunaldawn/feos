//! Bit readers and small fallible-allocation helpers shared by the decoders.
//!
//! FLAC and AAC read most-significant-bit first ([`MsbReader`]). Vorbis reads
//! least-significant-bit first ([`LsbReader`]). Neither can read past its
//! slice: a read that would run off the end reports it and consumes nothing.

use alloc::vec::Vec;

use crate::error::{AudioError, Result};

/// Allocates `len` copies of `value`, reporting failure instead of aborting.
pub(crate) fn try_vec<T: Clone>(len: usize, value: T) -> Result<Vec<T>> {
    let mut vec = Vec::new();
    vec.try_reserve_exact(len)
        .map_err(|_| AudioError::OutOfMemory)?;
    vec.resize(len, value);
    Ok(vec)
}

/// Appends to `vec`, reporting allocation failure instead of aborting.
pub(crate) fn try_push<T>(vec: &mut Vec<T>, value: T) -> Result<()> {
    if vec.len() == vec.capacity() {
        let extra = vec.capacity().clamp(4, 1 << 20);
        vec.try_reserve_exact(extra)
            .map_err(|_| AudioError::OutOfMemory)?;
    }
    vec.push(value);
    Ok(())
}

/// Reads bits most-significant first.
pub(crate) struct MsbReader<'a> {
    data: &'a [u8],
    /// Next byte to move into the cache.
    pos: usize,
    /// Unread bits, left-aligned.
    cache: u64,
    /// Number of valid bits in `cache`.
    count: u32,
}

impl<'a> MsbReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            cache: 0,
            count: 0,
        }
    }

    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&byte) = self.data.get(self.pos) else {
                break;
            };
            self.cache |= (byte as u64) << (56 - self.count);
            self.count += 8;
            self.pos += 1;
        }
    }

    /// Bits left in the input.
    pub(crate) fn bits_left(&self) -> usize {
        (self.data.len() - self.pos) * 8 + self.count as usize
    }

    /// Reads `n` bits, `n <= 32`.
    pub(crate) fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(AudioError::UnexpectedEnd);
            }
        }
        let value = (self.cache >> (64 - n)) as u32;
        self.cache <<= n;
        self.count -= n;
        Ok(value)
    }

    /// Reads one bit.
    pub(crate) fn bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }

    /// Reads an `n`-bit two's complement value, `n <= 33`.
    pub(crate) fn read_signed(&mut self, n: u32) -> Result<i64> {
        if n == 0 {
            return Ok(0);
        }
        let value = if n > 32 {
            ((self.read(n - 32)? as u64) << 32) | self.read(32)? as u64
        } else {
            self.read(n)? as u64
        };
        let shift = 64 - n;
        Ok(((value << shift) as i64) >> shift)
    }

    /// Returns up to `n <= 32` bits without consuming them, padding with
    /// zeros at the end of the input.
    pub(crate) fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.refill();
        }
        if n == 0 {
            0
        } else {
            (self.cache >> (64 - n)) as u32
        }
    }

    /// Consumes `n` bits previously seen through [`Self::peek`].
    pub(crate) fn skip(&mut self, n: u32) -> Result<()> {
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(AudioError::UnexpectedEnd);
            }
        }
        self.cache = if n >= 64 { 0 } else { self.cache << n };
        self.count -= n;
        Ok(())
    }

    /// Skips any number of bits.
    pub(crate) fn skip_long(&mut self, mut n: usize) -> Result<()> {
        if n > self.bits_left() {
            return Err(AudioError::UnexpectedEnd);
        }
        while n > 0 {
            let step = n.min(32) as u32;
            self.skip(step)?;
            n -= step as usize;
        }
        Ok(())
    }

    /// Counts and consumes zero bits up to and including the next one bit.
    pub(crate) fn unary(&mut self) -> Result<u64> {
        let mut zeros = 0u64;
        loop {
            if self.count == 0 {
                self.refill();
                if self.count == 0 {
                    return Err(AudioError::UnexpectedEnd);
                }
            }
            let run = self.cache.leading_zeros().min(self.count);
            if run == self.count {
                zeros += run as u64;
                self.cache = 0;
                self.count = 0;
                continue;
            }
            zeros += run as u64;
            // Consume the run and the terminating one bit; run <= 63.
            self.cache = (self.cache << run) << 1;
            self.count -= run + 1;
            return Ok(zeros);
        }
    }

    /// Whole bytes consumed so far, rounding a partial byte up.
    pub(crate) fn byte_pos(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }

    /// Discards bits up to the next byte boundary.
    pub(crate) fn align(&mut self) {
        let extra = self.count % 8;
        self.cache = if extra == 0 { self.cache } else { self.cache << extra };
        self.count -= extra;
    }
}

/// Reads bits least-significant first, as Vorbis packs them.
pub(crate) struct LsbReader<'a> {
    data: &'a [u8],
    pos: usize,
    cache: u64,
    count: u32,
    /// Set once a read ran past the end of the packet.
    ended: bool,
}

impl<'a> LsbReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            cache: 0,
            count: 0,
            ended: false,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&byte) = self.data.get(self.pos) else {
                break;
            };
            self.cache |= (byte as u64) << self.count;
            self.count += 8;
            self.pos += 1;
        }
    }

    /// Reads `n <= 32` bits; `None` at the end of the packet, which stays
    /// ended from then on.
    #[inline]
    pub(crate) fn read(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if self.ended {
            return None;
        }
        if n == 0 {
            return Some(0);
        }
        if self.count < n {
            self.refill();
            if self.count < n {
                self.ended = true;
                return None;
            }
        }
        let value = self.cache & ((1u64 << n) - 1);
        self.cache >>= n;
        self.count -= n;
        Some(value as u32)
    }

    /// The next `n <= 32` bits without consuming them, zero padded at the
    /// end of the packet, and how many of them are really there.
    #[inline]
    pub(crate) fn peek(&mut self, n: u32) -> (u32, u32) {
        if self.count < n {
            self.refill();
        }
        let value = if n == 0 { 0 } else { self.cache & ((1u64 << n) - 1) };
        (value as u32, self.count.min(n))
    }

    /// Consumes `n` bits previously seen through [`Self::peek`].
    #[inline]
    pub(crate) fn consume(&mut self, n: u32) {
        debug_assert!(n <= self.count);
        let n = n.min(self.count);
        self.cache = if n >= 64 { 0 } else { self.cache >> n };
        self.count -= n;
    }

    /// Marks the packet as ended.
    #[inline]
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_reads_across_the_cache() {
        let data = [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd];
        let mut reader = MsbReader::new(&data);
        assert_eq!(reader.read(4).unwrap(), 0xa);
        assert_eq!(reader.read(32).unwrap(), 0xbcdef012);
        assert_eq!(reader.read_signed(4).unwrap(), 3);
        assert_eq!(reader.read(32).unwrap(), 0x456789ab);
        assert_eq!(reader.read(8).unwrap(), 0xcd);
        assert_eq!(reader.read(1), Err(AudioError::UnexpectedEnd));
    }

    #[test]
    fn msb_unary_and_signed() {
        let data = [0b0001_0000, 0, 0, 0, 0, 0, 0, 0, 0, 0b0000_0001];
        let mut reader = MsbReader::new(&data);
        assert_eq!(reader.unary().unwrap(), 3);
        assert_eq!(reader.unary().unwrap(), 75);
        assert_eq!(reader.unary(), Err(AudioError::UnexpectedEnd));

        let mut reader = MsbReader::new(&[0xff, 0xff, 0xff, 0xff, 0x80]);
        assert_eq!(reader.read_signed(33).unwrap(), -1);
        let mut reader = MsbReader::new(&[0x7f, 0xff, 0xff, 0xff, 0x80]);
        assert_eq!(reader.read_signed(33).unwrap(), 0xffff_ffff);
    }

    #[test]
    fn msb_position_and_alignment() {
        let mut reader = MsbReader::new(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(reader.byte_pos(), 0);
        reader.read(3).unwrap();
        reader.align();
        assert_eq!(reader.byte_pos(), 1);
        assert_eq!(reader.read(8).unwrap(), 2);
        reader.skip_long(7 * 8).unwrap();
        assert_eq!(reader.read(8).unwrap(), 10);
        assert_eq!(reader.bits_left(), 0);
    }

    #[test]
    fn lsb_reads_and_ends() {
        let mut reader = LsbReader::new(&[0b1010_0101, 0xff]);
        assert_eq!(reader.read(4), Some(0b0101));
        assert_eq!(reader.read(4), Some(0b1010));
        assert_eq!(reader.peek(16), (0xff, 8));
        reader.consume(4);
        assert_eq!(reader.read(8), None);
        assert_eq!(reader.read(1), None);
    }
}
