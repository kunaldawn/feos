//! A flattened device tree reader.
//!
//! A tree is a blob the kernel can't trust: every read here is checked, nesting
//! is bounded, and anything malformed ends the walk.

use crate::{Region, RegionList};

const MAGIC: u32 = 0xd00d_feed;
const TOKEN_BEGIN: u32 = 1;
const TOKEN_END: u32 = 2;
const TOKEN_PROP: u32 = 3;
const TOKEN_NOP: u32 = 4;
const TOKEN_FINISH: u32 = 9;
/// How PSCI is reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PsciMethod {
    /// By hypervisor call.
    Hvc,
    /// By secure monitor call.
    Smc,
}

/// Deepest nesting walked. Trees in the wild stop well short of this.
const MAX_DEPTH: usize = 16;
/// Largest tree believed. `dumpdtb` shows QEMU's padded blob at 1 MiB.
pub const MAX_SIZE: usize = 4 * 1024 * 1024;

/// A tree's header fields, as read from memory before the blob is trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Whole blob, in bytes.
    pub total_size: u32,
}

/// Reads the magic and size from the start of a blob.
pub fn header(start: &[u8; 8]) -> Option<Header> {
    let magic = u32::from_be_bytes([start[0], start[1], start[2], start[3]]);
    let total_size = u32::from_be_bytes([start[4], start[5], start[6], start[7]]);
    (magic == MAGIC && total_size as usize <= MAX_SIZE).then_some(Header { total_size })
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let word: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_be_bytes(word))
}

/// A parsed tree over its blob.
#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    structure: &'a [u8],
    strings: &'a [u8],
}

/// What one node's walk reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'a> {
    /// A node starts, at this depth (the root is 0).
    Begin { name: &'a [u8], depth: usize },
    /// A property of the node just begun.
    Property { name: &'a [u8], value: &'a [u8], depth: usize },
    /// The node at this depth ends.
    End { depth: usize },
}

/// Which interrupt controller a tree describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gic {
    /// A GICv2: distributor and CPU interface, and a v2m MSI frame if it has one.
    V2 { distributor: Region, cpu: Region, v2m: Option<Region> },
    /// A GICv3: distributor and redistributors.
    V3 { distributor: Region, redistributor: Region },
}

impl<'a> Fdt<'a> {
    /// Checks the header and locates the blocks. `None` for anything malformed.
    pub fn new(blob: &'a [u8]) -> Option<Self> {
        let start: &[u8; 8] = blob.get(..8)?.try_into().ok()?;
        let total = header(start)?.total_size as usize;
        let blob = blob.get(..total)?;
        let struct_offset = be32(blob, 8)? as usize;
        let strings_offset = be32(blob, 12)? as usize;
        let version = be32(blob, 20)?;
        if version < 16 {
            return None;
        }
        let strings_size = be32(blob, 32)? as usize;
        let struct_size = be32(blob, 36)? as usize;
        Some(Self {
            structure: blob.get(struct_offset..struct_offset.checked_add(struct_size)?)?,
            strings: blob.get(strings_offset..strings_offset.checked_add(strings_size)?)?,
        })
    }

    /// Calls `visit` for every node and property, until the tree ends or stops
    /// making sense. Returns `false` in the second case.
    pub fn walk(&self, mut visit: impl FnMut(Event<'a>)) -> bool {
        let mut offset = 0usize;
        let mut depth = 0usize;
        loop {
            let Some(token) = be32(self.structure, offset) else {
                return false;
            };
            // `be32` read four bytes there, so none of these steps can wrap.
            offset = offset.saturating_add(4);
            match token {
                TOKEN_BEGIN => {
                    let Some(rest) = self.structure.get(offset..) else {
                        return false;
                    };
                    let Some(length) = rest.iter().position(|byte| *byte == 0) else {
                        return false;
                    };
                    let Some(name) = rest.get(..length) else {
                        return false;
                    };
                    if depth >= MAX_DEPTH {
                        return false;
                    }
                    visit(Event::Begin { name, depth });
                    depth = depth.saturating_add(1);
                    offset = offset.saturating_add(length).saturating_add(4) & !3;
                }
                TOKEN_END => {
                    let Some(next) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = next;
                    visit(Event::End { depth });
                }
                TOKEN_PROP => {
                    let (Some(size), Some(name_offset)) =
                        (be32(self.structure, offset), be32(self.structure, offset.saturating_add(4)))
                    else {
                        return false;
                    };
                    offset = offset.saturating_add(8);
                    let size = size as usize;
                    let Some(end) = offset.checked_add(size) else {
                        return false;
                    };
                    let Some(value) = self.structure.get(offset..end) else {
                        return false;
                    };
                    let Some(name) = self.string(name_offset as usize) else {
                        return false;
                    };
                    if depth == 0 {
                        return false;
                    }
                    visit(Event::Property { name, value, depth: depth.saturating_sub(1) });
                    let Some(next) = end.checked_add(3) else {
                        return false;
                    };
                    offset = next & !3;
                }
                TOKEN_NOP => {}
                TOKEN_FINISH => return depth == 0,
                _ => return false,
            }
        }
    }

    fn string(&self, offset: usize) -> Option<&'a [u8]> {
        let rest = self.strings.get(offset..)?;
        rest.get(..rest.iter().position(|byte| *byte == 0)?)
    }

