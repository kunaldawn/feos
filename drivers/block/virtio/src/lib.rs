//! A virtio block device on either transport.
//!
//! One request runs at a time: the caller owns the device, and a request is
//! finished before the call returns. With an interrupt, from
//! [`use_interrupts`](VirtioBlk::use_interrupts), the waiting task blocks. Without
//! one, or with interrupts masked as they are early in boot, it polls and calls
//! `relax` now and then, so other tasks run.
//!
//! Multi-block transfers go out as one request, up to [`MAX_REQUEST_BYTES`]. The
//! buffers are the caller's: every board identity-maps RAM, so an address is the
//! bus address. They must stay put, which the borrow guarantees.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, Ordering};
use feos_hal::dma::{self, DmaBuf};
use feos_hal::{BlockIo, BlockIoError};
use feos_virtio_core::{device, finish, negotiate, Queue, Segment, Transport};

/// Descriptors in the request queue. A request takes three.
pub const QUEUE_SIZE: usize = 16;

/// Largest transfer in one request.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Requests address the disk in units of this, whatever the block size.
const SECTOR_SIZE: u64 = 512;

/// How many polls of the used ring before a request counts as lost. Generous,
/// because a host can stall for seconds on a loaded disk.
const POLL_LIMIT: u64 = 1 << 34;
/// Polls between offers of the CPU to other tasks.
const POLLS_PER_RELAX: u64 = 1024;
/// How long a task waits for an interrupt before looking anyway. A device is
/// allowed to complete a request without one reaching this CPU, so a request can't
/// be left to depend on it.
const WAIT_MS: u64 = 100;
/// What one such wait counts for against [`POLL_LIMIT`]: about five minutes' worth.
const WAIT_COST: u64 = POLL_LIMIT / 3000;

mod feature {
    pub const SIZE_MAX: u64 = 1 << 1;
    pub const RO: u64 = 1 << 5;
    pub const BLK_SIZE: u64 = 1 << 6;
    pub const FLUSH: u64 = 1 << 9;
}

mod config {
    pub const CAPACITY: usize = 0;
    pub const SIZE_MAX: usize = 8;
    pub const BLK_SIZE: usize = 20;
}

mod request {
    pub const READ: u32 = 0;
    pub const WRITE: u32 = 1;
    pub const FLUSH: u32 = 4;
}

const STATUS_OK: u8 = 0;
/// A value no device writes, to tell an answered request from an ignored one.
const STATUS_UNSET: u8 = 0xff;

#[repr(C)]
#[derive(Clone, Copy)]
struct RequestHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// What the device reads and writes other than the caller's buffers.
#[repr(C)]
struct DeviceMemory {
    queue: Queue<QUEUE_SIZE>,
    header: DmaBuf<RequestHeader>,
    status: DmaBuf<u8>,
}

/// Why a device couldn't be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// The transport reaches some other kind of device.
    WrongDevice,
    /// The feature handshake failed.
    Refused,
    /// The request queue is missing or too small.
    NoQueue,
    /// The device reports a geometry this driver can't use.
    BadGeometry,
}

/// A virtio block device.
pub struct VirtioBlk<T: Transport> {
    transport: T,
    memory: Box<DeviceMemory>,
    capacity_blocks: u64,
    block_size: usize,
    /// Sectors in a block.
    sectors_per_block: u64,
    max_request: usize,
    read_only: bool,
    can_flush: bool,
    /// A request was lost: the device may still write to its buffer whenever it
    /// likes, so nothing more can safely be sent.
    failed: bool,
    relax: fn(),
    /// Moved by the interrupt handler, once there is one.
    events: Option<&'static AtomicU32>,
}

