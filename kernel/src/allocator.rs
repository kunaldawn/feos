//! A small first-fit heap for the kernel.
//!
//! The GUI and filesystem code are allocation-free, but the H.264 decoder used
//! by the video player stores reference pictures in `alloc` collections. A
//! real allocator is therefore needed before video playback can work.
//!
//! This allocator is intentionally simple: one 4 MiB static arena, a sorted
//! free list with boundary tags in front of each block, and a spin lock for the
//! preemptive scheduler. Allocation and deallocation are O(n) in the number of
//! free blocks, which is fine for the decoder's short-lived picture buffers.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use feos_sync::SpinLock;

/// Size of the static heap in bytes.
const HEAP_SIZE: usize = 16 * 1024 * 1024;
/// Header stored immediately before every allocated payload.
const HEADER: usize = 16;
/// Sentinel for "no block".
const NONE: usize = usize::MAX;

#[repr(align(16))]
struct HeapStorage([u8; HEAP_SIZE]);

static mut HEAP: HeapStorage = HeapStorage([0; HEAP_SIZE]);

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    /// Total block size including this header.
    size: usize,
    /// Offset of the next free block, or `NONE`.
    next: usize,
}

struct Heap {
    head: usize,
    initialized: bool,
}

// SAFETY: access to the Heap state is serialized by HEAP_LOCK.
unsafe impl Send for Heap {}

static HEAP_LOCK: SpinLock<Heap> = SpinLock::new(Heap {
    head: NONE,
    initialized: false,
});

