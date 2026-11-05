//! Changes to the identity map `boot.S` builds.
//!
//! The map is 1 GiB blocks. Taking a page out needs a level 2 table of 2 MiB blocks
//! and a level 3 table of 4 KiB pages in its place, so this splits what it has to.
//!
//! It runs on the boot CPU, before the others start. The architecture wants a block
//! replaced by way of an invalid entry and a TLB invalidation, break-before-make, and
//! that can't be done to the block this code is running from. So a block is replaced
//! with the MMU off, in [`replace_descriptor`]: see `boot.S`. QEMU's TCG, the only
//! place this has run, wouldn't have minded either way.

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut, read_volatile};

use feos_hal::PageBits;

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;
const L2_BLOCK: usize = 2 * 1024 * 1024;
const L1_BLOCK: usize = 1024 * 1024 * 1024;
const ENTRIES: usize = 512;
/// Descriptor types, in the low two bits.
const TYPE_MASK: u64 = 0b11;
const TYPE_BLOCK: u64 = 0b01;
const TYPE_TABLE: u64 = 0b11;
const TYPE_PAGE: u64 = 0b11;
/// Output address bits of a descriptor, for 40-bit addresses.
const ADDRESS: u64 = 0x0000_00ff_ffff_f000;
/// One past the addresses the top level covers.
const MAPPED: usize = ENTRIES * L1_BLOCK;

extern "C" {
    static mut page_table: [u64; ENTRIES];
    /// Puts `value` in the descriptor at `entry`, with the MMU off.
    fn replace_descriptor(entry: *mut u64, value: u64);
}

/// Mask interrupts for as long as the MMU is off: nothing's mapped for a handler.
///
/// # Safety
/// As for `replace_descriptor`.
unsafe fn swap_block(entry: *mut u64, value: u64) {
    let flags: u64;
    // SAFETY: reading and setting the mask has no other effect, and the table's been
    // written to memory before it's reached by the barrier here.
    unsafe {
        asm!("mrs {}, daif", "msr daifset, #0xf", "dsb ishst", out(reg) flags, options(nostack, preserves_flags));
        replace_descriptor(entry, value);
        asm!("msr daif, {}", in(reg) flags, options(nostack, preserves_flags));
    }
}

/// Fills a new table with the same mapping as a block covering it, and returns the
/// descriptor pointing at the table.
///
/// # Safety
/// `table` must be a page given over to being a table.
unsafe fn split(block: u64, table: usize, stride: usize, kind: u64) -> u64 {
    let attributes = block & !ADDRESS & !TYPE_MASK;
    let start = block & ADDRESS;
    for index in 0..ENTRIES {
        // SAFETY: as promised by the caller.
        unsafe {
            (table as *mut u64)
                .add(index)
                .write_volatile((start + (index * stride) as u64) | attributes | kind);
        }
    }
    table as u64 | TYPE_TABLE
}

/// Takes `[base, base + size)` out of the map, page by page, so that touching any
/// of it faults. `alloc_table` provides a page for each new table. Returns `false`
/// when the range is unaligned or outside the map, or there are no more tables.
///
/// # Safety
/// Nothing may use the range again, and no other CPU may be running.
pub unsafe fn unmap_range(
    base: usize,
    size: usize,
    alloc_table: &mut dyn FnMut() -> Option<usize>,
) -> bool {
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    if base % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 || end > MAPPED {
        return false;
    }

    let mut address = base;
    while address < end {
        // SAFETY: the index is inside the static table, as `end` was checked.
        let l1 = unsafe { addr_of_mut!(page_table).cast::<u64>().add(address / L1_BLOCK) };
        // SAFETY: as above. The rest of this walks what the entries point to.
        let mut value = unsafe { read_volatile(l1) };
        if value & TYPE_MASK == TYPE_BLOCK {
            let Some(table) = alloc_table() else {
                return false;
            };
            // SAFETY: `alloc_table` gave this page to be a table.
            value = unsafe { split(value, table, L2_BLOCK, TYPE_BLOCK) };
            // SAFETY: see the module's comment.
            unsafe { swap_block(l1, value) };
        }
        if value & TYPE_MASK != TYPE_TABLE {
            return false;
        }

        let l2 = (value & ADDRESS) as *mut u64;
        // SAFETY: the index is inside the table the entry above points to.
        let l2 = unsafe { l2.add(address / L2_BLOCK % ENTRIES) };
        // SAFETY: as above.
        let mut value = unsafe { read_volatile(l2) };
        if value & TYPE_MASK == TYPE_BLOCK {
            let Some(table) = alloc_table() else {
                return false;
            };
            // SAFETY: `alloc_table` gave this page to be a table.
            value = unsafe { split(value, table, PAGE_SIZE, TYPE_PAGE) };
            // SAFETY: see the module's comment.
            unsafe { swap_block(l2, value) };
        }
        if value & TYPE_MASK != TYPE_TABLE {
            return false;
        }

        let l3 = (value & ADDRESS) as *mut u64;
        // SAFETY: the index is inside the table the entry above points to.
        unsafe { l3.add(address / PAGE_SIZE % ENTRIES).write_volatile(0) };
        address += PAGE_SIZE;
    }

    // SAFETY: invalidating the TLB has no other effect.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        );
    }
    true
}

