//! CRC-32 (IEEE) and Adler-32 checksums.

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

/// Incremental CRC-32 (IEEE 802.3).
pub struct Crc32 {
    value: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Creates a CRC accumulator.
    pub const fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    /// Feeds more bytes into the checksum.
    pub fn update(&mut self, data: &[u8]) {
        let mut value = self.value;
        for &byte in data {
            let index = ((value ^ byte as u32) & 0xff) as usize;
            value = CRC_TABLE[index] ^ (value >> 8);
        }
        self.value = value;
    }

    /// Returns the final CRC value.
    pub const fn finish(self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

/// Computes the CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finish()
}

/// Computes the Adler-32 checksum of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}
