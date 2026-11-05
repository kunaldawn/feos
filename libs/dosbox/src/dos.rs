//! The DOS kernel: `INT 21h`, the process model, handles and files.
//!
//! The memory arena is a real MCB chain in guest memory. A process is a real PSP
//! with its job file table, and the JFT's entries index a system file table kept
//! here, so inherited and duplicated handles share one file position as on real
//! DOS. `EXEC` and terminate work through the stack frames of `INT 21h` itself:
//! the parent's `SS:SP` is saved in its PSP, the child is entered through the
//! `IRET` of the `INT 21h` stub, and terminating returns through the parent's own
//! saved frame, with `AH=4Dh` giving the exit code.
//!
//! Unsupported calls fail cleanly with `CF` set and an error code in `AX`, which
//! is what a DOS program expects.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_archive::zip::ZipReader;

use crate::bios::{clock_date, clock_time, day_of_week};
use crate::cpu::{CpuRegs, InterruptAction, DI, DS, ES, SI};
use crate::demo;
use crate::keyboard::{bda_flush, bda_peek, bda_pop};
use crate::memory::Memory;
use crate::ramdisk::{dos_name, fcb_match, fcb_name, strip_drive, RamDisk};
use crate::vga::Vga;

/// The segment of `COMMAND.COM`'s PSP, the root of the process tree.
pub const PSP_SEGMENT: u16 = COMMAND_PSP;
/// The DOS data segment: `InDOS`, the list of lists and the country tables.
pub const DOS_DATA: u16 = 0x0070;
/// The first MCB in the chain.
pub const FIRST_MCB: u16 = 0x0100;
/// The root PSP.
pub const COMMAND_PSP: u16 = 0x0101;
const COMMAND_PARAS: u16 = 0x20;
/// The root environment.
const COMMAND_ENV: u16 = 0x0122;
const COMMAND_ENV_PARAS: u16 = 0x40;
/// One past the last paragraph of conventional memory.
pub const MEMORY_TOP: u16 = 0xA000;
/// Number of job file table entries in a PSP.
pub const HANDLE_COUNT: u16 = 20;
/// The highest number of system file table entries.
const MAX_SFT: usize = 128;
/// How far nested `EXEC`s can go.
const MAX_DEPTH: usize = 16;
/// The largest executable the loader reads.
const MAX_IMAGE: usize = 0xA_0000;

/// DOS error codes.
mod error {
    pub const INVALID_FUNCTION: u16 = 1;
    pub const FILE_NOT_FOUND: u16 = 2;
    pub const PATH_NOT_FOUND: u16 = 3;
    pub const TOO_MANY_OPEN: u16 = 4;
    pub const ACCESS_DENIED: u16 = 5;
    pub const BAD_HANDLE: u16 = 6;
    pub const ARENA_TRASHED: u16 = 7;
    pub const NO_MEMORY: u16 = 8;
    pub const BAD_BLOCK: u16 = 9;
    pub const BAD_ENV: u16 = 10;
    pub const BAD_FORMAT: u16 = 11;
    pub const BAD_ACCESS_CODE: u16 = 12;
    pub const NO_MORE_FILES: u16 = 18;
    pub const FILE_EXISTS: u16 = 80;
}

/// What a system file table entry refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    /// A file on the RAM drive.
    File,
    Con,
    Nul,
    Aux,
    Prn,
    Clock,
    /// A driver name such as `EMMXXXX0`, which programs open just to probe.
    Driver,
}

/// A system file table entry: one open file, shared by duplicated handles.
#[derive(Clone)]
pub struct Sft {
    pub node: usize,
    pub offset: u32,
    pub mode: u8,
    pub device: Device,
    pub refs: u16,
}

/// State of `INT 21h` function 0Ah and of cooked reads from `CON`.
pub struct LineInput {
    pub text: Vec<u8>,
    pub active: bool,
    /// The bytes of a finished line a `CON` read hasn't taken yet.
    pub pending: Vec<u8>,
}

/// What the machine should start after a load.
#[derive(Clone, Copy, Debug)]
pub struct LoadedProgram {
    pub psp: u16,
    pub cs: u16,
    pub ip: u16,
    pub ss: u16,
    pub sp: u16,
}

/// The saved state of the parent while a child runs.
#[derive(Clone, Copy)]
struct Process {
    psp: u16,
    dta: u32,
    /// The parent's registers at `EXEC`, which DOS 3 and up preserve for it.
    regs: Option<([u32; 8], u16, u16)>,
}

/// The DOS kernel state.
pub struct Dos {
    pub disk: RamDisk,
    pub cwd: usize,
    pub sft: Vec<Option<Sft>>,
    pub dta: u32,
    pub psp: u16,
    /// `AH=4Dh`: the termination type and code of the last child.
    pub last_return: u16,
    /// A process has ended since this was last cleared.
    pub process_ended: bool,
    /// The last program loaded, if it brings an extender of its own.
    pub self_hosting: Option<u16>,
    pub last_error: u16,
    /// `AH=58h`: the allocation strategy.
    pub strategy: u8,
    stack: Vec<Process>,
    pub line: LineInput,
    /// The scan code half of an extended key, for the next console read.
    pending_scan: Option<u8>,
    /// `AH=0Ch` already flushed the keyboard for this call.
    flushed: bool,
    pub verify: bool,
    pub break_flag: bool,
    /// The work the last call did, in guest instruction units.
    pub cost: u32,
    /// How many times a program left a block behind with `AH=31h`.
    pub tsr_count: u32,
    /// The board's read-only filesystem, used by the `MOUNT` command.
    pub fs: Option<&'static dyn feos_fs::FileSystem>,
}

impl Default for Dos {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes a byte to guest memory the way the CPU would, through the VGA pipeline
/// and never over the ROM.
pub fn guest_write(mem: &mut Memory, vga: &mut Vga, addr: u32, value: u8) {
    if vga.is_vram(addr) {
        vga.vram_write(addr, value);
    } else if let Some(mapped) = mem.ems_address(addr) {
        if let Some(mapped) = mapped {
            mem.write8(mapped, value);
        }
    } else if !(0xC_0000..0x10_0000).contains(&addr) {
        mem.write8(addr, value);
        if (0xA_0000..0xC_0000).contains(&addr) {
            vga.mark_dirty();
        }
    }
}

/// Reads a byte of guest memory the way the CPU would.
pub fn guest_read(mem: &Memory, vga: &mut Vga, addr: u32) -> u8 {
    if vga.is_vram(addr) {
        vga.vram_read(addr)
    } else if let Some(mapped) = mem.ems_address(addr) {
        mapped.map_or(0xFF, |mapped| mem.read8(mapped))
    } else {
        mem.read8(addr)
    }
}

fn linear(segment: u16, offset: u16) -> u32 {
    ((segment as u32) << 4).wrapping_add(offset as u32)
}

fn fail(regs: &mut CpuRegs<'_>, code: u16) {
    regs.set_carry(true);
    regs.set_ax(code);
}

fn ok(regs: &mut CpuRegs<'_>, ax: u16) {
    regs.set_carry(false);
    regs.set_ax(ax);
}

/// The device a path names: any directory and extension, as DOS allows.
fn device_for(path: &str) -> Option<Device> {
    let path = strip_drive(path);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    match stem.as_str() {
        "CON" => Some(Device::Con),
        "NUL" => Some(Device::Nul),
        "AUX" | "COM1" | "COM2" | "COM3" | "COM4" => Some(Device::Aux),
        "PRN" | "LPT1" | "LPT2" | "LPT3" => Some(Device::Prn),
        "CLOCK$" => Some(Device::Clock),
        "EMMXXXX0" => Some(Device::Driver),
        _ => None,
    }
}

impl Dos {
    pub fn new() -> Self {
        let mut sft = Vec::new();
        for device in [Device::Con, Device::Aux, Device::Prn] {
            sft.push(Some(Sft {
                node: 0,
                offset: 0,
                mode: 2,
                device,
                refs: 1,
            }));
        }
        Self {
            disk: RamDisk::new(),
            cwd: 0,
            sft,
            dta: ((COMMAND_PSP as u32) << 4) + 0x80,
            psp: COMMAND_PSP,
            last_return: 0,
            last_error: 0,
            strategy: 0,
            stack: Vec::new(),
            process_ended: false,
            self_hosting: None,
            line: LineInput {
                text: Vec::new(),
                active: false,
                pending: Vec::new(),
            },
            pending_scan: None,
            flushed: false,
            verify: false,
            break_flag: false,
            cost: 0,
            tsr_count: 0,
            fs: None,
        }
    }

    /// Creates the standard files.
    pub fn seed(&mut self, fs: Option<&'static dyn feos_fs::FileSystem>) {
        self.disk
            .create_file(self.disk.root, "README.TXT", demo::README.to_vec());
        self.disk
            .create_file(self.disk.root, "AUTOEXEC.BAT", demo::AUTOEXEC.to_vec());
        self.disk
            .create_file(self.disk.root, "HELLO.COM", demo::HELLO_COM.to_vec());
        self.disk
            .create_file(self.disk.root, "BOX.COM", demo::BOX_COM.to_vec());
        self.fs = fs;
        // The FEOS filesystem is mounted on demand with `MOUNT`, because a
        // game image can be much larger than the kernel's heap.
    }

    /// Builds the MCB chain, the root process and the DOS data segment.
    pub fn install(&mut self, mem: &mut Memory) {
        let data = (DOS_DATA as u32) << 4;
        mem.fill(data, 0x400, 0);
        // The list of lists: the word before it is the first MCB's segment.
        mem.write16(data + 0x24, FIRST_MCB);
        mem.write16(data + 0x26, 0xFFFF);
        mem.write16(data + 0x28, 0xFFFF);
        // The case map, a far return in the stub area of the ROM.
        for index in 0..128u16 {
            let ch = 0x80 + index;
            mem.write8(data + 0x102 + index as u32, ch as u8);
        }
        mem.write16(data + 0x100, 128);
        // An empty lead-byte table.
        mem.write16(data + 0x200, 0);

        let write_mcb = |mem: &mut Memory, seg: u16, sig: u8, owner: u16, paras: u16| {
            let at = (seg as u32) << 4;
            mem.fill(at, 16, 0);
            mem.write8(at, sig);
            mem.write16(at + 1, owner);
            mem.write16(at + 3, paras);
        };
        write_mcb(mem, FIRST_MCB, b'M', COMMAND_PSP, COMMAND_PARAS);
        mem.copy_from(((FIRST_MCB as u32) << 4) + 8, b"COMMAND\0");
        write_mcb(mem, COMMAND_ENV - 1, b'M', COMMAND_PSP, COMMAND_ENV_PARAS);
        let free = COMMAND_ENV + COMMAND_ENV_PARAS;
        write_mcb(mem, free, b'Z', 0, MEMORY_TOP - free - 1);

        self.write_root_env(mem, &[]);
        write_psp(mem, COMMAND_PSP, COMMAND_PSP, COMMAND_ENV, MEMORY_TOP, b"");
        let base = (COMMAND_PSP as u32) << 4;
        mem.fill(base + 0x18, HANDLE_COUNT as usize, 0xFF);
        mem.write8(base + 0x18, 0);
        mem.write8(base + 0x19, 0);
        mem.write8(base + 0x1A, 0);
        mem.write8(base + 0x1B, 1);
        mem.write8(base + 0x1C, 2);
        for slot in self.sft.iter_mut().flatten() {
            slot.refs = 1;
        }
        if let Some(Some(con)) = self.sft.get_mut(0) {
            con.refs = 3;
        }
        self.psp = COMMAND_PSP;
        self.dta = linear(COMMAND_PSP, 0x80);
        self.stack.clear();
    }

    /// Rewrites the root environment. `extra` adds or replaces variables.
    pub fn write_root_env(&mut self, mem: &mut Memory, vars: &[(String, String)]) {
        let at = (COMMAND_ENV as u32) << 4;
        mem.fill(at, COMMAND_ENV_PARAS as usize * 16, 0);
        let limit = COMMAND_ENV_PARAS as u32 * 16 - 4;
        let mut cursor = 0u32;
        let mut put = |mem: &mut Memory, text: &str| {
            if cursor + text.len() as u32 + 1 >= limit {
                return;
            }
            mem.copy_from(at + cursor, text.as_bytes());
            cursor += text.len() as u32 + 1;
        };
        if vars.is_empty() {
            put(mem, "COMSPEC=C:\\COMMAND.COM");
            put(mem, "PATH=C:\\");
            put(mem, "PROMPT=$P$G");
            put(mem, "BLASTER=A220 I7 D1 H5 T6");
        } else {
            for (key, value) in vars {
                let line = format!("{key}={value}");
                put(mem, &line);
            }
        }
    }

    /// Whether a process is the running one, or waiting for a child.
    pub fn process_alive(&self, psp: u16) -> bool {
        self.psp == psp || self.stack.iter().any(|process| process.psp == psp)
    }

    /// Whether the current process is the root, meaning no program is running.
    pub fn at_root(&self) -> bool {
        self.psp == COMMAND_PSP
    }

    // ------------------------------------------------------------------
    // Mounting
    // ------------------------------------------------------------------

    /// Imports a directory from the board's filesystem into `C:\<dest>`.
    pub fn mount(&mut self, source: &str, dest: &str) -> Result<usize, &'static str> {
        let Some(fs) = self.fs else {
            return Err("no FEOS filesystem on this board");
        };
        let Some(node) = fs.resolve(source) else {
            return Err("path not found");
        };
        if !fs.is_dir(node) {
            return Err("not a directory");
        }
        if self.disk.lookup(self.disk.root, dest).is_some() {
            return Err("drive C: already has that directory");
        }
        let Some(dest_node) = self.disk.create_dir(self.disk.root, dest) else {
            return Err("cannot create directory");
        };
        let stats = self.disk.import_fs(dest_node, fs, node, 0);
        if stats.skipped != 0 {
            self.disk.remove_tree(dest_node);
            return Err("not enough room on drive C: for this directory");
        }
        Ok(stats.copied)
    }

