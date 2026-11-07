//! The arithmetic decoder everything in VP8 is read through.

/// Reads booleans of given probabilities. Past the end of its data it reads zeros.
pub(super) struct BoolDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    value: u64,
    /// Bits in `value` beyond the top eight that are valid.
    bits: i32,
    range: u32,
}

impl<'a> BoolDecoder<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        let mut decoder = Self {
            data,
            pos: 0,
            value: 0,
            bits: -8,
            range: 255,
        };
        decoder.fill();
        decoder
    }

    fn fill(&mut self) {
        while self.bits < 0 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos = self.pos.saturating_add(1);
            self.value = (self.value << 8) | byte as u64;
            self.bits += 8;
        }
    }

    /// Reads a boolean that's false with probability `prob` / 256.
    #[inline]
    pub(super) fn read(&mut self, prob: u8) -> bool {
        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        let big_split = (split as u64) << self.bits;
        let bit = if self.value >= big_split {
            self.range -= split;
            self.value -= big_split;
            true
        } else {
            self.range = split;
            false
        };
        // Back up to at least 128, a bit at a time as far as the arithmetic cares.
        let shift = self.range.leading_zeros() as i32 - 24;
        self.range <<= shift;
        self.bits -= shift;
        if self.bits < 0 {
            self.fill();
        }
        bit
    }

    /// Reads a boolean of even chances.
    #[inline]
    pub(super) fn flag(&mut self) -> bool {
        self.read(128)
    }

    /// Reads an unsigned number, most significant bit first.
    pub(super) fn bits(&mut self, count: u32) -> u32 {
        (0..count).fold(0, |value, _| (value << 1) | self.flag() as u32)
    }

    /// Reads a magnitude and a sign, if a flag says there's one.
    pub(super) fn maybe_signed(&mut self, count: u32) -> Option<i32> {
        if !self.flag() {
            return None;
        }
        let value = self.bits(count) as i32;
        Some(if self.flag() { -value } else { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_zeros_past_the_end() {
        let mut decoder = BoolDecoder::new(&[]);
        for _ in 0..1000 {
            assert!(!decoder.read(128));
        }
    }

    #[test]
    fn a_long_read_never_overflows() {
        let data = [0xff; 8];
        let mut decoder = BoolDecoder::new(&data);
        for prob in (1..=255u8).cycle().take(100_000) {
            let _ = decoder.read(prob);
        }
    }
}
