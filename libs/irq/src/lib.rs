//! Which device handles which interrupt.
//!
//! An interrupt ID is whatever the machine's controller reports: a vector on
//! x86, an INTID on a GIC. The board decides the routing, then registers a handler
//! for the ID here, and the kernel's IRQ entry calls [`dispatch`] with whatever
//! the controller acknowledged. Neither side needs to know the other.
//!
//! A line can be shared, as PCI INTx lines are, so an ID can have several handlers
//! and each one has to check its own device. Handlers run with this CPU's
//! interrupts masked and on its interrupt stack: they mustn't block, and should do
//! no more than quiet the device and wake whoever's waiting.
//!
//! Devices are found at boot and never leave, so handlers are never removed. That
//! lets the interrupt path read the table without a lock.

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Handlers the machine can have. A small table is searched faster than a large
/// one is kept consistent.
pub const MAX_HANDLERS: usize = 32;

/// A device's handler, and what it was registered with.
pub type Handler = fn(usize);

#[derive(Clone, Copy)]
struct Entry {
    id: u32,
    name: &'static str,
    handler: Handler,
    data: usize,
}

fn unused(_data: usize) {}

const EMPTY: Entry = Entry {
    id: u32::MAX,
    name: "",
    handler: unused,
    data: 0,
};

/// One row of [`IrqTable::for_each`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IrqInfo {
    /// The controller's ID.
    pub id: u32,
    /// What the board called it.
    pub name: &'static str,
    /// How many times its handler has run.
    pub count: u64,
}

/// The handlers registered so far.
pub struct IrqTable {
    entries: UnsafeCell<[Entry; MAX_HANDLERS]>,
    counts: [AtomicU64; MAX_HANDLERS],
    /// Entries below this are written and never change.
    published: AtomicUsize,
    /// Serializes registration, as a bit that's held. Spun on: it's boot code.
    registering: AtomicUsize,
    /// IDs dispatched that nothing handles.
    unclaimed: AtomicU64,
}

// SAFETY: an entry is written by one registrant, before `published` moves past it.
// After that it's only read.
unsafe impl Sync for IrqTable {}

impl IrqTable {
    /// An empty table.
    pub const fn new() -> Self {
        Self {
            entries: UnsafeCell::new([EMPTY; MAX_HANDLERS]),
            counts: [const { AtomicU64::new(0) }; MAX_HANDLERS],
            published: AtomicUsize::new(0),
            registering: AtomicUsize::new(0),
            unclaimed: AtomicU64::new(0),
        }
    }

    /// Adds a handler for `id`. Returns `false` when the table is full.
    pub fn register(&self, id: u32, name: &'static str, handler: Handler, data: usize) -> bool {
        while self
            .registering
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let index = self.published.load(Ordering::Relaxed);
        let added = index < MAX_HANDLERS;
        if added {
            // SAFETY: the flag makes this the only writer, and readers stop at
            // `published`, which is `index` until the store below.
            unsafe {
                (*self.entries.get())[index] = Entry { id, name, handler, data };
            }
            self.published.store(index + 1, Ordering::Release);
        }
        self.registering.store(0, Ordering::Release);
        added
    }

    fn published(&self) -> &[Entry] {
        let count = self.published.load(Ordering::Acquire);
        // SAFETY: entries below `published` never change again, and the Acquire
        // pairs with the Release that published them.
        unsafe { &(&*self.entries.get())[..count] }
    }

    /// Runs every handler for `id`. Returns `false` if there are none.
    pub fn dispatch(&self, id: u32) -> bool {
        let mut any = false;
        for (index, entry) in self.published().iter().enumerate() {
            if entry.id == id {
                self.counts[index].fetch_add(1, Ordering::Relaxed);
                (entry.handler)(entry.data);
                any = true;
            }
        }
        if !any {
            self.unclaimed.fetch_add(1, Ordering::Relaxed);
        }
        any
    }

    /// Calls `visit` for each registered handler.
    pub fn for_each(&self, mut visit: impl FnMut(IrqInfo)) {
        for (index, entry) in self.published().iter().enumerate() {
            visit(IrqInfo {
                id: entry.id,
                name: entry.name,
                count: self.counts[index].load(Ordering::Relaxed),
            });
        }
    }

    /// Interrupts that arrived with nothing registered to handle them.
    pub fn unclaimed(&self) -> u64 {
        self.unclaimed.load(Ordering::Relaxed)
    }
}

impl Default for IrqTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The machine's table.
pub static TABLE: IrqTable = IrqTable::new();

/// Adds a handler to the machine's table.
pub fn register(id: u32, name: &'static str, handler: Handler, data: usize) -> bool {
    TABLE.register(id, name, handler, data)
}

/// Runs the machine's handlers for `id`.
pub fn dispatch(id: u32) -> bool {
    TABLE.dispatch(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bump(data: usize) {
        // SAFETY: tests pass the address of a live counter.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn dispatch_reaches_the_right_handler_with_its_data() {
        let table = IrqTable::new();
        let (first, second) = (AtomicUsize::new(0), AtomicUsize::new(0));
        assert!(table.register(5, "a", bump, &first as *const _ as usize));
        assert!(table.register(6, "b", bump, &second as *const _ as usize));
        assert!(table.dispatch(6));
        assert_eq!(first.load(Ordering::Relaxed), 0);
        assert_eq!(second.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_shared_line_runs_every_handler() {
        let table = IrqTable::new();
        let (first, second) = (AtomicUsize::new(0), AtomicUsize::new(0));
        table.register(7, "a", bump, &first as *const _ as usize);
        table.register(7, "b", bump, &second as *const _ as usize);
        table.dispatch(7);
        assert_eq!(first.load(Ordering::Relaxed), 1);
        assert_eq!(second.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_unknown_id_is_counted_not_fatal() {
        let table = IrqTable::new();
        assert!(!table.dispatch(9));
        assert_eq!(table.unclaimed(), 1);
    }

    #[test]
    fn the_table_refuses_what_it_cannot_hold() {
        let table = IrqTable::new();
        for index in 0..MAX_HANDLERS {
            assert!(table.register(index as u32, "x", unused, 0));
        }
        assert!(!table.register(99, "x", unused, 0));
        assert!(!table.dispatch(99));
    }

    #[test]
    fn counts_are_reported_per_handler() {
        let table = IrqTable::new();
        table.register(1, "one", unused, 0);
        table.register(2, "two", unused, 0);
        table.dispatch(2);
        table.dispatch(2);
        let mut rows = Vec::new();
        table.for_each(|row| rows.push(row));
        assert_eq!(
            rows,
            [
                IrqInfo { id: 1, name: "one", count: 0 },
                IrqInfo { id: 2, name: "two", count: 2 },
            ]
        );
    }
}
