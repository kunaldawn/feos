//! A virtio network device on either transport.
//!
//! Frames are copied in and out of buffers the driver owns, so the stack above works
//! with ordinary slices. One buffer goes with each descriptor chain and stays with it:
//! a receive chain is offered again as soon as it's been read.
//!
//! The device writes a header before each frame, which is unused here because no
//! offloads are negotiated. Its size depends on the interface: ten bytes on a legacy
//! device, twelve on a modern one. A legacy device without `ANY_LAYOUT` wants the header
//! in a descriptor of its own, so it always gets one.
//!
//! This driver never waits. The caller polls it, on an interrupt from
//! [`enable_interrupts`](VirtioNet::enable_interrupts) or otherwise.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;

use feos_hal::dma;
use feos_hal::net::NetDevice;
use feos_virtio_core::{device, finish, negotiate, Queue, Segment, Transport, F_VERSION_1};

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
/// Descriptors in a queue. Each frame takes two.
const QUEUE_SIZE: usize = 128;
/// Frames that can be offered to or held by the device at once.
const FRAMES: usize = QUEUE_SIZE / 2;
/// Room for a frame: a 1500 byte MTU, Ethernet, and a VLAN tag on the way past.
const BUFFER_SIZE: usize = 1536;
/// The largest header either interface uses.
const HEADER_SIZE: usize = 12;

mod feature {
    pub const MAC: u64 = 1 << 5;
    pub const STATUS: u64 = 1 << 16;
}

mod config {
    pub const MAC: usize = 0;
    pub const STATUS: usize = 6;
}

const STATUS_LINK_UP: u8 = 1;

/// Why a device couldn't be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// The transport reaches some other kind of device.
    WrongDevice,
    /// The feature handshake failed.
    Refused,
    /// A queue is missing or too small.
    NoQueue,
}

/// The header and frame buffers, which the device reads and writes.
#[repr(C)]
struct Buffers {
    rx_headers: [[u8; HEADER_SIZE]; FRAMES],
    tx_headers: [[u8; HEADER_SIZE]; FRAMES],
    rx: [[u8; BUFFER_SIZE]; FRAMES],
    tx: [[u8; BUFFER_SIZE]; FRAMES],
}

/// A virtio network device.
pub struct VirtioNet<T: Transport> {
    transport: T,
    rx_queue: Box<Queue<QUEUE_SIZE>>,
    tx_queue: Box<Queue<QUEUE_SIZE>>,
    buffers: Box<Buffers>,
    /// Which buffer each receive chain's head owns.
    rx_owner: [u8; QUEUE_SIZE],
    /// Which transmit buffers are with the device, by chain head.
    tx_owner: [u8; QUEUE_SIZE],
    /// Transmit buffers not with the device.
    tx_free: [u8; FRAMES],
    tx_free_count: usize,
    header_size: usize,
    mac: [u8; 6],
    has_status: bool,
    needs_notify: bool,
}

// SAFETY: the device's memory is reached only through this value, which the caller
// owns, and the transport's registers are the same to any CPU.
unsafe impl<T: Transport + Send> Send for VirtioNet<T> {}

impl<T: Transport> VirtioNet<T> {
    /// Brings up the device behind `transport`, with every receive buffer offered.
    pub fn new(transport: T) -> Result<Self, InitError> {
        if transport.device_id() != device::NET {
            return Err(InitError::WrongDevice);
        }
        let features = negotiate(&transport, feature::MAC | feature::STATUS).ok_or(InitError::Refused)?;
        if (transport.queue_max(RX_QUEUE) as usize) < QUEUE_SIZE || (transport.queue_max(TX_QUEUE) as usize) < QUEUE_SIZE {
            return Err(InitError::NoQueue);
        }

        let mut rx_queue = Box::new(Queue::new());
        let mut tx_queue = Box::new(Queue::new());
        rx_queue.reset();
        tx_queue.reset();
        // SAFETY: all-zero bytes are valid for arrays of bytes.
        let buffers: Box<Buffers> = unsafe { Box::<Buffers>::new_zeroed().assume_init() };

        let (desc, avail, used) = rx_queue.addresses();
        if !transport.queue_activate(RX_QUEUE, QUEUE_SIZE as u16, desc, avail, used) {
            return Err(InitError::NoQueue);
        }
        let (desc, avail, used) = tx_queue.addresses();
        if !transport.queue_activate(TX_QUEUE, QUEUE_SIZE as u16, desc, avail, used) {
            return Err(InitError::NoQueue);
        }

        let mut mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        if features & feature::MAC != 0 {
            for (index, byte) in mac.iter_mut().enumerate() {
                *byte = transport.config_read8(config::MAC + index);
            }
        }

        let mut tx_free = [0u8; FRAMES];
        for (index, slot) in tx_free.iter_mut().enumerate() {
            *slot = index as u8;
        }
        let mut net = Self {
            transport,
            rx_queue,
            tx_queue,
            buffers,
            rx_owner: [0; QUEUE_SIZE],
            tx_owner: [0; QUEUE_SIZE],
            tx_free,
            tx_free_count: FRAMES,
            header_size: if features & F_VERSION_1 != 0 { 12 } else { 10 },
            mac,
            has_status: features & feature::STATUS != 0,
            needs_notify: false,
        };
        for index in 0..FRAMES {
            net.offer_rx(index);
        }
        if net.rx_queue.publish() {
            net.needs_notify = true;
        }
        finish(&net.transport);
        if net.needs_notify {
            net.transport.queue_notify(RX_QUEUE);
            net.needs_notify = false;
        }
        Ok(net)
    }

