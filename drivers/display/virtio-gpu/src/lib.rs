//! virtio-gpu in 2D: a framebuffer the host only sees when it's told to look.
//!
//! The guest draws into its own memory, which the device knows as a resource's
//! backing. [`VirtioGpu::flush`] copies a rectangle to the host and shows it, so
//! the display costs nothing while nothing changes, unlike ramfb, and the mode
//! comes from the device instead of being fixed at build time.
//!
//! Everything is polled, one command at a time.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use feos_hal::dma;
use feos_virtio_core::{device, finish, negotiate, Queue, Segment, Transport};

const QUEUE_SIZE: usize = 16;
/// The resource this driver makes. There's only the one.
const RESOURCE_ID: u32 = 1;
const SCANOUT_ID: u32 = 0;
/// How many polls before a command counts as lost.
const POLL_LIMIT: u64 = 1 << 30;

mod command {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
}

/// `B8G8R8X8_UNORM`: little-endian `0x00RRGGBB` words, what `feos-gfx` draws.
const FORMAT_XRGB: u32 = 2;
const HEADER_LEN: usize = 24;
/// Longest request: the header and attach-backing's single entry.
const REQUEST_LEN: usize = HEADER_LEN + 40;
/// Longest response: the header and 16 modes of 24 bytes each.
const RESPONSE_LEN: usize = HEADER_LEN + 16 * 24;

/// Why a device couldn't be used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuError {
    /// The transport reaches some other device.
    WrongDevice,
    /// The feature handshake failed.
    Refused,
    /// The control queue is missing or too small.
    NoQueue,
    /// The device answered a command with an error, or not at all.
    CommandFailed(u32),
}

#[repr(C)]
struct DeviceMemory {
    queue: Queue<QUEUE_SIZE>,
    request: [u8; REQUEST_LEN],
    response: [u8; RESPONSE_LEN],
}

/// A rectangle in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