impl<T: Transport> VirtioBlk<T> {
    /// Brings up the device behind `transport`.
    ///
    /// `relax` is called while a request is in progress, to let other tasks run.
    pub fn new(transport: T, relax: fn()) -> Result<Self, InitError> {
        if transport.device_id() != device::BLOCK {
            return Err(InitError::WrongDevice);
        }
        let wanted = feature::SIZE_MAX | feature::RO | feature::BLK_SIZE | feature::FLUSH;
        let features = negotiate(&transport, wanted).ok_or(InitError::Refused)?;

        let sectors = transport.config_read64(config::CAPACITY);
        let block_size = if features & feature::BLK_SIZE != 0 {
            transport.config_read32(config::BLK_SIZE) as usize
        } else {
            SECTOR_SIZE as usize
        };
        if block_size < SECTOR_SIZE as usize || !block_size.is_power_of_two() {
            return Err(InitError::BadGeometry);
        }
        let sectors_per_block = block_size as u64 / SECTOR_SIZE;

        let mut max_request = MAX_REQUEST_BYTES;
        if features & feature::SIZE_MAX != 0 {
            let limit = transport.config_read32(config::SIZE_MAX) as usize;
            if limit != 0 {
                max_request = max_request.min(limit);
            }
        }
        // Requests stay whole blocks, and the device must take at least one.
        max_request -= max_request % block_size;
        if max_request == 0 {
            return Err(InitError::BadGeometry);
        }

        let available = transport.queue_max(0) as usize;
        if available == 0 {
            return Err(InitError::NoQueue);
        }
        // A device's queue can't be shorter than a three-descriptor request, and
        // a modern device can take a size below its maximum. A legacy device
        // can't, and has to use the size it says.
        if available < QUEUE_SIZE {
            return Err(InitError::NoQueue);
        }

        let mut memory = Box::new(DeviceMemory {
            queue: Queue::new(),
            header: DmaBuf::new(RequestHeader {
                kind: 0,
                reserved: 0,
                sector: 0,
            }),
            status: DmaBuf::new(0),
        });
        memory.queue.reset();
        let (desc, avail, used) = memory.queue.addresses();
        if !transport.queue_activate(0, QUEUE_SIZE as u16, desc, avail, used) {
            return Err(InitError::NoQueue);
        }
        finish(&transport);

        Ok(Self {
            transport,
            memory,
            capacity_blocks: sectors / sectors_per_block,
            block_size,
            sectors_per_block,
            max_request,
            read_only: features & feature::RO != 0,
            can_flush: features & feature::FLUSH != 0,
            failed: false,
            relax,
            events: None,
        })
    }

    /// From now on, waits for this word to move instead of polling. The caller
    /// has arranged for the device's interrupt handler to move it and wake its
    /// waiters.
    pub fn use_interrupts(&mut self, events: &'static AtomicU32) {
        self.events = Some(events);
        self.memory.queue.set_interrupts(true);
    }

    /// True when requests wait on an interrupt.
    pub fn is_interrupt_driven(&self) -> bool {
        self.events.is_some()
    }

    /// Checks that a transfer is whole blocks inside the device.
    fn check_range(&self, start: u64, len: usize) -> Result<(), BlockIoError> {
        if !len.is_multiple_of(self.block_size) {
            return Err(BlockIoError::OutOfRange);
        }
        let blocks = (len / self.block_size) as u64;
        match start.checked_add(blocks) {
            Some(end) if end <= self.capacity_blocks => Ok(()),
            _ => Err(BlockIoError::OutOfRange),
        }
    }

    /// Sends one request and waits for it.
    fn submit(&mut self, kind: u32, sector: u64, data: Option<Segment>) -> Result<(), BlockIoError> {
        if self.failed {
            return Err(BlockIoError::Io);
        }
        self.memory.header.write(RequestHeader {
            kind,
            reserved: 0,
            sector,
        });
        self.memory.status.write(STATUS_UNSET);
        let header = Segment {
            addr: self.memory.header.phys(),
            len: core::mem::size_of::<RequestHeader>() as u32,
            device_writes: false,
        };
        let status = Segment {
            addr: self.memory.status.phys(),
            len: 1,
            device_writes: true,
        };
        let with_data;
        let without_data;
        let chain: &[Segment] = match data {
            Some(data) => {
                with_data = [header, data, status];
                &with_data
            }
            None => {
                without_data = [header, status];
                &without_data
            }
        };
        let Some(head) = self.memory.queue.add(chain) else {
            return Err(BlockIoError::Io);
        };
        if self.memory.queue.publish() {
            self.transport.queue_notify(0);
        }

        let mut polls = 0u64;
        let finished = loop {
            // Read before looking at the ring, so an interrupt for a request that
            // finishes after the look moves this and the wait doesn't sleep on it.
            let seen = self.events.map(|events| (events, events.load(Ordering::Acquire)));
            if let Some((done, _written)) = self.memory.queue.pop_used() {
                break done;
            }
            polls += 1;
            if polls >= POLL_LIMIT {
                // The device still holds the buffers. Never touch it again.
                self.failed = true;
                return Err(BlockIoError::Io);
            }
            match seen {
                Some((events, value)) if feos_sync::irq::is_enabled() => {
                    feos_sync::futex_wait_ms(events, value, WAIT_MS);
                    polls += WAIT_COST;
                }
                _ => {
                    if polls.is_multiple_of(POLLS_PER_RELAX) {
                        (self.relax)();
                    }
                    core::hint::spin_loop();
                }
            }
        };
        self.memory.queue.free_chain(finished);
        if finished != head {
            self.failed = true;
            return Err(BlockIoError::Io);
        }
        dma::rmb();
        if self.memory.status.read() == STATUS_OK {
            Ok(())
        } else {
            Err(BlockIoError::Io)
        }
    }