    /// Asks the device to interrupt when a frame comes or goes. The handler has to
    /// have been set up first.
    pub fn enable_interrupts(&mut self) {
        self.rx_queue.set_interrupts(true);
        self.tx_queue.set_interrupts(true);
    }

    fn offer_rx(&mut self, index: usize) {
        let header = self.buffers.rx_headers[index].as_ptr() as u64;
        let frame = self.buffers.rx[index].as_ptr() as u64;
        let chain = [
            Segment {
                addr: header,
                len: self.header_size as u32,
                device_writes: true,
            },
            Segment {
                addr: frame,
                len: BUFFER_SIZE as u32,
                device_writes: true,
            },
        ];
        if let Some(head) = self.rx_queue.add(&chain) {
            self.rx_owner[head as usize] = index as u8;
        }
    }

    /// Takes back whatever the device has finished sending.
    fn reclaim_tx(&mut self) {
        while let Some((head, _)) = self.tx_queue.pop_used() {
            let Some(&owner) = self.tx_owner.get(head as usize) else {
                continue;
            };
            self.tx_queue.free_chain(head);
            if self.tx_free_count < FRAMES {
                self.tx_free[self.tx_free_count] = owner;
                self.tx_free_count += 1;
            }
        }
    }
}

impl<T: Transport + Send> NetDevice for VirtioNet<T> {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&self) -> bool {
        !self.has_status || self.transport.config_read8(config::STATUS) & STATUS_LINK_UP != 0
    }

    fn can_transmit(&mut self) -> bool {
        self.reclaim_tx();
        self.tx_free_count != 0 && self.tx_queue.free_count() >= 2
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUFFER_SIZE || !self.can_transmit() {
            return false;
        }
        self.tx_free_count -= 1;
        let index = self.tx_free[self.tx_free_count] as usize;
        self.buffers.tx[index][..frame.len()].copy_from_slice(frame);
        // Never written: a zero header asks for no offloads.
        let chain = [
            Segment {
                addr: self.buffers.tx_headers[index].as_ptr() as u64,
                len: self.header_size as u32,
                device_writes: false,
            },
            Segment {
                addr: self.buffers.tx[index].as_ptr() as u64,
                len: frame.len() as u32,
                device_writes: false,
            },
        ];
        let Some(head) = self.tx_queue.add(&chain) else {
            self.tx_free[self.tx_free_count] = index as u8;
            self.tx_free_count += 1;
            return false;
        };
        self.tx_owner[head as usize] = index as u8;
        if self.tx_queue.publish() {
            self.transport.queue_notify(TX_QUEUE);
        }
        true
    }

    fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        loop {
            let (head, written) = self.rx_queue.pop_used()?;
            let owner = *self.rx_owner.get(head as usize)? as usize;
            dma::rmb();
            let length = (written as usize).saturating_sub(self.header_size);
            let usable = length <= BUFFER_SIZE && length <= out.len() && length != 0;
            if usable {
                out[..length].copy_from_slice(&self.buffers.rx[owner][..length]);
            }
            self.rx_queue.resubmit(head);
            if self.rx_queue.publish() {
                self.transport.queue_notify(RX_QUEUE);
            }
            if usable {
                return Some(length);
            }
            // Too big or empty: on to the next, without bothering the caller.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use feos_virtio_core::{Descriptor, DESC_F_NEXT, DESC_F_WRITE};

    /// A device with its queues in plain memory, driven by the test.
    struct Fake {
        modern: bool,
        features: u64,
        rx: Cell<(u64, u64, u64)>,
        tx: Cell<(u64, u64, u64)>,
        rx_seen: Cell<u16>,
        tx_seen: Cell<u16>,
        sent: RefCell<Vec<Vec<u8>>>,
        status: Cell<u32>,
        link: Cell<u8>,
        notifies: Cell<usize>,
    }

    // SAFETY: the tests drive one device from one thread.
    unsafe impl Sync for Fake {}

    impl Fake {
        fn new(modern: bool) -> &'static Self {
            Box::leak(Box::new(Self {
                modern,
                features: feature::MAC | feature::STATUS | if modern { F_VERSION_1 } else { 0 },
                rx: Cell::new((0, 0, 0)),
                tx: Cell::new((0, 0, 0)),
                rx_seen: Cell::new(0),
                tx_seen: Cell::new(0),
                sent: RefCell::new(Vec::new()),
                status: Cell::new(0),
                link: Cell::new(STATUS_LINK_UP),
                notifies: Cell::new(0),
            }))
        }

        fn header_size(&self) -> usize {
            if self.modern {
                12
            } else {
                10
            }
        }

        unsafe fn descriptor(base: u64, index: u16) -> Descriptor {
            std::ptr::read_volatile((base as *const Descriptor).add(index as usize))
        }

        unsafe fn used_push(used: u64, head: u16, written: u32) {
            let used = used as *mut u16;
            let index = std::ptr::read_volatile(used.add(1));
            let element = (used.add(2) as *mut u32).add(2 * (index as usize % QUEUE_SIZE));
            std::ptr::write_volatile(element, head as u32);
            std::ptr::write_volatile(element.add(1), written);
            std::ptr::write_volatile(used.add(1), index.wrapping_add(1));
        }

        /// Delivers a frame through the next receive buffer on offer.
        unsafe fn deliver(&self, frame: &[u8]) -> bool {
            let (desc, avail, used) = self.rx.get();
            let avail = avail as *const u16;
            let wanted = std::ptr::read_volatile(avail.add(1));
            if self.rx_seen.get() == wanted {
                return false;
            }
            let slot = self.rx_seen.get() as usize % QUEUE_SIZE;
            let head = std::ptr::read_volatile(avail.add(2 + slot));
            self.rx_seen.set(self.rx_seen.get().wrapping_add(1));
            let header = Self::descriptor(desc, head);
            assert_eq!(header.len as usize, self.header_size());
            assert_ne!(header.flags & DESC_F_WRITE, 0);
            assert_ne!(header.flags & DESC_F_NEXT, 0);
            let body = Self::descriptor(desc, header.next);
            assert_ne!(body.flags & DESC_F_WRITE, 0);
            assert!(frame.len() <= body.len as usize);
            std::ptr::copy_nonoverlapping(frame.as_ptr(), body.addr as *mut u8, frame.len());
            Self::used_push(used, head, (self.header_size() + frame.len()) as u32);
            true
        }

        /// Takes everything queued for sending.
        unsafe fn collect(&self) {
            let (desc, avail, used) = self.tx.get();
            let avail = avail as *const u16;
            let wanted = std::ptr::read_volatile(avail.add(1));
            while self.tx_seen.get() != wanted {
                let slot = self.tx_seen.get() as usize % QUEUE_SIZE;
                let head = std::ptr::read_volatile(avail.add(2 + slot));
                self.tx_seen.set(self.tx_seen.get().wrapping_add(1));
                let header = Self::descriptor(desc, head);
                assert_eq!(header.len as usize, self.header_size());
                assert_eq!(header.flags & DESC_F_WRITE, 0);
                let body = Self::descriptor(desc, header.next);
                assert_eq!(body.flags & DESC_F_NEXT, 0);
                let data = std::slice::from_raw_parts(body.addr as *const u8, body.len as usize);
                self.sent.borrow_mut().push(data.to_vec());
                Self::used_push(used, head, 0);
            }
        }
    }

    impl Transport for &'static Fake {
        fn device_id(&self) -> u32 {
            device::NET
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
            if index < 2 {
                256
            } else {
                0
            }
        }
        fn queue_activate(&self, index: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool {
            assert_eq!(size as usize, QUEUE_SIZE);
            if index == RX_QUEUE {
                self.rx.set((desc, avail, used));
            } else {
                self.tx.set((desc, avail, used));
            }
            true
        }
        fn queue_notify(&self, _index: u16) {
            self.notifies.set(self.notifies.get() + 1);
        }
        fn config_read8(&self, offset: usize) -> u8 {
            match offset {
                0..=5 => 0x10 + offset as u8,
                config::STATUS => self.link.get(),
                _ => 0,
            }
        }
        fn config_write8(&self, _offset: usize, _value: u8) {}
        fn config_read32(&self, _offset: usize) -> u32 {
            0
        }
    }

    #[test]
    fn reads_the_address_and_link_state() {
        let fake = Fake::new(true);
        let net = VirtioNet::new(fake).unwrap();
        assert_eq!(net.mac(), [0x10, 0x11, 0x12, 0x13, 0x14, 0x15]);
        assert!(net.link_up());
        fake.link.set(0);
        assert!(!net.link_up());
    }

    #[test]
    fn a_frame_received_comes_out_without_its_header() {
        for modern in [true, false] {
            let fake = Fake::new(modern);
            let mut net = VirtioNet::new(fake).unwrap();
            assert_eq!(net.header_size, fake.header_size());
            let frame: Vec<u8> = (0..200).map(|value| value as u8).collect();
            let mut out = [0u8; 2048];
            assert_eq!(net.receive(&mut out), None);
            // SAFETY: the driver handed over these addresses.
            assert!(unsafe { fake.deliver(&frame) });
            assert_eq!(net.receive(&mut out), Some(200));
            assert_eq!(&out[..200], &frame[..]);
        }
    }

    #[test]
    fn receive_buffers_go_round_and_round() {
        let fake = Fake::new(true);
        let mut net = VirtioNet::new(fake).unwrap();
        let mut out = [0u8; 2048];
        for round in 0..2000u32 {
            let frame = round.to_le_bytes();
            // SAFETY: the driver handed over these addresses.
            assert!(unsafe { fake.deliver(&frame) }, "buffer ran out at {round}");
            assert_eq!(net.receive(&mut out), Some(4));
            assert_eq!(out[..4], frame);
        }
    }

    #[test]
    fn a_frame_too_big_for_the_caller_is_skipped() {
        let fake = Fake::new(true);
        let mut net = VirtioNet::new(fake).unwrap();
        // SAFETY: the driver handed over these addresses.
        unsafe {
            fake.deliver(&[1u8; 1000]);
            fake.deliver(&[2u8; 10]);
        }
        let mut out = [0u8; 100];
        assert_eq!(net.receive(&mut out), Some(10));
        assert_eq!(out[0], 2);
    }

    #[test]
    fn sent_frames_reach_the_device_and_a_full_ring_says_no() {
        let fake = Fake::new(true);
        let mut net = VirtioNet::new(fake).unwrap();
        for index in 0..FRAMES {
            assert!(net.transmit(&[index as u8; 60]), "frame {index}");
        }
        assert!(!net.can_transmit());
        assert!(!net.transmit(&[0; 60]));
        // SAFETY: the driver handed over these addresses.
        unsafe { fake.collect() };
        assert!(net.can_transmit());
        assert_eq!(fake.sent.borrow().len(), FRAMES);
        assert_eq!(fake.sent.borrow()[3], vec![3u8; 60]);
        for _ in 0..1000 {
            assert!(net.transmit(&[9; 1514]));
            // SAFETY: as above.
            unsafe { fake.collect() };
        }
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let fake = Fake::new(true);
        let mut net = VirtioNet::new(fake).unwrap();
        assert!(!net.transmit(&[0; BUFFER_SIZE + 1]));
        assert!(net.transmit(&[0; BUFFER_SIZE]));
    }

    #[test]
    fn another_device_is_refused() {
        struct Other;
        impl Transport for Other {
            fn device_id(&self) -> u32 {
                device::BLOCK
            }
            fn is_modern(&self) -> bool {
                true
            }
            fn status(&self) -> u32 {
                0
            }
            fn set_status(&self, _status: u32) {}
            fn device_features(&self) -> u64 {
                0
            }
            fn set_driver_features(&self, _features: u64) {}
            fn queue_max(&self, _index: u16) -> u16 {
                0
            }
            fn queue_activate(&self, _: u16, _: u16, _: u64, _: u64, _: u64) -> bool {
                false
            }
            fn queue_notify(&self, _index: u16) {}
            fn config_read8(&self, _offset: usize) -> u8 {
                0
            }
            fn config_write8(&self, _offset: usize, _value: u8) {}
            fn config_read32(&self, _offset: usize) -> u32 {
                0
            }
        }
        assert!(matches!(VirtioNet::new(Other), Err(InitError::WrongDevice)));
    }
}
