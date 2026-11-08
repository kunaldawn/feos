//! Enough of the ACPI tables to start CPUs and route interrupts: the RSDP search,
//! the root table walk and the MADT.
//!
//! Every length here comes from the firmware, so each is checked against a sane
//! bound before it's used, and checksums are verified on what's read.

use crate::{field, read_u32, read_u64, PhysMem};

/// Size of the fixed RSDP, and of the extended one with revision 2 and up.
const RSDP_V1_SIZE: usize = 20;
const RSDP_V2_SIZE: usize = 36;
/// The size every table header shares.
const HEADER_SIZE: u32 = 36;
/// Larger than any table this kernel reads, so a corrupt length can't send it
/// walking through memory.
const MAX_TABLE: u32 = 64 * 1024;
/// Root-table entries looked at.
const MAX_ROOT_ENTRIES: u32 = 256;

/// CPUs a [`Madt`] keeps. More than the kernel can run on, so it can tell what it
/// left out.
pub const MAX_MADT_CPUS: usize = 64;
/// IOAPICs a [`Madt`] keeps.
pub const MAX_IOAPICS: usize = 8;
/// Interrupt source overrides a [`Madt`] keeps. ISA has 16 lines.
pub const MAX_OVERRIDES: usize = 16;

/// Where the root table is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rsdp {
    /// Physical address of the RSDT, always present.
    pub rsdt: u32,
    /// Physical address of the XSDT, from revision 2 up, when it's set.
    pub xsdt: Option<u64>,
}

fn checksum_ok<M: PhysMem + ?Sized>(mem: &M, address: u64, length: u32) -> bool {
    let mut sum = 0u8;
    let mut chunk = [0u8; 64];
    let mut done = 0u32;
    while done < length {
        let take = length.saturating_sub(done).min(chunk.len() as u32) as usize;
        let Some(bytes) = chunk.get_mut(..take) else {
            return false;
        };
        let Some(at) = address.checked_add(u64::from(done)) else {
            return false;
        };
        if !mem.read(at, bytes) {
            return false;
        }
        sum = bytes.iter().fold(sum, |sum, byte| sum.wrapping_add(*byte));
        // At least one byte, as `done < length`.
        done = done.saturating_add(take as u32);
    }
    sum == 0
}

pub fn rsdp_at<M: PhysMem + ?Sized>(mem: &M, address: u64) -> Option<Rsdp> {
    let mut bytes = [0u8; RSDP_V2_SIZE];
    let (head, _) = bytes.split_at_mut(RSDP_V1_SIZE);
    if !mem.read(address, head) || head.get(..8) != Some(b"RSD PTR ") {
        return None;
    }
    if head.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return None;
    }
    let revision = *head.get(15)?;
    let rsdt = u32::from_le_bytes(field(head, 16)?);

    let mut xsdt = None;
    if revision >= 2 {
        let length = read_u32(mem, address.checked_add(20)?)?;
        if (RSDP_V2_SIZE as u32..=1024).contains(&length) && checksum_ok(mem, address, length) {
            xsdt = read_u64(mem, address.checked_add(24)?)?.checked_add(0).filter(|&x| x != 0);
        }
    }
    Some(Rsdp { rsdt, xsdt })
}

/// Finds the RSDP where a BIOS leaves it: the first KiB of the EBDA, or the top
/// of the first MiB, on a 16-byte boundary.
pub fn find_rsdp<M: PhysMem + ?Sized>(mem: &M) -> Option<Rsdp> {
    let mut segment = [0u8; 2];
    if mem.read(0x40e, &mut segment) {
        let ebda = u64::from(u16::from_le_bytes(segment)) << 4;
        if ebda != 0 {
            for offset in (0..1024).step_by(16) {
                if let Some(rsdp) = ebda.checked_add(offset).and_then(|at| rsdp_at(mem, at)) {
                    return Some(rsdp);
                }
            }
        }
    }
    (0xe_0000u64..0x10_0000)
        .step_by(16)
        .find_map(|address| rsdp_at(mem, address))
}

