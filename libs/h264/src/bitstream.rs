/// Bitstream reader for parsing H.264 NAL unit RBSP data.
/// Reads bits left-to-right (MSB first).
///
/// The reader borrows the RBSP, so creating one costs nothing. Reads past the
/// end return zero bits for up to `PADDING` bytes, which keeps multi-slice
/// end-of-slice detection simple, and fail after that. Only `bits_remaining`
/// and `more_rbsp_data` use the true data length.
pub struct BitstreamReader<'a> {
    data: &'a [u8],
    /// One past the last non-zero byte of `data`, computed once so
    /// `more_rbsp_data` stays O(1) per macroblock however long the trailing
    /// zeros are.
    last_nz: usize,
    byte_offset: usize,
    bit_offset: u8, // 0-7, bits already consumed in current byte
}

/// Number of virtual zero bytes readable after the RBSP data.
/// Must be large enough that reading past the end during multi-slice
/// end-of-slice detection is not reported as an error.
/// 128 bytes = 1024 bits covers the worst case for any single MB decode.
const PADDING: usize = 128;

impl<'a> BitstreamReader<'a> {
    pub fn new(rbsp: &'a [u8]) -> Self {
        let mut last_nz = rbsp.len();
        while last_nz > 0 && rbsp[last_nz - 1] == 0 {
            last_nz -= 1;
        }
        Self {
            data: rbsp,
            last_nz,
            byte_offset: 0,
            bit_offset: 0,
        }
    }

