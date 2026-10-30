//! ZIP archive reading, extraction and writing.
//!
//! The reader supports stored (method 0) and deflate (method 8) members with
//! UTF-8 or CP437 names. ZIP64 and encrypted archives are reported as
//! unsupported. The writer emits stored or deflated entries without ZIP64.

use core::str;

use feos_compress::checksum::crc32;
use feos_compress::deflate;

use crate::error::{buffer_error, need, ArchiveError, Result};

const LOCAL_SIGNATURE: [u8; 4] = [b'P', b'K', 0x03, 0x04];
const CENTRAL_SIGNATURE: [u8; 4] = [b'P', b'K', 0x01, 0x02];
const EOCD_SIGNATURE: [u8; 4] = [b'P', b'K', 0x05, 0x06];

const LOCAL_HEADER_LEN: usize = 30;
const CENTRAL_HEADER_LEN: usize = 46;
const EOCD_LEN: usize = 22;
const MAX_COMMENT: usize = 65535;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;
const FLAG_UTF8: u16 = 0x0800;

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    need(data.len(), offset, 2)?;
    Ok(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    need(data.len(), offset, 4)?;
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

/// A member listed in a ZIP central directory.
pub struct ZipEntry<'a> {
    /// General purpose flags.
    pub flags: u16,
    /// Compression method (`0` stored, `8` deflate).
    pub method: u16,
    /// CRC-32 of the uncompressed data.
    pub crc32: u32,
    /// Compressed size in bytes.
    pub compressed_size: u32,
    /// Uncompressed size in bytes.
    pub uncompressed_size: u32,
    /// Offset of the local header in the archive.
    pub local_offset: u32,
    /// Raw external attributes.
    pub external_attrs: u32,
    /// Raw name bytes from the central directory.
    pub name_bytes: &'a [u8],
    /// Compressed member data.
    pub data: &'a [u8],
}

impl<'a> ZipEntry<'a> {
    /// Returns the member name as UTF-8.
    ///
    /// Names without the UTF-8 flag are usually CP437; callers that need
    /// legacy names can use [`ZipEntry::name_bytes`].
    pub fn name(&self) -> Result<&'a str> {
        str::from_utf8(self.name_bytes).map_err(|_| ArchiveError::InvalidData("name is not UTF-8"))
    }

    /// Returns whether this member is a directory.
    pub fn is_dir(&self) -> bool {
        self.name_bytes.ends_with(b"/") || self.external_attrs & 0x10 != 0
    }

    /// Returns whether the member is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0
    }

    /// Extracts the member into `out`, returning the number of bytes written.
    ///
    /// The size and CRC-32 recorded in the central directory are verified.
    pub fn extract(&self, out: &mut [u8]) -> Result<usize> {
        if self.is_encrypted() {
            return Err(ArchiveError::UnsupportedFeature("encrypted zip entry"));
        }
        let expected = self.uncompressed_size as usize;
        if out.len() < expected {
            return Err(buffer_error(expected, out.len()));
        }

        let written = match self.method {
            METHOD_STORED => {
                if self.compressed_size as usize != expected {
                    return Err(ArchiveError::InvalidData(
                        "stored zip entry has mismatched sizes",
                    ));
                }
                out[..expected].copy_from_slice(self.data);
                expected
            }
            METHOD_DEFLATE => feos_compress::inflate(self.data, out)?,
            _ => {
                return Err(ArchiveError::UnsupportedFeature(
                    "zip compression method (only stored and deflate are supported)",
                ))
            }
        };

        if written != expected {
            return Err(ArchiveError::InvalidData("zip size mismatch"));
        }
        if crc32(&out[..written]) != self.crc32 {
            return Err(ArchiveError::Checksum);
        }
        Ok(written)
    }
}

/// Iterates over the central directory of a ZIP archive.
pub struct ZipReader<'a> {
    data: &'a [u8],
    central: usize,
    central_end: usize,
    count: u16,
    index: u16,
}

