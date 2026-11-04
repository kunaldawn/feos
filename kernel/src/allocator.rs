//! The kernel heap: one static arena behind [`feos_heap`].
//!
//! The `StaticFs` image and the GUI are allocation-free, but the H.264 decoder
//! used by the video player and the writable FAT32/exFAT mounts use `alloc`
//! collections. The algorithm lives in `feos-heap` so its tests run on the host;
//! this file adds the arena, the lock and per-process accounting.
//!
//! The lock masks interrupts: a holder can't be preempted, so other tasks never
//! burn their time slice waiting for a descheduled one, and a task can't be
//! killed in the middle of updating the free list.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use feos_heap::{Heap, Stats};
use feos_sync::{IrqSpinLock, IrqSpinLockGuard};

use crate::sched;

/// Size of the static heap in bytes.
///
/// The default QEMU configuration boots with 512 MiB of RAM; the arena sits in
/// `.bss`, so the boot CPU's BSS clear and the linker script bound its size.
/// 64 MiB leaves room for several H.264 reference-picture sets while keeping
/// boot fast and leaving the rest of guest RAM unused for now.
const HEAP_SIZE: usize = 64 * 1024 * 1024;

#[repr(align(16))]
struct HeapStorage([u8; HEAP_SIZE]);

static mut STORAGE: HeapStorage = HeapStorage([0; HEAP_SIZE]);

static HEAP: IrqSpinLock<Heap> = IrqSpinLock::new(Heap::empty());

/// Locks the heap, creating the free list on first use.
fn locked() -> IrqSpinLockGuard<'static, Heap> {
    let mut heap = HEAP.lock();
    if !heap.is_initialized() {
        // SAFETY: the arena is only reached through this heap, and taking the
        // address of the static does not create a reference.
        unsafe { heap.init(ptr::addr_of_mut!(STORAGE.0).cast::<u8>(), HEAP_SIZE) };
    }
    heap
}

/// Returns the current heap usage.
pub fn stats() -> Stats {
    locked().stats()
}

struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = locked();
        // Interrupts are masked, so the owner can't change under this.
        let owner = sched::current_pid_fast();
        let (allocation, size) = heap.alloc_sized(layout, owner);
        sched::charge_heap(owner, size as isize);
        allocation
    }

    unsafe fn dealloc(&self, allocation: *mut u8, _layout: Layout) {
        let mut heap = locked();
        let result = heap.free(allocation);
        if let Ok(freed) = result {
            sched::charge_heap(freed.owner, -(freed.size as isize));
        }
        drop(heap);
        if let Err(error) = result {
            panic!("heap corruption: free of {allocation:p}: {error:?}");
        }
    }

    unsafe fn realloc(&self, allocation: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let mut heap = locked();
        let result = heap.resize(allocation, new_size);
        if let Ok(Some(resized)) = result {
            sched::charge_heap(
                resized.owner,
                resized.new_size as isize - resized.old_size as isize,
            );
        }
        drop(heap);
        match result {
            Ok(Some(_)) => return allocation,
            Ok(None) => {}
            Err(error) => panic!("heap corruption: realloc of {allocation:p}: {error:?}"),
        }

        // SAFETY: the caller guarantees `new_size` is valid for this alignment.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: as for any GlobalAlloc: both blocks are live and don't overlap.
        unsafe {
            let moved = self.alloc(new_layout);
            if !moved.is_null() {
                ptr::copy_nonoverlapping(allocation, moved, layout.size().min(new_size));
                self.dealloc(allocation, layout);
            }
            moved
        }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