    /// Imports a directory or extracts a per-game ZIP from the board's
    /// filesystem into `C:\<dest>`.
    pub fn mount_any(&mut self, name: &str, dest: &str) -> Result<usize, &'static str> {
        if let Some(fs) = self.fs {
            if let Some(node) = fs.resolve(name) {
                if fs.is_dir(node) {
                    return self.mount(name, dest);
                }
            }
        }
        let mut candidates: Vec<String> = Vec::new();
        if let Some(alias) = zip_alias(name) {
            candidates.push(alias.to_string());
        }
        candidates.push(format!("{name}.zip"));
        candidates.push(format!("{}.zip", name.to_ascii_lowercase()));
        candidates.push(format!("{}.zip", name.to_ascii_uppercase()));
        for candidate in candidates {
            match self.mount_zip(&candidate, dest) {
                Ok(count) => return Ok(count),
                Err("archive not found") => continue,
                Err(error) => return Err(error),
            }
        }
        Err("archive not found")
    }

    /// Extracts a ZIP from the board's filesystem into `C:\<dest>`.
    pub fn mount_zip(&mut self, zip_path: &str, dest: &str) -> Result<usize, &'static str> {
        let Some(data) = self.read_fs_file(zip_path) else {
            return Err("archive not found");
        };
        self.mount_zip_data(&data, dest)
    }

    /// Extracts ZIP data into `C:\<dest>`. A failure removes what was extracted.
    pub fn mount_zip_data(&mut self, data: &[u8], dest: &str) -> Result<usize, &'static str> {
        if self.disk.lookup(self.disk.root, dest).is_some() {
            return Err("drive C: already has that directory");
        }
        let Some(dest_node) = self.disk.create_dir(self.disk.root, dest) else {
            return Err("cannot create directory");
        };
        match self.extract_zip(data, dest_node) {
            Ok(count) => Ok(count),
            Err(message) => {
                self.disk.remove_tree(dest_node);
                Err(message)
            }
        }
    }

    fn extract_zip(&mut self, data: &[u8], dest_node: usize) -> Result<usize, &'static str> {
        let mut reader = ZipReader::open(data).map_err(|_| "invalid zip archive")?;
        let mut count = 0;
        loop {
            let entry = match reader.next() {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => return Err("invalid zip archive"),
            };
            let name = entry.name().map_err(|_| "invalid zip entry name")?;
            let normalized = name.replace('\\', "/");
            let parts: Vec<&str> = normalized
                .split('/')
                .filter(|part| !part.is_empty() && *part != "." && *part != "..")
                .collect();
            if parts.is_empty() {
                continue;
            }
            let parent_parts = &parts[..parts.len() - 1];
            let Some(parent) = self.ensure_dirs(dest_node, parent_parts) else {
                return Err("cannot create directory");
            };
            if entry.is_dir() {
                self.ensure_dirs(parent, &[parts[parts.len() - 1]]);
                continue;
            }
            // The declared size comes from the archive, so it isn't trusted.
            let size = entry.uncompressed_size as usize;
            let mut buffer = Vec::new();
            if size > self.disk.free_bytes() || buffer.try_reserve_exact(size).is_err() {
                return Err("not enough room on drive C: for this archive");
            }
            buffer.resize(size, 0);
            entry
                .extract(&mut buffer)
                .map_err(|_| "cannot extract zip entry")?;
            if self
                .disk
                .create_file(parent, parts[parts.len() - 1], buffer)
                .is_some()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Removes a directory created by `MOUNT` and returns its space.
    pub fn unmount(&mut self, dest: &str) -> Result<(), &'static str> {
        let Some(node) = self.disk.lookup(self.disk.root, dest) else {
            return Err("no such directory on drive C:");
        };
        if !self.disk.is_dir(node) {
            return Err("not a directory");
        }
        if !self.disk.remove_tree(node) {
            return Err("a file in that directory is still open");
        }
        if self.disk.node(self.cwd).is_none() {
            self.cwd = self.disk.root;
        }
        Ok(())
    }

    fn ensure_dirs(&mut self, parent: usize, parts: &[&str]) -> Option<usize> {
        let mut current = parent;
        for part in parts {
            current = match self.disk.lookup(current, part) {
                Some(node) if self.disk.is_dir(node) => node,
                _ => self.disk.create_dir(current, part)?,
            };
        }
        Some(current)
    }

    /// Reads a file from the board's filesystem.
    pub fn read_fs_file(&self, path: &str) -> Option<Vec<u8>> {
        let fs = self.fs?;
        // The boards mount a linked disk image at `/exfat` or `/fat32`, so the game
        // library is there rather than at the root.
        let node = ["", "exfat/", "fat32/"].iter().find_map(|prefix| {
            let node = fs.resolve(&format!("{prefix}{path}"))?;
            fs.is_file(node).then_some(node)
        })?;
        let size = fs.size(node);
        let mut data = Vec::new();
        data.try_reserve_exact(size).ok()?;
        data.resize(size, 0);
        let mut offset = 0;
        while offset < size {
            let read = fs.read(node, offset, &mut data[offset..]);
            if read == 0 {
                break;
            }
            offset += read;
        }
        data.truncate(offset);
        Some(data)
    }

    /// Lists the top-level entries of the board's filesystem.
    pub fn fs_entries(&self) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let Some(fs) = self.fs else {
            return out;
        };
        for index in 0..fs.child_count(fs.root()) {
            let Some(child) = fs.child_at(fs.root(), index) else {
                continue;
            };
            let Some(name) = fs.name(child) else {
                continue;
            };
            out.push((name.to_string(), fs.is_dir(child)));
        }
        out.sort();
        out
    }

    // ------------------------------------------------------------------
    // Paths
    // ------------------------------------------------------------------

    pub fn resolve(&self, path: &str) -> Option<usize> {
        self.disk.resolve(self.cwd, path)
    }

    pub fn current_path(&self) -> String {
        self.disk.path(self.cwd)
    }

    /// Splits a path into its parent directory and final component.
    fn split_parent(&self, path: &str) -> Option<(usize, String)> {
        let path = strip_drive(path);
        let trimmed = path.trim_end_matches(['\\', '/']);
        if trimmed.is_empty() {
            return None;
        }
        let absolute = trimmed.starts_with('\\') || trimmed.starts_with('/');
        let (dir_part, name) = match trimmed.rsplit_once(['\\', '/']) {
            Some((dir, name)) => (dir, name),
            None => ("", trimmed),
        };
        if name.is_empty() {
            return None;
        }
        let dir = if dir_part.is_empty() {
            if absolute {
                self.disk.root
            } else {
                self.cwd
            }
        } else {
            self.disk.resolve(self.cwd, dir_part)?
        };
        Some((dir, name.to_string()))
    }

    fn read_path(&self, mem: &Memory, segment: u16, offset: u16) -> String {
        let bytes = mem.read_cstring(segment, offset, 128);
        let mut path = String::from_utf8_lossy(&bytes).into_owned();
        // Blanks at the end of a name are padding.
        path.truncate(path.trim_end().len());
        path
    }

    // ------------------------------------------------------------------
    // Memory control blocks
    // ------------------------------------------------------------------

    /// Walks the arena. Returns false if the chain is corrupt.
    fn mcb_walk(&self, mem: &Memory, mut visit: impl FnMut(u16, u8, u16, u16) -> bool) -> bool {
        let mut seg = FIRST_MCB;
        loop {
            let at = (seg as u32) << 4;
            let sig = mem.read8(at);
            if sig != b'M' && sig != b'Z' {
                return false;
            }
            let owner = mem.read16(at + 1);
            let paras = mem.read16(at + 3);
            if !visit(seg, sig, owner, paras) {
                return true;
            }
            if sig == b'Z' {
                return true;
            }
            let Some(next) = seg.checked_add(paras).and_then(|s| s.checked_add(1)) else {
                return false;
            };
            if next >= MEMORY_TOP {
                return false;
            }
            seg = next;
        }
    }

    /// Joins adjacent free blocks.
    fn mcb_merge(&self, mem: &mut Memory) {
        let mut seg = FIRST_MCB;
        for _ in 0..0x10000u32 {
            let at = (seg as u32) << 4;
            let sig = mem.read8(at);
            let paras = mem.read16(at + 3);
            if sig != b'M' {
                return;
            }
            let Some(next) = seg.checked_add(paras).and_then(|s| s.checked_add(1)) else {
                return;
            };
            if next >= MEMORY_TOP {
                return;
            }
            let next_at = (next as u32) << 4;
            let next_sig = mem.read8(next_at);
            if next_sig != b'M' && next_sig != b'Z' {
                return;
            }
            if mem.read16(at + 1) == 0 && mem.read16(next_at + 1) == 0 {
                let joined = paras
                    .saturating_add(mem.read16(next_at + 3))
                    .saturating_add(1);
                mem.write16(at + 3, joined);
                mem.write8(at, next_sig);
            } else {
                seg = next;
            }
        }
    }

    /// The largest free block, in paragraphs.
    pub fn largest_block(&self, mem: &mut Memory) -> u16 {
        self.mcb_largest(mem)
    }

    /// Finds the largest free block's size.
    fn mcb_largest(&self, mem: &mut Memory) -> u16 {
        self.mcb_merge(mem);
        let mut largest = 0;
        self.mcb_walk(mem, |_, _, owner, paras| {
            if owner == 0 && paras > largest {
                largest = paras;
            }
            true
        });
        largest
    }

    /// Allocates a block. On failure, returns the error and the largest size.
    pub fn mcb_alloc(
        &mut self,
        mem: &mut Memory,
        paras: u16,
        owner: u16,
    ) -> Result<u16, (u16, u16)> {
        self.mcb_merge(mem);
        let mut chosen: Option<(u16, u16)> = None;
        let mut largest = 0;
        let strategy = self.strategy & 3;
        let intact = self.mcb_walk(mem, |seg, _, block_owner, size| {
            if block_owner == 0 {
                largest = largest.max(size);
                if size >= paras {
                    let better = match (chosen, strategy) {
                        (None, _) => true,
                        (Some(_), 0) => false,
                        (Some((_, best)), 1) => size < best,
                        (Some(_), _) => true,
                    };
                    if better {
                        chosen = Some((seg, size));
                    }
                }
            }
            true
        });
        if !intact {
            return Err((error::ARENA_TRASHED, 0));
        }
        let Some((seg, size)) = chosen else {
            return Err((error::NO_MEMORY, largest));
        };
        let at = (seg as u32) << 4;
        if size == paras {
            mem.write16(at + 1, owner);
            return Ok(seg + 1);
        }
        let sig = mem.read8(at);
        if strategy >= 2 {
            // Last fit: the new block takes the end of the free one.
            let rest = size - paras - 1;
            let new_seg = seg + rest + 1;
            let new_at = (new_seg as u32) << 4;
            mem.fill(new_at, 16, 0);
            mem.write8(new_at, sig);
            mem.write16(new_at + 1, owner);
            mem.write16(new_at + 3, paras);
            mem.write8(at, b'M');
            mem.write16(at + 3, rest);
            Ok(new_seg + 1)
        } else {
            let rest_seg = seg + paras + 1;
            let rest_at = (rest_seg as u32) << 4;
            mem.fill(rest_at, 16, 0);
            mem.write8(rest_at, sig);
            mem.write16(rest_at + 1, 0);
            mem.write16(rest_at + 3, size - paras - 1);
            mem.write8(at, b'M');
            mem.write16(at + 1, owner);
            mem.write16(at + 3, paras);
            for index in 0..8 {
                mem.write8(at + 8 + index, 0);
            }
            Ok(seg + 1)
        }
    }

    fn mcb_of(&self, mem: &Memory, block: u16) -> Option<u32> {
        if block <= FIRST_MCB || block >= MEMORY_TOP {
            return None;
        }
        let at = ((block - 1) as u32) << 4;
        let sig = mem.read8(at);
        (sig == b'M' || sig == b'Z').then_some(at)
    }

    /// Frees a block.
    pub fn mcb_free(&mut self, mem: &mut Memory, block: u16) -> Result<(), u16> {
        let at = self.mcb_of(mem, block).ok_or(error::BAD_BLOCK)?;
        mem.write16(at + 1, 0);
        self.mcb_merge(mem);
        Ok(())
    }

    /// Resizes a block. On failure, returns the error and the largest it can be.
    pub fn mcb_resize(
        &mut self,
        mem: &mut Memory,
        block: u16,
        paras: u16,
    ) -> Result<(), (u16, u16)> {
        let at = self.mcb_of(mem, block).ok_or((error::BAD_BLOCK, 0))?;
        self.mcb_merge(mem);
        let size = mem.read16(at + 3);
        let sig = mem.read8(at);
        if paras > size {
            // Grow into a free neighbour.
            let next = block as u32 + size as u32;
            let next_at = next << 4;
            let mut room = size;
            let next_free = sig == b'M'
                && next < MEMORY_TOP as u32
                && mem.read16(next_at + 1) == 0
                && matches!(mem.read8(next_at), b'M' | b'Z');
            if next_free {
                room = size
                    .saturating_add(mem.read16(next_at + 3))
                    .saturating_add(1);
            }
            if paras > room {
                return Err((error::NO_MEMORY, room));
            }
            let next_sig = mem.read8(next_at);
            mem.write8(at, next_sig);
            mem.write16(at + 3, room);
            if paras == room {
                return Ok(());
            }
            let sig = next_sig;
            let rest = room - paras - 1;
            let rest_at = ((block as u32 + paras as u32) & 0xFFFF) << 4;
            mem.fill(rest_at, 16, 0);
            mem.write8(rest_at, sig);
            mem.write16(rest_at + 3, rest);
            mem.write8(at, b'M');
            mem.write16(at + 3, paras);
            return Ok(());
        }
        if paras == size {
            return Ok(());
        }
        let rest_at = ((block as u32 + paras as u32) & 0xFFFF) << 4;
        mem.fill(rest_at, 16, 0);
        mem.write8(rest_at, sig);
        mem.write16(rest_at + 3, size - paras - 1);
        mem.write8(at, b'M');
        mem.write16(at + 3, paras);
        self.mcb_merge(mem);
        Ok(())
    }

    fn free_all_owned(&mut self, mem: &mut Memory, psp: u16) {
        let mut seg = FIRST_MCB;
        for _ in 0..0x10000u32 {
            let at = (seg as u32) << 4;
            let sig = mem.read8(at);
            if sig != b'M' && sig != b'Z' {
                break;
            }
            if mem.read16(at + 1) == psp {
                mem.write16(at + 1, 0);
            }
            if sig == b'Z' {
                break;
            }
            let Some(next) = seg
                .checked_add(mem.read16(at + 3))
                .and_then(|s| s.checked_add(1))
            else {
                break;
            };
            if next >= MEMORY_TOP {
                break;
            }
            seg = next;
        }
        self.mcb_merge(mem);
    }

    // ------------------------------------------------------------------
    // Handles
    // ------------------------------------------------------------------

    fn jft(&self, mem: &Memory) -> (u32, u16) {
        let base = (self.psp as u32) << 4;
        let size = mem.read16(base + 0x32).min(255);
        let pointer = linear(mem.read16(base + 0x36), mem.read16(base + 0x34));
        (pointer, size)
    }

    fn sft_index(&self, mem: &Memory, handle: u16) -> Option<usize> {
        let (table, size) = self.jft(mem);
        if handle >= size {
            return None;
        }
        let index = mem.read8(table + handle as u32);
        if index == 0xFF {
            return None;
        }
        let index = index as usize;
        self.sft.get(index)?.as_ref().map(|_| index)
    }

    /// Opens a file into the SFT and takes a handle in the current JFT.
    fn open_handle(&mut self, mem: &mut Memory, entry: Sft) -> Result<u16, u16> {
        let (table, size) = self.jft(mem);
        let mut free = None;
        for handle in 0..size {
            if mem.read8(table + handle as u32) == 0xFF {
                free = Some(handle);
                break;
            }
        }
        let Some(handle) = free else {
            return Err(error::TOO_MANY_OPEN);
        };
        let slot = match self.sft.iter().position(Option::is_none) {
            Some(slot) => slot,
            None if self.sft.len() < MAX_SFT => {
                self.sft.push(None);
                self.sft.len() - 1
            }
            None => return Err(error::TOO_MANY_OPEN),
        };
        if entry.device == Device::File {
            self.disk.open(entry.node);
        }
        self.sft[slot] = Some(entry);
        mem.write8(table + handle as u32, slot as u8);
        Ok(handle)
    }

    fn release_sft(&mut self, index: usize) {
        let Some(Some(entry)) = self.sft.get_mut(index) else {
            return;
        };
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            let (device, node) = (entry.device, entry.node);
            self.sft[index] = None;
            if device == Device::File {
                self.disk.close(node);
            }
        }
    }

    /// Closes a handle in the current JFT.
    fn close_handle(&mut self, mem: &mut Memory, handle: u16) -> bool {
        let Some(index) = self.sft_index(mem, handle) else {
            return false;
        };
        let (table, _) = self.jft(mem);
        mem.write8(table + handle as u32, 0xFF);
        self.release_sft(index);
        true
    }

    fn close_all_handles(&mut self, mem: &mut Memory) {
        let (_, size) = self.jft(mem);
        for handle in 0..size {
            self.close_handle(mem, handle);
        }
    }

    /// Duplicates a JFT entry into `target`, closing what's there.
    fn dup_to(&mut self, mem: &mut Memory, source: u16, target: u16) -> bool {
        let Some(index) = self.sft_index(mem, source) else {
            return false;
        };
        let (table, size) = self.jft(mem);
        if target >= size {
            return false;
        }
        if source == target {
            return true;
        }
        self.close_handle(mem, target);
        if let Some(Some(entry)) = self.sft.get_mut(index) {
            entry.refs = entry.refs.saturating_add(1);
        }
        mem.write8(table + target as u32, index as u8);
        true
    }

    /// Redirects a standard handle of the root process, for the prompt's `>`,
    /// `>>` and `<`. Returns false if the file can't be opened.
    pub fn redirect(
        &mut self,
        mem: &mut Memory,
        handle: u16,
        path: &str,
        write: bool,
        append: bool,
    ) -> bool {
        let entry = if let Some(device) = device_for(path) {
            Sft {
                node: 0,
                offset: 0,
                mode: 2,
                device,
                refs: 1,
            }
        } else if write {
            let node = if append {
                self.resolve(path).filter(|&node| self.disk.is_file(node))
            } else {
                None
            };
            let node = match node {
                Some(node) => node,
                None => {
                    let Some((parent, name)) = self.split_parent(path) else {
                        return false;
                    };
                    let Some(node) = self.disk.create_file(parent, &name, Vec::new()) else {
                        return false;
                    };
                    node
                }
            };
            Sft {
                node,
                offset: if append {
                    self.disk.size(node) as u32
                } else {
                    0
                },
                mode: 1,
                device: Device::File,
                refs: 1,
            }
        } else {
            let Some(node) = self.resolve(path).filter(|&node| self.disk.is_file(node)) else {
                return false;
            };
            Sft {
                node,
                offset: 0,
                mode: 0,
                device: Device::File,
                refs: 1,
            }
        };
        let Ok(new_handle) = self.open_handle(mem, entry) else {
            return false;
        };
        let moved = self.dup_to(mem, new_handle, handle);
        self.close_handle(mem, new_handle);
        moved
    }

    /// Takes a copy of a standard handle's entry, to restore later.
    pub fn save_handle(&mut self, mem: &mut Memory, handle: u16) -> Option<u8> {
        let index = self.sft_index(mem, handle)?;
        if let Some(Some(entry)) = self.sft.get_mut(index) {
            entry.refs = entry.refs.saturating_add(1);
        }
        Some(index as u8)
    }

    /// Restores a handle saved with [`save_handle`](Self::save_handle).
    pub fn restore_handle(&mut self, mem: &mut Memory, handle: u16, saved: u8) {
        self.close_handle(mem, handle);
        let (table, size) = self.jft(mem);
        if handle < size {
            mem.write8(table + handle as u32, saved);
        }
    }

    // ------------------------------------------------------------------
    // Console
    // ------------------------------------------------------------------

    /// Writes to standard output, which a program's parent may have redirected.
    fn write_stdout(&mut self, mem: &mut Memory, vga: &mut Vga, bytes: &[u8]) {
        match self.sft_index(mem, 1).and_then(|index| {
            self.sft
                .get(index)
                .and_then(|slot| slot.as_ref())
                .map(|entry| (index, entry.device))
        }) {
            Some((index, Device::File)) => {
                self.write_sft(index, bytes);
            }
            Some((_, Device::Con)) | None => vga.write_str(mem, bytes),
            Some(_) => {}
        }
    }

    fn write_sft(&mut self, index: usize, bytes: &[u8]) -> usize {
        let Some(Some(entry)) = self.sft.get(index) else {
            return 0;
        };
        let (node, offset) = (entry.node, entry.offset as usize);
        let written = self.disk.write(node, offset, bytes);
        if let Some(Some(entry)) = self.sft.get_mut(index) {
            entry.offset = entry.offset.saturating_add(written as u32);
        }
        written
    }

    /// The next console character. An extended key gives a zero, then its scan
    /// code on the next call.
    fn console_char(&mut self, mem: &mut Memory) -> Option<u8> {
        if let Some(scan) = self.pending_scan.take() {
            return Some(scan);
        }
        let word = bda_pop(mem)?;
        let ascii = word as u8;
        if ascii == 0 || ascii == 0xE0 {
            self.pending_scan = Some((word >> 8) as u8);
            return Some(0);
        }
        Some(ascii)
    }

    fn console_ready(&self, mem: &Memory) -> bool {
        self.pending_scan.is_some() || bda_peek(mem).is_some()
    }

    /// Whether `INT 23h` still points at the default handler.
    fn default_break_handler(mem: &Memory) -> bool {
        mem.read16(0x23 * 4 + 2) == crate::bios::ROM_SEGMENT
    }

    // ------------------------------------------------------------------
    // Program loading
    // ------------------------------------------------------------------

    /// Loads a program as a child of the current process. The caller enters it.
    pub fn load_program(
        &mut self,
        mem: &mut Memory,
        path_name: &str,
        node: usize,
        tail: &[u8],
        env_segment: u16,
    ) -> Result<LoadedProgram, u16> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(error::NO_MEMORY);
        }
        let Some(file) = self.disk.node(node) else {
            return Err(error::FILE_NOT_FOUND);
        };
        if file.data.len() > MAX_IMAGE && (file.data.len() < 2 || &file.data[..2] != b"MZ") {
            return Err(error::BAD_FORMAT);
        }
        let name_upper = file.name.to_ascii_uppercase();
        // Taken out rather than cloned: a large file only exists once in the heap.
        let data = core::mem::take(&mut self.disk.nodes[node].data);
        let is_mz = data.len() >= 2 && (&data[0..2] == b"MZ" || &data[0..2] == b"ZM");
        let is_exe = is_mz || name_upper.ends_with(".EXE");
        let parent = self.psp;
        let full_path = self.full_path(node, path_name);
        let result = (|| {
            let env = self.build_env(mem, env_segment, &full_path)?;
            if is_exe {
                self.load_exe(mem, &data, tail, env, parent)
            } else {
                self.load_com(mem, &data, tail, env, parent)
            }
        })();
        let self_hosting = crate::dpmi::brings_its_own_extender(&data);
        self.disk.nodes[node].data = data;
        if let Err(code) = result {
            self.free_all_owned_tagged(mem);
            return Err(code);
        }
        if let Ok(loaded) = &result {
            self.self_hosting = self_hosting.then_some(loaded.psp);
        }
        result
    }

    /// Frees blocks a failed load tagged with the placeholder owner.
    fn free_all_owned_tagged(&mut self, mem: &mut Memory) {
        self.free_all_owned(mem, LOAD_OWNER);
    }

    fn full_path(&self, node: usize, fallback: &str) -> String {
        if self.disk.node(node).is_some() {
            format!("C:{}", self.disk.path(node))
        } else {
            fallback.to_string()
        }
    }

    /// Copies an environment, adding the count word and the program's path.
    fn build_env(&mut self, mem: &mut Memory, source: u16, path: &str) -> Result<u16, u16> {
        let source = if source != 0 {
            source
        } else {
            mem.read16(((self.psp as u32) << 4) + 0x2C)
        };
        let from = (source as u32) << 4;
        let mut length = 0u32;
        // Bounded: an environment is at most 32 KiB.
        loop {
            if length >= 0x7FFE {
                return Err(error::BAD_ENV);
            }
            if mem.read8(from + length) == 0 && mem.read8(from + length + 1) == 0 {
                length += 2;
                break;
            }
            length += 1;
        }
        let path = path.as_bytes();
        let total = length + 2 + path.len() as u32 + 1;
        let paras = total.div_ceil(16) as u16;
        let block = self
            .mcb_alloc(mem, paras, LOAD_OWNER)
            .map_err(|(code, _)| code)?;
        let to = (block as u32) << 4;
        for index in 0..length {
            mem.write8(to + index, mem.read8(from + index));
        }
        mem.write16(to + length, 1);
        mem.copy_from(to + length + 2, path);
        mem.write8(to + length + 2 + path.len() as u32, 0);
        Ok(block)
    }

    fn inherit_handles(&mut self, mem: &mut Memory, child: u16) {
        let (table, size) = self.jft(mem);
        let base = (child as u32) << 4;
        for handle in 0..HANDLE_COUNT {
            let mut value = 0xFF;
            if handle < size {
                let entry = mem.read8(table + handle as u32);
                if let Some(Some(sft)) = self.sft.get_mut(entry as usize) {
                    if sft.mode & 0x80 == 0 {
                        sft.refs = sft.refs.saturating_add(1);
                        value = entry;
                    }
                }
            }
            mem.write8(base + 0x18 + handle as u32, value);
        }
    }

    fn finish_load(
        &mut self,
        mem: &mut Memory,
        psp: u16,
        env: u16,
        parent: u16,
        block_paras: u16,
        tail: &[u8],
    ) {
        let top = psp.saturating_add(block_paras);
        write_psp(mem, psp, parent, env, top, tail);
        parse_fcb_tail(mem, psp, tail);
        self.inherit_handles(mem, psp);
        // Both blocks now belong to the new process.
        self.mcb_retag(mem, env, psp);
        self.mcb_retag(mem, psp, psp);
        self.stack.push(Process {
            psp: parent,
            dta: self.dta,
            regs: None,
        });
        self.psp = psp;
        self.dta = linear(psp, 0x80);
        // The MCB name shows the program, as `MEM /D` would.
    }

    fn mcb_retag(&self, mem: &mut Memory, block: u16, owner: u16) {
        if block > FIRST_MCB {
            mem.write16(((block - 1) as u32) << 4 | 1, owner);
        }
    }

    fn load_com(
        &mut self,
        mem: &mut Memory,
        data: &[u8],
        tail: &[u8],
        env: u16,
        parent: u16,
    ) -> Result<LoadedProgram, u16> {
        // A .COM program gets the largest free block.
        let largest = self.mcb_largest(mem);
        let needed = (data.len() as u32 + 0x100 + 2).div_ceil(16);
        if (largest as u32) < needed || data.len() > 0xFF00 {
            return Err(error::NO_MEMORY);
        }
        let psp = self
            .mcb_alloc(mem, largest, LOAD_OWNER)
            .map_err(|(code, _)| code)?;
        self.finish_load(mem, psp, env, parent, largest, tail);
        let base = (psp as u32) << 4;
        mem.copy_from(base + 0x100, data);
        let sp = if largest >= 0x1000 {
            0xFFFE
        } else {
            (largest as u32 * 16 - 2) as u16
        };
        mem.write16(base + sp as u32, 0);
        Ok(LoadedProgram {
            psp,
            cs: psp,
            ip: 0x100,
            ss: psp,
            sp,
        })
    }

    fn load_exe(
        &mut self,
        mem: &mut Memory,
        data: &[u8],
        tail: &[u8],
        env: u16,
        parent: u16,
    ) -> Result<LoadedProgram, u16> {
        if data.len() < 0x1C || !(&data[0..2] == b"MZ" || &data[0..2] == b"ZM") {
            return Err(error::BAD_FORMAT);
        }
        let word = |at: usize| -> usize {
            data.get(at..at + 2)
                .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
        };
        let last_page = word(2);
        let pages = word(4);
        let reloc_count = word(6);
        let header_paras = word(8);
        let min_alloc = word(0x0A);
        let max_alloc = word(0x0C);
        let ss = word(0x0E) as u16;
        let sp = word(0x10) as u16;
        let ip = word(0x14) as u16;
        let cs = word(0x16) as u16;
        let reloc_table = word(0x18);

        let image_offset = header_paras * 16;
        if image_offset > data.len() {
            return Err(error::BAD_FORMAT);
        }
        let mut module = if last_page == 0 {
            pages * 512
        } else {
            pages.saturating_sub(1) * 512 + last_page
        };
        module = module.saturating_sub(image_offset);
        module = module.min(data.len() - image_offset);
        // A file without page counts is just its whole image.
        let image_paras = module.div_ceil(16) as u32;
        let base_paras = image_paras + 0x10;
        let needed = base_paras + min_alloc as u32;
        let wanted = (base_paras + max_alloc as u32).min(0xFFFF);
        let largest = self.mcb_largest(mem) as u32;
        if needed > largest {
            return Err(error::NO_MEMORY);
        }
        let load_high = min_alloc == 0 && max_alloc == 0;
        let paras = if load_high {
            largest
        } else {
            wanted.max(needed).min(largest)
        } as u16;
        let psp = self
            .mcb_alloc(mem, paras, LOAD_OWNER)
            .map_err(|(code, _)| code)?;
        let load_seg = if load_high {
            psp.wrapping_add(paras).wrapping_sub(image_paras as u16)
        } else {
            psp.wrapping_add(0x10)
        };
        self.finish_load(mem, psp, env, parent, paras, tail);
        self.load_image(
            mem,
            data,
            image_offset,
            module,
            load_seg,
            load_seg,
            reloc_table,
            reloc_count,
        );
        let entry_ss = load_seg.wrapping_add(ss);
        let entry_sp = if sp == 0 && ss == 0 { 0xFFFE } else { sp };
        Ok(LoadedProgram {
            psp,
            cs: load_seg.wrapping_add(cs),
            ip,
            ss: entry_ss,
            sp: entry_sp,
        })
    }

    /// Copies an MZ image and applies its relocations.
    #[allow(clippy::too_many_arguments)]
    fn load_image(
        &self,
        mem: &mut Memory,
        data: &[u8],
        image_offset: usize,
        module: usize,
        load_seg: u16,
        reloc_seg: u16,
        reloc_table: usize,
        reloc_count: usize,
    ) {
        let load_at = (load_seg as u32) << 4;
        let room = (0xA_0000u32).saturating_sub(load_at) as usize;
        let module = module.min(room);
        mem.copy_from(load_at, &data[image_offset..image_offset + module]);
        for index in 0..reloc_count {
            let at = reloc_table + index * 4;
            let Some(bytes) = data.get(at..at + 4) else {
                break;
            };
            let offset = u16::from_le_bytes([bytes[0], bytes[1]]);
            let segment = u16::from_le_bytes([bytes[2], bytes[3]]);
            let address = linear(load_seg.wrapping_add(segment), offset);
            if address.saturating_add(1) >= 0xA_0000 {
                continue;
            }
            let value = mem.read16(address).wrapping_add(reloc_seg);
            mem.write16(address, value);
        }
    }

    /// `AH=4Bh`: load and execute, load only, or overlay.
    fn exec(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        let path = self.read_path(mem, regs.ds(), regs.dx());
        let params = linear(regs.es(), regs.bx());
        let mode = regs.al();
        if mode == 3 {
            self.exec_overlay(regs, mem, &path, params);
            return;
        }
        if mode != 0 && mode != 1 {
            fail(regs, error::INVALID_FUNCTION);
            return;
        }
        let env = mem.read16(params);
        let tail_seg = mem.read16(params + 4);
        let tail_off = mem.read16(params + 2);
        let tail_at = linear(tail_seg, tail_off);
        let mut tail = Vec::new();
        let tail_len = mem.read8(tail_at).min(126);
        for index in 0..tail_len as u32 {
            tail.push(mem.read8(tail_at + 1 + index));
        }

        let (path, tail) = match self.command_com_target(&path, &tail) {
            Some(target) => target,
            None => (path, tail),
        };
        let node = self
            .resolve(&path)
            .filter(|&node| self.disk.is_file(node))
            .or_else(|| {
                self.resolve(&format!("{path}.COM"))
                    .or_else(|| self.resolve(&format!("{path}.EXE")))
            });
        let Some(node) = node else {
            self.last_error = error::FILE_NOT_FOUND;
            fail(regs, error::FILE_NOT_FOUND);
            return;
        };
        let parent = self.psp;
        // The parent's stack is where terminate returns through.
        let parent_base = (parent as u32) << 4;
        let loaded = match self.load_program(mem, &path, node, &tail, env) {
            Ok(loaded) => loaded,
            Err(code) => {
                self.last_error = code;
                fail(regs, code);
                return;
            }
        };
        mem.write16(parent_base + 0x2E, regs.sp());
        mem.write16(parent_base + 0x30, regs.ss());
        if let Some(process) = self.stack.last_mut() {
            process.regs = Some((*regs.r, regs.ds(), regs.es()));
        }
        self.cost = self.cost.saturating_add(2_000);
        // The child's terminate address is where this INT 21h returns to.
        let frame = linear(regs.ss(), regs.sp());
        let child_base = (loaded.psp as u32) << 4;
        mem.write16(child_base + 0x0A, mem.read16(frame));
        mem.write16(child_base + 0x0C, mem.read16(frame + 2));
        mem.write16(0x22 * 4, mem.read16(frame));
        mem.write16(0x22 * 4 + 2, mem.read16(frame + 2));

        if mode == 1 {
            // Load only: the parent gets the entry point and starts it itself.
            mem.write16(params + 0x0E, loaded.sp.wrapping_sub(2));
            mem.write16(params + 0x10, loaded.ss);
            mem.write16(params + 0x12, loaded.ip);
            mem.write16(params + 0x14, loaded.cs);
            mem.write16(linear(loaded.ss, loaded.sp.wrapping_sub(2)), 0);
            ok(regs, 0);
            return;
        }

        // Enter the child through the IRET of this INT 21h's stub.
        let sp = loaded.sp.wrapping_sub(6);
        let at = linear(loaded.ss, sp);
        mem.write16(at, loaded.ip);
        mem.write16(at + 2, loaded.cs);
        mem.write16(at + 4, 0x0202);
        regs.set_ss(loaded.ss);
        regs.set_sp(sp);
        regs.set_ds(loaded.psp);
        regs.set_es(loaded.psp);
        regs.set_ax(0);
        regs.set_bx(0);
        regs.set_cx(0x00FF);
        regs.set_dx(loaded.psp);
        regs.set_si(loaded.ip);
        regs.set_di(loaded.sp);
        regs.set_bp(0x091C);
        regs.set_carry(false);
    }

    /// `COMMAND.COM /C program args` runs the program directly, since there's no
    /// guest shell to run. The batch language lives in the prompt instead.
    fn command_com_target(&self, path: &str, tail: &[u8]) -> Option<(String, Vec<u8>)> {
        let upper = path.to_ascii_uppercase();
        if !upper.ends_with("COMMAND.COM") && !upper.ends_with("COMMAND") {
            return None;
        }
        let text = String::from_utf8_lossy(tail).into_owned();
        let trimmed = text.trim_start();
        let rest = trimmed
            .strip_prefix("/C")
            .or_else(|| trimmed.strip_prefix("/c"))?
            .trim_start();
        let (program, args) = match rest.split_once(' ') {
            Some((program, args)) => (program, args),
            None => (rest, ""),
        };
        let mut new_tail: Vec<u8> = Vec::new();
        if !args.is_empty() {
            new_tail.push(b' ');
            new_tail.extend_from_slice(args.as_bytes());
        }
        Some((program.to_string(), new_tail))
    }

    fn exec_overlay(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, path: &str, params: u32) {
        let Some(node) = self.resolve(path).filter(|&node| self.disk.is_file(node)) else {
            fail(regs, error::FILE_NOT_FOUND);
            return;
        };
        let load_seg = mem.read16(params);
        let reloc = mem.read16(params + 2);
        let data = core::mem::take(&mut self.disk.nodes[node].data);
        if data.len() >= 0x1C && (&data[0..2] == b"MZ" || &data[0..2] == b"ZM") {
            let word = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
            let image_offset = word(8) * 16;
            let (last_page, pages) = (word(2), word(4));
            let mut module = if last_page == 0 {
                pages * 512
            } else {
                pages.saturating_sub(1) * 512 + last_page
            };
            module = module.saturating_sub(image_offset);
            if image_offset <= data.len() {
                module = module.min(data.len() - image_offset);
                self.load_image(
                    mem,
                    &data,
                    image_offset,
                    module,
                    load_seg,
                    reloc,
                    word(0x18),
                    word(6),
                );
            }
        } else {
            let at = (load_seg as u32) << 4;
            let room = 0xA_0000u32.saturating_sub(at) as usize;
            mem.copy_from(at, &data[..data.len().min(room)]);
        }
        self.disk.nodes[node].data = data;
        self.cost = self.cost.saturating_add(2_000);
        ok(regs, 0);
    }

    /// Terminates the current process. Returns through the parent's saved frame,
    /// or halts the CPU when the parent is the prompt.
    pub fn terminate(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        code: u8,
        kind: u8,
        keep_paras: Option<u16>,
        exit_requested: &mut bool,
        exit_code: &mut u8,
    ) -> InterruptAction {
        let psp = self.psp;
        self.process_ended = true;
        self.last_return = ((kind as u16) << 8) | code as u16;
        let base = (psp as u32) << 4;
        let parent = mem.read16(base + 0x16);
        let terminate_ip = mem.read16(base + 0x0A);
        let terminate_cs = mem.read16(base + 0x0C);

        if let Some(paras) = keep_paras {
            let _ = self.mcb_resize(mem, psp, paras.max(6));
            self.tsr_count = self.tsr_count.saturating_add(1);
        } else {
            self.close_all_handles(mem);
            self.free_all_owned(mem, psp);
        }
        // Restore the handlers the process may have changed.
        for (vector, at) in [(0x22u32, 0x0Au32), (0x23, 0x0E), (0x24, 0x12)] {
            mem.write16(vector * 4, mem.read16(base + at));
            mem.write16(vector * 4 + 2, mem.read16(base + at + 2));
        }
        self.pending_scan = None;
        self.line.active = false;
        let saved = self.stack.pop();
        if let Some(process) = saved {
            self.psp = process.psp;
            self.dta = process.dta;
        } else {
            self.psp = COMMAND_PSP;
            self.dta = linear(COMMAND_PSP, 0x80);
        }
        if saved.is_none() || parent == COMMAND_PSP || self.psp == COMMAND_PSP {
            self.psp = COMMAND_PSP;
            self.stack.clear();
            *exit_requested = true;
            *exit_code = code;
            return InterruptAction::Halt;
        }
        let parent_base = (self.psp as u32) << 4;
        let sp = mem.read16(parent_base + 0x2E);
        let ss = mem.read16(parent_base + 0x30);
        regs.set_ss(ss);
        regs.set_sp(sp);
        let frame = linear(ss, sp);
        mem.write16(frame, terminate_ip);
        mem.write16(frame + 2, terminate_cs);
        if let Some((saved_regs, ds, es)) = saved.and_then(|process| process.regs) {
            *regs.r = saved_regs;
            regs.set_ds(ds);
            regs.set_es(es);
        } else {
            regs.set_ds(self.psp);
            regs.set_es(self.psp);
        }
        regs.set_sp(sp);
        regs.set_ax(0);
        regs.set_carry(false);
        InterruptAction::Continue
    }

    /// Forces the current program and all its parents back to the prompt: the
    /// machine calls this when a program is killed or faults.
    pub fn unwind_to_root(&mut self, mem: &mut Memory) {
        for _ in 0..=MAX_DEPTH {
            if self.at_root() {
                break;
            }
            let psp = self.psp;
            self.close_all_handles(mem);
            self.free_all_owned(mem, psp);
            let Some(process) = self.stack.pop() else {
                break;
            };
            self.psp = process.psp;
            self.dta = process.dta;
        }
        self.psp = COMMAND_PSP;
        self.dta = linear(COMMAND_PSP, 0x80);
        self.stack.clear();
    }

    // ------------------------------------------------------------------
    // Directory search
    // ------------------------------------------------------------------

    /// Fills the found-file part of the DTA.
    fn fill_found(
        &self,
        mem: &mut Memory,
        vga: &mut Vga,
        name: &str,
        attr: u8,
        time: u16,
        date: u16,
        size: u32,
    ) {
        let dta = self.dta;
        guest_write(mem, vga, dta + 0x15, attr);
        guest_write(mem, vga, dta + 0x16, time as u8);
        guest_write(mem, vga, dta + 0x17, (time >> 8) as u8);
        guest_write(mem, vga, dta + 0x18, date as u8);
        guest_write(mem, vga, dta + 0x19, (date >> 8) as u8);
        for (index, byte) in size.to_le_bytes().iter().enumerate() {
            guest_write(mem, vga, dta + 0x1A + index as u32, *byte);
        }
        for index in 0..13u32 {
            guest_write(mem, vga, dta + 0x1E + index, 0);
        }
        for (index, byte) in name.as_bytes().iter().take(12).enumerate() {
            guest_write(mem, vga, dta + 0x1E + index as u32, *byte);
        }
    }

    /// Finds the entry at or after `index` that matches, and describes it. The
    /// search state lives in the DTA so nested searches don't disturb each other.
    #[allow(clippy::type_complexity)]
    fn find_from(
        &self,
        dir: usize,
        pattern: &[u8; 11],
        attr: u8,
        mut index: usize,
    ) -> Option<(usize, String, u8, u16, u16, u32)> {
        let is_root = dir == self.disk.root;
        let dots = if is_root { 0 } else { 2 };
        loop {
            if index < dots {
                if attr & 0x10 != 0 {
                    let mut name = *b"           ";
                    name[0] = b'.';
                    if index == 1 {
                        name[1] = b'.';
                    }
                    let text = if index == 0 { "." } else { ".." };
                    if fcb_match(&name, pattern) {
                        return Some((index, text.to_string(), 0x10, 0, 0x5C21, 0));
                    }
                }
                index += 1;
                continue;
            }
            let children = self.disk.children(dir);
            let &child = children.get(index - dots)?;
            let entry = &self.disk.nodes[child];
            let hidden_ok = entry.attr & 0x06 == 0 || entry.attr & attr & 0x06 != 0;
            let dir_ok = !entry.is_dir || attr & 0x10 != 0;
            if hidden_ok && dir_ok && fcb_match(&fcb_name(&entry.short), pattern) {
                return Some((
                    index,
                    entry.short.clone(),
                    entry.attr,
                    entry.time,
                    entry.date,
                    entry.data.len() as u32,
                ));
            }
            index += 1;
        }
    }

    fn find_first(&mut self, mem: &mut Memory, vga: &mut Vga, path: &str, attr: u8) -> u16 {
        let stripped = strip_drive(path);
        let absolute = stripped.starts_with('\\') || stripped.starts_with('/');
        let (dir_part, pattern) = match stripped.rsplit_once(['\\', '/']) {
            Some((dir, pattern)) => (dir, pattern),
            None => ("", stripped),
        };
        let dir = if dir_part.is_empty() {
            if absolute {
                Some(self.disk.root)
            } else {
                Some(self.cwd)
            }
        } else {
            self.disk.resolve(self.cwd, dir_part)
        };
        let Some(dir) = dir.filter(|&dir| self.disk.is_dir(dir)) else {
            return error::PATH_NOT_FOUND;
        };
        let pattern = crate::ramdisk::fcb_pattern(if pattern.is_empty() { "*.*" } else { pattern });
        let dta = self.dta;
        if attr == 0x08 {
            self.write_search_state(mem, vga, dta, dir, &pattern, attr, usize::MAX >> 1);
            self.fill_found(mem, vga, "FEOS-DOS", 0x08, 0, 0x5C21, 0);
            return 0;
        }
        let Some(found) = self.find_from(dir, &pattern, attr, 0) else {
            return error::NO_MORE_FILES;
        };
        self.write_search_state(mem, vga, dta, dir, &pattern, attr, found.0 + 1);
        self.fill_found(mem, vga, &found.1, found.2, found.3, found.4, found.5);
        0
    }

    #[allow(clippy::too_many_arguments)]
    fn write_search_state(
        &self,
        mem: &mut Memory,
        vga: &mut Vga,
        dta: u32,
        dir: usize,
        pattern: &[u8; 11],
        attr: u8,
        next: usize,
    ) {
        guest_write(mem, vga, dta, 3);
        for (index, byte) in pattern.iter().enumerate() {
            guest_write(mem, vga, dta + 1 + index as u32, *byte);
        }
        guest_write(mem, vga, dta + 0x0C, attr);
        for (index, byte) in (next.min(0xFFFF_FFFF) as u32)
            .to_le_bytes()
            .iter()
            .enumerate()
        {
            guest_write(mem, vga, dta + 0x0D + index as u32, *byte);
        }
        for (index, byte) in (dir as u32).to_le_bytes().iter().enumerate() {
            guest_write(mem, vga, dta + 0x11 + index as u32, *byte);
        }
    }

    fn find_next(&mut self, mem: &mut Memory, vga: &mut Vga) -> u16 {
        let dta = self.dta;
        let mut pattern = [0u8; 11];
        for (index, slot) in pattern.iter_mut().enumerate() {
            *slot = guest_read(mem, vga, dta + 1 + index as u32);
        }
        let attr = guest_read(mem, vga, dta + 0x0C);
        let read32 = |mem: &Memory, vga: &mut Vga, at: u32| {
            u32::from_le_bytes([
                guest_read(mem, vga, at),
                guest_read(mem, vga, at + 1),
                guest_read(mem, vga, at + 2),
                guest_read(mem, vga, at + 3),
            ])
        };
        let next = read32(mem, vga, dta + 0x0D) as usize;
        let dir = read32(mem, vga, dta + 0x11) as usize;
        if !self.disk.is_dir(dir) || next > self.disk.nodes.len() + 2 {
            return error::NO_MORE_FILES;
        }
        let Some(found) = self.find_from(dir, &pattern, attr, next) else {
            return error::NO_MORE_FILES;
        };
        self.write_search_state(mem, vga, dta, dir, &pattern, attr, found.0 + 1);
        self.fill_found(mem, vga, &found.1, found.2, found.3, found.4, found.5);
        0
    }

    // ------------------------------------------------------------------
    // INT 21h
    // ------------------------------------------------------------------

    fn set_last_error(&mut self, regs: &mut CpuRegs<'_>, code: u16) {
        self.last_error = code;
        fail(regs, code);
    }

    /// Opens a file for functions `3Dh` and `6Ch`.
    fn open_path(&mut self, mem: &mut Memory, path: &str, mode: u8) -> Result<u16, u16> {
        if mode & 0x07 > 2 {
            return Err(error::BAD_ACCESS_CODE);
        }
        if let Some(device) = device_for(path) {
            return self.open_handle(
                mem,
                Sft {
                    node: 0,
                    offset: 0,
                    mode,
                    device,
                    refs: 1,
                },
            );
        }
        let Some(node) = self.resolve(path) else {
            return Err(self.missing_error(path));
        };
        if !self.disk.is_file(node) {
            return Err(error::ACCESS_DENIED);
        }
        let attr = self.disk.node(node).map_or(0, |entry| entry.attr);
        if attr & 1 != 0 && mode & 7 != 0 {
            return Err(error::ACCESS_DENIED);
        }
        self.open_handle(
            mem,
            Sft {
                node,
                offset: 0,
                mode,
                device: Device::File,
                refs: 1,
            },
        )
    }

    /// File-not-found if the directory exists, path-not-found otherwise.
    fn missing_error(&self, path: &str) -> u16 {
        if self.split_parent(path).is_some() {
            error::FILE_NOT_FOUND
        } else {
            error::PATH_NOT_FOUND
        }
    }

    /// Creates or truncates a file and opens it.
    fn create_path(
        &mut self,
        mem: &mut Memory,
        path: &str,
        attr: u8,
        mode: u8,
    ) -> Result<u16, u16> {
        if let Some(device) = device_for(path) {
            return self.open_handle(
                mem,
                Sft {
                    node: 0,
                    offset: 0,
                    mode,
                    device,
                    refs: 1,
                },
            );
        }
        let Some((parent, name)) = self.split_parent(path) else {
            return Err(error::PATH_NOT_FOUND);
        };
        if let Some(existing) = self.disk.lookup(parent, &name) {
            let entry_attr = self.disk.node(existing).map_or(0, |entry| entry.attr);
            if !self.disk.is_file(existing) || entry_attr & 1 != 0 {
                return Err(error::ACCESS_DENIED);
            }
        }
        let Some(node) = self.disk.create_file(parent, &name, Vec::new()) else {
            return Err(error::ACCESS_DENIED);
        };
        if let Some(entry) = self.disk.node_mut(node) {
            entry.attr = (attr & 0x27) | 0x20;
        }
        self.open_handle(
            mem,
            Sft {
                node,
                offset: 0,
                mode,
                device: Device::File,
                refs: 1,
            },
        )
    }

    /// `INT 20h`, `INT 27h` and the default break handler end up here.
    pub fn terminate_kind(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        code: u8,
        kind: u8,
        keep: Option<u16>,
        exit_requested: &mut bool,
        exit_code: &mut u8,
    ) -> InterruptAction {
        self.terminate(regs, mem, code, kind, keep, exit_requested, exit_code)
    }

    /// `INT 21h`.
    #[allow(clippy::too_many_arguments)]
    pub fn int21(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
        now_ms: u64,
        exit_requested: &mut bool,
        exit_code: &mut u8,
    ) -> InterruptAction {
        self.cost = self.cost.saturating_add(20);
        match regs.ah() {
            0x00 => {
                return self.terminate(regs, mem, 0, 0, None, exit_requested, exit_code);
            }
            0x01 => {
                let Some(ch) = self.console_char(mem) else {
                    return InterruptAction::Blocked;
                };
                if ch == 3 && Self::default_break_handler(mem) {
                    vga.write_str(mem, b"^C\r\n");
                    return self.terminate(regs, mem, 3, 1, None, exit_requested, exit_code);
                }
                if ch != 0 {
                    self.write_stdout(mem, vga, &[ch]);
                }
                regs.set_al(ch);
            }
            0x02 => {
                let ch = regs.dl();
                self.write_stdout(mem, vga, &[ch]);
                regs.set_al(ch);
            }
            0x03 | 0x04 | 0x05 => regs.set_al(0),
            0x06 => {
                if regs.dl() == 0xFF {
                    if let Some(ch) = self.console_char(mem) {
                        regs.set_al(ch);
                        regs.set_zero(false);
                    } else {
                        regs.set_al(0);
                        regs.set_zero(true);
                    }
                } else {
                    let ch = regs.dl();
                    self.write_stdout(mem, vga, &[ch]);
                    regs.set_al(ch);
                }
            }
            0x07 | 0x08 => {
                let Some(ch) = self.console_char(mem) else {
                    return InterruptAction::Blocked;
                };
                if ch == 3 && regs.ah() == 0x08 && Self::default_break_handler(mem) {
                    vga.write_str(mem, b"^C\r\n");
                    return self.terminate(regs, mem, 3, 1, None, exit_requested, exit_code);
                }
                regs.set_al(ch);
            }
            0x09 => {
                let text = mem.read_dollar_string(regs.ds(), regs.dx(), 4096);
                self.cost = self.cost.saturating_add(text.len() as u32 / 2);
                self.write_stdout(mem, vga, &text);
                regs.set_al(b'$');
            }
            0x0A => return self.buffered_input(regs, mem, vga),
            0x0B => regs.set_al(if self.console_ready(mem) { 0xFF } else { 0 }),
            0x0C => {
                // The flush happens once, however often the read retries.
                if !self.flushed {
                    bda_flush(mem);
                    self.pending_scan = None;
                    self.flushed = true;
                }
                match regs.al() {
                    0x01 | 0x07 | 0x08 => {
                        let Some(ch) = self.console_char(mem) else {
                            return InterruptAction::Blocked;
                        };
                        if regs.al() == 0x01 && ch != 0 {
                            self.write_stdout(mem, vga, &[ch]);
                        }
                        regs.set_al(ch);
                    }
                    0x06 => {
                        if regs.dl() == 0xFF {
                            if let Some(ch) = self.console_char(mem) {
                                regs.set_al(ch);
                                regs.set_zero(false);
                            } else {
                                regs.set_al(0);
                                regs.set_zero(true);
                            }
                        }
                    }
                    0x0A => {
                        self.flushed = true;
                        let action = self.buffered_input(regs, mem, vga);
                        if action == InterruptAction::Blocked {
                            return action;
                        }
                    }
                    _ => regs.set_al(0),
                }
                self.flushed = false;
            }
            0x0D => {}
            0x0E => regs.set_al(3),
            0x0F..=0x17 | 0x21..=0x24 | 0x27..=0x29 => {
                return self.fcb_call(regs, mem, vga);
            }
            0x19 => regs.set_al(2),
            0x1A => self.dta = linear(regs.ds(), regs.dx()),
            0x1B | 0x1C => {
                if regs.ah() == 0x1C && regs.dl() != 0 && regs.dl() != 3 {
                    regs.set_al(0xFF);
                } else {
                    regs.set_al(64);
                    regs.set_cx(512);
                    regs.set_dx(0xFFFF);
                    regs.set_ds(DOS_DATA);
                    regs.set_bx(0x03F0);
                    mem.write8(((DOS_DATA as u32) << 4) + 0x3F0, 0xF8);
                }
            }
            0x1D | 0x1E | 0x20 => regs.set_al(0),
            0x1F | 0x32 => regs.set_al(0xFF),
            0x25 => {
                let vector = regs.al() as u32;
                mem.write16(vector * 4, regs.dx());
                mem.write16(vector * 4 + 2, regs.ds());
            }
            0x26 => {
                // Create a new PSP as a copy of the current one.
                let base = (self.psp as u32) << 4;
                let to = (regs.dx() as u32) << 4;
                for index in 0..0x100 {
                    mem.write8(to + index, mem.read8(base + index));
                }
                mem.write16(to + 0x16, self.psp);
            }
            0x2A => {
                let (century, year, month, day) = clock_date(now_ms);
                regs.set_cx(((century as u16) * 100) + year as u16);
                regs.set_dh(month);
                regs.set_dl(day);
                regs.set_al(day_of_week(now_ms));
            }
            0x2B => regs.set_al(0),
            0x2C => {
                let (hour, minute, second) = clock_time(now_ms);
                regs.set_ch(hour);
                regs.set_cl(minute);
                regs.set_dh(second);
                regs.set_dl(((now_ms % 1000) / 10) as u8);
            }
            0x2D => regs.set_al(0),
            0x2E => self.verify = regs.al() != 0,
            0x2F => {
                regs.set_es((self.dta >> 4) as u16);
                regs.set_bx((self.dta & 0x0F) as u16);
            }
            0x30 => {
                let oem = regs.al() == 0;
                regs.set_al(crate::DOS_VERSION);
                regs.set_ah(0);
                regs.set_bx(if oem { 0xFF00 } else { 0x0000 });
                regs.set_cx(0);
            }
            0x31 => {
                let code = regs.al();
                let keep = regs.dx();
                return self.terminate(regs, mem, code, 3, Some(keep), exit_requested, exit_code);
            }
            0x33 => match regs.al() {
                0x00 => regs.set_dl(u8::from(self.break_flag)),
                0x01 => self.break_flag = regs.dl() != 0,
                0x05 => regs.set_dl(3),
                0x06 => {
                    regs.set_bl(crate::DOS_VERSION);
                    regs.set_bh(0);
                    regs.set_dl(0);
                    regs.set_dh(0x10);
                }
                _ => regs.set_al(0xFF),
            },
            0x34 => {
                regs.set_es(DOS_DATA);
                regs.set_bx(0);
            }
            0x35 => {
                let vector = regs.al() as u32;
                regs.set_bx(mem.read16(vector * 4));
                regs.set_es(mem.read16(vector * 4 + 2));
            }
            0x36 => {
                if regs.dl() != 0 && regs.dl() != 3 {
                    regs.set_ax(0xFFFF);
                } else {
                    let cluster = 32 * 1024;
                    regs.set_ax(64);
                    regs.set_bx((self.disk.free_bytes() / cluster).min(0xFFFF) as u16);
                    regs.set_cx(512);
                    regs.set_dx((self.disk.quota / cluster).min(0xFFFF) as u16);
                }
            }
            0x37 => match regs.al() {
                0 => {
                    regs.set_al(0);
                    regs.set_dl(b'/');
                }
                1 => regs.set_al(0),
                _ => regs.set_al(0xFF),
            },
            0x38 => {
                if regs.al() == 0 || regs.al() == 1 {
                    let at = linear(regs.ds(), regs.dx());
                    write_country(mem, vga, at);
                    regs.set_bx(1);
                    regs.set_carry(false);
                } else {
                    self.set_last_error(regs, error::INVALID_FUNCTION);
                }
            }
            0x39 => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.split_parent(&path) {
                    Some((parent, name)) => {
                        if self.disk.create_dir(parent, &name).is_some() {
                            ok(regs, 0);
                        } else {
                            self.set_last_error(regs, error::ACCESS_DENIED);
                        }
                    }
                    None => self.set_last_error(regs, error::PATH_NOT_FOUND),
                }
            }
            0x3A => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.resolve(&path) {
                    Some(node)
                        if self.disk.is_dir(node)
                            && !self.disk.is_within(self.cwd, node)
                            && self.disk.remove(node) =>
                    {
                        ok(regs, 0);
                    }
                    Some(_) => self.set_last_error(regs, error::ACCESS_DENIED),
                    None => self.set_last_error(regs, error::PATH_NOT_FOUND),
                }
            }
            0x3B => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.resolve(&path) {
                    Some(node) if self.disk.is_dir(node) => {
                        self.cwd = node;
                        ok(regs, 0);
                    }
                    _ => self.set_last_error(regs, error::PATH_NOT_FOUND),
                }
            }
            0x3C => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                let attr = regs.cl();
                match self.create_path(mem, &path, attr, 2) {
                    Ok(handle) => ok(regs, handle),
                    Err(code) => self.set_last_error(regs, code),
                }
            }
            0x3D => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.open_path(mem, &path, regs.al()) {
                    Ok(handle) => ok(regs, handle),
                    Err(code) => self.set_last_error(regs, code),
                }
            }
            0x3E => {
                if self.close_handle(mem, regs.bx()) {
                    regs.set_carry(false);
                } else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                }
            }
            0x3F => return self.read_file(regs, mem, vga),
            0x40 => return self.write_file(regs, mem, vga),
            0x41 => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.resolve(&path) {
                    Some(node) if self.disk.is_file(node) => {
                        let attr = self.disk.node(node).map_or(0, |entry| entry.attr);
                        if attr & 1 != 0 || !self.disk.remove(node) {
                            self.set_last_error(regs, error::ACCESS_DENIED);
                        } else {
                            ok(regs, 0);
                        }
                    }
                    Some(_) => self.set_last_error(regs, error::ACCESS_DENIED),
                    None => {
                        let code = self.missing_error(&path);
                        self.set_last_error(regs, code);
                    }
                }
            }
            0x42 => {
                let Some(index) = self.sft_index(mem, regs.bx()) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return InterruptAction::Continue;
                };
                let offset = (((regs.cx() as u32) << 16) | regs.dx() as u32) as i32 as i64;
                let Some(Some(file)) = self.sft.get(index) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return InterruptAction::Continue;
                };
                let base = match regs.al() {
                    0 => 0,
                    1 => file.offset as i64,
                    2 => self.disk.size(file.node) as i64,
                    _ => {
                        self.set_last_error(regs, error::INVALID_FUNCTION);
                        return InterruptAction::Continue;
                    }
                };
                let position = (base + offset).clamp(0, u32::MAX as i64) as u32;
                if let Some(Some(file)) = self.sft.get_mut(index) {
                    file.offset = position;
                }
                regs.set_dx((position >> 16) as u16);
                ok(regs, position as u16);
            }
            0x43 => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                match self.resolve(&path) {
                    Some(node) => {
                        if regs.al() == 0 {
                            let attr = self.disk.node(node).map_or(0x20, |node| node.attr);
                            regs.set_cx(attr as u16);
                            ok(regs, attr as u16);
                        } else if regs.al() == 1 {
                            let keep_dir = self.disk.is_dir(node);
                            let attr = regs.cl() & 0x27;
                            if let Some(entry) = self.disk.node_mut(node) {
                                entry.attr = attr | if keep_dir { 0x10 } else { 0 };
                            }
                            ok(regs, 0);
                        } else {
                            self.set_last_error(regs, error::INVALID_FUNCTION);
                        }
                    }
                    None => {
                        let code = self.missing_error(&path);
                        self.set_last_error(regs, code);
                    }
                }
            }
            0x44 => self.ioctl(regs, mem),
            0x45 => {
                let Some(index) = self.sft_index(mem, regs.bx()) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return InterruptAction::Continue;
                };
                let (table, size) = self.jft(mem);
                let free = (0..size).find(|&handle| mem.read8(table + handle as u32) == 0xFF);
                match free {
                    Some(handle) => {
                        if let Some(Some(entry)) = self.sft.get_mut(index) {
                            entry.refs = entry.refs.saturating_add(1);
                        }
                        mem.write8(table + handle as u32, index as u8);
                        ok(regs, handle);
                    }
                    None => self.set_last_error(regs, error::TOO_MANY_OPEN),
                }
            }
            0x46 => {
                if self.dup_to(mem, regs.bx(), regs.cx()) {
                    regs.set_carry(false);
                } else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                }
            }
            0x47 => {
                if regs.dl() != 0 && regs.dl() != 3 {
                    self.set_last_error(regs, 15);
                } else {
                    let path = self.current_path();
                    let path = path.trim_start_matches('\\');
                    // A guest buffer holds 64 bytes.
                    let bytes = &path.as_bytes()[..path.len().min(63)];
                    let at = linear(regs.ds(), regs.si());
                    for (index, byte) in bytes.iter().enumerate() {
                        guest_write(mem, vga, at + index as u32, *byte);
                    }
                    guest_write(mem, vga, at + bytes.len() as u32, 0);
                    ok(regs, 0x0100);
                }
            }
            0x48 => match self.mcb_alloc(mem, regs.bx(), self.psp) {
                Ok(segment) => ok(regs, segment),
                Err((code, largest)) => {
                    regs.set_bx(largest);
                    self.set_last_error(regs, code);
                }
            },
            0x49 => match self.mcb_free(mem, regs.es()) {
                Ok(()) => regs.set_carry(false),
                Err(code) => self.set_last_error(regs, code),
            },
            0x4A => match self.mcb_resize(mem, regs.es(), regs.bx()) {
                Ok(()) => regs.set_carry(false),
                Err((code, largest)) => {
                    regs.set_bx(largest);
                    self.set_last_error(regs, code);
                }
            },
            0x4B => self.exec(regs, mem),
            0x4C => {
                let code = regs.al();
                return self.terminate(regs, mem, code, 0, None, exit_requested, exit_code);
            }
            0x4D => {
                let value = core::mem::take(&mut self.last_return);
                ok(regs, value);
            }
            0x4E => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                let code = self.find_first(mem, vga, &path, regs.cl());
                if code == 0 {
                    ok(regs, 0);
                } else {
                    self.set_last_error(regs, code);
                }
            }
            0x4F => {
                let code = self.find_next(mem, vga);
                if code == 0 {
                    ok(regs, 0);
                } else {
                    self.set_last_error(regs, code);
                }
            }
            0x50 => self.psp = regs.bx(),
            0x51 | 0x62 => regs.set_bx(self.psp),
            0x52 => {
                regs.set_es(DOS_DATA);
                regs.set_bx(0x26);
            }
            0x54 => regs.set_al(u8::from(self.verify)),
            0x55 => {
                let base = (self.psp as u32) << 4;
                let to = (regs.dx() as u32) << 4;
                for index in 0..0x100 {
                    mem.write8(to + index, mem.read8(base + index));
                }
                mem.write16(to + 0x16, self.psp);
                self.psp = regs.dx();
            }
            0x56 => {
                let old = self.read_path(mem, regs.ds(), regs.dx());
                let new = self.read_path(mem, regs.es(), regs.di());
                let Some(node) = self.resolve(&old) else {
                    let code = self.missing_error(&old);
                    self.set_last_error(regs, code);
                    return InterruptAction::Continue;
                };
                let Some((parent, name)) = self.split_parent(&new) else {
                    self.set_last_error(regs, error::PATH_NOT_FOUND);
                    return InterruptAction::Continue;
                };
                if self.disk.rename(node, parent, &name) {
                    ok(regs, 0);
                } else {
                    self.set_last_error(regs, error::ACCESS_DENIED);
                }
            }
            0x57 => {
                let Some(index) = self.sft_index(mem, regs.bx()) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return InterruptAction::Continue;
                };
                let node = self.sft[index].as_ref().map_or(0, |file| file.node);
                if regs.al() == 0 {
                    if let Some(entry) = self.disk.node(node) {
                        regs.set_cx(entry.time);
                        regs.set_dx(entry.date);
                    }
                    regs.set_carry(false);
                } else {
                    let (time, date) = (regs.cx(), regs.dx());
                    if let Some(entry) = self.disk.node_mut(node) {
                        entry.time = time;
                        entry.date = date;
                    }
                    regs.set_carry(false);
                }
            }
            0x58 => match regs.al() {
                0 => ok(regs, self.strategy as u16),
                1 => {
                    self.strategy = regs.bl();
                    regs.set_carry(false);
                }
                2 => {
                    regs.set_al(0);
                    regs.set_carry(false);
                }
                3 => regs.set_carry(false),
                _ => self.set_last_error(regs, error::INVALID_FUNCTION),
            },
            0x59 => {
                regs.set_ax(self.last_error);
                let class = match self.last_error {
                    0 => 0,
                    error::NO_MEMORY => 1,
                    error::FILE_NOT_FOUND | error::PATH_NOT_FOUND | error::NO_MORE_FILES => 8,
                    error::ACCESS_DENIED => 3,
                    _ => 7,
                };
                regs.set_bh(class);
                regs.set_bl(if self.last_error == 0 { 0 } else { 4 });
                regs.set_ch(if self.last_error == 0 { 0 } else { 2 });
            }
            0x5A => {
                let dir = self.read_path(mem, regs.ds(), regs.dx());
                let attr = regs.cl();
                let name_at = linear(regs.ds(), regs.dx()).wrapping_add(dir.len() as u32);
                let mut counter = (now_ms as u32) & 0xFFFF;
                let mut created = None;
                for _ in 0..64 {
                    let candidate = format!("{:08X}", counter);
                    let full = format!("{dir}{candidate}");
                    if self.resolve(&full).is_none() {
                        created = Some((full, candidate));
                        break;
                    }
                    counter = counter.wrapping_add(1);
                }
                match created {
                    Some((full, candidate)) => match self.create_path(mem, &full, attr, 2) {
                        Ok(handle) => {
                            for (index, byte) in candidate.as_bytes().iter().enumerate() {
                                guest_write(mem, vga, name_at + index as u32, *byte);
                            }
                            guest_write(mem, vga, name_at + candidate.len() as u32, 0);
                            ok(regs, handle);
                        }
                        Err(code) => self.set_last_error(regs, code),
                    },
                    None => self.set_last_error(regs, error::ACCESS_DENIED),
                }
            }
            0x5B => {
                let path = self.read_path(mem, regs.ds(), regs.dx());
                if self.resolve(&path).is_some() {
                    self.set_last_error(regs, error::FILE_EXISTS);
                } else {
                    let attr = regs.cl();
                    match self.create_path(mem, &path, attr, 2) {
                        Ok(handle) => ok(regs, handle),
                        Err(code) => self.set_last_error(regs, code),
                    }
                }
            }
            0x5C => ok(regs, 0),
            0x60 => {
                let path = self.read_path(mem, regs.ds(), regs.si());
                let text = match self.resolve(&path) {
                    Some(node) => format!("C:{}", self.disk.path(node)),
                    None => match self.split_parent(&path) {
                        Some((parent, name)) => {
                            let base = self.disk.path(parent);
                            let base = base.trim_end_matches('\\');
                            format!("C:{}\\{}", base, dos_name(&name))
                        }
                        None => format!("C:{}", self.disk.path(self.cwd)),
                    },
                };
                let at = linear(regs.es(), regs.di());
                let bytes = &text.as_bytes()[..text.len().min(127)];
                for (index, byte) in bytes.iter().enumerate() {
                    guest_write(mem, vga, at + index as u32, *byte);
                }
                guest_write(mem, vga, at + bytes.len() as u32, 0);
                ok(regs, 0);
            }
            0x63 => {
                regs.set_ds(DOS_DATA);
                regs.set_si(0x200);
                regs.set_al(0);
                regs.set_carry(false);
            }
            0x65 => self.country_ext(regs, mem, vga),
            0x66 => {
                if regs.al() == 1 {
                    regs.set_bx(437);
                    regs.set_dx(437);
                    regs.set_carry(false);
                } else if regs.al() == 2 {
                    ok(regs, 0);
                } else {
                    self.set_last_error(regs, error::INVALID_FUNCTION);
                }
            }
            0x67 => self.set_handle_count(regs, mem),
            0x68 | 0x6A => {
                if self.sft_index(mem, regs.bx()).is_some() {
                    regs.set_carry(false);
                } else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                }
            }
            0x6C => self.extended_open(regs, mem),
            _ => self.set_last_error(regs, error::INVALID_FUNCTION),
        }
        InterruptAction::Continue
    }

    fn set_handle_count(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        let wanted = regs.bx();
        let (table, size) = self.jft(mem);
        if wanted > 255 {
            self.set_last_error(regs, error::TOO_MANY_OPEN);
            return;
        }
        if wanted <= size {
            regs.set_carry(false);
            return;
        }
        let paras = (wanted as u32).div_ceil(16) as u16;
        let block = match self.mcb_alloc(mem, paras, self.psp) {
            Ok(block) => block,
            Err((code, _)) => {
                self.set_last_error(regs, code);
                return;
            }
        };
        let to = (block as u32) << 4;
        mem.fill(to, wanted as usize, 0xFF);
        for handle in 0..size as u32 {
            mem.write8(to + handle, mem.read8(table + handle));
        }
        let base = (self.psp as u32) << 4;
        mem.write16(base + 0x32, wanted);
        mem.write16(base + 0x34, 0);
        mem.write16(base + 0x36, block);
        regs.set_carry(false);
    }

    fn extended_open(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        let path = self.read_path(mem, regs.ds(), regs.si());
        let mode = regs.bl();
        let attr = regs.cl();
        let action = regs.dl();
        let exists = device_for(&path).is_some()
            || self
                .resolve(&path)
                .is_some_and(|node| self.disk.is_file(node));
        let result = if exists {
            match action & 0x0F {
                1 => self.open_path(mem, &path, mode).map(|handle| (handle, 1)),
                2 => self
                    .create_path(mem, &path, attr, mode)
                    .map(|handle| (handle, 3)),
                _ => Err(error::FILE_EXISTS),
            }
        } else if action & 0xF0 == 0x10 {
            self.create_path(mem, &path, attr, mode)
                .map(|handle| (handle, 2))
        } else {
            Err(self.missing_error(&path))
        };
        match result {
            Ok((handle, taken)) => {
                regs.set_cx(taken);
                ok(regs, handle);
            }
            Err(code) => self.set_last_error(regs, code),
        }
    }

    fn country_ext(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) {
        let at = linear(regs.es(), regs.di());
        match regs.al() {
            0x01 => {
                let size = regs.cx().min(41);
                let mut buffer = [0u8; 41];
                buffer[0] = 1;
                buffer[1] = 38;
                buffer[3] = 1;
                buffer[5] = 0xB5;
                buffer[6] = 1;
                let country = country_table();
                buffer[7..41].copy_from_slice(&country);
                for (index, byte) in buffer.iter().take(size as usize).enumerate() {
                    guest_write(mem, vga, at + index as u32, *byte);
                }
                regs.set_cx(size);
                regs.set_carry(false);
            }
            0x02 | 0x04 | 0x05 | 0x06 | 0x07 => {
                guest_write(mem, vga, at, regs.al());
                let offset = if regs.al() == 0x07 { 0x200u16 } else { 0x100 };
                for (index, byte) in offset.to_le_bytes().iter().enumerate() {
                    guest_write(mem, vga, at + 1 + index as u32, *byte);
                }
                for (index, byte) in DOS_DATA.to_le_bytes().iter().enumerate() {
                    guest_write(mem, vga, at + 3 + index as u32, *byte);
                }
                regs.set_cx(5);
                regs.set_carry(false);
            }
            0x20 => {
                let ch = regs.dl().to_ascii_uppercase();
                regs.set_dl(ch);
                regs.set_carry(false);
            }
            0x21 | 0x22 => {
                let start = linear(regs.ds(), regs.dx());
                let count = if regs.al() == 0x21 {
                    regs.cx() as u32
                } else {
                    0x1_0000
                };
                for index in 0..count {
                    let byte = mem.read8(start + index);
                    if regs.al() == 0x22 && byte == 0 {
                        break;
                    }
                    guest_write(mem, vga, start + index, byte.to_ascii_uppercase());
                }
                regs.set_carry(false);
            }
            0x23 => {
                let yes = matches!(regs.dl().to_ascii_uppercase(), b'Y');
                regs.set_ax(u16::from(yes));
                regs.set_carry(false);
            }
            _ => self.set_last_error(regs, error::INVALID_FUNCTION),
        }
    }

    fn ioctl(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory) {
        match regs.al() {
            0x00 | 0x01 | 0x06 | 0x07 | 0x0A => {
                let Some(index) = self.sft_index(mem, regs.bx()) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return;
                };
                let Some(Some(file)) = self.sft.get(index) else {
                    self.set_last_error(regs, error::BAD_HANDLE);
                    return;
                };
                match regs.al() {
                    0x00 => {
                        let info: u16 = match file.device {
                            Device::File => 0x0002,
                            Device::Con => 0x80D3,
                            Device::Nul => 0x8084,
                            Device::Clock => 0x8088,
                            Device::Driver => 0x4080,
                            _ => 0x80C0,
                        };
                        regs.set_dx(info);
                        ok(regs, info);
                    }
                    0x01 => ok(regs, regs.dx()),
                    0x06 => {
                        let ready = match file.device {
                            Device::File => (file.offset as usize) < self.disk.size(file.node),
                            Device::Con => self.console_ready(mem),
                            Device::Nul => false,
                            _ => true,
                        };
                        regs.set_al(if ready { 0xFF } else { 0 });
                        regs.set_carry(false);
                    }
                    0x07 => {
                        regs.set_al(0xFF);
                        regs.set_carry(false);
                    }
                    _ => ok(regs, 0),
                }
            }
            0x08 => {
                if regs.bl() == 0 || regs.bl() == 3 {
                    ok(regs, 1);
                } else {
                    self.set_last_error(regs, 15);
                }
            }
            0x09 => {
                if regs.bl() == 0 || regs.bl() == 3 {
                    regs.set_dx(0);
                    ok(regs, 0);
                } else {
                    self.set_last_error(regs, 15);
                }
            }
            0x0B => regs.set_carry(false),
            0x0E | 0x0F => {
                regs.set_al(0);
                regs.set_carry(false);
            }
            _ => self.set_last_error(regs, error::INVALID_FUNCTION),
        }
    }

    fn buffered_input(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
    ) -> InterruptAction {
        let dest = linear(regs.ds(), regs.dx());
        let max = mem.read8(dest);
        match self.edit_line(mem, vga, max.saturating_sub(1) as usize) {
            None => InterruptAction::Blocked,
            Some(text) => {
                guest_write(mem, vga, dest + 1, text.len() as u8);
                for (index, byte) in text.iter().enumerate() {
                    guest_write(mem, vga, dest + 2 + index as u32, *byte);
                }
                if max != 0 {
                    guest_write(mem, vga, dest + 2 + text.len() as u32, 0x0D);
                }
                InterruptAction::Continue
            }
        }
    }

    /// Runs the line editor over waiting keys. Returns the line once Enter comes.
    fn edit_line(&mut self, mem: &mut Memory, vga: &mut Vga, limit: usize) -> Option<Vec<u8>> {
        if !self.line.active {
            self.line.active = true;
            self.line.text.clear();
        }
        while let Some(word) = bda_pop(mem) {
            match word as u8 {
                0x0D => {
                    vga.teletype(mem, 0x0D);
                    vga.teletype(mem, 0x0A);
                    self.line.active = false;
                    let text = core::mem::take(&mut self.line.text);
                    return Some(text);
                }
                0x08 => {
                    if self.line.text.pop().is_some() {
                        vga.teletype(mem, 0x08);
                        vga.teletype(mem, b' ');
                        vga.teletype(mem, 0x08);
                    }
                }
                0x1B => {
                    self.line.text.clear();
                    vga.teletype(mem, 0x0D);
                    vga.teletype(mem, 0x0A);
                }
                0x00 | 0xE0 => {}
                ch if ch >= 0x20 && self.line.text.len() < limit => {
                    self.line.text.push(ch);
                    vga.teletype(mem, ch);
                }
                _ => {}
            }
        }
        None
    }

    fn read_file(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
    ) -> InterruptAction {
        let Some(index) = self.sft_index(mem, regs.bx()) else {
            self.set_last_error(regs, error::BAD_HANDLE);
            return InterruptAction::Continue;
        };
        let Some(file) = self.sft[index].clone() else {
            self.set_last_error(regs, error::BAD_HANDLE);
            return InterruptAction::Continue;
        };
        let count = regs.cx() as usize;
        let dest = linear(regs.ds(), regs.dx());
        match file.device {
            Device::File => {}
            Device::Con => {
                if self.line.pending.is_empty() {
                    match self.edit_line(mem, vga, 126) {
                        Some(mut text) => {
                            text.push(0x0D);
                            text.push(0x0A);
                            self.line.pending = text;
                        }
                        None => return InterruptAction::Blocked,
                    }
                }
                let take = count.min(self.line.pending.len());
                for offset in 0..take {
                    let byte = self.line.pending[offset];
                    guest_write(mem, vga, dest.wrapping_add(offset as u32), byte);
                }
                self.line.pending.drain(..take);
                ok(regs, take as u16);
                return InterruptAction::Continue;
            }
            _ => {
                ok(regs, 0);
                return InterruptAction::Continue;
            }
        }
        if file.mode & 7 == 1 {
            self.set_last_error(regs, error::ACCESS_DENIED);
            return InterruptAction::Continue;
        }
        let offset = file.offset as usize;
        let read = self.disk.size(file.node).saturating_sub(offset).min(count);
        // A copy from the drive's own data, so no second buffer is needed.
        if let Some(node) = self.disk.nodes.get(file.node) {
            for (at, byte) in node.data[offset..offset + read].iter().enumerate() {
                guest_write(mem, vga, dest.wrapping_add(at as u32), *byte);
            }
        }
        self.cost = self.cost.saturating_add(read as u32 / 8);
        if let Some(Some(slot)) = self.sft.get_mut(index) {
            slot.offset = slot.offset.saturating_add(read as u32);
        }
        ok(regs, read as u16);
        InterruptAction::Continue
    }

    fn write_file(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
    ) -> InterruptAction {
        let Some(index) = self.sft_index(mem, regs.bx()) else {
            self.set_last_error(regs, error::BAD_HANDLE);
            return InterruptAction::Continue;
        };
        let Some(file) = self.sft[index].clone() else {
            self.set_last_error(regs, error::BAD_HANDLE);
            return InterruptAction::Continue;
        };
        let count = regs.cx() as usize;
        let source = linear(regs.ds(), regs.dx());
        match file.device {
            Device::File => {}
            Device::Con => {
                for offset in 0..count {
                    let byte = guest_read(mem, vga, source.wrapping_add(offset as u32));
                    vga.teletype(mem, byte);
                }
                self.cost = self.cost.saturating_add(count as u32 / 2);
                ok(regs, count as u16);
                return InterruptAction::Continue;
            }
            _ => {
                ok(regs, count as u16);
                return InterruptAction::Continue;
            }
        }
        if file.mode & 7 == 0 {
            self.set_last_error(regs, error::ACCESS_DENIED);
            return InterruptAction::Continue;
        }
        if count == 0 {
            // A zero-length write sets the file's size to the current position.
            if self.disk.truncate(file.node, file.offset as usize) {
                ok(regs, 0);
            } else {
                self.set_last_error(regs, error::ACCESS_DENIED);
            }
            return InterruptAction::Continue;
        }
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(count).is_err() {
            ok(regs, 0);
            return InterruptAction::Continue;
        }
        for offset in 0..count {
            buffer.push(guest_read(mem, vga, source.wrapping_add(offset as u32)));
        }
        let written = self.write_sft(index, &buffer);
        self.cost = self.cost.saturating_add(written as u32 / 8);
        ok(regs, written as u16);
        InterruptAction::Continue
    }

    // ------------------------------------------------------------------
    // File control blocks
    // ------------------------------------------------------------------

    /// The 11-byte name and the block's address, past any extended header.
    fn fcb_at(&self, regs: &CpuRegs<'_>, mem: &Memory) -> (u32, u8) {
        let mut at = linear(regs.ds(), regs.dx());
        let mut attr = 0;
        if mem.read8(at) == 0xFF {
            attr = mem.read8(at + 6);
            at += 7;
        }
        (at, attr)
    }

    fn fcb_name_of(mem: &Memory, at: u32) -> [u8; 11] {
        let mut name = [b' '; 11];
        for (index, slot) in name.iter_mut().enumerate() {
            *slot = mem.read8(at + 1 + index as u32).to_ascii_uppercase();
        }
        name
    }

    fn fcb_path(name: &[u8; 11]) -> String {
        let base = String::from_utf8_lossy(&name[..8]).trim_end().to_string();
        let ext = String::from_utf8_lossy(&name[8..]).trim_end().to_string();
        if ext.is_empty() {
            base
        } else {
            format!("{base}.{ext}")
        }
    }

    fn fcb_call(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
    ) -> InterruptAction {
        let (at, attr) = self.fcb_at(regs, mem);
        let drive_ok = matches!(mem.read8(at), 0 | 3);
        match regs.ah() {
            0x0F | 0x16 => {
                let name = Self::fcb_name_of(mem, at);
                if !drive_ok {
                    regs.set_al(0xFF);
                    return InterruptAction::Continue;
                }
                let path = Self::fcb_path(&name);
                let result = if regs.ah() == 0x0F {
                    self.open_path(mem, &path, 2)
                } else {
                    self.create_path(mem, &path, attr, 2)
                };
                match result {
                    Ok(handle) => {
                        let index = self.sft_index(mem, handle).unwrap_or(0);
                        // The FCB keeps the SFT entry; the JFT slot goes back.
                        let (table, _) = self.jft(mem);
                        mem.write8(table + handle as u32, 0xFF);
                        let node = self.sft[index].as_ref().map_or(0, |file| file.node);
                        let entry = self.disk.node(node);
                        mem.write16(at + 0x0C, 0);
                        mem.write16(at + 0x0E, 128);
                        mem.write32(at + 0x10, entry.map_or(0, |file| file.data.len() as u32));
                        mem.write16(at + 0x14, entry.map_or(0, |file| file.date));
                        mem.write16(at + 0x16, entry.map_or(0, |file| file.time));
                        mem.write8(at + 0x18, index as u8);
                        mem.write8(at + 0x19, FCB_MAGIC);
                        mem.write8(at + 0x20, 0);
                        regs.set_al(0);
                    }
                    Err(_) => regs.set_al(0xFF),
                }
            }
            0x10 => {
                let index = mem.read8(at + 0x18) as usize;
                if mem.read8(at + 0x19) == FCB_MAGIC {
                    self.release_sft(index);
                    mem.write8(at + 0x19, 0);
                    regs.set_al(0);
                } else {
                    regs.set_al(0xFF);
                }
            }
            0x11 | 0x12 => {
                let pattern = Self::fcb_name_of(mem, at);
                let dta = self.dta;
                let dir = self.cwd;
                let start = if regs.ah() == 0x11 {
                    0
                } else {
                    mem.read32(at + 0x0C) as usize
                };
                match self.find_from(dir, &pattern, attr & 0x16, start) {
                    Some((found, short, entry_attr, time, date, size)) => {
                        mem.write32(at + 0x0C, found as u32 + 1);
                        let name = fcb_name(&short);
                        guest_write(mem, vga, dta, 3);
                        for (index, byte) in name.iter().enumerate() {
                            guest_write(mem, vga, dta + 1 + index as u32, *byte);
                        }
                        guest_write(mem, vga, dta + 0x0C, entry_attr);
                        for index in 0x0D..0x20 {
                            guest_write(mem, vga, dta + index, 0);
                        }
                        guest_write(mem, vga, dta + 0x17, time as u8);
                        guest_write(mem, vga, dta + 0x18, (time >> 8) as u8);
                        guest_write(mem, vga, dta + 0x19, date as u8);
                        guest_write(mem, vga, dta + 0x1A, (date >> 8) as u8);
                        for (index, byte) in size.to_le_bytes().iter().enumerate() {
                            guest_write(mem, vga, dta + 0x1D + index as u32, *byte);
                        }
                        regs.set_al(0);
                    }
                    None => regs.set_al(0xFF),
                }
            }
            0x13 => {
                let pattern = Self::fcb_name_of(mem, at);
                let matches: Vec<usize> = self
                    .disk
                    .children(self.cwd)
                    .iter()
                    .copied()
                    .filter(|&child| {
                        let entry = &self.disk.nodes[child];
                        !entry.is_dir && fcb_match(&fcb_name(&entry.short), &pattern)
                    })
                    .collect();
                let mut removed = false;
                for node in matches {
                    let attr = self.disk.nodes[node].attr;
                    if attr & 1 == 0 && self.disk.remove(node) {
                        removed = true;
                    }
                }
                regs.set_al(if removed { 0 } else { 0xFF });
            }
            0x14 | 0x15 | 0x21 | 0x22 | 0x27 | 0x28 => self.fcb_io(regs, mem, vga, at),
            0x17 => {
                let old = Self::fcb_name_of(mem, at);
                let mut new = [b' '; 11];
                for (index, slot) in new.iter_mut().enumerate() {
                    *slot = mem.read8(at + 0x11 + index as u32).to_ascii_uppercase();
                }
                let path = Self::fcb_path(&old);
                let new_path = Self::fcb_path(&new);
                match self.resolve(&path) {
                    Some(node) if self.disk.rename(node, self.cwd, &new_path) => regs.set_al(0),
                    _ => regs.set_al(0xFF),
                }
            }
            0x23 => {
                let name = Self::fcb_name_of(mem, at);
                let path = Self::fcb_path(&name);
                let record = mem.read16(at + 0x0E).max(1) as u32;
                match self.resolve(&path) {
                    Some(node) if self.disk.is_file(node) => {
                        let size = self.disk.size(node) as u32;
                        mem.write32(at + 0x21, size.div_ceil(record));
                        regs.set_al(0);
                    }
                    _ => regs.set_al(0xFF),
                }
            }
            0x24 => {
                let block = mem.read16(at + 0x0C) as u32;
                let record = mem.read8(at + 0x20) as u32;
                mem.write32(at + 0x21, block * 128 + record);
            }
            0x29 => {
                let (consumed, wild) = parse_filename(
                    mem,
                    vga,
                    regs.sregs[DS],
                    regs.r[SI] as u16,
                    regs.sregs[ES],
                    regs.r[DI] as u16,
                    regs.al(),
                );
                regs.set_si((regs.r[SI] as u16).wrapping_add(consumed));
                regs.set_al(u8::from(wild));
            }
            _ => regs.set_al(0xFF),
        }
        InterruptAction::Continue
    }

    /// Sequential, random and random-block FCB reads and writes.
    fn fcb_io(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga, at: u32) {
        let index = mem.read8(at + 0x18) as usize;
        if mem.read8(at + 0x19) != FCB_MAGIC {
            regs.set_al(1);
            return;
        }
        let Some(file) = self.sft.get(index).and_then(|slot| slot.clone()) else {
            regs.set_al(1);
            return;
        };
        let record_size = mem.read16(at + 0x0E).max(1) as u32;
        let function = regs.ah();
        let sequential = matches!(function, 0x14 | 0x15);
        let block_mode = matches!(function, 0x27 | 0x28);
        let write = matches!(function, 0x15 | 0x22 | 0x28);
        let mut record = if sequential {
            mem.read16(at + 0x0C) as u32 * 128 + mem.read8(at + 0x20) as u32
        } else {
            mem.read32(at + 0x21)
        };
        let count = if block_mode { regs.cx() as u32 } else { 1 };
        let dta = self.dta;
        let mut done = 0u32;
        let mut status = 0u8;
        for _ in 0..count {
            let Some(offset) = record.checked_mul(record_size) else {
                status = 2;
                break;
            };
            if write {
                let mut buffer = Vec::new();
                if buffer.try_reserve_exact(record_size as usize).is_err() {
                    status = 1;
                    break;
                }
                for byte in 0..record_size {
                    buffer.push(guest_read(mem, vga, dta.wrapping_add(byte)));
                }
                if self.disk.write(file.node, offset as usize, &buffer) != buffer.len() {
                    status = 1;
                    break;
                }
                mem.write32(at + 0x10, self.disk.size(file.node) as u32);
            } else {
                let size = self.disk.size(file.node) as u32;
                if offset >= size {
                    status = 1;
                    break;
                }
                let got = (size - offset).min(record_size);
                for byte in 0..record_size {
                    let value = if byte < got {
                        self.disk.nodes[file.node].data[(offset + byte) as usize]
                    } else {
                        0
                    };
                    guest_write(mem, vga, dta.wrapping_add(byte), value);
                }
                if got < record_size {
                    status = 3;
                }
            }
            self.cost = self.cost.saturating_add(record_size / 8);
            done += 1;
            record = record.wrapping_add(1);
            if status != 0 {
                break;
            }
        }
        if sequential {
            let new = record;
            mem.write16(at + 0x0C, (new / 128) as u16);
            mem.write8(at + 0x20, (new % 128) as u8);
        } else {
            mem.write16(at + 0x0C, (record / 128) as u16);
            mem.write8(at + 0x20, (record % 128) as u8);
            if block_mode {
                mem.write32(at + 0x21, record);
                regs.set_cx(done as u16);
            }
        }
        regs.set_al(status);
    }
}

