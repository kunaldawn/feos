//! TAR (ustar, GNU tar and PAX) reading and writing.

use core::str;

use crate::error::{buffer_error, need, ArchiveError, Result};

const BLOCK: usize = 512;
const NAME_LEN: usize = 100;
const PREFIX_LEN: usize = 155;
const LINK_LEN: usize = 100;

/// The kind of a TAR entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryType {
    /// A regular file (`0` or NUL).
    Regular,
    /// A hard link to another entry.
    HardLink,
    /// A symbolic link.
    Symlink,
    /// A character device.
    Character,
    /// A block device.
    Block,
    /// A directory.
    Directory,
    /// A FIFO.
    Fifo,
    /// A contiguous file.
    Contiguous,
    /// An unrecognised type flag.
    Other(u8),
}

impl EntryType {
    fn from_flag(flag: u8) -> Self {
        match flag {
            0 | b'0' => Self::Regular,
            b'1' => Self::HardLink,
            b'2' => Self::Symlink,
            b'3' => Self::Character,
            b'4' => Self::Block,
            b'5' => Self::Directory,
            b'6' => Self::Fifo,
            b'7' => Self::Contiguous,
            other => Self::Other(other),
        }
    }
}

/// One entry in a TAR archive.
///
/// Names may use the ustar prefix field, so use [`Entry::path`] to obtain the
/// complete path in a caller-provided buffer. [`Entry::name_bytes`] returns the
/// name field on its own.
pub struct Entry<'a> {
    /// Entry type.
    pub entry_type: EntryType,
    /// Unix permission bits.
    pub mode: u32,
    /// Modification time in seconds since the Unix epoch.
    pub mtime: u64,
    /// Size of the entry data in bytes.
    pub size: u64,
    /// File contents (empty for directories and links).
    pub data: &'a [u8],
    name: &'a [u8],
    prefix: &'a [u8],
    link: &'a [u8],
}

impl<'a> Entry<'a> {
    /// Returns the entry name without the ustar prefix.
    pub fn name_bytes(&self) -> &'a [u8] {
        self.name
    }

    /// Returns the ustar prefix (empty for most entries).
    pub fn prefix_bytes(&self) -> &'a [u8] {
        self.prefix
    }

    /// Returns the link target for hard and symbolic links.
    pub fn link_bytes(&self) -> &'a [u8] {
        self.link
    }

    /// Returns the number of bytes [`Entry::path`] will write.
    pub fn path_len(&self) -> usize {
        if self.prefix.is_empty() {
            self.name.len()
        } else {
            self.prefix.len() + 1 + self.name.len()
        }
    }

    /// Writes the complete path into `buffer` and returns it as UTF-8.
    pub fn path<'b>(&self, buffer: &'b mut [u8]) -> Result<&'b str> {
        let len = self.path_len();
        if buffer.len() < len {
            return Err(buffer_error(len, buffer.len()));
        }
        if self.prefix.is_empty() {
            buffer[..len].copy_from_slice(self.name);
        } else {
            buffer[..self.prefix.len()].copy_from_slice(self.prefix);
            buffer[self.prefix.len()] = b'/';
            buffer[self.prefix.len() + 1..len].copy_from_slice(self.name);
        }
        str::from_utf8(&buffer[..len]).map_err(|_| ArchiveError::InvalidData("path is not UTF-8"))
    }

    /// Returns the link target as UTF-8.
    pub fn link(&self) -> Result<&'a str> {
        str::from_utf8(self.link).map_err(|_| ArchiveError::InvalidData("link is not UTF-8"))
    }

    /// Returns whether this is a regular file.
    pub fn is_file(&self) -> bool {
        matches!(self.entry_type, EntryType::Regular | EntryType::Contiguous)
    }

    /// Returns whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.entry_type == EntryType::Directory
    }

    /// Returns whether this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.entry_type == EntryType::Symlink
    }
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn trim_spaces(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && (bytes[start] == b' ' || bytes[start] == 0) {
        start += 1;
    }
    while end > start && (bytes[end - 1] == b' ' || bytes[end - 1] == 0) {
        end -= 1;
    }
    &bytes[start..end]
}

