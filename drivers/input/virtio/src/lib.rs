//! A virtio-input pointer (tablet or mouse) on the virtio MMIO transport.
//!
//! QEMU's `virt` machine exposes 32 virtio MMIO slots starting at
//! `0x0a00_0000`. Adding `-device virtio-tablet-device` gives the guest an
//! absolute pointing device; `-device virtio-mouse-device` gives a relative
//! one. This driver implements the modern (version 2) and legacy (version 1)
//! transports, negotiates only the features it understands, and keeps a small
//! pre-filled event queue so pointer reports arrive without interrupts.
//!
//! The driver is polled. [`VirtioInput::poll`] decodes one SYN_REPORT-terminated
//! group at a time and returns an absolute framebuffer position plus any button
//! transitions, which is exactly what the desktop session feeds into
//! `feos_gui::Input`.

#![no_std]

use core::sync::atomic::{fence, Ordering};

use feos_hal::{KeyboardEvent, PointerEvent};

/// Number of descriptors in the event queue. Must be a power of two.
pub const QUEUE_SIZE: usize = 16;

/// Number of decoded key transitions buffered for the caller.
pub const KEY_QUEUE_SIZE: usize = 32;

/// First virtio MMIO slot on QEMU's `virt` machine.
pub const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;

/// Distance between virtio MMIO slots.
pub const VIRTIO_MMIO_STRIDE: usize = 0x200;

/// Number of virtio MMIO slots exposed by QEMU's `virt` machine.
pub const VIRTIO_MMIO_SLOTS: usize = 32;

const MAGIC: u32 = 0x7472_6976;
const DEVICE_ID_INPUT: u32 = 18;

mod reg {
    pub const MAGIC: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    pub const GUEST_PAGE_SIZE: usize = 0x028;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_ALIGN: usize = 0x03c;
    pub const QUEUE_PFN: usize = 0x040;
    pub const QUEUE_READY: usize = 0x044;
    pub const QUEUE_NOTIFY: usize = 0x050;
    pub const STATUS: usize = 0x070;
    pub const QUEUE_DESC_LOW: usize = 0x080;
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    pub const QUEUE_DRIVER_LOW: usize = 0x090;
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    pub const QUEUE_DEVICE_LOW: usize = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: usize = 0x0a4;
    pub const CONFIG: usize = 0x100;
}

mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
}

mod config {
    pub const SELECT: usize = 0;
    pub const SUBSELL: usize = 1;
    pub const SIZE: usize = 2;
    pub const UNION: usize = 8;
}

mod input {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_REL: u16 = 0x02;
    pub const EV_ABS: u16 = 0x03;

    pub const SYN_REPORT: u16 = 0x00;

    pub const REL_X: u16 = 0x00;
    pub const REL_Y: u16 = 0x01;
    pub const REL_WHEEL: u16 = 0x08;

    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;

    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;

    pub const KEY_A: u16 = 30;

    pub const CFG_EV_BITS: u8 = 0x11;
    pub const CFG_ABS_INFO: u8 = 0x12;
}

const DESC_F_WRITE: u16 = 2;

/// What kind of pointing device was found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerKind {
    /// Absolute coordinates (QEMU `virtio-tablet`).
    Absolute,
    /// Relative motion (QEMU `virtio-mouse`).
    Relative,
}

/// What kind of virtio input device a slot contains.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    /// A tablet or mouse.
    Pointer(PointerKind),
    /// A keyboard.
    Keyboard,
}

/// One virtqueue descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