/// A placeholder owner while a load is in progress, so a failure can free it.
const LOAD_OWNER: u16 = 0x0008;
/// Marks an open FCB.
const FCB_MAGIC: u8 = 0xFC;

/// Writes a PSP.
pub fn write_psp(mem: &mut Memory, psp: u16, parent: u16, env: u16, top: u16, tail: &[u8]) {
    let base = (psp as u32) << 4;
    mem.fill(base, 0x100, 0);
    mem.write16(base, 0x20CD);
    mem.write16(base + 2, top);
    // Far call to the DOS dispatcher, used by CP/M-style programs.
    mem.write8(base + 5, 0x9A);
    mem.write16(base + 6, 0xFFF0);
    mem.write16(base + 8, 0xFEFF);
    for (vector, at) in [(0x22u32, 0x0Au32), (0x23, 0x0E), (0x24, 0x12)] {
        mem.write32(base + at, mem.read32(vector * 4));
    }
    mem.write16(base + 0x16, parent);
    mem.fill(base + 0x18, HANDLE_COUNT as usize, 0xFF);
    mem.write16(base + 0x2C, env);
    mem.write16(base + 0x2E, mem.read16(((parent as u32) << 4) + 0x2E));
    mem.write16(base + 0x30, mem.read16(((parent as u32) << 4) + 0x30));
    mem.write16(base + 0x32, HANDLE_COUNT);
    mem.write16(base + 0x34, 0x18);
    mem.write16(base + 0x36, psp);
    mem.write32(base + 0x38, 0xFFFF_FFFF);
    mem.write8(base + 0x50, 0xCD);
    mem.write8(base + 0x51, 0x21);
    mem.write8(base + 0x52, 0xCB);
    mem.fill(base + 0x5C, 0x24, 0);
    for offset in [0x5Du32, 0x6D] {
        mem.fill(base + offset, 11, b' ');
    }
    let len = tail.len().min(126);
    mem.write8(base + 0x80, len as u8);
    mem.copy_from(base + 0x81, &tail[..len]);
    mem.write8(base + 0x81 + len as u32, 0x0D);
}

