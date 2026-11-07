//! The CABAC arithmetic decoder (spec 9.3.3.2) and the binarizations of its syntax
//! elements that don't depend on neighbours.

/// Table 9-44: rangeTabLPS, by pStateIdx and qCodIRangeIdx.
#[rustfmt::skip]
static RANGE_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [ 95, 116, 137, 158],
    [ 90, 110, 130, 150],
    [ 85, 104, 123, 142],
    [ 81,  99, 117, 135],
    [ 77,  94, 111, 128],
    [ 73,  89, 105, 122],
    [ 69,  85, 100, 116],
    [ 66,  80,  95, 110],
    [ 62,  76,  90, 104],
    [ 59,  72,  86,  99],
    [ 56,  69,  81,  94],
    [ 53,  65,  77,  89],
    [ 51,  62,  73,  85],
    [ 48,  59,  69,  80],
    [ 46,  56,  66,  76],
    [ 43,  53,  63,  72],
    [ 41,  50,  59,  69],
    [ 39,  48,  56,  65],
    [ 37,  45,  54,  62],
    [ 35,  43,  51,  59],
    [ 33,  41,  48,  56],
    [ 32,  39,  46,  53],
    [ 30,  37,  43,  50],
    [ 29,  35,  41,  48],
    [ 27,  33,  39,  45],
    [ 26,  31,  37,  43],
    [ 24,  30,  35,  41],
    [ 23,  28,  33,  39],
    [ 22,  27,  32,  37],
    [ 21,  26,  30,  35],
    [ 20,  24,  29,  33],
    [ 19,  23,  27,  31],
    [ 18,  22,  26,  30],
    [ 17,  21,  25,  28],
    [ 16,  20,  23,  27],
    [ 15,  19,  22,  25],
    [ 14,  18,  21,  24],
    [ 14,  17,  20,  23],
    [ 13,  16,  19,  22],
    [ 12,  15,  18,  21],
    [ 12,  14,  17,  20],
    [ 11,  14,  16,  19],
    [ 11,  13,  15,  18],
    [ 10,  12,  15,  17],
    [ 10,  12,  14,  16],
    [  9,  11,  13,  15],
    [  9,  11,  12,  14],
    [  8,  10,  12,  14],
    [  8,   9,  11,  13],
    [  7,   9,  11,  12],
    [  7,   9,  10,  12],
    [  7,   8,  10,  11],
    [  6,   8,   9,  11],
    [  6,   7,   9,  10],
    [  6,   7,   8,   9],
    [  2,   2,   2,   2],
];

/// Table 9-45: transIdxLPS.
#[rustfmt::skip]
static NEXT_LPS: [u8; 64] = [
     0,  0,  1,  2,  2,  4,  4,  5,  6,  7,  8,  9,  9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// Contexts used by this decoder: everything but those of 4:4:4.
pub(crate) const CONTEXTS: usize = 460;

/// Arithmetic decoder over an RBSP. `value` holds codIOffset above `bits`
/// not-yet-consumed bits, so that refills are rare. Past the end of the data it
/// reads zeros, and whoever loops on it has a bound of its own.
pub(crate) struct Cabac<'a> {
    data: &'a [u8],
    pos: usize,
    value: u64,
    bits: i32,
    range: u32,
}

impl<'a> Cabac<'a> {
    /// Starts at byte `pos` (spec 9.3.1.2).
    pub fn new(data: &'a [u8], pos: usize) -> Self {
        let mut cabac = Self {
            data,
            pos,
            value: 0,
            bits: -9,
            range: 510,
        };
        cabac.refill();
        cabac
    }

    #[inline(always)]
    fn refill(&mut self) {
        while self.bits < 16 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.value = (self.value << 8) | byte as u64;
            self.bits += 8;
        }
    }

    /// True once more than a little has been read past the end of the data.
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() + 8
    }

    /// Decodes a bin with the context state `ctx`: `(pStateIdx << 1) | valMPS`.
    #[inline(always)]
    pub fn decision(&mut self, ctx: &mut u8) -> bool {
        let state = (*ctx >> 1) as usize;
        let mps = *ctx & 1;
        let lps = RANGE_LPS[state][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        let scaled = (self.range as u64) << self.bits;
        if self.value < scaled {
            *ctx = if state < 62 { *ctx + 2 } else { *ctx };
            if self.range < 256 {
                self.range <<= 1;
                self.bits -= 1;
                if self.bits < 16 {
                    self.refill();
                }
            }
            mps != 0
        } else {
            self.value -= scaled;
            self.range = lps;
            let shift = lps.leading_zeros() as i32 - 23;
            self.range <<= shift;
            self.bits -= shift;
            let new_mps = if state == 0 { mps ^ 1 } else { mps };
            *ctx = (NEXT_LPS[state] << 1) | new_mps;
            if self.bits < 16 {
                self.refill();
            }
            mps == 0
        }
    }

    #[inline(always)]
    pub fn bypass(&mut self) -> bool {
        self.bits -= 1;
        let scaled = (self.range as u64) << self.bits;
        let bin = if self.value >= scaled {
            self.value -= scaled;
            true
        } else {
            false
        };
        if self.bits < 16 {
            self.refill();
        }
        bin
    }

    pub fn terminate(&mut self) -> bool {
        self.range -= 2;
        let scaled = (self.range as u64) << self.bits;
        if self.value >= scaled {
            true
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.bits -= 1;
                if self.bits < 16 {
                    self.refill();
                }
            }
            false
        }
    }

    /// Where I_PCM samples start after a terminating bin: the next byte after
    /// the bits consumed so far.
    pub fn pcm_start(&self) -> usize {
        // Everything read, less what's still unconsumed in `value`.
        (self.pos * 8 - self.bits as usize).div_ceil(8)
    }

    /// Restarts after I_PCM samples.
    pub fn restart(&mut self, pos: usize) {
        *self = Self::new(self.data, pos);
    }
}

/// Initial context states for a slice (9.3.1.1).
pub(crate) fn init_contexts(states: &mut [u8; CONTEXTS], slice_qp: i32, table: Option<u32>) {
    use crate::cabac_tables::{CABAC_CONTEXT_INIT_I, CABAC_CONTEXT_INIT_PB};
    let qp = slice_qp.clamp(0, 51);
    let init = match table {
        None => &CABAC_CONTEXT_INIT_I,
        Some(idc) => &CABAC_CONTEXT_INIT_PB[(idc as usize).min(2)],
    };
    for (state, pair) in states.iter_mut().zip(init.iter()) {
        let pre = (((pair[0] as i32 * qp) >> 4) + pair[1] as i32).clamp(1, 126);
        *state = if pre <= 63 {
            ((63 - pre) << 1) as u8
        } else {
            (((pre - 64) << 1) | 1) as u8
        };
    }
}