/// Finds a table by signature and returns its address and length.
pub fn find_table<M: PhysMem + ?Sized>(mem: &M, rsdp: &Rsdp, signature: &[u8; 4]) -> Option<(u64, u32)> {
    let (root, entry_size) = match rsdp.xsdt {
        Some(xsdt) => (xsdt, 8u32),
        None => (u64::from(rsdp.rsdt), 4),
    };
    let length = table_length(mem, root)?;
    // `table_length` checked `length` is at least a header.
    let count = length.saturating_sub(HEADER_SIZE).checked_div(entry_size).unwrap_or(0).min(MAX_ROOT_ENTRIES);
    for index in 0..count {
        let at = root
            .checked_add(u64::from(HEADER_SIZE))?
            .checked_add(u64::from(index.checked_mul(entry_size)?))?;
        let table = if entry_size == 8 {
            read_u64(mem, at)?
        } else {
            u64::from(read_u32(mem, at)?)
        };
        let mut found = [0u8; 4];
        if mem.read(table, &mut found) && &found == signature {
            if let Some(length) = table_length(mem, table) {
                return Some((table, length));
            }
        }
    }
    None
}

/// A table's length, once it's plausible and its checksum holds.
fn table_length<M: PhysMem + ?Sized>(mem: &M, table: u64) -> Option<u32> {
    let length = read_u32(mem, table.checked_add(4)?)?;
    ((HEADER_SIZE..=MAX_TABLE).contains(&length) && checksum_ok(mem, table, length)).then_some(length)
}

/// An IOAPIC from the MADT.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IoApicEntry {
    /// Its MMIO base.
    pub address: u32,
    /// The first interrupt line it owns.
    pub gsi_base: u32,
}

/// An ISA interrupt wired somewhere other than the same-numbered GSI.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InterruptOverride {
    /// ISA IRQ number.
    pub source: u8,
    /// The GSI it arrives on.
    pub gsi: u32,
    /// MPS flags: polarity in bits 0-1, trigger mode in bits 2-3.
    pub flags: u16,
}

/// What the MADT says about the machine's CPUs and interrupt hardware.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Madt {
    /// Local APIC MMIO base.
    pub lapic_address: u64,
    apic_ids: [u32; MAX_MADT_CPUS],
    cpu_count: usize,
    ioapics: [IoApicEntry; MAX_IOAPICS],
    ioapic_count: usize,
    overrides: [InterruptOverride; MAX_OVERRIDES],
    override_count: usize,
}

impl Madt {
    /// The APIC ID of each usable CPU, in MADT order.
    pub fn apic_ids(&self) -> &[u32] {
        self.apic_ids.get(..self.cpu_count).unwrap_or(&[])
    }

    /// The machine's IOAPICs.
    pub fn ioapics(&self) -> &[IoApicEntry] {
        self.ioapics.get(..self.ioapic_count).unwrap_or(&[])
    }

    /// The interrupt source overrides.
    pub fn overrides(&self) -> &[InterruptOverride] {
        self.overrides.get(..self.override_count).unwrap_or(&[])
    }

    /// The GSI an ISA IRQ arrives on, and its MPS flags.
    pub fn isa_gsi(&self, irq: u8) -> (u32, u16) {
        self.overrides()
            .iter()
            .find(|entry| entry.source == irq)
            .map_or((u32::from(irq), 0), |entry| (entry.gsi, entry.flags))
    }
}

/// Reads the MADT.
pub fn parse_madt<M: PhysMem + ?Sized>(mem: &M, rsdp: &Rsdp) -> Option<Madt> {
    let (table, length) = find_table(mem, rsdp, b"APIC")?;
    let mut buffer = [0u8; 4096];
    let length = (length as usize).min(buffer.len());
    let bytes = buffer.get_mut(..length)?;
    if !mem.read(table, bytes) {
        return None;
    }
    parse_madt_bytes(bytes)
}