/// Fills the PSP's default FCBs from the first two command-line arguments.
fn parse_fcb_tail(mem: &mut Memory, psp: u16, tail: &[u8]) {
    let text = String::from_utf8_lossy(tail);
    let base = (psp as u32) << 4;
    for (index, arg) in text
        .split([' ', '\t', ',', ';', '='])
        .filter(|part| !part.is_empty() && !part.starts_with('/'))
        .take(2)
        .enumerate()
    {
        let at = base + if index == 0 { 0x5C } else { 0x6C };
        let (drive, name) = match arg.as_bytes() {
            [letter, b':', ..] => (letter.to_ascii_uppercase().saturating_sub(b'@'), &arg[2..]),
            _ => (0, arg),
        };
        mem.write8(at, drive);
        let pattern = crate::ramdisk::fcb_pattern(name);
        mem.copy_from(at + 1, &pattern);
    }
}

/// `AH=29h`: parses a filename into an FCB. Returns the bytes used and whether
/// the name has wildcards.
fn parse_filename(
    mem: &mut Memory,
    vga: &mut Vga,
    src_seg: u16,
    src_off: u16,
    fcb_seg: u16,
    fcb_off: u16,
    flags: u8,
) -> (u16, bool) {
    let source = linear(src_seg, src_off);
    let fcb = linear(fcb_seg, fcb_off);
    let mut at = 0u32;
    if flags & 1 != 0 {
        while matches!(
            mem.read8(source + at),
            b' ' | b'\t' | b',' | b';' | b':' | b'=' | b'+'
        ) {
            at += 1;
            if at > 128 {
                return (at as u16, false);
            }
        }
    }
    let drive_letter = mem.read8(source + at);
    let mut drive = None;
    if mem.read8(source + at + 1) == b':' && drive_letter.is_ascii_alphabetic() {
        drive = Some(drive_letter.to_ascii_uppercase() - b'@');
        at += 2;
    }
    if let Some(drive) = drive {
        guest_write(mem, vga, fcb, drive);
    } else if flags & 2 == 0 {
        guest_write(mem, vga, fcb, 0);
    }
    let mut wild = false;
    let mut name = [b' '; 11];
    let mut read_field = |mem: &Memory, field: &mut [u8], at: &mut u32| {
        let mut position = 0;
        loop {
            let byte = mem.read8(source + *at);
            if byte <= b' ' || b".\"/\\[]:|<>+=;,".contains(&byte) {
                break;
            }
            *at += 1;
            if position < field.len() {
                if byte == b'*' {
                    for slot in field[position..].iter_mut() {
                        *slot = b'?';
                    }
                    position = field.len();
                    wild = true;
                } else {
                    if byte == b'?' {
                        wild = true;
                    }
                    field[position] = byte.to_ascii_uppercase();
                    position += 1;
                }
            }
        }
        position
    };
    let (base, ext) = name.split_at_mut(8);
    let base_len = read_field(mem, base, &mut at);
    let mut ext_len = 0;
    if mem.read8(source + at) == b'.' {
        at += 1;
        ext_len = read_field(mem, ext, &mut at);
    }
    let keep_name = flags & 4 != 0 && base_len == 0;
    let keep_ext = flags & 8 != 0 && ext_len == 0;
    for (index, byte) in name.iter().enumerate() {
        let is_ext = index >= 8;
        if (!is_ext && !keep_name) || (is_ext && !keep_ext) {
            guest_write(mem, vga, fcb + 1 + index as u32, *byte);
        }
    }
    (at as u16, wild)
}

