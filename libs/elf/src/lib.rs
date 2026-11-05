//! A static ELF64 program loader's parsing half.
//!
//! The input is whatever someone put in a file, so nothing here trusts it: every read
//! is checked, every sum can't overflow, and anything off is an [`Error`], never a
//! panic. Host tests run it over malformed files and over mutations of a good one.
//!
//! This only says where things go. The caller owns the pages, and copies from the file.

#![cfg_attr(not(test), no_std)]

/// Most segments a program may load. Real static programs have about four.
pub const MAX_SEGMENTS: usize = 16;
/// Program header entries looked at. A file claiming more is refused.
const MAX_HEADERS: usize = 64;

const HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const ET_EXEC: u16 = 2;
/// `e_machine` of AArch64.
pub const EM_AARCH64: u16 = 183;
/// `e_machine` of x86_64.
pub const EM_X86_64: u16 = 62;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PAGE: u64 = 4096;

/// Why a file can't be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Too short to hold what it says it has.
    Truncated,
    /// Not an ELF file.
    BadMagic,
    /// Not 64-bit little-endian, or a version that doesn't exist.
    BadClass,
    /// Not a static executable.
    NotStatic,
    /// For another CPU.
    WrongMachine,
    /// The headers' sizes or number aren't sensible.
    BadHeaders,
    /// More segments than there's room for, or none.
    BadSegmentCount,
    /// A segment's addresses, sizes or offset don't work.
    BadSegment,
    /// A segment outside the range allowed.
    OutOfRange,
    /// Two segments share a page.
    Overlap,
    /// A segment both writable and executable.
    WriteAndExecute,
    /// The entry point isn't in an executable segment.
    BadEntry,
}

impl Error {
    /// What went wrong, for a person.
    pub const fn text(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::BadMagic => "not an ELF file",
            Self::BadClass => "not 64-bit little-endian ELF",
            Self::NotStatic => "not a static executable",
            Self::WrongMachine => "built for another CPU",
            Self::BadHeaders => "bad program headers",
            Self::BadSegmentCount => "wrong number of segments",
            Self::BadSegment => "bad segment",
            Self::OutOfRange => "a segment is outside the user address range",
            Self::Overlap => "segments overlap",
            Self::WriteAndExecute => "a segment is both writable and executable",
            Self::BadEntry => "the entry point isn't in executable code",
        }
    }
}

/// One piece of a program to map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// Where it starts in memory. May not be page aligned.
    pub address: u64,
    /// Bytes in memory, including the zeroed tail.
    pub memory_size: u64,
    /// Bytes that come from the file.
    pub file_size: u64,
    /// Where in the file those start.
    pub offset: u64,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

impl Segment {
    /// The first page's address.
    pub const fn page_start(&self) -> u64 {
        self.address & !(PAGE - 1)
    }

    /// One past the last page. Checked not to overflow by [`parse`].
    pub const fn page_end(&self) -> u64 {
        (self.address + self.memory_size + PAGE - 1) & !(PAGE - 1)
    }
}

/// A validated program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    pub entry: u64,
    pub segments: [Segment; MAX_SEGMENTS],
    pub count: usize,
}