fn put_u32(buffer: &mut [u8], at: usize, value: u32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(buffer: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
}

/// A virtio-gpu device.
pub struct VirtioGpu<T: Transport> {
    transport: T,
    memory: Box<DeviceMemory>,
    width: u32,
    height: u32,
    /// A command was lost: the device may still write to its buffers.
    failed: bool,
}

impl<T: Transport> VirtioGpu<T> {
    /// Brings up the device. It shows nothing until [`attach`](Self::attach).
    pub fn new(transport: T) -> Result<Self, GpuError> {
        if transport.device_id() != device::GPU {
            return Err(GpuError::WrongDevice);
        }
        negotiate(&transport, 0).ok_or(GpuError::Refused)?;
        if (transport.queue_max(0) as usize) < QUEUE_SIZE {
            return Err(GpuError::NoQueue);
        }
        let mut memory = Box::new(DeviceMemory {
            queue: Queue::new(),
            request: [0; REQUEST_LEN],
            response: [0; RESPONSE_LEN],
        });
        memory.queue.reset();
        let (desc, avail, used) = memory.queue.addresses();
        if !transport.queue_activate(0, QUEUE_SIZE as u16, desc, avail, used) {
            return Err(GpuError::NoQueue);
        }
        finish(&transport);
        Ok(Self {
            transport,
            memory,
            width: 0,
            height: 0,
            failed: false,
        })
    }

    /// Sends the request built in `memory.request` and waits for the answer.
    fn execute(&mut self, kind: u32, len: usize, expected: u32) -> Result<(), GpuError> {
        if self.failed {
            return Err(GpuError::CommandFailed(0));
        }
        self.memory.request[..HEADER_LEN].fill(0);
        put_u32(&mut self.memory.request, 0, kind);
        self.memory.response[..HEADER_LEN].fill(0);
        let segments = [
            Segment {
                addr: core::ptr::addr_of!(self.memory.request) as u64,
                len: len as u32,
                device_writes: false,
            },
            Segment {
                addr: core::ptr::addr_of!(self.memory.response) as u64,
                len: RESPONSE_LEN as u32,
                device_writes: true,
            },
        ];
        let Some(head) = self.memory.queue.add(&segments) else {
            return Err(GpuError::CommandFailed(0));
        };
        if self.memory.queue.publish() {
            self.transport.queue_notify(0);
        }
        let mut polls = 0u64;
        let done = loop {
            if let Some((done, _)) = self.memory.queue.pop_used() {
                break done;
            }
            polls += 1;
            if polls >= POLL_LIMIT {
                self.failed = true;
                return Err(GpuError::CommandFailed(0));
            }
            core::hint::spin_loop();
        };
        self.memory.queue.free_chain(done);
        if done != head {
            self.failed = true;
            return Err(GpuError::CommandFailed(0));
        }
        dma::rmb();
        // SAFETY: the device has finished with the buffer; volatile because the
        // compiler can't see that it wrote there.
        let answer = unsafe {
            core::ptr::read_volatile(core::ptr::addr_of!(self.memory.response[0]).cast::<[u8; 4]>())
        };
        let answer = u32::from_le_bytes(answer);
        if answer == expected {
            Ok(())
        } else {
            Err(GpuError::CommandFailed(answer))
        }
    }

    /// The mode the host would like on the first scanout, if it says.
    pub fn preferred_mode(&mut self) -> Result<Option<(u32, u32)>, GpuError> {
        self.execute(
            command::GET_DISPLAY_INFO,
            HEADER_LEN,
            command::OK_DISPLAY_INFO,
        )?;
        dma::rmb();
        // SAFETY: as in `execute`.
        let response = unsafe {
            core::ptr::read_volatile(core::ptr::addr_of!(self.memory.response))
        };
        let first = HEADER_LEN;
        let (width, height) = (get_u32(&response, first + 8), get_u32(&response, first + 12));
        let enabled = get_u32(&response, first + 16) != 0;
        Ok((enabled && width != 0 && height != 0).then_some((width, height)))
    }

    /// Shows a `width` x `height` framebuffer of 32-bit pixels at `address`.
    ///
    /// The memory must stay where it is for good: the device reads it on each
    /// [`flush`](Self::flush).
    pub fn attach(&mut self, address: u64, width: u32, height: u32) -> Result<(), GpuError> {
        let request = &mut self.memory.request;
        put_u32(request, HEADER_LEN, RESOURCE_ID);
        put_u32(request, HEADER_LEN + 4, FORMAT_XRGB);
        put_u32(request, HEADER_LEN + 8, width);
        put_u32(request, HEADER_LEN + 12, height);
        self.execute(command::RESOURCE_CREATE_2D, HEADER_LEN + 16, command::OK_NODATA)?;

        let bytes = width.saturating_mul(height).saturating_mul(4);
        let request = &mut self.memory.request;
        put_u32(request, HEADER_LEN, RESOURCE_ID);
        put_u32(request, HEADER_LEN + 4, 1);
        put_u32(request, HEADER_LEN + 8, address as u32);
        put_u32(request, HEADER_LEN + 12, (address >> 32) as u32);
        put_u32(request, HEADER_LEN + 16, bytes);
        put_u32(request, HEADER_LEN + 20, 0);
        self.execute(
            command::RESOURCE_ATTACH_BACKING,
            HEADER_LEN + 24,
            command::OK_NODATA,
        )?;

        let request = &mut self.memory.request;
        put_u32(request, HEADER_LEN, 0);
        put_u32(request, HEADER_LEN + 4, 0);
        put_u32(request, HEADER_LEN + 8, width);
        put_u32(request, HEADER_LEN + 12, height);
        put_u32(request, HEADER_LEN + 16, SCANOUT_ID);
        put_u32(request, HEADER_LEN + 20, RESOURCE_ID);
        self.execute(command::SET_SCANOUT, HEADER_LEN + 24, command::OK_NODATA)?;
        self.width = width;
        self.height = height;
        self.flush(Rect {
            x: 0,
            y: 0,
            width,
            height,
        })
    }

    /// Clips a rectangle to the screen. `None` when nothing of it is left.
    fn clip(&self, rect: Rect) -> Option<Rect> {
        if rect.x >= self.width || rect.y >= self.height {
            return None;
        }
        let width = rect.width.min(self.width - rect.x);
        let height = rect.height.min(self.height - rect.y);
        (width != 0 && height != 0).then_some(Rect {
            x: rect.x,
            y: rect.y,
            width,
            height,
        })
    }

    /// Shows what's changed in `rect`.
    pub fn flush(&mut self, rect: Rect) -> Result<(), GpuError> {
        let Some(rect) = self.clip(rect) else {
            return Ok(());
        };
        let stride = self.width as u64 * 4;
        let offset = rect.y as u64 * stride + rect.x as u64 * 4;
        let request = &mut self.memory.request;
        put_u32(request, HEADER_LEN, rect.x);
        put_u32(request, HEADER_LEN + 4, rect.y);
        put_u32(request, HEADER_LEN + 8, rect.width);
        put_u32(request, HEADER_LEN + 12, rect.height);
        put_u32(request, HEADER_LEN + 16, offset as u32);
        put_u32(request, HEADER_LEN + 20, (offset >> 32) as u32);
        put_u32(request, HEADER_LEN + 24, RESOURCE_ID);
        put_u32(request, HEADER_LEN + 28, 0);
        self.execute(
            command::TRANSFER_TO_HOST_2D,
            HEADER_LEN + 32,
            command::OK_NODATA,
        )?;

        let request = &mut self.memory.request;
        put_u32(request, HEADER_LEN, rect.x);
        put_u32(request, HEADER_LEN + 4, rect.y);
        put_u32(request, HEADER_LEN + 8, rect.width);
        put_u32(request, HEADER_LEN + 12, rect.height);
        put_u32(request, HEADER_LEN + 16, RESOURCE_ID);
        put_u32(request, HEADER_LEN + 20, 0);
        self.execute(command::RESOURCE_FLUSH, HEADER_LEN + 24, command::OK_NODATA)
    }
}

// SAFETY: the driver's only shared state is its box, reached through `&mut self`.
unsafe impl<T: Transport + Send> Send for VirtioGpu<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use feos_virtio_core::{Descriptor, F_VERSION_1};

    /// Plays the device, logging each command and its numbers.
    struct FakeGpu {
        desc: Cell<u64>,
        avail: Cell<u64>,
        used: Cell<u64>,
        seen: Cell<u16>,
        status: Cell<u32>,
        mode: (u32, u32),
        log: RefCell<Vec<(u32, Vec<u32>)>>,
        reject: Cell<u32>,
    }

    impl FakeGpu {
        fn new(mode: (u32, u32)) -> &'static Self {
            Box::leak(Box::new(Self {
                desc: Cell::new(0),
                avail: Cell::new(0),
                used: Cell::new(0),
                seen: Cell::new(0),
                status: Cell::new(0),
                mode,
                log: RefCell::new(Vec::new()),
                reject: Cell::new(0),
            }))
        }

        unsafe fn process(&self) {
            let avail = self.avail.get() as *const u16;
            let wanted = std::ptr::read_volatile(avail.add(1));
            while self.seen.get() != wanted {
                let slot = self.seen.get() as usize % QUEUE_SIZE;
                let head = std::ptr::read_volatile(avail.add(2 + slot));
                self.seen.set(self.seen.get().wrapping_add(1));

                let table = self.desc.get() as *const Descriptor;
                let request = std::ptr::read(table.add(head as usize));
                let response = std::ptr::read(table.add(request.next as usize));
                let bytes =
                    std::slice::from_raw_parts(request.addr as *const u8, request.len as usize);
                let kind = get_u32(bytes, 0);
                let words = bytes[HEADER_LEN..]
                    .chunks_exact(4)
                    .map(|chunk| get_u32(chunk, 0))
                    .collect();
                self.log.borrow_mut().push((kind, words));

                let out = std::slice::from_raw_parts_mut(
                    response.addr as *mut u8,
                    response.len as usize,
                );
                let answer = if kind == self.reject.get() {
                    0x1200
                } else if kind == command::GET_DISPLAY_INFO {
                    put_u32(out, HEADER_LEN + 8, self.mode.0);
                    put_u32(out, HEADER_LEN + 12, self.mode.1);
                    put_u32(out, HEADER_LEN + 16, (self.mode.0 != 0) as u32);
                    command::OK_DISPLAY_INFO
                } else {
                    command::OK_NODATA
                };
                put_u32(out, 0, answer);

                let used = self.used.get() as *mut u16;
                let index = std::ptr::read_volatile(used.add(1));
                let element = (used.add(2) as *mut u32).add(2 * (index as usize % QUEUE_SIZE));
                std::ptr::write_volatile(element, head as u32);
                std::ptr::write_volatile(element.add(1), 0);
                std::ptr::write_volatile(used.add(1), index.wrapping_add(1));
            }
        }
    }

    // SAFETY: tests drive one device from one thread.
    unsafe impl Sync for FakeGpu {}

    impl Transport for &'static FakeGpu {
        fn device_id(&self) -> u32 {
            device::GPU
        }
        fn is_modern(&self) -> bool {
            true
        }
        fn status(&self) -> u32 {
            self.status.get()
        }
        fn set_status(&self, status: u32) {
            self.status.set(status);
        }
        fn device_features(&self) -> u64 {
            F_VERSION_1
        }
        fn set_driver_features(&self, _features: u64) {}
        fn queue_max(&self, _index: u16) -> u16 {
            64
        }
        fn queue_activate(&self, _: u16, _: u16, desc: u64, avail: u64, used: u64) -> bool {
            self.desc.set(desc);
            self.avail.set(avail);
            self.used.set(used);
            true
        }
        fn queue_notify(&self, _index: u16) {
            // SAFETY: the driver gave these addresses to the "device".
            unsafe { self.process() };
        }
        fn config_read8(&self, _offset: usize) -> u8 {
            0
        }
        fn config_write8(&self, _offset: usize, _value: u8) {}
        fn config_read32(&self, _offset: usize) -> u32 {
            0
        }
    }

    #[test]
    fn the_mode_comes_from_the_device() {
        let mut gpu = VirtioGpu::new(FakeGpu::new((1280, 800))).unwrap();
        assert_eq!(gpu.preferred_mode().unwrap(), Some((1280, 800)));
        let mut gpu = VirtioGpu::new(FakeGpu::new((0, 0))).unwrap();
        assert_eq!(gpu.preferred_mode().unwrap(), None);
    }

    #[test]
    fn attaching_creates_backs_shows_and_flushes() {
        let fake = FakeGpu::new((800, 600));
        let mut gpu = VirtioGpu::new(fake).unwrap();
        gpu.attach(0x1234_5678_9000, 800, 600).unwrap();
        let log = fake.log.borrow();
        let kinds: Vec<u32> = log.iter().map(|entry| entry.0).collect();
        assert_eq!(
            kinds,
            [
                command::RESOURCE_CREATE_2D,
                command::RESOURCE_ATTACH_BACKING,
                command::SET_SCANOUT,
                command::TRANSFER_TO_HOST_2D,
                command::RESOURCE_FLUSH
            ]
        );
        assert_eq!(log[0].1, [RESOURCE_ID, FORMAT_XRGB, 800, 600]);
        assert_eq!(log[1].1, [RESOURCE_ID, 1, 0x5678_9000, 0x1234, 800 * 600 * 4, 0]);
        assert_eq!(log[2].1, [0, 0, 800, 600, SCANOUT_ID, RESOURCE_ID]);
    }

    #[test]
    fn a_flush_covers_only_the_damage_and_stays_on_screen() {
        let fake = FakeGpu::new((800, 600));
        let mut gpu = VirtioGpu::new(fake).unwrap();
        gpu.attach(0x1000, 800, 600).unwrap();
        fake.log.borrow_mut().clear();

        gpu.flush(Rect { x: 10, y: 20, width: 30, height: 40 }).unwrap();
        {
            let log = fake.log.borrow();
            assert_eq!(log[0].1[..6], [10, 20, 30, 40, 20 * 800 * 4 + 10 * 4, 0]);
            assert_eq!(log[1].1[..4], [10, 20, 30, 40]);
        }
        fake.log.borrow_mut().clear();

        gpu.flush(Rect { x: 790, y: 590, width: 100, height: 100 }).unwrap();
        assert_eq!(fake.log.borrow()[1].1[..4], [790, 590, 10, 10]);
        fake.log.borrow_mut().clear();

        gpu.flush(Rect { x: 900, y: 0, width: 1, height: 1 }).unwrap();
        gpu.flush(Rect { x: 0, y: 0, width: 0, height: 5 }).unwrap();
        assert!(fake.log.borrow().is_empty(), "nothing on screen, nothing sent");
    }

    #[test]
    fn a_refused_command_is_an_error() {
        let fake = FakeGpu::new((800, 600));
        fake.reject.set(command::SET_SCANOUT);
        let mut gpu = VirtioGpu::new(fake).unwrap();
        assert_eq!(gpu.attach(0x1000, 800, 600), Err(GpuError::CommandFailed(0x1200)));
    }

    #[test]
    fn descriptors_are_recycled() {
        let fake = FakeGpu::new((800, 600));
        let mut gpu = VirtioGpu::new(fake).unwrap();
        gpu.attach(0x1000, 800, 600).unwrap();
        for _ in 0..100 {
            gpu.flush(Rect { x: 0, y: 0, width: 8, height: 8 }).unwrap();
        }
    }
}