fn parse_madt_bytes(bytes: &[u8]) -> Option<Madt> {
    let mut madt = Madt {
        lapic_address: u64::from(u32::from_le_bytes(field(bytes, 36)?)),
        apic_ids: [0; MAX_MADT_CPUS],
        cpu_count: 0,
        ioapics: [IoApicEntry::default(); MAX_IOAPICS],
        ioapic_count: 0,
        overrides: [InterruptOverride::default(); MAX_OVERRIDES],
        override_count: 0,
    };

    // The local APIC address and flags come first, then the entries.
    let mut offset = 44usize;
    while let Some(&[kind, size]) = bytes.get(offset..offset.checked_add(2)?).and_then(|b| <&[u8; 2]>::try_from(b).ok()) {
        let size = usize::from(size);
        let entry = match size {
            0..=1 => break,
            _ => match bytes.get(offset..offset.saturating_add(size)) {
                Some(entry) => entry,
                None => break,
            },
        };
        match kind {
            // Processor local APIC.
            0 => {
                if let (Some(&id), Some(flags)) = (entry.get(3), field::<4>(entry, 4)) {
                    add_cpu(&mut madt, u32::from(id), u32::from_le_bytes(flags));
                }
            }
            // IOAPIC.
            1 => {
                if let (Some(address), Some(gsi_base)) = (field::<4>(entry, 4), field::<4>(entry, 8)) {
                    if let Some(slot) = madt.ioapics.get_mut(madt.ioapic_count) {
                        *slot = IoApicEntry {
                            address: u32::from_le_bytes(address),
                            gsi_base: u32::from_le_bytes(gsi_base),
                        };
                        madt.ioapic_count = madt.ioapic_count.saturating_add(1);
                    }
                }
            }
            // Interrupt source override.
            2 => {
                if let (Some(&source), Some(gsi), Some(flags)) =
                    (entry.get(3), field::<4>(entry, 4), field::<2>(entry, 8))
                {
                    if let Some(slot) = madt.overrides.get_mut(madt.override_count) {
                        *slot = InterruptOverride {
                            source,
                            gsi: u32::from_le_bytes(gsi),
                            flags: u16::from_le_bytes(flags),
                        };
                        madt.override_count = madt.override_count.saturating_add(1);
                    }
                }
            }
            // 64-bit local APIC address override.
            5 => {
                if let Some(address) = field::<8>(entry, 4) {
                    madt.lapic_address = u64::from_le_bytes(address);
                }
            }
            // Processor local x2APIC.
            9 => {
                if let (Some(id), Some(flags)) = (field::<4>(entry, 4), field::<4>(entry, 8)) {
                    add_cpu(&mut madt, u32::from_le_bytes(id), u32::from_le_bytes(flags));
                }
            }
            _ => {}
        }
        offset = offset.saturating_add(size);
    }
    Some(madt)
}

