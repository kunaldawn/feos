//! Changes to the identity map `boot.S` builds.
//!
//! The map is 2 MiB pages over the first 4 GiB. Taking a page out needs a 4 KiB
//! table in its place, so this splits what it has to. It runs on the boot CPU,
//! before the others start, and reloads CR3 afterwards.

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

use feos_hal::PageBits;

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;
const LARGE_PAGE: usize = 2 * 1024 * 1024;
const ENTRIES: usize = 512;
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const LARGE: u64 = 1 << 7;
/// The uncached attributes: write-through and cache-disable.
const CACHE_OFF: u64 = 0x18;
/// Bits a 2 MiB page and a 4 KiB page share the meaning of: present, writable,
/// user, write-through and cache-disable.
const SHARED_FLAGS: u64 = 0x1f;
/// Address bits of a table entry.
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
/// One past the addresses the map covers.
const MAPPED: usize = 0x1_0000_0000;

extern "C" {
    /// 2048 entries, one per 2 MiB of the first 4 GiB.
    static mut page_directories: [u64; 2048];
    /// The boot page tables' top two levels, which every process shares the kernel's
    /// half of.
    static pml4: [u64; ENTRIES];
    static pdpt: [u64; ENTRIES];
}

fn directory_entry(address: usize) -> *mut u64 {
    // SAFETY: the caller has checked `address` is inside the map, and the array is
    // static. Taking the address doesn't touch it.
    unsafe { addr_of_mut!(page_directories).cast::<u64>().add(address / LARGE_PAGE) }
}

/// Maps `[base, base + size)`, RAM in the part of the map that's uncached for device
/// windows, as ordinary memory. Whole 2 MiB pages only: any partial one at the end stays
/// uncached, and RAM there works, if slowly.
///
/// # Safety
/// The range must be RAM, and no other CPU may be running.
pub unsafe fn map_ram_cached(base: usize, size: usize) {
    let end = base.saturating_add(size).min(MAPPED);
    let mut address = base.next_multiple_of(LARGE_PAGE);
    while address + LARGE_PAGE <= end {
        let entry = directory_entry(address);
        // SAFETY: the entry is in the static table, and is a large page: nothing's been
        // split yet above 2 GiB, and if it had it'd be left alone.
        unsafe {
            let value = read_volatile(entry);
            if value & LARGE != 0 {
                write_volatile(entry, value & !CACHE_OFF);
            }
        }
        address += LARGE_PAGE;
    }
    // SAFETY: reloading CR3 flushes the TLB, and changes nothing else.
    unsafe {
        asm!("mov {tmp}, cr3", "mov cr3, {tmp}", tmp = out(reg) _, options(nostack, preserves_flags));
    }
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
    if !base.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) || end > MAPPED {
        return false;
    }

    let mut address = base;
    while address < end {
        let entry = directory_entry(address);
        // SAFETY: the entry is in the static table.
        let mut value = unsafe { read_volatile(entry) };
        if value & LARGE != 0 {
            let Some(table) = alloc_table() else {
                return false;
            };
            let flags = value & SHARED_FLAGS;
            let start = value & ADDRESS;
            for index in 0..ENTRIES {
                // SAFETY: the caller gave this page to be a table.
                unsafe {
                    (table as *mut u64)
                        .add(index)
                        .write_volatile((start + (index * PAGE_SIZE) as u64) | flags);
                }
            }
            value = table as u64 | PRESENT | WRITABLE;
            // SAFETY: the same memory is mapped the same way either side of this.
            unsafe { write_volatile(entry, value) };
        }
        let table = (value & ADDRESS) as *mut u64;
        let index = address / PAGE_SIZE % ENTRIES;
        // SAFETY: `value` is a table entry here, and `index` is inside the table.
        unsafe { table.add(index).write_volatile(0) };
        address += PAGE_SIZE;
    }

    // SAFETY: rewriting CR3 with its own value only flushes the TLB.
    unsafe {
        asm!(
            "mov {scratch}, cr3",
            "mov cr3, {scratch}",
            scratch = out(reg) _,
            options(nostack, preserves_flags)
        );
    }
    true
}

const USER: u64 = 1 << 2;
const NO_EXECUTE: u64 = 1 << 63;

/// How a user page table entry is written.
pub fn user_bits() -> PageBits {
    PageBits {
        present: PRESENT,
        table: PRESENT | WRITABLE | USER,
        leaf: PRESENT | USER,
        writable: WRITABLE,
        read_only: 0,
        no_execute: if crate::cpu::has_nx() { NO_EXECUTE } else { 0 },
        address: ADDRESS,
    }
}

/// Puts a process's tables, `tables` being its page directory pointer table, under a
/// new top table. The kernel's first 4 GiB are in the same table as user memory. The
/// entries for it don't have the user bit, which makes all of it unreachable from ring
/// 3 whatever's below them.
///
/// # Safety
/// `tables` must be a table given over to this, past boot's changes to the kernel's.
pub unsafe fn prepare_user_tables(
    tables: usize,
    alloc_table: &mut dyn FnMut() -> Option<usize>,
) -> Option<usize> {
    let top = alloc_table()?;
    // SAFETY: as the caller promises for `tables`, and `alloc_table` for `top`.
    unsafe {
        core::ptr::write_bytes(top as *mut u8, 0, PAGE_SIZE);
        for index in 0..4 {
            let entry = addr_of!(pdpt).cast::<u64>().add(index).read_volatile();
            (tables as *mut u64).add(index).write_volatile(entry);
        }
        (top as *mut u64).write_volatile(tables as u64 | PRESENT | WRITABLE | USER);
    }
    Some(top)
}

/// Loads a process's tables, or the kernel's with `root` zero. There are no PCIDs here:
/// every change of tables flushes the TLB, which is why nothing has to be told when a
/// space goes away.
pub fn activate(root: usize) {
    let wanted = if root == 0 { addr_of!(pml4) as u64 } else { root as u64 };
    let current: u64;
    // SAFETY: every set of tables maps the kernel the same way, so what's running stays
    // where it is.
    unsafe {
        asm!("mov {}, cr3", out(reg) current, options(nomem, nostack, preserves_flags));
        if current & ADDRESS != wanted {
            asm!("mov cr3, {}", in(reg) wanted, options(nostack, preserves_flags));
        }
    }
}

/// Forgets one page on this CPU. No other CPU can have it: a process has one thread, and
/// the CPU it last ran on flushed everything when it changed tables.
pub fn flush_page(address: usize) {
    // SAFETY: invalidating a TLB entry has no other effect.
    unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}