fn parse_number(bytes: &[u8]) -> Result<u64> {
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes[0] & 0x80 != 0 {
        // GNU base-256 encoding: the marker bit is stripped from the first
        // byte, the rest is a big-endian value.
        if bytes[0] & 0x40 != 0 {
            return Err(ArchiveError::InvalidData("negative tar number"));
        }
        let mut value = (bytes[0] & 0x3f) as u64;
        for &byte in &bytes[1..] {
            value = value
                .checked_mul(256)
                .and_then(|value| value.checked_add(byte as u64))
                .ok_or(ArchiveError::Overflow)?;
        }
        return Ok(value);
    }

    let digits = trim_spaces(bytes);
    if digits.is_empty() {
        return Ok(0);
    }
    let mut value = 0u64;
    for &byte in digits {
        if !(b'0'..=b'7').contains(&byte) {
            return Err(ArchiveError::InvalidData("bad octal tar field"));
        }
        value = value
            .checked_mul(8)
            .and_then(|value| value.checked_add((byte - b'0') as u64))
            .ok_or(ArchiveError::Overflow)?;
    }
    Ok(value)
}

fn parse_decimal(bytes: &[u8]) -> Result<u64> {
    if bytes.is_empty() {
        return Err(ArchiveError::InvalidData("empty pax number"));
    }
    let mut value = 0u64;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(ArchiveError::InvalidData("bad pax number"));
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as u64))
            .ok_or(ArchiveError::Overflow)?;
    }
    Ok(value)
}

fn verify_checksum(header: &[u8], expected: u64) -> Result<()> {
    let mut unsigned = 0u64;
    let mut signed = 0i64;
    for (index, &byte) in header.iter().enumerate() {
        let value = if (148..156).contains(&index) {
            b' '
        } else {
            byte
        };
        unsigned += value as u64;
        signed += value as i8 as i64;
    }
    if expected == unsigned || (signed >= 0 && expected == signed as u64) {
        Ok(())
    } else {
        Err(ArchiveError::Checksum)
    }
}

type PaxFields<'a> = (Option<&'a [u8]>, Option<&'a [u8]>, Option<u64>);

fn parse_pax(data: &[u8]) -> Result<PaxFields<'_>> {
    let mut path = None;
    let mut link = None;
    let mut size = None;
    let mut pos = 0usize;

    while pos < data.len() {
        if data[pos] == 0 {
            break;
        }
        let start = pos;
        let mut length = 0usize;
        while pos < data.len() && data[pos].is_ascii_digit() {
            length = length
                .checked_mul(10)
                .and_then(|value| value.checked_add((data[pos] - b'0') as usize))
                .ok_or(ArchiveError::Overflow)?;
            pos += 1;
        }
        if pos >= data.len() || data[pos] != b' ' {
            return Err(ArchiveError::InvalidData("malformed pax record"));
        }
        pos += 1;
        if length == 0 {
            return Err(ArchiveError::InvalidData("zero pax record length"));
        }
        let end = start
            .checked_add(length)
            .filter(|&end| end <= data.len())
            .ok_or(ArchiveError::InvalidData("pax record overruns data"))?;
        if end < pos {
            return Err(ArchiveError::InvalidData("pax record length too short"));
        }
        let content_end = if end > pos && data[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        let record = &data[pos..content_end];
        if let Some(equals) = record.iter().position(|&byte| byte == b'=') {
            let key = &record[..equals];
            let value = &record[equals + 1..];
            match key {
                b"path" => path = Some(value),
                b"linkpath" => link = Some(value),
                b"size" => size = Some(parse_decimal(value)?),
                _ => {}
            }
        }
        if end == start {
            return Err(ArchiveError::InvalidData("zero pax record length"));
        }
        pos = end;
    }

    Ok((path, link, size))
}

/// Iterates over the entries of a TAR archive.
pub struct TarReader<'a> {
    data: &'a [u8],
    pos: usize,
    long_name: Option<&'a [u8]>,
    long_link: Option<&'a [u8]>,
    pax_path: Option<&'a [u8]>,
    pax_link: Option<&'a [u8]>,
    pax_size: Option<u64>,
}