    /// The root's `#address-cells` and `#size-cells`. The spec's defaults are 2
    /// and 1, though real trees state them.
    fn root_cells(&self) -> (u32, u32) {
        let mut cells = (2, 1);
        self.walk(|event| {
            if let Event::Property { name, value, depth: 0 } = event {
                if let Some(count) = be32(value, 0) {
                    match name {
                        b"#address-cells" => cells.0 = count,
                        b"#size-cells" => cells.1 = count,
                        _ => {}
                    }
                }
            }
        });
        cells
    }

    /// RAM, from the `memory` nodes under the root.
    pub fn memory(&self) -> RegionList {
        let cells = self.root_cells();
        let mut list = RegionList::new();
        let mut in_memory = false;
        self.walk(|event| match event {
            Event::Begin { name, depth: 1 } => {
                in_memory = name == b"memory" || name.starts_with(b"memory@");
            }
            Event::End { depth: 1 } => in_memory = false,
            Event::Property { name: b"reg", value, depth: 1 } if in_memory => {
                for region in Regs::new(value, cells) {
                    list.push(region);
                }
            }
            _ => {}
        });
        list
    }

    /// CPUs: the children of `/cpus` whose names start with `cpu@`.
    pub fn cpu_count(&self) -> usize {
        let mut in_cpus = false;
        let mut count = 0usize;
        self.walk(|event| match event {
            Event::Begin { name, depth: 1 } => in_cpus = name == b"cpus",
            Event::End { depth: 1 } => in_cpus = false,
            Event::Begin { name, depth: 2 } if in_cpus && name.starts_with(b"cpu@") => count = count.saturating_add(1),
            _ => {}
        });
        count
    }

    /// How `/psci` says it's reached: its `method`, or `None` with no such node, which
    /// is what a machine with no PSCI looks like.
    pub fn psci_method(&self) -> Option<PsciMethod> {
        let mut in_psci = false;
        let mut found = None;
        self.walk(|event| match event {
            Event::Begin { name, depth: 1 } => in_psci = name == b"psci",
            Event::End { depth: 1 } => in_psci = false,
            Event::Property { name: b"method", value, depth: 1 } if in_psci => {
                if has_string(value, b"smc") {
                    found = Some(PsciMethod::Smc);
                } else if has_string(value, b"hvc") {
                    found = Some(PsciMethod::Hvc);
                }
            }
            _ => {}
        });
        found
    }

    /// The first interrupt controller found directly under the root.
    pub fn gic(&self) -> Option<Gic> {
        let root = self.root_cells();
        let mut found = None;
        // Fields of the depth-1 node being read.
        let mut version = 0u8;
        let mut reg: &[u8] = &[];
        let mut cells = root;
        let mut v2m = None;
        let mut frame = false;
        let mut frame_reg: &[u8] = &[];
        self.walk(|event| match event {
            Event::Begin { depth: 1, .. } => {
                version = 0;
                reg = &[];
                cells = root;
                v2m = None;
            }
            Event::Begin { depth: 2, .. } => {
                frame = false;
                frame_reg = &[];
            }
            Event::Property { name, value, depth: 1 } => match name {
                b"compatible" if has_string(value, b"arm,gic-v3") => version = 3,
                b"compatible"
                    if has_string(value, b"arm,cortex-a15-gic")
                        || has_string(value, b"arm,gic-400") =>
                {
                    version = 2;
                }
                b"reg" => reg = value,
                b"#address-cells" => {
                    if let Some(count) = be32(value, 0) {
                        cells.0 = count;
                    }
                }
                b"#size-cells" => {
                    if let Some(count) = be32(value, 0) {
                        cells.1 = count;
                    }
                }
                _ => {}
            },
            Event::Property { name, value, depth: 2 } if version == 2 => match name {
                b"compatible" => frame = has_string(value, b"arm,gic-v2m-frame"),
                b"reg" => frame_reg = value,
                _ => {}
            },
            // A frame's `reg` uses the controller's cell sizes.
            Event::End { depth: 2 } if frame && v2m.is_none() => {
                v2m = Regs::new(frame_reg, cells).next();
            }
            Event::End { depth: 1 } if found.is_none() && version != 0 => {
                let mut regs = Regs::new(reg, root);
                let (first, second) = (regs.next(), regs.next());
                if let (Some(first), Some(second)) = (first, second) {
                    found = Some(if version == 3 {
                        Gic::V3 { distributor: first, redistributor: second }
                    } else {
                        Gic::V2 { distributor: first, cpu: second, v2m }
                    });
                }
            }
            _ => {}
        });
        found
    }
}

/// Whether a NUL-separated string list contains `wanted`.
fn has_string(list: &[u8], wanted: &[u8]) -> bool {
    list.split(|byte| *byte == 0).any(|entry| entry == wanted)
}

