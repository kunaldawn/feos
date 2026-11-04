//! CRC-32 (IEEE) and Adler-32 checksums.

const CRC_POLY: u32 = 0xedb8_8320;

/// Slicing-by-8 tables: `[0]` is the classic byte table, and `[k]` gives the
/// CRC of a byte followed by `k` zero bytes.
const fn make_crc_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
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
        tables[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut slice = 1;
        while slice < 8 {
            let previous = tables[slice - 1][n];
            tables[slice][n] = tables[0][(previous & 0xff) as usize] ^ (previous >> 8);
            slice += 1;
        }
        n += 1;
    }
    tables
}

static CRC_TABLES: [[u32; 256]; 8] = make_crc_tables();

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
        let mut chunks = data.chunks_exact(8);
        for chunk in &mut chunks {
            let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ value;
            let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            value = CRC_TABLES[7][(low & 0xff) as usize]
                ^ CRC_TABLES[6][((low >> 8) & 0xff) as usize]
                ^ CRC_TABLES[5][((low >> 16) & 0xff) as usize]
                ^ CRC_TABLES[4][(low >> 24) as usize]
                ^ CRC_TABLES[3][(high & 0xff) as usize]
                ^ CRC_TABLES[2][((high >> 8) & 0xff) as usize]
                ^ CRC_TABLES[1][((high >> 16) & 0xff) as usize]
                ^ CRC_TABLES[0][(high >> 24) as usize];
        }
        for &byte in chunks.remainder() {
            let index = ((value ^ byte as u32) & 0xff) as usize;
            value = CRC_TABLES[0][index] ^ (value >> 8);
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