/// A user page: access flag, inner shareable, write-back, not global, so that it only
/// counts for its ASID. The kernel may never run it, and a program can reach it.
const USER_PAGE: u64 = TYPE_PAGE | (1 << 2) | (3 << 8) | (1 << 10) | (1 << 11) | (1 << 6) | (1 << 53);

/// How a user page table entry is written.
pub const USER_BITS: PageBits = PageBits {
    present: 1,
    table: TYPE_TABLE,
    leaf: USER_PAGE,
    writable: 0,
    read_only: 1 << 7,
    no_execute: 1 << 54,
    address: ADDRESS,
};

/// Puts the kernel's top-level entries in a process's top table. They point at the
/// kernel's own tables, for EL1 only, so a program's view of them is that they aren't
/// there. No window a process can use overlaps them: see `USER_BASE` in `feos-abi`.
///
/// # Safety
/// `tables` must be a table given over to this, past boot's changes to the kernel's.
pub unsafe fn prepare_user_tables(tables: usize) -> usize {
    let target = tables as *mut u64;
    for index in 0..ENTRIES {
        // SAFETY: both are tables of `ENTRIES`, and the kernel's no longer changes.
        unsafe {
            let entry = addr_of!(page_table).cast::<u64>().add(index).read_volatile();
            if entry & 1 != 0 {
                target.add(index).write_volatile(entry);
            }
        }
    }
    tables
}

/// Loads a process's tables, or the kernel's with `root` zero.
pub fn activate(root: usize, asid: u16) {
    let value = if root == 0 {
        addr_of!(page_table) as u64
    } else {
        root as u64 | (u64::from(asid) << 48)
    };
    // SAFETY: every set of tables maps the kernel the same way, so what's running stays
    // where it is. A program can leave anything in TPIDR_EL0, so it's cleared on the way
    // past, or it would carry a message to the next one.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, {}",
            "isb",
            "msr tpidr_el0, xzr",
            in(reg) value,
            options(nostack, preserves_flags)
        );
    }
}

/// Forgets a space on every CPU.
pub fn flush_space(asid: u16) {
    // SAFETY: invalidating the TLB has no other effect.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi aside1is, {}",
            "dsb ish",
            "isb",
            in(reg) u64::from(asid) << 48,
            options(nostack, preserves_flags)
        );
    }
}

/// Forgets one page of a space on every CPU.
pub fn flush_page(address: usize, asid: u16) {
    let operand = (u64::from(asid) << 48) | (address as u64 >> 12);
    // SAFETY: invalidating the TLB has no other effect.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vae1is, {}",
            "dsb ish",
            "isb",
            in(reg) operand,
            options(nostack, preserves_flags)
        );
    }
}

/// Makes table writes visible to the walker.
pub fn sync_tables() {
    // SAFETY: a barrier has no other effect.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
}

/// Makes code written through the kernel's mapping safe to run.
pub fn sync_code(address: usize, length: usize) {
    let cache_type: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) cache_type, options(nomem, nostack)) };
    let line = 4usize << ((cache_type >> 16) & 0xf);
    let mut at = address & !(line - 1);
    while at < address + length {
        // SAFETY: the range is mapped, and cleaning a cache line changes no data.
        unsafe { asm!("dc cvau, {}", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    // SAFETY: invalidating instruction caches has no other effect.
    unsafe { asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack, preserves_flags)) };
}