fn base() -> usize {
    // SAFETY: taking the address of the static does not create a reference.
    unsafe { ptr::addr_of_mut!(HEAP.0).cast::<u8>() as usize }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

impl Heap {
    /// Writes the initial single-block free list on first use.
    fn initialize(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        self.head = 0;
        // SAFETY: offset 0 is inside the static arena and the list is empty.
        unsafe {
            write_header(
                0,
                Header {
                    size: HEAP_SIZE,
                    next: NONE,
                },
            );
        }
    }

    /// Allocates one aligned block from the free list.
    ///
    /// # Safety
    /// `layout` must describe a valid allocation request.
    unsafe fn allocate(&mut self, layout: Layout) -> *mut u8 {
        self.initialize();
        if layout.size() == 0 {
            return ptr::null_mut();
        }

        let align = layout.align().max(HEADER);
        let size = align_up(layout.size(), HEADER).unwrap_or(usize::MAX);

        let mut previous = NONE;
        let mut current = self.head;
        while current != NONE {
            // SAFETY: `current` comes from the free list and lies inside the
            // arena. All links are written by this allocator.
            let header = unsafe { read_header(current) };
            let block_end = match current.checked_add(header.size) {
                Some(end) if end <= HEAP_SIZE => end,
                _ => break,
            };

            let payload = match align_up(current + HEADER, align) {
                Some(payload) if payload >= current + HEADER => payload,
                _ => break,
            };
            let header_offset = payload - HEADER;
            let prefix = header_offset - current;
            let end = match payload.checked_add(size) {
                Some(end) => end,
                None => break,
            };

            // A prefix smaller than a header cannot be returned to the free
            // list. In practice alignments are 16-byte multiples, so this only
            // guards against malformed layouts.
            if prefix > 0 && prefix < HEADER {
                previous = current;
                current = header.next;
                continue;
            }

            if end <= block_end {
                let tail_offset = end;
                let tail_size = block_end - end;

                if prefix >= HEADER {
                    // Keep the alignment padding as a free block and put the
                    // allocated header directly before the payload.
                    unsafe {
                        write_header(
                            current,
                            Header {
                                size: prefix,
                                next: header.next,
                            },
                        );
                    }
                    if tail_size >= HEADER {
                        unsafe {
                            write_header(
                                tail_offset,
                                Header {
                                    size: tail_size,
                                    next: header.next,
                                },
                            );
                            write_header(
                                current,
                                Header {
                                    size: prefix,
                                    next: tail_offset,
                                },
                            );
                        }
                    }
                } else {
                    // The header sits at the start of the free block.
                    if previous == NONE {
                        self.head = header.next;
                    } else {
                        unsafe {
                            let mut before = read_header(previous);
                            before.next = header.next;
                            write_header(previous, before);
                        }
                    }
                    if tail_size >= HEADER {
                        unsafe {
                            write_header(
                                tail_offset,
                                Header {
                                    size: tail_size,
                                    next: header.next,
                                },
                            );
                            if previous == NONE {
                                self.head = tail_offset;
                            } else {
                                let mut before = read_header(previous);
                                before.next = tail_offset;
                                write_header(previous, before);
                            }
                        }
                    }
                }

                unsafe {
                    write_header(
                        header_offset,
                        Header {
                            size: end - header_offset,
                            next: NONE,
                        },
                    );
                }
                return (base() + header_offset + HEADER) as *mut u8;
            }

            previous = current;
            current = header.next;
        }

        ptr::null_mut()
    }

    /// Returns a block to the free list, merging adjacent neighbours.
    ///
    /// # Safety
    /// `allocation` must point to the payload of a block from this allocator.
    unsafe fn free(&mut self, allocation: *mut u8) {
        if !self.initialized || allocation.is_null() {
            return;
        }
        let address = allocation as usize;
        let arena = base();
        if address < arena + HEADER || address >= arena + HEAP_SIZE {
            return;
        }

        let header_offset = address - arena - HEADER;
        let header = unsafe { read_header(header_offset) };
        if header.size < HEADER || header_offset + header.size > HEAP_SIZE {
            return;
        }

        let mut previous = NONE;
        let mut current = self.head;
        while current != NONE && current < header_offset {
            previous = current;
            current = unsafe { read_header(current) }.next;
        }
        // Double free would corrupt the list; a block that is already in the
        // free list is visible because its `next` is not NONE or its neighbour
        // points at it. Keep the common case cheap and ignore obvious repeats.
        if current == header_offset {
            return;
        }

        unsafe {
            write_header(
                header_offset,
                Header {
                    size: header.size,
                    next: current,
                },
            );
            if previous == NONE {
                self.head = header_offset;
            } else {
                let mut before = read_header(previous);
                before.next = header_offset;
                write_header(previous, before);
            }
        }

        let mut size = header.size;
        let mut link_next = current;

        // Merge with the following block.
        if current != NONE {
            let next = unsafe { read_header(current) };
            if header_offset + size == current {
                size += next.size;
                link_next = next.next;
                unsafe {
                    write_header(
                        header_offset,
                        Header {
                            size,
                            next: link_next,
                        },
                    );
                }
            }
        }

        // Merge with the preceding block. Its `next` must skip this block,
        // which is no longer a valid entry after the merge above.
        if previous != NONE {
            let mut before = unsafe { read_header(previous) };
            if previous + before.size == header_offset {
                before.size += size;
                before.next = link_next;
                unsafe {
                    write_header(previous, before);
                }
            }
        }
    }
}

unsafe fn read_header(offset: usize) -> Header {
    unsafe { ptr::read((base() + offset) as *const Header) }
}

unsafe fn write_header(offset: usize, header: Header) {
    unsafe {
        ptr::write((base() + offset) as *mut Header, header);
    }
}

struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP_LOCK.lock().allocate(layout)
    }

    unsafe fn dealloc(&self, allocation: *mut u8, _layout: Layout) {
        HEAP_LOCK.lock().free(allocation);
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_handles_power_of_two_alignment() {
        assert_eq!(align_up(0, 16), Some(0));
        assert_eq!(align_up(1, 16), Some(16));
        assert_eq!(align_up(16, 16), Some(16));
        assert_eq!(align_up(17, 16), Some(32));
    }
}