/// `(address, size)` pairs from a `reg` property.
struct Regs<'a> {
    value: &'a [u8],
    offset: usize,
    address_cells: usize,
    size_cells: usize,
}

impl<'a> Regs<'a> {
    fn new(value: &'a [u8], cells: (u32, u32)) -> Self {
        Self {
            value,
            offset: 0,
            address_cells: cells.0 as usize,
            size_cells: cells.1 as usize,
        }
    }

    fn cells(&mut self, count: usize) -> Option<u64> {
        // Anything over two cells can't be a 64-bit value this kernel could use.
        if count > 2 {
            return None;
        }
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 32) | u64::from(be32(self.value, self.offset)?);
            self.offset = self.offset.checked_add(4)?;
        }
        Some(value)
    }
}

impl Iterator for Regs<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        let base = self.cells(self.address_cells)?;
        let size = self.cells(self.size_cells)?;
        Some(Region { base, size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real trees from `qemu-system-aarch64 -machine virt,dumpdtb=...` with
    /// `-m 512M -smp 4`, trimmed of the padding QEMU leaves after the strings.
    const GICV2: &[u8] = include_bytes!("../fixtures/virt-gicv2.dtb");
    const GICV3: &[u8] = include_bytes!("../fixtures/virt-gicv3.dtb");

    #[test]
    fn reads_memory_from_real_trees() {
        for blob in [GICV2, GICV3] {
            let fdt = Fdt::new(blob).unwrap();
            assert_eq!(
                fdt.memory().as_slice(),
                &[Region { base: 0x4000_0000, size: 512 * 1024 * 1024 }]
            );
        }
    }

    #[test]
    fn finds_the_psci_method() {
        assert_eq!(Fdt::new(GICV2).unwrap().psci_method(), Some(PsciMethod::Hvc));
        assert_eq!(Fdt::new(GICV3).unwrap().psci_method(), Some(PsciMethod::Hvc));
    }

    #[test]
    fn counts_cpus() {
        assert_eq!(Fdt::new(GICV2).unwrap().cpu_count(), 4);
        assert_eq!(Fdt::new(GICV3).unwrap().cpu_count(), 4);
    }

    #[test]
    fn tells_the_gic_versions_apart() {
        assert_eq!(
            Fdt::new(GICV2).unwrap().gic(),
            Some(Gic::V2 {
                distributor: Region { base: 0x0800_0000, size: 0x1_0000 },
                cpu: Region { base: 0x0801_0000, size: 0x1_0000 },
                v2m: Some(Region { base: 0x0802_0000, size: 0x1000 }),
            })
        );
        let Some(Gic::V3 { distributor, redistributor }) = Fdt::new(GICV3).unwrap().gic() else {
            panic!("expected a GICv3");
        };
        assert_eq!(distributor.base, 0x0800_0000);
        assert_eq!(redistributor.base, 0x080a_0000);
    }

    #[test]
    fn walks_a_whole_tree_and_balances() {
        let mut begins = 0;
        let mut ends = 0;
        let whole = Fdt::new(GICV2).unwrap().walk(|event| match event {
            Event::Begin { .. } => begins += 1,
            Event::End { .. } => ends += 1,
            Event::Property { .. } => {}
        });
        assert!(whole);
        assert!(begins > 10);
        assert_eq!(begins, ends);
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(Fdt::new(&[]).is_none());
        assert!(Fdt::new(&GICV2[..7]).is_none());
        let mut bad = GICV2.to_vec();
        bad[0] ^= 1;
        assert!(Fdt::new(&bad).is_none());
        // Claiming to be longer than the blob.
        let mut bad = GICV2.to_vec();
        bad[4..8].copy_from_slice(&(GICV2.len() as u32 + 1).to_be_bytes());
        assert!(Fdt::new(&bad).is_none());
        // Offsets that wrap, or point outside.
        for field in [8usize, 12, 32, 36] {
            let mut bad = GICV2.to_vec();
            bad[field..field + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(Fdt::new(&bad).is_none());
        }
    }

    /// Every truncation and a sweep of byte changes: answers may be wrong, but a
    /// panic or a hang isn't allowed.
    #[test]
    fn corrupt_trees_never_panic() {
        let exercise = |blob: &[u8]| {
            if let Some(fdt) = Fdt::new(blob) {
                let _ = fdt.memory();
                let _ = fdt.cpu_count();
                let _ = fdt.gic();
            }
        };
        for cut in (0..GICV2.len()).step_by(7) {
            exercise(&GICV2[..cut]);
            // Keep the header consistent, so the walk itself sees the cut.
            let mut shortened = GICV2[..cut].to_vec();
            if cut >= 8 {
                shortened[4..8].copy_from_slice(&(cut as u32).to_be_bytes());
            }
            exercise(&shortened);
        }
        for index in (0..GICV3.len()).step_by(3) {
            for value in [0u8, 0xff, 1] {
                let mut bad = GICV3.to_vec();
                bad[index] = value;
                exercise(&bad);
            }
        }
    }
}
