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

/// Bytes one page directory pointer table entry covers.
const DIRECTORY_SPAN: usize = 1 << 30;

/// Zeroes a page given over to being a table.
///
/// # Safety
/// `table` must be a page nothing else uses.
unsafe fn clear_table(table: usize) {
    // SAFETY: as promised by the caller.
    unsafe { core::ptr::write_bytes(table as *mut u8, 0, PAGE_SIZE) };
}

/// See `Arch::reserve_kernel_window`. The window's PDPT entry gets a page directory, and
/// that a page table for each 2 MiB, all from the start: a process's PDPT copies the
/// kernel's entries when it's made, so only what's below them can change later, and
/// nothing below them is ever allocated or freed.
///
/// # Safety
/// As for `Arch::reserve_kernel_window`.
pub unsafe fn reserve_kernel_window(
    base: usize,
    size: usize,
    alloc_table: &mut dyn FnMut() -> Option<usize>,
) -> bool {
    if !base.is_multiple_of(DIRECTORY_SPAN)
        || size == 0
        || size > DIRECTORY_SPAN
        || !size.is_multiple_of(LARGE_PAGE)
        || base / DIRECTORY_SPAN >= ENTRIES
    {
        return false;
    }
    let entry = addr_of!(pdpt).cast::<u64>().cast_mut().wrapping_add(base / DIRECTORY_SPAN);
    // SAFETY: the index is inside the static table, as `base` was checked.
    if unsafe { read_volatile(entry) } & PRESENT != 0 {
        return false;
    }
    let Some(directory) = alloc_table() else {
        return false;
    };
    // SAFETY: `alloc_table` gave this page to be a table.
    unsafe { clear_table(directory) };
    for index in 0..size / LARGE_PAGE {
        let Some(table) = alloc_table() else {
            return false;
        };
        // SAFETY: as above, for both tables.
        unsafe {
            clear_table(table);
            (directory as *mut u64).add(index).write_volatile(table as u64 | PRESENT | WRITABLE);
        }
    }
    // Not present to present: nothing to flush. Not user, so ring 3 never reaches it.
    // SAFETY: the PDPT is the kernel's own, writable, and its entry was free.
    unsafe { write_volatile(entry, directory as u64 | PRESENT | WRITABLE) };
    true
}

/// The page table entry for `address` in a kernel window, or `None` if it isn't in one.
fn window_entry(address: usize) -> Option<*mut u64> {
    if address / DIRECTORY_SPAN >= ENTRIES {
        return None;
    }
    // SAFETY: the index is inside the static table; the tables below a window's entry
    // never change once made.
    unsafe {
        let top = read_volatile(addr_of!(pdpt).cast::<u64>().add(address / DIRECTORY_SPAN));
        if top & PRESENT == 0 || top & LARGE != 0 {
            return None;
        }
        let directory = read_volatile(((top & ADDRESS) as *const u64).add(address / LARGE_PAGE % ENTRIES));
        if directory & PRESENT == 0 || directory & LARGE != 0 {
            return None;
        }
        Some(((directory & ADDRESS) as *mut u64).add(address / PAGE_SIZE % ENTRIES))
    }
}

/// See `Arch::map_kernel_page`.
///
/// # Safety
/// As for `Arch::map_kernel_page`.
pub unsafe fn map_kernel_page(address: usize, frame: usize) {
    let Some(entry) = window_entry(address) else {
        return;
    };
    let no_execute = if crate::cpu::has_nx() { NO_EXECUTE } else { 0 };
    // SAFETY: the entry is the window's, and the caller's to change. A page that wasn't
    // present can't be cached, so there's nothing to flush.
    unsafe { write_volatile(entry, frame as u64 & ADDRESS | PRESENT | WRITABLE | no_execute) };
}

/// See `Arch::unmap_kernel_page`. Only this CPU forgets the page: the caller shoots it
/// down elsewhere.
///
/// # Safety
/// As for `Arch::unmap_kernel_page`.
pub unsafe fn unmap_kernel_page(address: usize) {
    let Some(entry) = window_entry(address) else {
        return;
    };
    // SAFETY: as above; invalidating a TLB entry has no other effect.
    unsafe {
        write_volatile(entry, 0);
        asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
    }
}

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
/// new top table. The kernel's mappings below 512 GiB are in the same table as user
/// memory: every present entry of the kernel's is copied, as AArch64 copies its whole
/// top table, so what [`kernel_map_end`] counts is what a process has, and the boot check
/// keeps those entries below the window. They don't have the user bit, which makes all
/// of it unreachable from ring 3 whatever's below them.
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
        for index in 0..ENTRIES {
            let entry = addr_of!(pdpt).cast::<u64>().add(index).read_volatile();
            if entry & PRESENT != 0 {
                (tables as *mut u64).add(index).write_volatile(entry);
            }
        }
        (top as *mut u64).write_volatile(tables as u64 | PRESENT | WRITABLE | USER);
    }
    Some(top)
}

/// One past the highest address the kernel's tables map anything at. A process's top
/// table has only entry 0, so a mapping under any other would be missing in a process;
/// it counts here, past the window, so the boot check catches it. Its PDPT has every
/// present entry of the kernel's.
pub fn kernel_map_end() -> usize {
    const PDPT_SPAN: usize = 1 << 30;
    const PML4_SPAN: usize = 1 << 39;
    let present = |table: *const u64, index: usize| {
        // SAFETY: both tables have `ENTRIES`, and only boot changes them.
        unsafe { table.add(index).read_volatile() & PRESENT != 0 }
    };
    let top = addr_of!(pml4).cast::<u64>();
    if let Some(index) = (1..ENTRIES).rev().find(|&index| present(top, index)) {
        return (index + 1) * PML4_SPAN;
    }
    let second = addr_of!(pdpt).cast::<u64>();
    (0..ENTRIES).rev().find(|&index| present(second, index)).map_or(0, |index| (index + 1) * PDPT_SPAN)
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

/// Forgets one page on this CPU. Other CPUs that have the same tables loaded may have
/// it too: the kernel asks them to [`flush_local`] with an IPI. A CPU that has changed
/// tables since has forgotten it already, with everything else.
pub fn flush_page(address: usize) {
    // SAFETY: invalidating a TLB entry has no other effect.
    unsafe { asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

/// Forgets every user translation on this CPU. There are no global user pages, so
/// reloading CR3 with what it holds does it.
pub fn flush_local() {
    // SAFETY: rewriting CR3 with its own value only flushes the TLB.
    unsafe {
        asm!("mov {tmp}, cr3", "mov cr3, {tmp}", tmp = out(reg) _, options(nostack, preserves_flags));
    }
}

/// Reads a word of the loaded user tables, with SMAP lifted for the one load.
///
/// # Safety
/// `address` is mapped readable and aligned in the loaded tables.
pub unsafe fn read_user_word(address: usize) -> u64 {
    let value: u64;
    if crate::cpu::has_smep_smap().1 {
        // SAFETY: as the caller promises; `stac` and `clac` only change RFLAGS.AC, and
        // interrupts can't see it set since each saves and restores RFLAGS.
        unsafe {
            asm!("stac", "mov {value}, [{address}]", "clac", value = out(reg) value, address = in(reg) address, options(nostack, readonly))
        };
    } else {
        // SAFETY: as the caller promises.
        unsafe { asm!("mov {value}, [{address}]", value = out(reg) value, address = in(reg) address, options(nostack, readonly, preserves_flags)) };
    }
    value
}