    /// Splits a transfer into requests the device accepts.
    fn transfer(
        &mut self,
        kind: u32,
        start: u64,
        address: usize,
        len: usize,
        device_writes: bool,
    ) -> Result<(), BlockIoError> {
        self.check_range(start, len)?;
        let mut done = 0usize;
        while done < len {
            let take = (len - done).min(self.max_request);
            let block = start + (done / self.block_size) as u64;
            let data = Segment {
                addr: (address + done) as u64,
                len: take as u32,
                device_writes,
            };
            self.submit(kind, block * self.sectors_per_block, Some(data))?;
            done += take;
        }
        Ok(())
    }
}

impl<T: Transport + Send> BlockIo for VirtioBlk<T> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.capacity_blocks
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), BlockIoError> {
        self.transfer(request::READ, start, out.as_mut_ptr() as usize, out.len(), true)
    }

    fn write(&mut self, start: u64, data: &[u8]) -> Result<(), BlockIoError> {
        if self.read_only {
            return Err(BlockIoError::ReadOnly);
        }
        self.transfer(request::WRITE, start, data.as_ptr() as usize, data.len(), false)
    }

    fn flush(&mut self) -> Result<(), BlockIoError> {
        // Without the feature, the device promises that a finished write is
        // already stable.
        if !self.can_flush || self.read_only {
            return Ok(());
        }
        self.submit(request::FLUSH, 0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use feos_virtio_core::{Descriptor, F_VERSION_1, DESC_F_NEXT, DESC_F_WRITE};

    /// Plays a virtio-blk device: finishes each request when it's notified,
    /// reading the rings at the addresses the driver gave it, as hardware would.
    struct FakeDisk {
        modern: bool,
        features: u64,
        block_size: u32,
        size_max: u32,
        queue_max: u16,
        data: RefCell<Vec<u8>>,
        desc: Cell<u64>,
        avail: Cell<u64>,
        used: Cell<u64>,
        size: Cell<u16>,
        seen: Cell<u16>,
        status: Cell<u32>,
        ignore_requests: Cell<bool>,
        error_status: Cell<u8>,
        requests: RefCell<Vec<(u32, u64, usize)>>,
        flushes: Cell<usize>,
    }

    impl FakeDisk {
        fn new(blocks: usize, block_size: u32) -> &'static Self {
            Box::leak(Box::new(Self {
                modern: true,
                features: F_VERSION_1
                    | feature::FLUSH
                    | feature::BLK_SIZE
                    | feature::SIZE_MAX,
                block_size,
                size_max: 8192,
                queue_max: 64,
                data: RefCell::new(vec![0; blocks * block_size as usize]),
                desc: Cell::new(0),
                avail: Cell::new(0),
                used: Cell::new(0),
                size: Cell::new(0),
                seen: Cell::new(0),
                status: Cell::new(0),
                ignore_requests: Cell::new(false),
                error_status: Cell::new(0),
                requests: RefCell::new(Vec::new()),
                flushes: Cell::new(0),
            }))
        }

        unsafe fn descriptor(&self, index: u16) -> Descriptor {
            std::ptr::read_volatile((self.desc.get() as *const Descriptor).add(index as usize))
        }

        unsafe fn process(&self) {
            let size = self.size.get();
            let avail = self.avail.get() as *const u16;
            let wanted = std::ptr::read_volatile(avail.add(1));
            while self.seen.get() != wanted {
                let slot = self.seen.get() % size;
                let head = std::ptr::read_volatile(avail.add(2 + slot as usize));
                self.seen.set(self.seen.get().wrapping_add(1));
                let written = self.run_chain(head);

                let used = self.used.get() as *mut u16;
                let index = std::ptr::read_volatile(used.add(1));
                let element = (used.add(2) as *mut u32).add(2 * (index % size) as usize);
                std::ptr::write_volatile(element, head as u32);
                std::ptr::write_volatile(element.add(1), written);
                std::ptr::write_volatile(used.add(1), index.wrapping_add(1));
            }
        }

        unsafe fn run_chain(&self, head: u16) -> u32 {
            let header_desc = self.descriptor(head);
            assert_eq!(header_desc.flags & DESC_F_WRITE, 0);
            assert_eq!(header_desc.len, 16);
            let header = std::ptr::read_unaligned(header_desc.addr as *const RequestHeader);
            assert_ne!(header_desc.flags & DESC_F_NEXT, 0);
            let mut desc = self.descriptor(header_desc.next);
            let mut written = 1;
            let mut len = 0usize;
            if desc.flags & DESC_F_NEXT != 0 {
                len = desc.len as usize;
                let offset = header.sector as usize * 512;
                let mut disk = self.data.borrow_mut();
                let disk = disk.get_mut(offset..offset + len).expect("in range");
                match header.kind {
                    request::READ => {
                        assert_ne!(desc.flags & DESC_F_WRITE, 0);
                        std::ptr::copy_nonoverlapping(disk.as_ptr(), desc.addr as *mut u8, len);
                        written += len as u32;
                    }
                    request::WRITE => {
                        assert_eq!(desc.flags & DESC_F_WRITE, 0);
                        std::ptr::copy_nonoverlapping(desc.addr as *const u8, disk.as_mut_ptr(), len);
                    }
                    other => panic!("request {other} carried data"),
                }
                desc = self.descriptor(desc.next);
            } else {
                assert_eq!(header.kind, request::FLUSH);
                self.flushes.set(self.flushes.get() + 1);
            }
            assert_eq!(desc.len, 1);
            assert_ne!(desc.flags & DESC_F_WRITE, 0);
            std::ptr::write_volatile(desc.addr as *mut u8, self.error_status.get());
            self.requests
                .borrow_mut()
                .push((header.kind, header.sector, len));
            written
        }
    }

    // SAFETY: the tests drive one device from one thread.
    unsafe impl Sync for FakeDisk {}

    impl Transport for &'static FakeDisk {
        fn device_id(&self) -> u32 {
            device::BLOCK
        }
        fn is_modern(&self) -> bool {
            self.modern
        }
        fn status(&self) -> u32 {
            self.status.get()
        }
        fn set_status(&self, status: u32) {
            self.status.set(status);
        }
        fn device_features(&self) -> u64 {
            self.features
        }
        fn set_driver_features(&self, _features: u64) {}
        fn queue_max(&self, index: u16) -> u16 {
            if index == 0 {
                self.queue_max
            } else {
                0
            }
        }
        fn queue_activate(&self, _: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool {
            self.size.set(size);
            self.desc.set(desc);
            self.avail.set(avail);
            self.used.set(used);
            self.seen.set(0);
            true
        }
        fn queue_notify(&self, _index: u16) {
            if !self.ignore_requests.get() {
                // SAFETY: the driver handed these addresses to the "device".
                unsafe { self.process() };
            }
        }
        fn config_read8(&self, _offset: usize) -> u8 {
            0
        }
        fn config_write8(&self, _offset: usize, _value: u8) {}
        fn config_read32(&self, offset: usize) -> u32 {
            match offset {
                0 => (self.data.borrow().len() / 512) as u32,
                4 => 0,
                config::SIZE_MAX => self.size_max,
                config::BLK_SIZE => self.block_size,
                _ => 0,
            }
        }
    }

    fn device(disk: &'static FakeDisk) -> VirtioBlk<&'static FakeDisk> {
        VirtioBlk::new(disk, || {}).unwrap()
    }

    #[test]
    fn writes_then_reads_several_blocks() {
        let disk = FakeDisk::new(64, 512);
        let mut blk = device(disk);
        assert_eq!(blk.block_count(), 64);
        assert_eq!(blk.block_size(), 512);
        let data: Vec<u8> = (0..1024).map(|value| value as u8).collect();
        blk.write(3, &data).unwrap();
        assert_eq!(disk.data.borrow()[3 * 512..5 * 512], data[..]);
        let mut back = vec![0u8; 1024];
        blk.read(3, &mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn large_transfers_respect_the_device_limit() {
        let disk = FakeDisk::new(64, 512);
        let mut blk = device(disk);
        let data = vec![7u8; 20 * 512];
        blk.write(0, &data).unwrap();
        let requests = disk.requests.borrow();
        assert_eq!(
            *requests,
            [
                (request::WRITE, 0, 8192),
                (request::WRITE, 16, 2048)
            ]
        );
    }

    #[test]
    fn four_kib_blocks_address_the_disk_in_sectors() {
        let disk = FakeDisk::new(8, 4096);
        let mut blk = device(disk);
        assert_eq!((blk.block_size(), blk.block_count()), (4096, 8));
        blk.write(2, &[1u8; 4096]).unwrap();
        assert_eq!(disk.requests.borrow()[0], (request::WRITE, 16, 4096));
    }

    #[test]
    fn out_of_range_and_partial_transfers_are_refused() {
        let disk = FakeDisk::new(4, 512);
        let mut blk = device(disk);
        let mut block = [0u8; 512];
        assert_eq!(blk.read(4, &mut block), Err(BlockIoError::OutOfRange));
        assert_eq!(blk.read(u64::MAX, &mut block), Err(BlockIoError::OutOfRange));
        assert_eq!(blk.read(0, &mut block[..100]), Err(BlockIoError::OutOfRange));
        assert!(disk.requests.borrow().is_empty());
    }

    #[test]
    fn a_failed_request_is_an_error_and_the_device_stays_usable() {
        let disk = FakeDisk::new(4, 512);
        let mut blk = device(disk);
        let mut block = [0u8; 512];
        disk.error_status.set(1);
        assert_eq!(blk.read(0, &mut block), Err(BlockIoError::Io));
        disk.error_status.set(0);
        assert_eq!(blk.read(0, &mut block), Ok(()));
    }

    #[test]
    fn flush_reaches_the_device() {
        let disk = FakeDisk::new(4, 512);
        let mut blk = device(disk);
        blk.flush().unwrap();
        assert_eq!(disk.flushes.get(), 1);
    }

    #[test]
    fn the_queue_recycles_descriptors() {
        let disk = FakeDisk::new(4, 512);
        let mut blk = device(disk);
        let mut block = [0u8; 512];
        for _ in 0..200 {
            blk.read(1, &mut block).unwrap();
        }
    }

    #[test]
    fn a_read_only_device_refuses_writes_and_skips_flush() {
        let disk = Box::leak(Box::new(FakeDisk {
            features: F_VERSION_1 | feature::RO | feature::FLUSH,
            modern: true,
            block_size: 512,
            size_max: 0,
            queue_max: 16,
            data: RefCell::new(vec![0; 4096]),
            desc: Cell::new(0),
            avail: Cell::new(0),
            used: Cell::new(0),
            size: Cell::new(0),
            seen: Cell::new(0),
            status: Cell::new(0),
            ignore_requests: Cell::new(false),
            error_status: Cell::new(0),
            requests: RefCell::new(Vec::new()),
            flushes: Cell::new(0),
        }));
        let mut blk = VirtioBlk::new(&*disk, || {}).unwrap();
        assert!(blk.is_read_only());
        assert_eq!(blk.write(0, &[0u8; 512]), Err(BlockIoError::ReadOnly));
        blk.flush().unwrap();
        assert_eq!(disk.flushes.get(), 0);
    }

    #[test]
    fn a_bad_device_is_refused() {
        let disk = FakeDisk::new(4, 513);
        assert!(matches!(
            VirtioBlk::new(disk, || {}),
            Err(InitError::BadGeometry)
        ));
    }
}