impl<'a> TarReader<'a> {
    /// Creates a reader over an uncompressed TAR archive.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            long_name: None,
            long_link: None,
            pax_path: None,
            pax_link: None,
            pax_size: None,
        }
    }

    /// Returns the byte offset of the next header.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Returns the next entry, or `None` at the end of the archive.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Entry<'a>>> {
        loop {
            if self.pos >= self.data.len() {
                return Ok(None);
            }
            need(self.data.len(), self.pos, BLOCK)?;
            let header = &self.data[self.pos..self.pos + BLOCK];
            if header.iter().all(|&byte| byte == 0) {
                self.pos = self.data.len();
                return Ok(None);
            }

            let expected = parse_number(&header[148..156])?;
            verify_checksum(header, expected)?;

            let flag = header[156];
            let header_size = parse_number(&header[124..136])?;
            let data_start = self.pos + BLOCK;
            // Links, devices, directories and FIFOs carry no data blocks whatever
            // their size field says, and a PAX size only describes the file it
            // precedes: applying it to a metadata header would desync the walk.
            let size = match flag {
                b'L' | b'K' | b'x' | b'g' => header_size,
                b'1'..=b'6' => 0,
                _ => self.pax_size.unwrap_or(header_size),
            };
            let data_end = data_start
                .checked_add(usize::try_from(size).map_err(|_| ArchiveError::Overflow)?)
                .ok_or(ArchiveError::Overflow)?;
            need(self.data.len(), data_start, data_end - data_start)?;
            let data = &self.data[data_start..data_end];
            let padded = data_end
                .checked_add((BLOCK - data.len() % BLOCK) % BLOCK)
                .ok_or(ArchiveError::Overflow)?;
            need(self.data.len(), data_end, padded - data_end)?;
            self.pos = padded;

            match flag {
                b'L' => {
                    self.long_name = Some(trim_nul(data));
                    continue;
                }
                b'K' => {
                    self.long_link = Some(trim_nul(data));
                    continue;
                }
                b'x' => {
                    let (path, link, size) = parse_pax(data)?;
                    self.pax_path = path;
                    self.pax_link = link;
                    if size.is_some() {
                        self.pax_size = size;
                    }
                    continue;
                }
                b'g' => continue,
                _ => {}
            }

            let direct_name = trim_nul(&header[..NAME_LEN]);
            let use_long_name = self.pax_path.is_some() || self.long_name.is_some();
            let name = self.pax_path.or(self.long_name).unwrap_or(direct_name);
            let prefix = if use_long_name {
                &[][..]
            } else {
                trim_nul(&header[345..345 + PREFIX_LEN])
            };
            let link = self
                .pax_link
                .or(self.long_link)
                .unwrap_or_else(|| trim_nul(&header[157..157 + LINK_LEN]));

            let entry = Entry {
                entry_type: EntryType::from_flag(flag),
                mode: parse_number(&header[100..108])? as u32,
                mtime: parse_number(&header[136..148])?,
                size,
                data,
                name,
                prefix,
                link,
            };

            self.long_name = None;
            self.long_link = None;
            self.pax_path = None;
            self.pax_link = None;
            self.pax_size = None;
            return Ok(Some(entry));
        }
    }
}