impl Program {
    /// The segments to load, in address order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// What a program has to fit in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The value `e_machine` must have.
    pub machine: u16,
    /// The lowest address a segment may use.
    pub base: u64,
    /// One past the highest.
    pub end: u64,
    /// Most bytes all segments may take in memory.
    pub max_memory: u64,
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, Error> {
    let end = at.checked_add(2).ok_or(Error::Truncated)?;
    let part = bytes.get(at..end).ok_or(Error::Truncated)?;
    Ok(u16::from_le_bytes([part[0], part[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, Error> {
    let end = at.checked_add(4).ok_or(Error::Truncated)?;
    let part = bytes.get(at..end).ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([part[0], part[1], part[2], part[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, Error> {
    let end = at.checked_add(8).ok_or(Error::Truncated)?;
    let part = bytes.get(at..end).ok_or(Error::Truncated)?;
    let mut word = [0u8; 8];
    word.copy_from_slice(part);
    Ok(u64::from_le_bytes(word))
}

/// Bytes of the file's start a caller must supply to [`parse`] for any file it would
/// accept: the header and as many program headers as it looks at.
pub const HEADER_BYTES: usize = HEADER_SIZE + MAX_HEADERS * PROGRAM_HEADER_SIZE;

/// Checks a file and says what to load.
///
/// `head` is the file's start, as much of [`HEADER_BYTES`] as it has, and `file_size`
/// its whole length. Program headers have to be in `head`, which any linker puts them.
pub fn parse(head: &[u8], file_size: u64, limits: &Limits) -> Result<Program, Error> {
    if head.len() < 4 {
        return Err(Error::Truncated);
    }
    if head[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(Error::BadMagic);
    }
    if head.len() < HEADER_SIZE {
        return Err(Error::Truncated);
    }
    // ELFCLASS64, little-endian, version 1.
    if head[4] != 2 || head[5] != 1 || head[6] != 1 {
        return Err(Error::BadClass);
    }
    if read_u32(head, 20)? != 1 {
        return Err(Error::BadClass);
    }
    if read_u16(head, 16)? != ET_EXEC {
        return Err(Error::NotStatic);
    }
    if read_u16(head, 18)? != limits.machine {
        return Err(Error::WrongMachine);
    }

    let entry = read_u64(head, 24)?;
    let table = read_u64(head, 32)?;
    let entry_size = read_u16(head, 54)? as usize;
    let entries = read_u16(head, 56)? as usize;
    if entry_size != PROGRAM_HEADER_SIZE || entries == 0 || entries > MAX_HEADERS {
        return Err(Error::BadHeaders);
    }
    let table = usize::try_from(table).map_err(|_| Error::BadHeaders)?;
    let table_end = table
        .checked_add(entries * PROGRAM_HEADER_SIZE)
        .ok_or(Error::BadHeaders)?;
    if table_end > head.len() {
        return Err(Error::Truncated);
    }

    let mut segments = [Segment::default(); MAX_SEGMENTS];
    let mut count = 0;
    let mut total = 0u64;
    for index in 0..entries {
        let at = table + index * PROGRAM_HEADER_SIZE;
        let kind = read_u32(head, at)?;
        if kind == PT_DYNAMIC || kind == PT_INTERP {
            return Err(Error::NotStatic);
        }
        if kind != PT_LOAD {
            continue;
        }
        let flags = read_u32(head, at + 4)?;
        let offset = read_u64(head, at + 8)?;
        let address = read_u64(head, at + 16)?;
        let file_size_here = read_u64(head, at + 32)?;
        let memory_size = read_u64(head, at + 40)?;
        let align = read_u64(head, at + 48)?;

        if memory_size == 0 {
            continue;
        }
        if count == MAX_SEGMENTS {
            return Err(Error::BadSegmentCount);
        }
        if file_size_here > memory_size {
            return Err(Error::BadSegment);
        }
        // An alignment is zero, one or a power of two, and the offset and address
        // agree within a page, or the bytes couldn't be mapped where they belong.
        if align > 1 && !align.is_power_of_two() {
            return Err(Error::BadSegment);
        }
        if offset % PAGE != address % PAGE {
            return Err(Error::BadSegment);
        }
        let file_end = offset.checked_add(file_size_here).ok_or(Error::BadSegment)?;
        if file_end > file_size {
            return Err(Error::Truncated);
        }
        let end = address.checked_add(memory_size).ok_or(Error::OutOfRange)?;
        let rounded_end = end.checked_add(PAGE - 1).ok_or(Error::OutOfRange)? & !(PAGE - 1);
        if address < limits.base || rounded_end > limits.end {
            return Err(Error::OutOfRange);
        }
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(Error::WriteAndExecute);
        }
        total = total.checked_add(rounded_end - (address & !(PAGE - 1))).ok_or(Error::BadSegment)?;
        if total > limits.max_memory {
            return Err(Error::BadSegment);
        }

        segments[count] = Segment {
            address,
            memory_size,
            file_size: file_size_here,
            offset,
            readable: flags & PF_R != 0,
            writable: flags & PF_W != 0,
            executable: flags & PF_X != 0,
        };
        count += 1;
    }
    if count == 0 {
        return Err(Error::BadSegmentCount);
    }

    segments[..count].sort_unstable_by_key(|segment| segment.address);
    for pair in segments[..count].windows(2) {
        if pair[0].page_end() > pair[1].page_start() {
            return Err(Error::Overlap);
        }
    }

    let runnable = segments[..count].iter().any(|segment| {
        segment.executable
            && entry >= segment.address
            && entry < segment.address + segment.memory_size
    });
    if !runnable {
        return Err(Error::BadEntry);
    }

    Ok(Program {
        entry,
        segments,
        count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        machine: EM_AARCH64,
        base: 0x10_0000_0000,
        end: 0x11_0000_0000,
        max_memory: 16 * 1024 * 1024,
    };
    const BASE: u64 = 0x10_0040_0000;

    fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    /// A good file: code at `BASE`, then data with a zeroed tail on the next page.
    fn good() -> Vec<u8> {
        let mut file = vec![0u8; 0x3000];
        put(&mut file, 0, &[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        put(&mut file, 16, &ET_EXEC.to_le_bytes());
        put(&mut file, 18, &EM_AARCH64.to_le_bytes());
        put(&mut file, 20, &1u32.to_le_bytes());
        put(&mut file, 24, &(BASE + 0x1000).to_le_bytes());
        put(&mut file, 32, &64u64.to_le_bytes());
        put(&mut file, 52, &64u16.to_le_bytes());
        put(&mut file, 54, &56u16.to_le_bytes());
        put(&mut file, 56, &2u16.to_le_bytes());
        header(&mut file, 0, PF_R | PF_X, 0x1000, BASE + 0x1000, 0x800, 0x800);
        header(&mut file, 1, PF_R | PF_W, 0x2000, BASE + 0x2000, 0x100, 0x5000);
        file
    }

    #[allow(clippy::too_many_arguments)]
    fn header(file: &mut [u8], index: usize, flags: u32, offset: u64, address: u64, filesz: u64, memsz: u64) {
        let at = 64 + index * 56;
        put(file, at, &PT_LOAD.to_le_bytes());
        put(file, at + 4, &flags.to_le_bytes());
        put(file, at + 8, &offset.to_le_bytes());
        put(file, at + 16, &address.to_le_bytes());
        put(file, at + 32, &filesz.to_le_bytes());
        put(file, at + 40, &memsz.to_le_bytes());
        put(file, at + 48, &0x1000u64.to_le_bytes());
    }

    fn check(file: &[u8]) -> Result<Program, Error> {
        let head = &file[..file.len().min(HEADER_BYTES)];
        parse(head, file.len() as u64, &LIMITS)
    }

    #[test]
    fn a_good_file_parses() {
        let program = check(&good()).unwrap();
        assert_eq!(program.entry, BASE + 0x1000);
        assert_eq!(program.segments().len(), 2);
        let data = program.segments()[1];
        assert!(data.writable && !data.executable);
        assert_eq!(data.page_start(), BASE + 0x2000);
        assert_eq!(data.page_end(), BASE + 0x7000);
    }

    #[test]
    fn each_kind_of_bad_header_is_refused() {
        let mut cases: Vec<(&str, Vec<u8>, Error)> = Vec::new();
        let mut case = |name, edit: &dyn Fn(&mut Vec<u8>), error| {
            let mut file = good();
            edit(&mut file);
            cases.push((name, file, error));
        };
        case("magic", &|f| f[0] = 0, Error::BadMagic);
        case("32-bit", &|f| f[4] = 1, Error::BadClass);
        case("big-endian", &|f| f[5] = 2, Error::BadClass);
        case("version", &|f| f[6] = 9, Error::BadClass);
        case("shared object", &|f| f[16] = 3, Error::NotStatic);
        case("other machine", &|f| f[18] = 62, Error::WrongMachine);
        case("header size", &|f| f[54] = 32, Error::BadHeaders);
        case("no headers", &|f| { f[56] = 0; f[57] = 0 }, Error::BadHeaders);
        case("too many headers", &|f| f[57] = 0xff, Error::BadHeaders);
        case("table past the end", &|f| put(f, 32, &u64::MAX.to_le_bytes()), Error::BadHeaders);
        case("table outside", &|f| put(f, 32, &0x7fff_0000u64.to_le_bytes()), Error::Truncated);
        case("interpreter", &|f| put(f, 64, &PT_INTERP.to_le_bytes()), Error::NotStatic);
        case("dynamic", &|f| put(f, 120, &PT_DYNAMIC.to_le_bytes()), Error::NotStatic);
        case("file bigger than memory", &|f| put(f, 64 + 32, &0x9000u64.to_le_bytes()), Error::BadSegment);
        case("offset past the end", &|f| put(f, 64 + 8, &0x9000u64.to_le_bytes()), Error::Truncated);
        case("offset wraps", &|f| {
            put(f, 64 + 8, &(u64::MAX - 0xfff).to_le_bytes());
            put(f, 64 + 32, &0xfff_ffffu64.to_le_bytes());
            put(f, 64 + 40, &0xfff_ffffu64.to_le_bytes());
        }, Error::BadSegment);
        case("into the kernel", &|f| {
            put(f, 64 + 16, &0x4020_0000u64.to_le_bytes());
            put(f, 64 + 8, &0u64.to_le_bytes());
        }, Error::OutOfRange);
        case("address wraps", &|f| {
            put(f, 64 + 16, &(u64::MAX - 0xfff).to_le_bytes());
            put(f, 64 + 8, &0x1000u64.to_le_bytes());
        }, Error::OutOfRange);
        case("memory size wraps", &|f| put(f, 64 + 40, &u64::MAX.to_le_bytes()), Error::OutOfRange);
        case("past the window", &|f| put(f, 120 + 40, &0x1_0000_0000u64.to_le_bytes()), Error::OutOfRange);
        case("huge", &|f| put(f, 120 + 40, &0xfff_0000u64.to_le_bytes()), Error::BadSegment);
        case("writable code", &|f| put(f, 64 + 4, &(PF_R | PF_W | PF_X).to_le_bytes()), Error::WriteAndExecute);
        case("overlap", &|f| {
            put(f, 120 + 16, &(BASE + 0x1800).to_le_bytes());
            put(f, 120 + 8, &0x1800u64.to_le_bytes());
        }, Error::Overlap);
        case("entry in data", &|f| put(f, 24, &(BASE + 0x2000).to_le_bytes()), Error::BadEntry);
        case("entry in the kernel", &|f| put(f, 24, &0x4020_0000u64.to_le_bytes()), Error::BadEntry);
        case("no code", &|f| put(f, 64 + 4, &PF_R.to_le_bytes()), Error::BadEntry);
        case("bad alignment", &|f| put(f, 64 + 48, &3u64.to_le_bytes()), Error::BadSegment);
        for (name, file, error) in cases {
            assert_eq!(check(&file), Err(error), "{name}");
        }
    }

    #[test]
    fn an_empty_or_short_file_is_refused() {
        assert_eq!(check(&[]), Err(Error::Truncated));
        assert_eq!(check(&[0x7f]), Err(Error::Truncated));
        assert_eq!(check(b"#!/bin/sh\n"), Err(Error::BadMagic));
        let file = good();
        for length in 4..file.len().min(0x2100) {
            // Every truncation either fails or, past what's needed, still parses.
            let cut = &file[..length];
            let result = check(cut);
            if length < 0x2100 {
                assert!(result.is_err(), "length {length}");
            }
        }
    }

    #[test]
    fn segments_count_is_bounded() {
        let mut file = good();
        file.resize(HEADER_BYTES + 0x3000, 0);
        put(&mut file, 56, &(MAX_SEGMENTS as u16 + 2).to_le_bytes());
        for index in 0..MAX_SEGMENTS + 2 {
            let address = BASE + 0x10_0000 * (index as u64 + 1);
            header(&mut file, index, PF_R | PF_X, 0x1000, address, 0x10, 0x10);
        }
        put(&mut file, 24, &(BASE + 0x10_0000).to_le_bytes());
        assert_eq!(check(&file), Err(Error::BadSegmentCount));
    }

    /// A tiny deterministic generator: no dependency, and a failure can be replayed.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn mutated_files_never_panic_and_what_passes_is_in_range() {
        let original = good();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..200_000 {
            let mut file = original.clone();
            let flips = 1 + next(&mut state) % 8;
            for _ in 0..flips {
                // Mostly the headers, since that's what's parsed.
                let at = (next(&mut state) % (64 + 56 * 2 + 8)) as usize;
                let value = match next(&mut state) % 4 {
                    0 => 0,
                    1 => 0xff,
                    2 => 0x80,
                    _ => next(&mut state) as u8,
                };
                file[at] = value;
            }
            if let Ok(program) = check(&file) {
                for segment in program.segments() {
                    assert!(segment.address >= LIMITS.base);
                    assert!(segment.page_end() <= LIMITS.end);
                    assert!(segment.file_size <= segment.memory_size);
                    assert!(segment.offset + segment.file_size <= file.len() as u64);
                    assert!(!(segment.writable && segment.executable));
                }
                assert!(program.entry >= LIMITS.base && program.entry < LIMITS.end);
            }
        }
    }

    #[test]
    fn random_files_never_panic() {
        let mut state = 0x1234_5678_9abc_def1u64;
        for _ in 0..20_000 {
            let length = (next(&mut state) % 600) as usize;
            let mut file: Vec<u8> = (0..length).map(|_| next(&mut state) as u8).collect();
            if file.len() >= 4 && next(&mut state) % 2 == 0 {
                file[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
            }
            let _ = check(&file);
        }
    }
}