impl<'a> ZipReader<'a> {
    /// Opens a ZIP archive.
    pub fn open(data: &'a [u8]) -> Result<Self> {
        let eocd = find_eocd(data)?;
        need(data.len(), eocd, EOCD_LEN)?;

        let disk = u16_at(data, eocd + 4)?;
        let central_disk = u16_at(data, eocd + 6)?;
        let entries_disk = u16_at(data, eocd + 8)?;
        let total = u16_at(data, eocd + 10)?;
        let central_size = u32_at(data, eocd + 12)?;
        let central_offset = u32_at(data, eocd + 16)?;
        let comment_len = u16_at(data, eocd + 20)? as usize;

        need(data.len(), eocd + EOCD_LEN, comment_len)?;
        if disk != 0 || central_disk != 0 {
            return Err(ArchiveError::UnsupportedFeature("multi-disk zip archive"));
        }
        if entries_disk != total {
            return Err(ArchiveError::InvalidData(
                "zip entry counts disagree between disks",
            ));
        }
        if total == 0xffff || central_size == 0xffff_ffff || central_offset == 0xffff_ffff {
            return Err(ArchiveError::UnsupportedFeature("zip64 archive"));
        }

        let central = central_offset as usize;
        let central_end = central
            .checked_add(central_size as usize)
            .ok_or(ArchiveError::Overflow)?;
        need(data.len(), central, central_size as usize)?;
        if central_end > eocd {
            return Err(ArchiveError::InvalidData(
                "zip central directory overlaps its end record",
            ));
        }

        Ok(Self {
            data,
            central,
            central_end,
            count: total,
            index: 0,
        })
    }

    /// Returns the number of members in the central directory.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Returns whether the archive has no members.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the next central-directory entry.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<ZipEntry<'a>>> {
        if self.index >= self.count {
            return Ok(None);
        }
        if self.central + CENTRAL_HEADER_LEN > self.central_end {
            return Err(ArchiveError::UnexpectedEnd);
        }
        let header = &self.data[self.central..self.central + CENTRAL_HEADER_LEN];
        if header[..4] != CENTRAL_SIGNATURE {
            return Err(ArchiveError::InvalidData(
                "bad zip central directory signature",
            ));
        }

        let flags = u16::from_le_bytes([header[8], header[9]]);
        let method = u16::from_le_bytes([header[10], header[11]]);
        let crc = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
        let compressed_size = u32::from_le_bytes([header[20], header[21], header[22], header[23]]);
        let uncompressed_size =
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        let name_len = u16::from_le_bytes([header[28], header[29]]) as usize;
        let extra_len = u16::from_le_bytes([header[30], header[31]]) as usize;
        let comment_len = u16::from_le_bytes([header[32], header[33]]) as usize;
        let disk_start = u16::from_le_bytes([header[34], header[35]]);
        let external_attrs = u32::from_le_bytes([header[38], header[39], header[40], header[41]]);
        let local_offset = u32::from_le_bytes([header[42], header[43], header[44], header[45]]);

        if disk_start != 0 {
            return Err(ArchiveError::UnsupportedFeature("multi-disk zip entry"));
        }
        if compressed_size == 0xffff_ffff
            || uncompressed_size == 0xffff_ffff
            || local_offset == 0xffff_ffff
        {
            return Err(ArchiveError::UnsupportedFeature("zip64 entry"));
        }

        let name_start = self.central + CENTRAL_HEADER_LEN;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or(ArchiveError::Overflow)?;
        let record_end = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or(ArchiveError::Overflow)?;
        need(
            self.data.len(),
            name_start,
            name_len + extra_len + comment_len,
        )?;
        if record_end > self.central_end {
            return Err(ArchiveError::InvalidData(
                "zip central directory record overruns directory",
            ));
        }
        let name = &self.data[name_start..name_end];

        let local = local_offset as usize;
        need(self.data.len(), local, LOCAL_HEADER_LEN)?;
        if self.data[local..local + 4] != LOCAL_SIGNATURE {
            return Err(ArchiveError::InvalidData("bad zip local header signature"));
        }
        let local_name_len = u16_at(self.data, local + 26)? as usize;
        let local_extra_len = u16_at(self.data, local + 28)? as usize;
        let data_start = local
            .checked_add(LOCAL_HEADER_LEN)
            .and_then(|value| value.checked_add(local_name_len))
            .and_then(|value| value.checked_add(local_extra_len))
            .ok_or(ArchiveError::Overflow)?;
        let data_end = data_start
            .checked_add(compressed_size as usize)
            .ok_or(ArchiveError::Overflow)?;
        need(self.data.len(), data_start, compressed_size as usize)?;

        self.central = record_end;
        self.index += 1;

        Ok(Some(ZipEntry {
            flags,
            method,
            crc32: crc,
            compressed_size,
            uncompressed_size,
            local_offset,
            external_attrs,
            name_bytes: name,
            data: &self.data[data_start..data_end],
        }))
    }
}

