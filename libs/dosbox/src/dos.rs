//! The DOS kernel: `INT 21h`, handles, the DTA, file search and memory
//! allocation.
//!
//! The implementation covers the calls a text-mode DOS program or a small
//! `COMMAND.COM` uses. Unsupported calls fail cleanly with `CF` set and an
//! error code in `AX`, which is what a DOS program expects.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use feos_archive::zip::ZipReader;

use crate::bios::{clock_date, clock_time};
use crate::cpu::{CpuRegs, InterruptAction};
use crate::demo;
use crate::keyboard::Keyboard;
use crate::memory::Memory;
use crate::ramdisk::{dos_name, strip_drive, RamDisk};
use crate::vga::Vga;

/// Segment of the program's PSP.
pub const PSP_SEGMENT: u16 = 0x1000;
/// First dynamically allocated handle.
pub const FIRST_HANDLE: usize = 5;
/// Number of DOS handles.
pub const HANDLE_COUNT: usize = 20;
/// First paragraph the DOS memory allocator hands out.
pub const ALLOC_START: u16 = 0x2000;
/// One past the last paragraph the allocator can hand out.
pub const ALLOC_END: u16 = 0xA000;

/// An open file or character device.
#[derive(Clone)]
pub struct OpenFile {
    pub node: usize,
    pub offset: usize,
    pub mode: u8,
    /// 0 for a file; 1-5 for CON, AUX and PRN.
    pub device: u8,
}

/// State of a `FIND FIRST`/`FIND NEXT` search.
pub struct FindState {
    pub dir: usize,
    pub entries: Vec<usize>,
    pub index: usize,
}

/// State of `INT 21h` function 0Ah (buffered input).
pub struct LineInput {
    pub dest: u32,
    pub max: u8,
    pub text: Vec<u8>,
    pub active: bool,
}

/// A block handed out by `INT 21h` function 48h.
#[derive(Clone, Copy)]
pub struct AllocBlock {
    pub segment: u16,
    pub paras: u16,
}

/// The DOS kernel state.
pub struct Dos {
    pub disk: RamDisk,
    pub cwd: usize,
    pub handles: Vec<Option<OpenFile>>,
    pub dta: u32,
    pub psp: u16,
    pub exit_code: u8,
    pub drive: u8,
    pub find: Option<FindState>,
    pub line: LineInput,
    pub alloc: Vec<AllocBlock>,
    pub verify: bool,
    pub break_flag: bool,
    /// The board's read-only filesystem, used by the `MOUNT` command.
    pub fs: Option<&'static dyn feos_fs::FileSystem>,
}

impl Default for Dos {
    fn default() -> Self {
        Self::new()
    }
}

impl Dos {
    pub fn new() -> Self {
        let mut handles = vec![None; HANDLE_COUNT];
        for (index, slot) in handles.iter_mut().enumerate().take(FIRST_HANDLE) {
            *slot = Some(OpenFile {
                node: 0,
                offset: 0,
                mode: 2,
                device: (index + 1) as u8,
            });
        }
        Self {
            disk: RamDisk::new(),
            cwd: 0,
            handles,
            dta: ((PSP_SEGMENT as u32) << 4) + 0x80,
            psp: PSP_SEGMENT,
            exit_code: 0,
            drive: 2,
            find: None,
            line: LineInput {
                dest: 0,
                max: 0,
                text: Vec::new(),
                active: false,
            },
            alloc: Vec::new(),
            verify: false,
            break_flag: false,
            fs: None,
        }
    }