fn add_cpu(madt: &mut Madt, id: u32, flags: u32) {
    // Enabled, or online-capable: either can be started. An xAPIC can't address
    // the broadcast ID or above.
    if flags & 0b11 == 0 || id >= 0xff {
        return;
    }
    if let Some(slot) = madt.apic_ids.get_mut(madt.cpu_count) {
        *slot = id;
        madt.cpu_count = madt.cpu_count.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Tables a real q35 with `-smp 4` built, from SeaBIOS and QEMU, at the
    /// addresses it put them.
    const RSDP: &[u8] = include_bytes!("../fixtures/q35-smp4-rsdp.bin");
    const RSDT: &[u8] = include_bytes!("../fixtures/q35-smp4-rsdt.bin");
    const MADT: &[u8] = include_bytes!("../fixtures/q35-smp4-madt.bin");
    const RSDP_AT: u64 = 0xf59e0;
    const RSDT_AT: u64 = 0x1ffe_23cc;
    const MADT_AT: u64 = 0x1ffe_22a0;

    struct Blobs(Vec<(u64, Vec<u8>)>);

    impl PhysMem for Blobs {
        fn read(&self, address: u64, out: &mut [u8]) -> bool {
            for (base, blob) in &self.0 {
                if address >= *base {
                    let offset = (address - base) as usize;
                    if let Some(bytes) = offset
                        .checked_add(out.len())
                        .and_then(|end| blob.get(offset..end))
                    {
                        out.copy_from_slice(bytes);
                        return true;
                    }
                }
            }
            false
        }
    }

    fn machine() -> Blobs {
        Blobs(vec![
            (RSDP_AT, RSDP.to_vec()),
            (RSDT_AT, RSDT.to_vec()),
            (MADT_AT, MADT.to_vec()),
        ])
    }

    #[test]
    fn finds_the_rsdp_in_the_bios_area() {
        let rsdp = find_rsdp(&machine()).unwrap();
        assert_eq!(rsdp.rsdt as u64, RSDT_AT);
        assert_eq!(rsdp.xsdt, None);
    }

    #[test]
    fn finds_tables_by_signature() {
        let mem = machine();
        let rsdp = find_rsdp(&mem).unwrap();
        assert_eq!(find_table(&mem, &rsdp, b"APIC"), Some((MADT_AT, MADT.len() as u32)));
        // Listed in the root, but its bytes aren't in this memory.
        assert_eq!(find_table(&mem, &rsdp, b"HPET"), None);
        assert_eq!(find_table(&mem, &rsdp, b"NONE"), None);
    }

    #[test]
    fn reads_the_real_madt() {
        let mem = machine();
        let madt = parse_madt(&mem, &find_rsdp(&mem).unwrap()).unwrap();
        assert_eq!(madt.lapic_address, 0xfee0_0000);
        assert_eq!(madt.apic_ids(), &[0, 1, 2, 3]);
        assert_eq!(madt.ioapics(), &[IoApicEntry { address: 0xfec0_0000, gsi_base: 0 }]);
        // QEMU wires the PIT to GSI 2 and marks the PCI-ish lines level triggered.
        assert_eq!(madt.isa_gsi(0).0, 2);
        assert_eq!(madt.isa_gsi(1), (1, 0));
        assert_eq!(madt.isa_gsi(12), (12, 0));
    }

    /// A bad checksum makes the table invisible, however good it looks.
    #[test]
    fn checksums_are_enforced() {
        let mut mem = machine();
        mem.0[2].1[50] ^= 1;
        let rsdp = find_rsdp(&mem).unwrap();
        assert_eq!(find_table(&mem, &rsdp, b"APIC"), None);

        let mut mem = machine();
        mem.0[0].1[3] ^= 1;
        assert_eq!(find_rsdp(&mem), None);
    }

    #[test]
    fn disabled_cpus_and_broadcast_ids_are_skipped() {
        let mut bytes = MADT.to_vec();
        // Entry 0 starts at 44: type, length, ACPI id, APIC id, flags.
        bytes[44 + 4] = 0;
        let madt = parse_madt_bytes(&bytes).unwrap();
        assert_eq!(madt.apic_ids(), &[1, 2, 3]);
        let mut bytes = MADT.to_vec();
        bytes[44 + 3] = 0xff;
        assert_eq!(parse_madt_bytes(&bytes).unwrap().apic_ids(), &[1, 2, 3]);
    }

    #[test]
    fn zero_length_entries_end_the_walk() {
        for size in [0u8, 1] {
            let mut bytes = MADT.to_vec();
            bytes[45] = size;
            assert_eq!(parse_madt_bytes(&bytes).unwrap().apic_ids(), &[] as &[u32]);
        }
    }

    #[test]
    fn every_truncation_and_flip_is_survivable() {
        for cut in 0..MADT.len() {
            let _ = parse_madt_bytes(&MADT[..cut]);
        }
        for index in 0..MADT.len() {
            for value in [0u8, 1, 0x7f, 0xff] {
                let mut bytes = MADT.to_vec();
                bytes[index] = value;
                let _ = parse_madt_bytes(&bytes);
                // And through the walk, with the checksum left wrong or not.
                let mut mem = machine();
                mem.0[2].1 = bytes;
                let rsdp = find_rsdp(&mem).unwrap();
                let _ = parse_madt(&mem, &rsdp);
            }
        }
        for index in 0..RSDT.len() {
            let mut bytes = RSDT.to_vec();
            bytes[index] ^= 0xff;
            let mut mem = machine();
            mem.0[1].1 = bytes;
            if let Some(rsdp) = find_rsdp(&mem) {
                let _ = find_table(&mem, &rsdp, b"APIC");
            }
        }
    }

    #[test]
    fn nothing_there_finds_nothing() {
        assert_eq!(find_rsdp(&Blobs(vec![])), None);
    }

    #[test]
    fn a_huge_root_length_is_rejected() {
        let mut mem = machine();
        mem.0[1].1[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let rsdp = find_rsdp(&mem).unwrap();
        assert_eq!(find_table(&mem, &rsdp, b"APIC"), None);
    }
}