impl Descriptor {
    const EMPTY: Self = Self {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvailRing {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
}

const AVAIL_BYTES: usize = 4 + 2 * QUEUE_SIZE;
const FIRST_PAD: usize = 4096 - 16 * QUEUE_SIZE - AVAIL_BYTES;

#[repr(C)]
#[derive(Clone, Copy)]
struct UsedElement {
    id: u32,
    len: u32,
}

impl UsedElement {
    const EMPTY: Self = Self { id: 0, len: 0 };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UsedRing {
    flags: u16,
    idx: u16,
    ring: [UsedElement; QUEUE_SIZE],
}

const USED_BYTES: usize = 4 + 8 * QUEUE_SIZE;
const SECOND_PAD: usize = 4096 - USED_BYTES;

/// Legacy virtqueues are one page-aligned region: descriptors, then the
/// available ring, then (at the next page) the used ring. Giving the modern
/// layout the same shape keeps one buffer usable for both transports.
#[repr(C, align(4096))]
struct QueueMemory {
    desc: [Descriptor; QUEUE_SIZE],
    avail: AvailRing,
    first_pad: [u8; FIRST_PAD],
    used: UsedRing,
    second_pad: [u8; SECOND_PAD],
}

impl QueueMemory {
    const fn new() -> Self {
        Self {
            desc: [Descriptor::EMPTY; QUEUE_SIZE],
            avail: AvailRing {
                flags: 0,
                idx: 0,
                ring: [0; QUEUE_SIZE],
            },
            first_pad: [0; FIRST_PAD],
            used: UsedRing {
                flags: 0,
                idx: 0,
                ring: [UsedElement::EMPTY; QUEUE_SIZE],
            },
            second_pad: [0; SECOND_PAD],
        }
    }
}

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct EventBuffer {
    bytes: [u8; 8],
}

impl EventBuffer {
    const EMPTY: Self = Self { bytes: [0; 8] };
}

/// One raw Linux input event from the virtio input queue.
#[derive(Clone, Copy)]
struct RawEvent {
    kind: u16,
    code: u16,
    value: u32,
}

impl RawEvent {
    fn from_bytes(bytes: &[u8; 8]) -> Self {
        Self {
            kind: u16::from_le_bytes([bytes[0], bytes[1]]),
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            value: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        }
    }
}

fn read32_at(base: usize, offset: usize) -> u32 {
    // SAFETY: `base` is a virtio MMIO window supplied by the board.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

fn write32_at(base: usize, offset: usize, value: u32) {
    // SAFETY: see `read32_at`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

fn read8_at(base: usize, offset: usize) -> u8 {
    // SAFETY: see `read32_at`.
    unsafe { core::ptr::read_volatile((base + offset) as *const u8) }
}

fn write8_at(base: usize, offset: usize, value: u8) {
    // SAFETY: see `read32_at`.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u8, value) }
}

fn config_bit(base: usize, select: u8, subsel: u8, bit: u16) -> bool {
    write8_at(base, reg::CONFIG + config::SELECT, select);
    write8_at(base, reg::CONFIG + config::SUBSELL, subsel);
    let size = read8_at(base, reg::CONFIG + config::SIZE) as usize;
    let byte = bit as usize / 8;
    if byte >= size {
        return false;
    }
    read8_at(base, reg::CONFIG + config::UNION + byte) & (1 << (bit % 8)) != 0
}

/// Probes one MMIO slot and reports which kind of input device it contains.
pub fn probe(base: usize) -> Option<DeviceKind> {
    if read32_at(base, reg::MAGIC) != MAGIC {
        return None;
    }
    let version = read32_at(base, reg::VERSION);
    if version != 1 && version != 2 {
        return None;
    }
    if read32_at(base, reg::DEVICE_ID) != DEVICE_ID_INPUT {
        return None;
    }

    if config_bit(base, input::CFG_EV_BITS, input::EV_ABS as u8, input::ABS_X) {
        Some(DeviceKind::Pointer(PointerKind::Absolute))
    } else if config_bit(base, input::CFG_EV_BITS, input::EV_REL as u8, input::REL_X) {
        Some(DeviceKind::Pointer(PointerKind::Relative))
    } else if config_bit(base, input::CFG_EV_BITS, input::EV_KEY as u8, input::KEY_A) {
        Some(DeviceKind::Keyboard)
    } else {
        None
    }
}

/// Finds the first pointer in the QEMU virtio MMIO range.
pub fn find(base: usize, stride: usize, slots: usize) -> Option<(usize, PointerKind)> {
    for slot in 0..slots {
        let address = base + slot * stride;
        if let Some(DeviceKind::Pointer(kind)) = probe(address) {
            return Some((address, kind));
        }
    }
    None
}

/// Finds the first keyboard in the QEMU virtio MMIO range.
pub fn find_keyboard(base: usize, stride: usize, slots: usize) -> Option<usize> {
    for slot in 0..slots {
        let address = base + slot * stride;
        if probe(address) == Some(DeviceKind::Keyboard) {
            return Some(address);
        }
    }
    None
}

/// A virtio input device that exposes a pointer.
pub struct VirtioInput {
    base: usize,
    version: u32,
    kind: DeviceKind,
    width: u16,
    height: u16,
    abs_min_x: u32,
    abs_max_x: u32,
    abs_min_y: u32,
    abs_max_y: u32,
    queue: QueueMemory,
    buffers: [EventBuffer; QUEUE_SIZE],
    avail_idx: u16,
    published_idx: u16,
    used_seen: u16,
    ready: bool,
    x: u16,
    y: u16,
    left: bool,
    right: bool,
    middle: bool,
    pending_x: Option<u16>,
    pending_y: Option<u16>,
    pending_left: Option<bool>,
    pending_right: Option<bool>,
    pending_middle: Option<bool>,
    pending_wheel: i32,
    keys: [Option<KeyboardEvent>; KEY_QUEUE_SIZE],
    key_head: usize,
    key_tail: usize,
}

impl VirtioInput {
    /// Creates an uninitialised device.
    pub const fn new() -> Self {
        Self {
            base: 0,
            version: 0,
            kind: DeviceKind::Pointer(PointerKind::Absolute),
            width: 0,
            height: 0,
            abs_min_x: 0,
            abs_max_x: 32767,
            abs_min_y: 0,
            abs_max_y: 32767,
            queue: QueueMemory::new(),
            buffers: [EventBuffer::EMPTY; QUEUE_SIZE],
            avail_idx: 0,
            published_idx: 0,
            used_seen: 0,
            ready: false,
            x: 0,
            y: 0,
            left: false,
            right: false,
            middle: false,
            pending_x: None,
            pending_y: None,
            pending_left: None,
            pending_right: None,
            pending_middle: None,
            pending_wheel: 0,
            keys: [None; KEY_QUEUE_SIZE],
            key_head: 0,
            key_tail: 0,
        }
    }

    /// Negotiated virtio transport version (1 = legacy, 2 = modern).
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Kind of input device this instance represents.
    pub const fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// Returns `true` once the queue is live.
    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    /// Current cursor position.
    pub const fn position(&self) -> (u16, u16) {
        (self.x, self.y)
    }

    fn read32(&self, offset: usize) -> u32 {
        read32_at(self.base, offset)
    }

    fn write32(&self, offset: usize, value: u32) {
        write32_at(self.base, offset, value)
    }

    fn write8(&self, offset: usize, value: u8) {
        write8_at(self.base, offset, value)
    }

    fn read_abs_info(&self, axis: u8) -> (u32, u32) {
        self.write8(reg::CONFIG + config::SELECT, input::CFG_ABS_INFO);
        self.write8(reg::CONFIG + config::SUBSELL, axis);
        let min = self.read32(reg::CONFIG + config::UNION);
        let max = self.read32(reg::CONFIG + config::UNION + 4);
        if max <= min {
            (0, 32767)
        } else {
            (min, max)
        }
    }

    /// Initialises `base` as a pointer sized for a `width` x `height` screen.
    ///
    /// Returns `false` when the slot is not a virtio input pointer or the
    /// device does not accept the queue.
    pub fn init(&mut self, base: usize, width: u16, height: u16) -> bool {
        self.base = base;
        self.width = width.max(1);
        self.height = height.max(1);
        self.x = self.width / 2;
        self.y = self.height / 2;

        let Some(kind) = probe(base) else {
            return false;
        };
        self.kind = kind;
        self.version = self.read32(reg::VERSION);
        self.key_head = 0;
        self.key_tail = 0;
        self.keys = [None; KEY_QUEUE_SIZE];

        // Reset, then announce the driver.
        self.write32(reg::STATUS, 0);
        self.write32(reg::STATUS, status::ACKNOWLEDGE);
        self.write32(reg::STATUS, status::ACKNOWLEDGE | status::DRIVER);

        if self.version == 2 {
            // Accept VIRTIO_F_VERSION_1 (feature bit 32) and nothing else.
            self.write32(reg::DEVICE_FEATURES_SEL, 1);
            let high = self.read32(reg::DEVICE_FEATURES);
            if high & 1 == 0 {
                return false;
            }
            self.write32(reg::DRIVER_FEATURES_SEL, 1);
            self.write32(reg::DRIVER_FEATURES, 1);
            self.write32(reg::DRIVER_FEATURES_SEL, 0);
            self.write32(reg::DRIVER_FEATURES, 0);

            let status = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
            self.write32(reg::STATUS, status);
            if self.read32(reg::STATUS) & status::FEATURES_OK == 0 {
                return false;
            }
        } else {
            // Legacy: no optional features are required for input.
            self.write32(reg::DEVICE_FEATURES_SEL, 0);
            let _ = self.read32(reg::DEVICE_FEATURES);
            self.write32(reg::DRIVER_FEATURES_SEL, 0);
            self.write32(reg::DRIVER_FEATURES, 0);
        }

        if self.kind == DeviceKind::Pointer(PointerKind::Absolute) {
            let (min, max) = self.read_abs_info(input::ABS_X as u8);
            self.abs_min_x = min;
            self.abs_max_x = max;
            let (min, max) = self.read_abs_info(input::ABS_Y as u8);
            self.abs_min_y = min;
            self.abs_max_y = max;
        }

        if !self.setup_queue() {
            return false;
        }

        let mut final_status = status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK;
        if self.version == 2 {
            final_status |= status::FEATURES_OK;
        }
        self.write32(reg::STATUS, final_status);

        self.ready = true;
        // The queue was filled before the device was told where it lives, so
        // force the first notification even though the publish index already
        // matches the available index.
        self.published_idx = 0;
        self.publish();
        true
    }

    fn setup_queue(&mut self) -> bool {
        self.write32(reg::QUEUE_SEL, 0);
        let max = self.read32(reg::QUEUE_NUM_MAX) as usize;
        if max < QUEUE_SIZE {
            return false;
        }
        self.write32(reg::QUEUE_NUM, QUEUE_SIZE as u32);

        // Fill the queue before the device is told where it lives.
        self.avail_idx = 0;
        self.published_idx = 0;
        self.used_seen = 0;
        for index in 0..QUEUE_SIZE {
            let buffer = core::ptr::addr_of!(self.buffers[index]) as u64;
            self.queue.desc[index] = Descriptor {
                addr: buffer,
                len: 8,
                flags: DESC_F_WRITE,
                next: 0,
            };
            self.queue.avail.ring[index] = index as u16;
        }
        self.queue.avail.flags = 0;
        self.queue.avail.idx = QUEUE_SIZE as u16;
        self.avail_idx = QUEUE_SIZE as u16;
        self.published_idx = QUEUE_SIZE as u16;
        fence(Ordering::Release);

        let desc = core::ptr::addr_of!(self.queue.desc) as u64;
        let avail = core::ptr::addr_of!(self.queue.avail) as u64;
        let used = core::ptr::addr_of!(self.queue.used) as u64;

        if self.version == 2 {
            self.write32(reg::QUEUE_DESC_LOW, desc as u32);
            self.write32(reg::QUEUE_DESC_HIGH, (desc >> 32) as u32);
            self.write32(reg::QUEUE_DRIVER_LOW, avail as u32);
            self.write32(reg::QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
            self.write32(reg::QUEUE_DEVICE_LOW, used as u32);
            self.write32(reg::QUEUE_DEVICE_HIGH, (used >> 32) as u32);
            self.write32(reg::QUEUE_READY, 1);
        } else {
            self.write32(reg::QUEUE_ALIGN, 4096);
            self.write32(reg::GUEST_PAGE_SIZE, 4096);
            self.write32(reg::QUEUE_PFN, (desc >> 12) as u32);
        }
        true
    }

    fn publish(&mut self) {
        if self.published_idx == self.avail_idx {
            return;
        }
        self.published_idx = self.avail_idx;
        fence(Ordering::Release);
        // SAFETY: `avail.idx` is part of the queue owned by this driver.
        unsafe {
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!(self.queue.avail.idx),
                self.avail_idx,
            );
        }
        fence(Ordering::Release);
        self.write32(reg::QUEUE_NOTIFY, 0);
    }

    fn release_descriptor(&mut self, id: u16) {
        let slot = (self.avail_idx as usize) % QUEUE_SIZE;
        self.queue.avail.ring[slot] = id;
        self.avail_idx = self.avail_idx.wrapping_add(1);
    }

    fn scale(value: u32, min: u32, max: u32, extent: u16) -> u16 {
        if extent == 0 {
            return 0;
        }
        if max <= min {
            return value.min(extent as u32) as u16;
        }
        let value = value.clamp(min, max) - min;
        let span = (max - min) as u64;
        let pixels = extent.saturating_sub(1) as u64;
        ((value as u64 * pixels) / span) as u16
    }

    fn set_button(&mut self, button: u16, down: bool) {
        match button {
            input::BTN_LEFT => {
                if self.left != down {
                    self.left = down;
                    self.pending_left = Some(down);
                }
            }
            input::BTN_RIGHT => {
                if self.right != down {
                    self.right = down;
                    self.pending_right = Some(down);
                }
            }
            input::BTN_MIDDLE => {
                if self.middle != down {
                    self.middle = down;
                    self.pending_middle = Some(down);
                }
            }
            _ => {}
        }
    }

    fn handle_event(&mut self, event: RawEvent) -> Option<PointerEvent> {
        match event.kind {
            input::EV_ABS if self.kind == DeviceKind::Pointer(PointerKind::Absolute) => {
                match event.code {
                    input::ABS_X => {
                        self.pending_x = Some(Self::scale(
                            event.value,
                            self.abs_min_x,
                            self.abs_max_x,
                            self.width,
                        ));
                    }
                    input::ABS_Y => {
                        self.pending_y = Some(Self::scale(
                            event.value,
                            self.abs_min_y,
                            self.abs_max_y,
                            self.height,
                        ));
                    }
                    _ => {}
                }
                None
            }
            input::EV_REL if self.kind == DeviceKind::Pointer(PointerKind::Relative) => {
                match event.code {
                    input::REL_X => {
                        let current = self.pending_x.unwrap_or(self.x) as i32;
                        let next = (current + event.value as i32).clamp(0, self.width as i32 - 1);
                        self.pending_x = Some(next as u16);
                        None
                    }
                    input::REL_Y => {
                        let current = self.pending_y.unwrap_or(self.y) as i32;
                        let next = (current + event.value as i32).clamp(0, self.height as i32 - 1);
                        self.pending_y = Some(next as u16);
                        None
                    }
                    input::REL_WHEEL => {
                        self.pending_wheel = self.pending_wheel.saturating_add(event.value as i32);
                        // A wheel notch is a discrete action; report it now so
                        // a notch and its SYN never cancel out.
                        self.take_report()
                    }
                    _ => None,
                }
            }
            input::EV_KEY if self.kind == DeviceKind::Keyboard => {
                self.push_key(KeyboardEvent {
                    code: event.code,
                    pressed: event.value != 0,
                });
                None
            }
            input::EV_KEY => {
                let before = (self.left, self.right, self.middle);
                self.set_button(event.code, event.value != 0);
                let after = (self.left, self.right, self.middle);
                if before == after {
                    None
                } else {
                    // Report the transition immediately. A press and release
                    // delivered back-to-back therefore stays ordered for the
                    // desktop instead of collapsing into one state change.
                    self.take_report()
                }
            }
            input::EV_SYN if event.code == input::SYN_REPORT => self.take_report(),
            _ => None,
        }
    }

    fn take_report(&mut self) -> Option<PointerEvent> {
        let old_x = self.x;
        let old_y = self.y;
        let x = self.pending_x.take().unwrap_or(old_x);
        let y = self.pending_y.take().unwrap_or(old_y);
        let left = self.pending_left.take();
        let right = self.pending_right.take();
        let middle = self.pending_middle.take();
        let wheel = core::mem::take(&mut self.pending_wheel);
        self.x = x;
        self.y = y;

        if x == old_x
            && y == old_y
            && left.is_none()
            && right.is_none()
            && middle.is_none()
            && wheel == 0
        {
            return None;
        }

        Some(PointerEvent {
            x,
            y,
            left,
            right,
            middle,
            wheel,
        })
    }

    fn push_key(&mut self, event: KeyboardEvent) {
        let next = (self.key_tail + 1) % KEY_QUEUE_SIZE;
        if next == self.key_head {
            // Queue full: drop the newest transition rather than overwrite
            // older ones that the desktop has not seen yet.
            return;
        }
        self.keys[self.key_tail] = Some(event);
        self.key_tail = next;
    }

    fn take_key(&mut self) -> Option<KeyboardEvent> {
        if self.key_head == self.key_tail {
            return None;
        }
        let event = self.keys[self.key_head].take();
        self.key_head = (self.key_head + 1) % KEY_QUEUE_SIZE;
        event
    }

    /// Processes one descriptor from the used ring.
    ///
    /// `Some(report)` means a descriptor was consumed, with an optional pointer
    /// report; `None` means the queue is empty.
    fn process_next(&mut self) -> Option<Option<PointerEvent>> {
        if !self.ready {
            return None;
        }

        fence(Ordering::Acquire);
        // SAFETY: the device writes the used ring in the queue memory owned by
        // this driver; the index is volatile because it changes asynchronously.
        let used_idx =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!(self.queue.used.idx)) };
        if self.used_seen == used_idx {
            return None;
        }

        let slot = (self.used_seen as usize) % QUEUE_SIZE;
        // SAFETY: `slot` is inside the used ring.
        let (id, len) = unsafe {
            let element = core::ptr::addr_of!(self.queue.used.ring[slot]);
            (
                core::ptr::read_volatile(core::ptr::addr_of!((*element).id)),
                core::ptr::read_volatile(core::ptr::addr_of!((*element).len)),
            )
        };
        self.used_seen = self.used_seen.wrapping_add(1);

        if (id as usize) < QUEUE_SIZE && len >= 8 {
            let raw = RawEvent::from_bytes(&self.buffers[id as usize].bytes);
            let report = self.handle_event(raw);
            self.release_descriptor(id as u16);
            self.publish();
            Some(report)
        } else {
            self.release_descriptor(id as u16);
            self.publish();
            Some(None)
        }
    }

    /// Returns the next complete pointer report.
    ///
    /// Call this repeatedly until it returns `None`; each call processes at
    /// least one descriptor when input is pending.
    pub fn poll(&mut self) -> Option<PointerEvent> {
        loop {
            match self.process_next()? {
                Some(report) => return Some(report),
                None => {}
            }
        }
    }

    /// Returns the next key transition from a keyboard device.
    ///
    /// Call this repeatedly until it returns `None`.
    pub fn poll_key(&mut self) -> Option<KeyboardEvent> {
        loop {
            if let Some(event) = self.take_key() {
                return Some(event);
            }
            self.process_next()?;
        }
    }
}

impl Default for VirtioInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_memory_keeps_the_legacy_page_layout() {
        assert_eq!(core::mem::size_of::<QueueMemory>(), 8192);
        assert_eq!(core::mem::align_of::<QueueMemory>(), 4096);
        assert_eq!(core::mem::size_of::<Descriptor>(), 16);
        assert_eq!(core::mem::size_of::<AvailRing>(), AVAIL_BYTES);
        assert_eq!(core::mem::size_of::<UsedRing>(), USED_BYTES);
    }

    #[test]
    fn absolute_coordinates_scale_to_the_framebuffer() {
        assert_eq!(VirtioInput::scale(0, 0, 32767, 1024), 0);
        assert_eq!(VirtioInput::scale(32767, 0, 32767, 1024), 1023);
        assert_eq!(VirtioInput::scale(16384, 0, 32767, 1024), 511);
        assert_eq!(VirtioInput::scale(500, 0, 1000, 101), 50);
        assert_eq!(VirtioInput::scale(9, 0, 0, 100), 9);
        assert_eq!(VirtioInput::scale(100, 0, 10, 0), 0);
    }

    #[test]
    fn raw_events_are_little_endian() {
        let bytes = [3, 0, 0, 0, 0x10, 0x20, 0, 0];
        let event = RawEvent::from_bytes(&bytes);
        assert_eq!(event.kind, input::EV_ABS);
        assert_eq!(event.code, input::ABS_X);
        assert_eq!(event.value, 0x2010);
    }

    #[test]
    fn reports_carry_only_transitions() {
        let mut device = VirtioInput::new();
        device.width = 1024;
        device.height = 768;
        device.x = 10;
        device.y = 20;

        device.pending_x = Some(40);
        device.pending_y = Some(60);
        device.pending_left = Some(true);
        let report = device.take_report().expect("report");
        assert_eq!((report.x, report.y), (40, 60));
        assert_eq!(report.left, Some(true));
        assert_eq!(report.right, None);

        assert!(device.take_report().is_none());
        assert_eq!(device.position(), (40, 60));
    }

    #[test]
    fn keyboard_events_are_queued_in_order() {
        let mut device = VirtioInput::new();
        device.kind = DeviceKind::Keyboard;
        assert!(device
            .handle_event(RawEvent {
                kind: input::EV_KEY,
                code: input::KEY_A,
                value: 1,
            })
            .is_none());
        assert!(device
            .handle_event(RawEvent {
                kind: input::EV_KEY,
                code: input::KEY_A,
                value: 0,
            })
            .is_none());

        assert_eq!(
            device.take_key(),
            Some(KeyboardEvent {
                code: input::KEY_A,
                pressed: true
            })
        );
        assert_eq!(
            device.take_key(),
            Some(KeyboardEvent {
                code: input::KEY_A,
                pressed: false
            })
        );
        assert_eq!(device.take_key(), None);
    }

    #[test]
    fn a_press_and_release_in_one_group_stays_ordered() {
        let mut device = VirtioInput::new();
        device.width = 100;
        device.height = 100;
        let down = device
            .handle_event(RawEvent {
                kind: input::EV_KEY,
                code: input::BTN_LEFT,
                value: 1,
            })
            .expect("press");
        assert_eq!(down.left, Some(true));
        let up = device
            .handle_event(RawEvent {
                kind: input::EV_KEY,
                code: input::BTN_LEFT,
                value: 0,
            })
            .expect("release");
        assert_eq!(up.left, Some(false));
    }
}