/// The 34-byte country information block.
fn country_table() -> [u8; 34] {
    let mut table = [0u8; 34];
    table[2] = b'$';
    table[7] = b',';
    table[9] = b'.';
    table[11] = b'-';
    table[13] = b':';
    table[15] = 0;
    table[16] = 2;
    table[17] = 0;
    // The case map is a far pointer to a RETF in the ROM's stubs.
    table[18] = 0xA1;
    table[19] = 0x04;
    table[20] = 0x00;
    table[21] = 0xF0;
    table[22] = b',';
    table
}

fn write_country(mem: &mut Memory, vga: &mut Vga, at: u32) {
    for (index, byte) in country_table().iter().enumerate() {
        guest_write(mem, vga, at + index as u32, *byte);
    }
}

fn zip_alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_uppercase().as_str() {
        "DOOM" => Some("doom.zip"),
        "DUKE3D" => Some("duke-nukem-3d.zip"),
        "WOLF3D" => Some("wolfenstein-3d.zip"),
        "QUAKE" => Some("quake.zip"),
        "SSF2T" => Some("super-street-fighter-ii-turbo.zip"),
        "TANK" => Some("tank-wars.zip"),
        "WAR2" => Some("warcraft-2-tides-of-darkness.zip"),
        "CKONG" => Some("champ-kong.zip"),
        "GALACTA" => Some("galacta-the-battle-for-saturn.zip"),
        "MARIO" => Some("mario-bros-vga.zip"),
        "SC2000" => Some("simcity-2000.zip"),
        "SNOOD" => Some("snood.zip"),
        "DIGGER" => Some("digger-remastered.zip"),
        _ => None,
    }
}