/// Writes ustar archives into a caller-provided buffer.
pub struct TarWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> TarWriter<'a> {
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

    fn write_zeros(&mut self, count: usize) -> Result<()> {
        let end = self.pos.checked_add(count).ok_or(ArchiveError::Overflow)?;
        if end > self.out.len() {
            return Err(buffer_error(end, self.out.len()));
        }
        for byte in &mut self.out[self.pos..end] {
            *byte = 0;
        }
        self.pos = end;
        Ok(())
    }

    fn write_header(
        &mut self,
        name: &[u8],
        entry_type: EntryType,
        mode: u32,
        size: u64,
        link: &[u8],
        mtime: u64,
    ) -> Result<()> {
        let (prefix, base) = split_name(name)?;
        if base.len() > NAME_LEN {
            return Err(ArchiveError::InvalidData("tar name too long"));
        }
        if prefix.len() > PREFIX_LEN {
            return Err(ArchiveError::InvalidData("tar prefix too long"));
        }
        if link.len() > LINK_LEN {
            return Err(ArchiveError::InvalidData("tar link name too long"));
        }
        if mtime > MAX_OCTAL_11 {
            return Err(ArchiveError::InvalidData("tar mtime too large"));
        }
        if size > MAX_OCTAL_11 {
            return Err(ArchiveError::InvalidData("tar size too large"));
        }

        let mut header = [0u8; BLOCK];
        header[..base.len()].copy_from_slice(base);
        write_octal(&mut header[100..108], mode as u64)?;
        write_octal(&mut header[108..116], 0)?;
        write_octal(&mut header[116..124], 0)?;
        write_octal(&mut header[124..136], size)?;
        write_octal(&mut header[136..148], mtime)?;

        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        let flag = match entry_type {
            EntryType::Regular => b'0',
            EntryType::HardLink => b'1',
            EntryType::Symlink => b'2',
            EntryType::Character => b'3',
            EntryType::Block => b'4',
            EntryType::Directory => b'5',
            EntryType::Fifo => b'6',
            EntryType::Contiguous => b'7',
            EntryType::Other(flag) => flag,
        };
        header[156] = flag;
        header[157..157 + link.len()].copy_from_slice(link);

        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[265..269].copy_from_slice(b"root");
        header[297..301].copy_from_slice(b"root");
        header[345..345 + prefix.len()].copy_from_slice(prefix);

        let mut sum = 0u32;
        for &byte in &header {
            sum += byte as u32;
        }
        let digits = format_octal(sum as u64, 6);
        header[148..154].copy_from_slice(&digits[..6]);
        header[154] = 0;
        header[155] = b' ';

        self.write_all(&header)
    }

    fn write_data(&mut self, data: &[u8]) -> Result<()> {
        self.write_all(data)?;
        let padding = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.write_zeros(padding)
    }

    /// Writes a regular file with mode `0644` and mtime `0`.
    pub fn write_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.write_file_with(name, data, 0o644, 0)
    }

    /// Writes a regular file with explicit mode and modification time.
    pub fn write_file_with(
        &mut self,
        name: &str,
        data: &[u8],
        mode: u32,
        mtime: u64,
    ) -> Result<()> {
        self.write_header(
            name.as_bytes(),
            EntryType::Regular,
            mode,
            data.len() as u64,
            &[],
            mtime,
        )?;
        self.write_data(data)
    }

    /// Writes a directory entry with mode `0755` and mtime `0`.
    pub fn write_dir(&mut self, name: &str) -> Result<()> {
        self.write_dir_with(name, 0o755, 0)
    }

    /// Writes a directory entry with explicit mode and modification time.
    pub fn write_dir_with(&mut self, name: &str, mode: u32, mtime: u64) -> Result<()> {
        self.write_header(name.as_bytes(), EntryType::Directory, mode, 0, &[], mtime)
    }

    /// Writes a symbolic link with mode `0777` and mtime `0`.
    pub fn write_symlink(&mut self, name: &str, target: &str) -> Result<()> {
        self.write_symlink_with(name, target, 0o777, 0)
    }

    /// Writes a symbolic link with explicit mode and modification time.
    pub fn write_symlink_with(
        &mut self,
        name: &str,
        target: &str,
        mode: u32,
        mtime: u64,
    ) -> Result<()> {
        self.write_header(
            name.as_bytes(),
            EntryType::Symlink,
            mode,
            0,
            target.as_bytes(),
            mtime,
        )
    }

    /// Writes the two zero blocks that terminate an archive and returns its
    /// total length.
    pub fn finish(&mut self) -> Result<usize> {
        self.write_zeros(BLOCK * 2)?;
        Ok(self.pos)
    }
}

const MAX_OCTAL_11: u64 = (1u64 << 33) - 1;

fn split_name(name: &[u8]) -> Result<(&[u8], &[u8])> {
    if name.len() <= NAME_LEN {
        return Ok((&[], name));
    }
    let mut best = None;
    for (index, &byte) in name.iter().enumerate() {
        if byte == b'/' && index > 0 && index <= PREFIX_LEN {
            let base = &name[index + 1..];
            if !base.is_empty() && base.len() <= NAME_LEN {
                best = Some(index);
            }
        }
    }
    match best {
        Some(index) => Ok((&name[..index], &name[index + 1..])),
        None => Err(ArchiveError::InvalidData("tar name does not fit")),
    }
}

fn format_octal(value: u64, digits: usize) -> [u8; 8] {
    let mut field = [b'0'; 8];
    let mut value = value;
    let mut index = digits;
    while index > 0 {
        index -= 1;
        field[index] = b'0' + (value & 7) as u8;
        value >>= 3;
    }
    field
}

fn write_octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = field.len() - 1;
    let max = if digits >= 21 {
        u64::MAX
    } else {
        (1u64 << (digits * 3)) - 1
    };
    if value > max {
        return Err(ArchiveError::InvalidData("tar field too large"));
    }
    let formatted = format_octal(value, digits.min(8));
    if digits <= 8 {
        field[..digits].copy_from_slice(&formatted[..digits]);
    } else {
        for byte in &mut field[..digits] {
            *byte = b'0';
        }
        let mut value = value;
        let mut index = digits;
        while index > 0 && value > 0 {
            index -= 1;
            field[index] = b'0' + (value & 7) as u8;
            value >>= 3;
        }
    }
    field[digits] = 0;
    Ok(())
}