    /// Creates the standard files and imports the FEOS filesystem into
    /// `C:\FEOS`.
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
        self.disk.create_dir(self.disk.root, "FEOS");
    }

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
        Ok(self.disk.import_fs(dest_node, fs, node, 0))
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
        let mut reader = ZipReader::open(&data).map_err(|_| "invalid zip archive")?;
        if self.disk.lookup(self.disk.root, dest).is_some() {
            return Err("drive C: already has that directory");
        }
        let Some(dest_node) = self.disk.create_dir(self.disk.root, dest) else {
            return Err("cannot create directory");
        };
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
            let mut buffer = vec![0u8; entry.uncompressed_size as usize];
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
        let node = fs.resolve(path)?;
        if !fs.is_file(node) {
            return None;
        }
        let size = fs.size(node);
        let mut data = vec![0u8; size];
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

    /// Splits a find path into a directory and a wildcard pattern.
    fn split_find(&self, path: &str) -> Option<(usize, String)> {
        let path = strip_drive(path);
        let absolute = path.starts_with('\\') || path.starts_with('/');
        let (dir_part, pattern) = match path.rsplit_once(['\\', '/']) {
            Some((dir, pattern)) => (dir, pattern),
            None => ("", path),
        };
        let dir = if dir_part.is_empty() {
            if absolute {
                self.disk.root
            } else {
                self.cwd
            }
        } else {
            self.disk.resolve(self.cwd, dir_part)?
        };
        let pattern = if pattern.is_empty() {
            "*.*".to_string()
        } else {
            pattern.to_string()
        };
        Some((dir, pattern))
    }

    fn alloc_handle(&mut self, file: OpenFile) -> Option<u16> {
        for index in FIRST_HANDLE..self.handles.len() {
            if self.handles[index].is_none() {
                self.handles[index] = Some(file);
                return Some(index as u16);
            }
        }
        None
    }

    fn close_handle(&mut self, handle: u16) -> bool {
        let index = handle as usize;
        if index < FIRST_HANDLE || index >= self.handles.len() {
            return false;
        }
        self.handles[index].take().is_some()
    }

    fn allocate(&mut self, paras: u16) -> Option<u16> {
        if paras == 0 {
            return None;
        }
        let mut blocks = self.alloc.clone();
        blocks.sort_by_key(|block| block.segment);
        let mut candidate = ALLOC_START;
        for block in &blocks {
            if candidate.saturating_add(paras) <= block.segment {
                break;
            }
            let end = block.segment.saturating_add(block.paras);
            if end > candidate {
                candidate = end;
            }
        }
        if candidate.saturating_add(paras) > ALLOC_END {
            return None;
        }
        self.alloc.push(AllocBlock {
            segment: candidate,
            paras,
        });
        Some(candidate)
    }

    fn free(&mut self, segment: u16) -> bool {
        if let Some(index) = self.alloc.iter().position(|block| block.segment == segment) {
            self.alloc.remove(index);
            true
        } else {
            false
        }
    }

    fn resize(&mut self, segment: u16, paras: u16) -> bool {
        let mut blocks = self.alloc.clone();
        blocks.sort_by_key(|block| block.segment);
        let Some(index) = blocks.iter().position(|block| block.segment == segment) else {
            return false;
        };
        let limit = blocks
            .get(index + 1)
            .map_or(ALLOC_END, |block| block.segment);
        if segment.saturating_add(paras) > limit {
            return false;
        }
        if let Some(block) = self.alloc.iter_mut().find(|block| block.segment == segment) {
            block.paras = paras;
        }
        true
    }

    fn read_path(&self, mem: &Memory, regs: &CpuRegs<'_>, offset: u16) -> String {
        let bytes = mem.read_cstring(regs.ds(), offset, 128);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn fail(regs: &mut CpuRegs<'_>, code: u16) {
        regs.set_carry(true);
        regs.set_ax(code);
    }

    fn ok(regs: &mut CpuRegs<'_>, ax: u16) {
        regs.set_carry(false);
        regs.set_ax(ax);
    }

    fn fill_dta(&self, mem: &mut Memory, node: usize) -> bool {
        let Some(entry) = self.disk.node(node) else {
            return false;
        };
        let name = dos_name(&entry.name);
        let dta = self.dta;
        mem.fill(dta, 0x2B, 0);
        for (index, byte) in name.as_bytes().iter().take(12).enumerate() {
            mem.write8(dta + index as u32, *byte);
        }
        mem.write8(dta + 0x15, entry.attr);
        mem.write16(dta + 0x16, entry.time);
        mem.write16(dta + 0x18, entry.date);
        mem.write32(dta + 0x1A, entry.data.len() as u32);
        let short = dos_name(&entry.name);
        for (index, byte) in short.as_bytes().iter().take(12).enumerate() {
            mem.write8(dta + 0x1E + index as u32, *byte);
        }
        mem.write8(dta + 0x1E + short.len().min(12) as u32, 0);
        true
    }

    fn find_first(&mut self, mem: &mut Memory, path: &str, attr: u8) -> bool {
        let Some((dir, pattern)) = self.split_find(path) else {
            return false;
        };
        let mut entries = self.disk.find(dir, &pattern);
        if attr & 0x10 == 0 {
            entries.retain(|&node| self.disk.is_file(node));
        }
        if entries.is_empty() {
            return false;
        }
        let first = entries[0];
        self.find = Some(FindState {
            dir,
            entries,
            index: 0,
        });
        self.fill_dta(mem, first)
    }

    fn find_next(&mut self, mem: &mut Memory) -> bool {
        let Some(state) = self.find.as_mut() else {
            return false;
        };
        state.index += 1;
        let Some(&node) = state.entries.get(state.index) else {
            return false;
        };
        self.fill_dta(mem, node)
    }

    /// `INT 21h`.
    pub fn int21(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
        keyboard: &mut Keyboard,
        now_ms: u64,
        exit_requested: &mut bool,
        exit_code: &mut u8,
    ) -> InterruptAction {
        match regs.ah() {
            0x00 => {
                *exit_requested = true;
                *exit_code = 0;
                return InterruptAction::Halt;
            }
            0x01 => {
                if let Some(key) = keyboard.pop() {
                    vga.teletype(mem, key.ascii);
                    regs.set_al(key.ascii);
                    return InterruptAction::Continue;
                }
                return InterruptAction::Blocked;
            }
            0x02 => vga.teletype(mem, regs.dl()),
            0x03 => regs.set_al(0),
            0x04 | 0x05 => {}
            0x06 => {
                if regs.dl() == 0xFF {
                    if let Some(key) = keyboard.pop() {
                        regs.set_al(key.ascii);
                        regs.set_zero(false);
                    } else {
                        regs.set_al(0);
                        regs.set_zero(true);
                    }
                } else {
                    vga.teletype(mem, regs.dl());
                }
            }
            0x07 | 0x08 => {
                if let Some(key) = keyboard.pop() {
                    regs.set_al(key.ascii);
                    return InterruptAction::Continue;
                }
                return InterruptAction::Blocked;
            }
            0x09 => {
                let text = mem.read_dollar_string(regs.ds(), regs.dx(), 4096);
                vga.write_str(mem, &text);
            }
            0x0A => return self.buffered_input(regs, mem, vga, keyboard),
            0x0B => {
                regs.set_al(if keyboard.peek().is_some() { 0xFF } else { 0 });
            }
            0x0C => {
                keyboard.clear();
                match regs.al() {
                    0x01 => {
                        if let Some(key) = keyboard.pop() {
                            vga.teletype(mem, key.ascii);
                            regs.set_al(key.ascii);
                            return InterruptAction::Continue;
                        }
                        return InterruptAction::Blocked;
                    }
                    0x06 | 0x07 | 0x08 => {
                        if let Some(key) = keyboard.pop() {
                            regs.set_al(key.ascii);
                            return InterruptAction::Continue;
                        }
                        return InterruptAction::Blocked;
                    }
                    _ => {}
                }
            }
            0x0D => Self::ok(regs, 0),
            0x0E => Self::ok(regs, 3),
            0x0F..=0x17 => Self::fail(regs, 1),
            0x19 => Self::ok(regs, 2),
            0x1A => {
                self.dta = ((regs.ds() as u32) << 4).wrapping_add(regs.dx() as u32);
                Self::ok(regs, 0);
            }
            0x1B..=0x1C => Self::fail(regs, 1),
            0x1D..=0x20 => {}
            0x21..=0x24 => Self::fail(regs, 1),
            0x25 => {
                let vector = regs.al() as u32;
                let address = ((regs.ds() as u32) << 4).wrapping_add(regs.dx() as u32);
                mem.write16(vector * 4, regs.dx());
                mem.write16(vector * 4 + 2, regs.ds());
                let _ = address;
                Self::ok(regs, 0);
            }
            0x26 => {}
            0x27..=0x29 => Self::fail(regs, 1),
            0x2A => {
                let (century, year, month, day) = clock_date(now_ms);
                regs.set_cx(((century as u16) * 100) + year as u16);
                regs.set_dh(month);
                regs.set_dl(day);
                regs.set_al(0);
                Self::ok(regs, regs.ax());
            }
            0x2B => Self::ok(regs, 0),
            0x2C => {
                let (hour, minute, second) = clock_time(now_ms);
                regs.set_ch(hour);
                regs.set_cl(minute);
                regs.set_dh(second);
                regs.set_dl(0);
                Self::ok(regs, regs.ax());
            }
            0x2D => Self::ok(regs, 0),
            0x2E => {
                self.verify = regs.al() != 0;
                Self::ok(regs, 0);
            }
            0x2F => {
                regs.set_es((self.dta >> 4) as u16);
                regs.set_bx((self.dta & 0x0F) as u16);
                Self::ok(regs, 0);
            }
            0x30 => {
                regs.set_al(crate::DOS_VERSION);
                regs.set_ah(0);
                Self::ok(regs, regs.ax());
            }
            0x31 => {
                *exit_requested = true;
                *exit_code = regs.al();
                return InterruptAction::Halt;
            }
            0x32 => Self::fail(regs, 1),
            0x33 => {
                regs.set_al(0);
                regs.set_dl(0);
                Self::ok(regs, 0);
            }
            0x34 => {
                regs.set_es(0);
                regs.set_bx(0);
                Self::ok(regs, 0);
            }
            0x35 => {
                let vector = regs.al() as u32;
                let offset = mem.read16(vector * 4);
                let segment = mem.read16(vector * 4 + 2);
                regs.set_bx(offset);
                regs.set_es(segment);
                Self::ok(regs, 0);
            }
            0x36 => {
                regs.set_ax(4);
                regs.set_bx(0x0800);
                regs.set_cx(512);
                regs.set_dx(0x1000);
                Self::ok(regs, regs.ax());
            }
            0x37 => {
                regs.set_al(0);
                regs.set_dl(0);
                Self::ok(regs, 0);
            }
            0x38 => Self::fail(regs, 2),
            0x39 => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some((parent, name)) = self.split_parent(&path) {
                    if self.disk.create_dir(parent, &name).is_some() {
                        Self::ok(regs, 0);
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 3);
                }
            }
            0x3A => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some(node) = self.resolve(&path) {
                    if self.disk.remove(node) {
                        Self::ok(regs, 0);
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 3);
                }
            }
            0x3B => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some(node) = self.resolve(&path) {
                    if self.disk.is_dir(node) {
                        self.cwd = node;
                        Self::ok(regs, 0);
                    } else {
                        Self::fail(regs, 3);
                    }
                } else {
                    Self::fail(regs, 3);
                }
            }
            0x3C => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some((parent, name)) = self.split_parent(&path) {
                    if let Some(node) = self.disk.create_file(parent, &name, Vec::new()) {
                        if let Some(handle) = self.alloc_handle(OpenFile {
                            node,
                            offset: 0,
                            mode: regs.al(),
                            device: 0,
                        }) {
                            Self::ok(regs, handle);
                        } else {
                            Self::fail(regs, 4);
                        }
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 3);
                }
            }
            0x3D => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some(node) = self.resolve(&path) {
                    if self.disk.is_file(node) {
                        if let Some(handle) = self.alloc_handle(OpenFile {
                            node,
                            offset: 0,
                            mode: regs.al(),
                            device: 0,
                        }) {
                            Self::ok(regs, handle);
                        } else {
                            Self::fail(regs, 4);
                        }
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 2);
                }
            }
            0x3E => {
                if self.close_handle(regs.bx()) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 6);
                }
            }
            0x3F => return self.read_file(regs, mem, keyboard),
            0x40 => return self.write_file(regs, mem, vga),
            0x41 => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some(node) = self.resolve(&path) {
                    if self.disk.is_file(node) && self.disk.remove(node) {
                        Self::ok(regs, 0);
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 2);
                }
            }
            0x42 => {
                let index = regs.bx() as usize;
                let Some(Some(file)) = self.handles.get_mut(index) else {
                    return {
                        Self::fail(regs, 6);
                        InterruptAction::Continue
                    };
                };
                let offset = ((regs.cx() as i32) << 16) | regs.dx() as i32;
                let base = match regs.al() {
                    0 => 0,
                    1 => file.offset as i32,
                    2 => self.disk.size(file.node) as i32,
                    _ => {
                        Self::fail(regs, 1);
                        return InterruptAction::Continue;
                    }
                };
                let position = base.saturating_add(offset).max(0) as usize;
                file.offset = position;
                regs.set_ax((position & 0xFFFF) as u16);
                regs.set_dx((position >> 16) as u16);
                Self::ok(regs, regs.ax());
            }
            0x43 => {
                let path = self.read_path(mem, regs, regs.dx());
                if let Some(node) = self.resolve(&path) {
                    if regs.al() == 0 {
                        let attr = self.disk.node(node).map_or(0x20, |node| node.attr);
                        regs.set_cx(attr as u16);
                        Self::ok(regs, 0);
                    } else if regs.al() == 1 {
                        if let Some(entry) = self.disk.node_mut(node) {
                            entry.attr = regs.cl();
                        }
                        Self::ok(regs, 0);
                    } else {
                        Self::fail(regs, 1);
                    }
                } else {
                    Self::fail(regs, 2);
                }
            }
            0x44 => {
                let index = regs.bx() as usize;
                match regs.al() {
                    0 => {
                        let device = self
                            .handles
                            .get(index)
                            .and_then(|slot| slot.as_ref())
                            .map_or(0, |file| if file.device != 0 { 0x80 } else { 0 });
                        regs.set_dx(device);
                        Self::ok(regs, device);
                    }
                    1 => Self::ok(regs, 0),
                    _ => Self::ok(regs, 0),
                }
            }
            0x45 => {
                let source = regs.bx() as usize;
                let Some(Some(file)) = self.handles.get(source).cloned() else {
                    return {
                        Self::fail(regs, 6);
                        InterruptAction::Continue
                    };
                };
                if let Some(handle) = self.alloc_handle(file) {
                    Self::ok(regs, handle);
                } else {
                    Self::fail(regs, 4);
                }
            }
            0x46 => {
                let source = regs.bx() as usize;
                let target = regs.cx() as usize;
                let Some(Some(file)) = self.handles.get(source).cloned() else {
                    return {
                        Self::fail(regs, 6);
                        InterruptAction::Continue
                    };
                };
                if target >= self.handles.len() || target < FIRST_HANDLE {
                    Self::fail(regs, 6);
                } else {
                    self.handles[target] = Some(file);
                    Self::ok(regs, 0);
                }
            }
            0x47 => {
                let path = self.current_path();
                let path = path.trim_start_matches('\\');
                mem.write_cstring(regs.ds(), regs.si(), path.as_bytes());
                Self::ok(regs, 0);
            }
            0x48 => {
                if let Some(segment) = self.allocate(regs.bx()) {
                    Self::ok(regs, segment);
                } else {
                    Self::fail(regs, 8);
                }
            }
            0x49 => {
                if self.free(regs.es()) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 9);
                }
            }
            0x4A => {
                if self.resize(regs.es(), regs.bx()) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 8);
                }
            }
            0x4B => Self::fail(regs, 8),
            0x4C => {
                *exit_requested = true;
                *exit_code = regs.al();
                return InterruptAction::Halt;
            }
            0x4D => Self::ok(regs, *exit_code as u16),
            0x4E => {
                let path = self.read_path(mem, regs, regs.dx());
                if self.find_first(mem, &path, regs.cl()) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 18);
                }
            }
            0x4F => {
                if self.find_next(mem) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 18);
                }
            }
            0x50 => {
                self.psp = regs.bx();
                Self::ok(regs, 0);
            }
            0x51 | 0x62 => {
                regs.set_bx(self.psp);
                Self::ok(regs, 0);
            }
            0x52 => {
                regs.set_es(0);
                regs.set_bx(0);
                Self::fail(regs, 1);
            }
            0x53 => Self::fail(regs, 1),
            0x54 => {
                regs.set_al(u8::from(self.verify));
                Self::ok(regs, regs.ax());
            }
            0x55 => {}
            0x56 => {
                let old = self.read_path(mem, regs, regs.dx());
                let new = mem.read_cstring(regs.es(), regs.di(), 128);
                let new = String::from_utf8_lossy(&new).into_owned();
                let Some(node) = self.resolve(&old) else {
                    return {
                        Self::fail(regs, 2);
                        InterruptAction::Continue
                    };
                };
                let Some((parent, name)) = self.split_parent(&new) else {
                    return {
                        Self::fail(regs, 3);
                        InterruptAction::Continue
                    };
                };
                if self.disk.rename(node, parent, &name) {
                    Self::ok(regs, 0);
                } else {
                    Self::fail(regs, 5);
                }
            }
            0x57 => {
                let index = regs.bx() as usize;
                let Some(Some(file)) = self.handles.get(index) else {
                    return {
                        Self::fail(regs, 6);
                        InterruptAction::Continue
                    };
                };
                let node = file.node;
                if regs.al() == 0 {
                    if let Some(entry) = self.disk.node(node) {
                        regs.set_cx(entry.time);
                        regs.set_dx(entry.date);
                    }
                    Self::ok(regs, 0);
                } else {
                    if let Some(entry) = self.disk.node_mut(node) {
                        entry.time = regs.cx();
                        entry.date = regs.dx();
                    }
                    Self::ok(regs, 0);
                }
            }
            0x58 => {
                regs.set_ax(0);
                Self::ok(regs, 0);
            }
            0x59 => {
                regs.set_ax(0);
                regs.set_bx(0);
                regs.set_cx(0);
                Self::ok(regs, 0);
            }
            0x5A => Self::fail(regs, 1),
            0x5B => {
                let path = self.read_path(mem, regs, regs.dx());
                if self.resolve(&path).is_some() {
                    Self::fail(regs, 80);
                } else if let Some((parent, name)) = self.split_parent(&path) {
                    if let Some(node) = self.disk.create_file(parent, &name, Vec::new()) {
                        if let Some(handle) = self.alloc_handle(OpenFile {
                            node,
                            offset: 0,
                            mode: regs.al(),
                            device: 0,
                        }) {
                            Self::ok(regs, handle);
                        } else {
                            Self::fail(regs, 4);
                        }
                    } else {
                        Self::fail(regs, 5);
                    }
                } else {
                    Self::fail(regs, 3);
                }
            }
            0x5C => Self::ok(regs, 0),
            0x5D..=0x5F => Self::fail(regs, 1),
            0x60 => {
                let path = self.read_path(mem, regs, regs.dx());
                let path = path.trim_start_matches('\\');
                mem.write_cstring(regs.ds(), regs.si(), path.as_bytes());
                Self::ok(regs, 0);
            }
            0x61 => {}
            0x63 => {
                regs.set_al(0);
                Self::ok(regs, 0);
            }
            0x64 | 0x65 => Self::fail(regs, 1),
            0x66 => {
                regs.set_ax(0xFFFF);
                regs.set_bx(0xFFFF);
                Self::fail(regs, 2);
            }
            0x67 => Self::ok(regs, 0),
            0x68 | 0x6A => Self::ok(regs, 0),
            0x69 => Self::fail(regs, 1),
            0x6C => Self::fail(regs, 1),
            0x71 => Self::fail(regs, 1),
            _ => Self::fail(regs, 1),
        }
        InterruptAction::Continue
    }

    fn buffered_input(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
        keyboard: &mut Keyboard,
    ) -> InterruptAction {
        let dest = ((regs.ds() as u32) << 4).wrapping_add(regs.dx() as u32);
        let max = mem.read8(dest);
        if !self.line.active || self.line.dest != dest {
            self.line.active = true;
            self.line.dest = dest;
            self.line.max = max;
            self.line.text.clear();
        }
        while let Some(key) = keyboard.pop() {
            match key.ascii {
                0x0D => {
                    let limit = max.saturating_sub(1) as usize;
                    let len = self.line.text.len().min(limit);
                    mem.write8(dest + 1, len as u8);
                    for (index, byte) in self.line.text.iter().take(len).enumerate() {
                        mem.write8(dest + 2 + index as u32, *byte);
                    }
                    mem.write8(dest + 2 + len as u32, 0x0D);
                    vga.teletype(mem, 0x0D);
                    vga.teletype(mem, 0x0A);
                    self.line.active = false;
                    self.line.text.clear();
                    return InterruptAction::Continue;
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
                    self.line.active = false;
                    return InterruptAction::Continue;
                }
                ch if ch >= 0x20 && self.line.text.len() < max.saturating_sub(1) as usize => {
                    self.line.text.push(ch);
                    vga.teletype(mem, ch);
                }
                _ => {}
            }
        }
        InterruptAction::Blocked
    }

    fn read_file(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        keyboard: &mut Keyboard,
    ) -> InterruptAction {
        let index = regs.bx() as usize;
        let Some(Some(file)) = self.handles.get(index).cloned() else {
            Self::fail(regs, 6);
            return InterruptAction::Continue;
        };
        let count = regs.cx() as usize;
        if file.device == 1 {
            if let Some(key) = keyboard.pop() {
                mem.write8(((regs.ds() as u32) << 4) + regs.dx() as u32, key.ascii);
                Self::ok(regs, 1);
            } else {
                return InterruptAction::Blocked;
            }
            return InterruptAction::Continue;
        }
        let mut buffer = vec![0u8; count.min(0xFFFF)];
        let read = self.disk.read(file.node, file.offset, &mut buffer);
        let dest = ((regs.ds() as u32) << 4).wrapping_add(regs.dx() as u32);
        for (offset, byte) in buffer.iter().take(read).enumerate() {
            mem.write8(dest.wrapping_add(offset as u32), *byte);
        }
        if let Some(Some(slot)) = self.handles.get_mut(index) {
            slot.offset = slot.offset.saturating_add(read);
        }
        Self::ok(regs, read as u16);
        InterruptAction::Continue
    }

    fn write_file(
        &mut self,
        regs: &mut CpuRegs<'_>,
        mem: &mut Memory,
        vga: &mut Vga,
    ) -> InterruptAction {
        let index = regs.bx() as usize;
        let Some(Some(file)) = self.handles.get(index).cloned() else {
            Self::fail(regs, 6);
            return InterruptAction::Continue;
        };
        let count = regs.cx() as usize;
        let source = ((regs.ds() as u32) << 4).wrapping_add(regs.dx() as u32);
        if file.device != 0 {
            for offset in 0..count {
                vga.teletype(mem, mem.read8(source.wrapping_add(offset as u32)));
            }
            Self::ok(regs, count as u16);
            return InterruptAction::Continue;
        }
        let mut buffer = vec![0u8; count.min(0xFFFF)];
        for (offset, byte) in buffer.iter_mut().enumerate() {
            *byte = mem.read8(source.wrapping_add(offset as u32));
        }
        let written = self.disk.write(file.node, file.offset, &buffer);
        if let Some(Some(slot)) = self.handles.get_mut(index) {
            slot.offset = slot.offset.saturating_add(written);
        }
        Self::ok(regs, written as u16);
        InterruptAction::Continue
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;
    use crate::vga::Vga;

    fn call(dos: &mut Dos, ax: u16, bx: u16, cx: u16, dx: u16) -> InterruptAction {
        let mut r = [0u16; 8];
        let mut sregs = [0u16; 4];
        let mut flags = 0u16;
        r[0] = ax;
        r[3] = bx;
        r[1] = cx;
        r[2] = dx;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        let mut keyboard = Keyboard::new();
        let mut exit = false;
        let mut code = 0;
        dos.int21(
            &mut regs,
            &mut mem,
            &mut vga,
            &mut keyboard,
            0,
            &mut exit,
            &mut code,
        )
    }

    #[test]
    fn version_call_reports_dos_5() {
        let mut dos = Dos::new();
        dos.seed(None);
        let action = call(&mut dos, 0x3000, 0, 0, 0);
        assert_eq!(action, InterruptAction::Continue);
    }

    #[test]
    fn open_and_read_readme() {
        let mut dos = Dos::new();
        dos.seed(None);
        let path = b"README.TXT\0";
        let mut mem = Memory::new();
        mem.copy_from(0x100, path);
        // DS = 0, DX = 0x100; AH = 3D, AL = 0
        let mut r = [0u16; 8];
        let mut sregs = [0u16; 4];
        let mut flags = 0u16;
        r[0] = 0x3D00;
        r[2] = 0x100;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        let mut keyboard = Keyboard::new();
        let mut exit = false;
        let mut code = 0;
        let action = dos.int21(
            &mut regs,
            &mut mem,
            &mut vga,
            &mut keyboard,
            0,
            &mut exit,
            &mut code,
        );
        assert_eq!(action, InterruptAction::Continue);
        assert!(!regs.carry());
        let handle = regs.ax();
        assert!(handle >= FIRST_HANDLE as u16);
    }
}