    /// Read a single bit, returning 0 or 1.
    /// Reads inside the padding return zero bits; `bits_remaining` tells a
    /// caller whether real data is left before a bulk read.
    #[inline(always)]
    pub fn read_bit(&mut self) -> Result<u8, &'static str> {
        let bit = match self.data.get(self.byte_offset) {
            Some(byte) => (byte >> (7 - self.bit_offset)) & 1,
            None if self.byte_offset < self.data.len() + PADDING => 0,
            None => return Err("end of bitstream"),
        };
        self.bit_offset += 1;
        if self.bit_offset == 8 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
        Ok(bit)
    }

    /// Read `n` bits (up to 32) as a u32.
    pub fn read_bits(&mut self, n: u8) -> Result<u32, &'static str> {
        if n > 32 {
            return Err("cannot read more than 32 bits");
        }
        let total = self.bit_offset as usize + n as usize;
        if self.byte_offset + total.div_ceil(8) > self.data.len() + PADDING {
            return Err("end of bitstream");
        }
        let val = self.peek_bits(n);
        self.byte_offset += total / 8;
        self.bit_offset = (total % 8) as u8;
        Ok(val)
    }

    /// Peek at the next `n` bits (up to 32) without consuming them. Zero-pads
    /// if fewer bits remain.
    pub fn peek_bits(&self, n: u8) -> u32 {
        if n == 0 {
            return 0;
        }
        // Five bytes hold any 32-bit field at any bit offset.
        let mut window = 0u64;
        for i in 0..5 {
            let byte = self.data.get(self.byte_offset + i).copied().unwrap_or(0);
            window = (window << 8) | byte as u64;
        }
        let shift = 40 - self.bit_offset as u32 - n as u32;
        ((window >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// Advance position by `n` bits without reading them.
    pub fn skip_bits(&mut self, n: u8) {
        let total = self.bit_offset as usize + n as usize;
        self.byte_offset += total / 8;
        self.bit_offset = (total % 8) as u8;
    }

    /// Read an unsigned Exp-Golomb coded value (ue(v)).
    pub fn read_ue(&mut self) -> Result<u32, &'static str> {
        let mut leading_zeros: u32 = 0;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err("exp-golomb overflow");
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeros as u8)?;
        Ok(((1u32 << leading_zeros) - 1).wrapping_add(suffix))
    }

    /// Read a truncated Exp-Golomb coded value (te(v)).
    /// When max == 1, reads a single bit: bit=0 → value=1, bit=1 → value=0.
    /// When max > 1, reads a standard ue(v).
    pub fn read_te(&mut self, max: u32) -> Result<u32, &'static str> {
        if max > 1 {
            self.read_ue()
        } else {
            // Spec 9.1: value = 1 - bit
            Ok(1 - self.read_bit()? as u32)
        }
    }

    /// Read a signed Exp-Golomb coded value (se(v)).
    pub fn read_se(&mut self) -> Result<i32, &'static str> {
        let code = self.read_ue()?;
        let val = code.div_ceil(2) as i32;
        if code % 2 == 0 { Ok(-val) } else { Ok(val) }
    }

    /// Returns true if there is more RBSP data before the trailing bits.
    /// The RBSP trailing bits are: a stop bit (1) followed by zero bits to byte-align.
    pub fn more_rbsp_data(&self) -> bool {
        if self.byte_offset >= self.data.len() {
            return false;
        }

        let last_nz = self.last_nz;
        if last_nz <= self.byte_offset {
            return false;
        }

        // If we're before the last non-zero byte, there's definitely more data
        if self.byte_offset < last_nz - 1 {
            return true;
        }

        // We're in the last non-zero byte. The RBSP stop bit is the lowest set bit.
        // Everything above it (towards MSB) is data. Check if there are unconsumed
        // data bits above the stop bit.
        let byte = self.data[self.byte_offset];
        let stop_bit = byte.trailing_zeros();
        let data_bits_in_byte = 7 - stop_bit as u8;
        self.bit_offset < data_bits_in_byte
    }

    /// Advance to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        if self.bit_offset != 0 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
    }

    pub fn position(&self) -> (usize, u8) {
        (self.byte_offset, self.bit_offset)
    }

    /// Peek at the byte at the current byte_offset (for debugging).
    pub fn peek_byte(&self) -> u8 {
        self.data.get(self.byte_offset).copied().unwrap_or(0)
    }

    /// Peek at a slice of bytes starting at byte_offset (for debugging).
    pub fn peek_bytes(&self, n: usize) -> &[u8] {
        let start = self.byte_offset.min(self.data.len());
        let end = start.saturating_add(n).min(self.data.len());
        &self.data[start..end]
    }

    /// Get the byte-aligned position for CABAC initialization.
    /// Aligns to the next byte boundary and returns (byte_offset, reference to data).
    pub fn cabac_start(&mut self) -> (usize, &[u8]) {
        // CABAC starts after cabac_alignment_one_bit + byte alignment
        // The slice header is followed by a 1-bit then byte-aligned CABAC data
        if self.bit_offset != 0 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
        (self.byte_offset, self.data)
    }

    pub fn bits_remaining(&self) -> usize {
        if self.byte_offset >= self.data.len() {
            return 0;
        }
        (self.data.len() - self.byte_offset) * 8 - self.bit_offset as usize
    }
}

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;

    #[test]
    fn test_read_bits() {
        let data = [0b1010_0011, 0b1100_0000];
        let mut r = BitstreamReader::new(&data);
        assert_eq!(r.read_bits(4).unwrap(), 0b1010);
        assert_eq!(r.read_bits(4).unwrap(), 0b0011);
        assert_eq!(r.read_bits(2).unwrap(), 0b11);
    }

    #[test]
    fn test_read_ue() {
        // ue(0) = 1 -> code 0
        // ue(1) = 010 -> code 1
        // ue(2) = 011 -> code 2
        // ue(3) = 00100 -> code 3
        // Bit pattern: 1 010 011 00100 = 1010 0110 0100 = 0xA64
        let data = [0xA6, 0x40];
        let mut r = BitstreamReader::new(&data);
        assert_eq!(r.read_ue().unwrap(), 0);
        assert_eq!(r.read_ue().unwrap(), 1);
        assert_eq!(r.read_ue().unwrap(), 2);
        assert_eq!(r.read_ue().unwrap(), 3);
    }

    #[test]
    fn test_read_se() {
        // se mapping: code 0->0, 1->1, 2->-1, 3->2, 4->-2
        // ue values:   0       1       2       3       4
        // bits:        1      010     011    00100   00101
        // Combined: 1 010 011 00100 00101 = 1010 0110 0100 0010 1
        let data = [0xA6, 0x42, 0x80];
        let mut r = BitstreamReader::new(&data);
        assert_eq!(r.read_se().unwrap(), 0);
        assert_eq!(r.read_se().unwrap(), 1);
        assert_eq!(r.read_se().unwrap(), -1);
        assert_eq!(r.read_se().unwrap(), 2);
        assert_eq!(r.read_se().unwrap(), -2);
    }

    #[test]
    fn test_padding_allows_overread() {
        // After reading all real data, reads return 0 (from padding) rather than panicking
        let data = [0xFF];
        let mut r = BitstreamReader::new(&data);
        assert_eq!(r.bits_remaining(), 8);
        assert_eq!(r.read_bits(8).unwrap(), 0xFF);
        assert_eq!(r.bits_remaining(), 0);
        // Reading into padding returns 0 bits without panic
        assert_eq!(r.read_bits(8).unwrap(), 0);
    }

    /// Fuzz regression: reading far past the buffer must return Err, not panic.
    #[test]
    fn test_read_past_buffer_returns_err() {
        let data = [0xAB; 4]; // 4 bytes = 32 bits
        let mut r = BitstreamReader::new(&data);
        // Read the actual data + padding (128 bytes = 1024 bits)
        for _ in 0..(4 + 128) * 8 {
            let _ = r.read_bit(); // may be Ok or Err
        }
        // Now we're past the padded buffer — must return Err, not panic
        assert!(r.read_bit().is_err());
    }
}