fn find_eocd(data: &[u8]) -> Result<usize> {
    if data.len() < EOCD_LEN {
        return Err(ArchiveError::UnexpectedEnd);
    }
    let minimum = data.len().saturating_sub(EOCD_LEN + MAX_COMMENT);
    let mut pos = data.len() - EOCD_LEN;
    loop {
        if data[pos..pos + 4] == EOCD_SIGNATURE {
            let comment_len = u16::from_le_bytes([data[pos + 20], data[pos + 21]]) as usize;
            if pos + EOCD_LEN + comment_len == data.len() {
                return Ok(pos);
            }
        }
        if pos == minimum {
            break;
        }
        pos -= 1;
    }
    Err(ArchiveError::InvalidData(
        "missing zip end of central directory",
    ))
}

/// Writes ZIP archives into a caller-provided buffer.
pub struct ZipWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> ZipWriter<'a> {
    /// Creates a writer over `out`.
    pub fn new(out: &'a mut [u8]) -> Self {
        Self { out, pos: 0 }
    }

    /// Returns the number of bytes written so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(ArchiveError::Overflow)?;
        if end > self.out.len() {
            return Err(buffer_error(end, self.out.len()));
        }
        self.out[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    fn write_entry(
        &mut self,
        name: &str,
        compressed: &[u8],
        method: u16,
        crc: u32,
        uncompressed_size: usize,
    ) -> Result<()> {
        if name.len() > u16::MAX as usize {
            return Err(ArchiveError::InvalidData("zip name too long"));
        }
        let compressed_size =
            u32::try_from(compressed.len()).map_err(|_| ArchiveError::Overflow)?;
        let uncompressed_size =
            u32::try_from(uncompressed_size).map_err(|_| ArchiveError::Overflow)?;
        let mut header = [0u8; LOCAL_HEADER_LEN];
        header[..4].copy_from_slice(&LOCAL_SIGNATURE);
        header[4..6].copy_from_slice(&20u16.to_le_bytes());
        header[6..8].copy_from_slice(&FLAG_UTF8.to_le_bytes());
        header[8..10].copy_from_slice(&method.to_le_bytes());
        header[10..12].copy_from_slice(&0u16.to_le_bytes());
        header[12..14].copy_from_slice(&0x21u16.to_le_bytes());
        header[14..18].copy_from_slice(&crc.to_le_bytes());
        header[18..22].copy_from_slice(&compressed_size.to_le_bytes());
        header[22..26].copy_from_slice(&uncompressed_size.to_le_bytes());
        header[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        header[28..30].copy_from_slice(&0u16.to_le_bytes());

        // Write the fixed part, the name, then the compressed data.
        let end = self
            .pos
            .checked_add(name.len() + LOCAL_HEADER_LEN)
            .and_then(|value| value.checked_add(compressed.len()))
            .ok_or(ArchiveError::Overflow)?;
        if end > self.out.len() {
            return Err(buffer_error(end, self.out.len()));
        }
        self.out[self.pos..self.pos + LOCAL_HEADER_LEN]
            .copy_from_slice(&header[..LOCAL_HEADER_LEN]);
        self.pos += LOCAL_HEADER_LEN;
        self.write_all(name.as_bytes())?;
        self.write_all(compressed)?;
        Ok(())
    }

    /// Writes a stored (uncompressed) file entry.
    pub fn write_stored(&mut self, name: &str, data: &[u8]) -> Result<()> {
        let crc = crc32(data);
        self.write_entry(name, data, METHOD_STORED, crc, data.len())
    }

    /// Writes a deflated file entry using `scratch` for the compressed data.
    pub fn write_deflated(&mut self, name: &str, data: &[u8], scratch: &mut [u8]) -> Result<()> {
        let len = deflate::compress(data, scratch)?;
        let crc = crc32(data);
        self.write_entry(name, &scratch[..len], METHOD_DEFLATE, crc, data.len())
    }

    /// Writes a directory entry. `name` should end with `/`.
    pub fn write_dir(&mut self, name: &str) -> Result<()> {
        self.write_stored(name, &[])
    }

    /// Writes the central directory and end record, returning the total
    /// archive length.
    pub fn finish(&mut self) -> Result<usize> {
        let central_start = self.pos;
        let total_len = self.out.len();
        let central_start_u32 =
            u32::try_from(central_start).map_err(|_| ArchiveError::UnsupportedFeature("zip64"))?;
        let (left, right) = self.out.split_at_mut(central_start);

        let mut scan = 0usize;
        let mut written = 0usize;
        let mut count = 0u16;

        while scan < left.len() {
            if scan + LOCAL_HEADER_LEN > left.len() {
                return Err(ArchiveError::UnexpectedEnd);
            }
            if left[scan..scan + 4] != LOCAL_SIGNATURE {
                return Err(ArchiveError::InvalidData(
                    "bad zip local header while writing central directory",
                ));
            }
            let flags = u16_at(left, scan + 6)?;
            let method = u16_at(left, scan + 8)?;
            let time = u16_at(left, scan + 10)?;
            let date = u16_at(left, scan + 12)?;
            let crc = u32_at(left, scan + 14)?;
            let compressed_size = u32_at(left, scan + 18)?;
            let uncompressed_size = u32_at(left, scan + 22)?;
            let name_len = u16_at(left, scan + 26)? as usize;
            let extra_len = u16_at(left, scan + 28)? as usize;

            let name_start = scan + LOCAL_HEADER_LEN;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or(ArchiveError::Overflow)?;
            let data_start = name_end
                .checked_add(extra_len)
                .ok_or(ArchiveError::Overflow)?;
            let data_end = data_start
                .checked_add(compressed_size as usize)
                .ok_or(ArchiveError::Overflow)?;
            need(left.len(), name_start, name_len)?;
            need(left.len(), data_end, 0)?;

            let record_len = CENTRAL_HEADER_LEN
                .checked_add(name_len)
                .ok_or(ArchiveError::Overflow)?;
            let record_end = written
                .checked_add(record_len)
                .ok_or(ArchiveError::Overflow)?;
            if record_end > right.len() {
                return Err(buffer_error(central_start + record_end, total_len));
            }

            let record = &mut right[written..written + CENTRAL_HEADER_LEN];
            record[0..4].copy_from_slice(&CENTRAL_SIGNATURE);
            record[4..6].copy_from_slice(&20u16.to_le_bytes());
            record[6..8].copy_from_slice(&20u16.to_le_bytes());
            record[8..10].copy_from_slice(&flags.to_le_bytes());
            record[10..12].copy_from_slice(&method.to_le_bytes());
            record[12..14].copy_from_slice(&time.to_le_bytes());
            record[14..16].copy_from_slice(&date.to_le_bytes());
            record[16..20].copy_from_slice(&crc.to_le_bytes());
            record[20..24].copy_from_slice(&compressed_size.to_le_bytes());
            record[24..28].copy_from_slice(&uncompressed_size.to_le_bytes());
            record[28..30].copy_from_slice(&(name_len as u16).to_le_bytes());
            record[30..32].copy_from_slice(&0u16.to_le_bytes());
            record[32..34].copy_from_slice(&0u16.to_le_bytes());
            record[34..36].copy_from_slice(&0u16.to_le_bytes());
            record[36..38].copy_from_slice(&0u16.to_le_bytes());
            let attrs = if name_len > 0 && left[name_start + name_len - 1] == b'/' {
                0x10u32
            } else {
                0
            };
            record[38..42].copy_from_slice(&attrs.to_le_bytes());
            record[42..46].copy_from_slice(
                &u32::try_from(scan)
                    .map_err(|_| ArchiveError::UnsupportedFeature("zip64"))?
                    .to_le_bytes(),
            );

            right[written + CENTRAL_HEADER_LEN..record_end]
                .copy_from_slice(&left[name_start..name_end]);
            written = record_end;
            count = count
                .checked_add(1)
                .ok_or(ArchiveError::UnsupportedFeature("zip64"))?;
            scan = data_end;
        }

        let eocd_end = written
            .checked_add(EOCD_LEN)
            .ok_or(ArchiveError::Overflow)?;
        if eocd_end > right.len() {
            return Err(buffer_error(central_start + eocd_end, total_len));
        }
        let eocd = &mut right[written..written + EOCD_LEN];
        eocd[0..4].copy_from_slice(&EOCD_SIGNATURE);
        eocd[4..6].copy_from_slice(&0u16.to_le_bytes());
        eocd[6..8].copy_from_slice(&0u16.to_le_bytes());
        eocd[8..10].copy_from_slice(&count.to_le_bytes());
        eocd[10..12].copy_from_slice(&count.to_le_bytes());
        let central_size =
            u32::try_from(written).map_err(|_| ArchiveError::UnsupportedFeature("zip64"))?;
        eocd[12..16].copy_from_slice(&central_size.to_le_bytes());
        eocd[16..20].copy_from_slice(&central_start_u32.to_le_bytes());
        eocd[20..22].copy_from_slice(&0u16.to_le_bytes());

        self.pos = central_start + eocd_end;
        Ok(self.pos)
    }
}
