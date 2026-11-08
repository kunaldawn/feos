//! A virtio-input pointer (tablet or mouse) or keyboard.
//!
//! QEMU's `virt` machine exposes 32 virtio MMIO slots starting at
//! `0x0a00_0000`. Adding `-device virtio-tablet-device` gives the guest an
//! absolute pointing device; `-device virtio-mouse-device` gives a relative
//! one. The transport comes from `feos-virtio-core`, so `virtio-tablet-pci`
//! works the same way through [`VirtioInput::init_transport`]. The driver
//! negotiates no optional features and keeps a small pre-filled event queue so
//! pointer reports arrive without interrupts.
//!
//! The driver is polled. [`VirtioInput::poll`] decodes one SYN_REPORT-terminated
//! group at a time and returns an absolute framebuffer position plus any button
//! transitions, which is exactly what the desktop session feeds into
//! `feos_gui::Input`.

#![no_std]

use feos_hal::{dma, KeyboardEvent, PointerEvent};
use feos_virtio_core::{AnyTransport, MmioTransport, Queue, Segment, Transport};

/// Number of descriptors in the event queue. Must be a power of two.
///
/// One tablet report takes up to five descriptors, and a release that doesn't
/// fit is lost, which leaves a button or modifier stuck. So this needs to hold
/// what a stalled poller can miss. QEMU's virtio-input queues hold at most 64.
pub const QUEUE_SIZE: usize = 64;

/// Number of decoded key transitions buffered for the caller.
pub const KEY_QUEUE_SIZE: usize = 32;

/// First virtio MMIO slot on QEMU's `virt` machine.
pub const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;

/// Distance between virtio MMIO slots.
pub const VIRTIO_MMIO_STRIDE: usize = 0x200;

/// Number of virtio MMIO slots exposed by QEMU's `virt` machine.
pub const VIRTIO_MMIO_SLOTS: usize = 32;

const DEVICE_ID_INPUT: u32 = feos_virtio_core::device::INPUT;

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
    pub const BTN_GEAR_DOWN: u16 = 0x150;
    pub const BTN_GEAR_UP: u16 = 0x151;

    pub const KEY_A: u16 = 30;

    pub const CFG_EV_BITS: u8 = 0x11;
    pub const CFG_ABS_INFO: u8 = 0x12;
}

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

fn config_bit<T: Transport>(transport: &T, select: u8, subsel: u8, bit: u16) -> bool {
    transport.config_write8(config::SELECT, select);
    transport.config_write8(config::SUBSELL, subsel);
    let size = transport.config_read8(config::SIZE) as usize;
    let byte = bit as usize / 8;
    if byte >= size {
        return false;
    }
    transport.config_read8(config::UNION + byte) & (1 << (bit % 8)) != 0
}

/// Reports which kind of input device `transport` reaches.
pub fn probe_transport<T: Transport>(transport: &T) -> Option<DeviceKind> {
    if transport.device_id() != DEVICE_ID_INPUT {
        return None;
    }
    if config_bit(transport, input::CFG_EV_BITS, input::EV_ABS as u8, input::ABS_X) {
        Some(DeviceKind::Pointer(PointerKind::Absolute))
    } else if config_bit(transport, input::CFG_EV_BITS, input::EV_REL as u8, input::REL_X) {
        Some(DeviceKind::Pointer(PointerKind::Relative))
    } else if config_bit(transport, input::CFG_EV_BITS, input::EV_KEY as u8, input::KEY_A) {
        Some(DeviceKind::Keyboard)
    } else {
        None
    }
}

/// Probes one MMIO slot and reports which kind of input device it contains.
pub fn probe(base: usize) -> Option<DeviceKind> {
    probe_transport(&MmioTransport::probe(base)?)
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
    transport: Option<AnyTransport>,
    kind: DeviceKind,
    width: u16,
    height: u16,
    abs_min_x: u32,
    abs_max_x: u32,
    abs_min_y: u32,
    abs_max_y: u32,
    queue: Queue<QUEUE_SIZE>,
    buffers: [EventBuffer; QUEUE_SIZE],
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
            transport: None,
            kind: DeviceKind::Pointer(PointerKind::Absolute),
            width: 0,
            height: 0,
            abs_min_x: 0,
            abs_max_x: 32767,
            abs_min_y: 0,
            abs_max_y: 32767,
            queue: Queue::new(),
            buffers: [EventBuffer::EMPTY; QUEUE_SIZE],
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

    fn read_abs_info<T: Transport>(transport: &T, axis: u8) -> (u32, u32) {
        transport.config_write8(config::SELECT, input::CFG_ABS_INFO);
        transport.config_write8(config::SUBSELL, axis);
        let min = transport.config_read32(config::UNION);
        let max = transport.config_read32(config::UNION + 4);
        if max <= min {
            (0, 32767)
        } else {
            (min, max)
        }
    }

    /// Initialises the MMIO slot at `base` as a pointer sized for a
    /// `width` x `height` screen.
    ///
    /// Returns `false` when the slot is not a virtio input device or the
    /// device does not accept the queue.
    pub fn init(&mut self, base: usize, width: u16, height: u16) -> bool {
        match MmioTransport::probe(base) {
            Some(transport) => self.init_transport(AnyTransport::Mmio(transport), width, height),
            None => false,
        }
    }

    /// Initialises the device behind `transport`, on either bus.
    pub fn init_transport(&mut self, transport: AnyTransport, width: u16, height: u16) -> bool {
        self.width = width.max(1);
        self.height = height.max(1);
        self.x = self.width / 2;
        self.y = self.height / 2;

        let Some(kind) = probe_transport(&transport) else {
            return false;
        };
        self.kind = kind;
        self.key_head = 0;
        self.key_tail = 0;
        self.keys = [None; KEY_QUEUE_SIZE];

        // No optional features are required for input.
        if feos_virtio_core::negotiate(&transport, 0).is_none() {
            return false;
        }

        if self.kind == DeviceKind::Pointer(PointerKind::Absolute) {
            let (min, max) = Self::read_abs_info(&transport, input::ABS_X as u8);
            self.abs_min_x = min;
            self.abs_max_x = max;
            let (min, max) = Self::read_abs_info(&transport, input::ABS_Y as u8);
            self.abs_min_y = min;
            self.abs_max_y = max;
        }

        if !self.setup_queue(&transport) {
            return false;
        }
        feos_virtio_core::finish(&transport);
        self.transport = Some(transport);
        self.ready = true;
        // The queue was filled before the device was told where it lives, so
        // the first notification tells it about all of them.
        self.publish();
        true
    }

    fn setup_queue(&mut self, transport: &AnyTransport) -> bool {
        if (transport.queue_max(0) as usize) < QUEUE_SIZE {
            return false;
        }

        // Fill the queue before the device is told where it lives.
        self.queue.reset();
        for index in 0..QUEUE_SIZE {
            let buffer = core::ptr::addr_of!(self.buffers[index]) as u64;
            let segment = Segment {
                addr: buffer,
                len: 8,
                device_writes: true,
            };
            if self.queue.add(&[segment]).is_none() {
                return false;
            }
        }
        let (desc, avail, used) = self.queue.addresses();
        transport.queue_activate(0, QUEUE_SIZE as u16, desc, avail, used)
    }

    fn publish(&mut self) {
        if self.queue.publish() {
            if let Some(transport) = self.transport.as_ref() {
                transport.queue_notify(0);
            }
        }
    }

    fn release_descriptor(&mut self, id: u16) {
        self.queue.resubmit(id);
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
            input::BTN_MIDDLE
                if self.middle != down => {
                    self.middle = down;
                    self.pending_middle = Some(down);
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
            // A tablet reports position with EV_ABS but still sends its wheel as
            // EV_REL, so the wheel is accepted from every kind of pointer.
            input::EV_REL
                if event.code == input::REL_WHEEL && matches!(self.kind, DeviceKind::Pointer(_)) =>
            {
                self.pending_wheel = self.pending_wheel.saturating_add(event.value as i32);
                // A wheel notch is a discrete action; report it now so a notch
                // and its SYN never cancel out.
                self.take_report()
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
            // Older QEMU versions send the wheel as these two buttons.
            input::EV_KEY if event.code == input::BTN_GEAR_UP => {
                self.pending_wheel = self
                    .pending_wheel
                    .saturating_add((event.value != 0) as i32);
                self.take_report()
            }
            input::EV_KEY if event.code == input::BTN_GEAR_DOWN => {
                self.pending_wheel = self
                    .pending_wheel
                    .saturating_sub((event.value != 0) as i32);
                self.take_report()
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

        let (id, len) = self.queue.pop_used()?;
        // The event buffer was written before the used index, and `pop_used`
        // ordered this read after it.
        dma::rmb();

        if (id as usize) >= QUEUE_SIZE {
            // Not a buffer this driver offered, so there's nothing to give back.
            return Some(None);
        }
        let report = if len >= 8 {
            // The device writes these bytes behind the compiler's back.
            // SAFETY: `id` is in range, and the buffer is the device's no longer.
            let bytes = unsafe {
                core::ptr::read_volatile(core::ptr::addr_of!(self.buffers[id as usize].bytes))
            };
            self.handle_event(RawEvent::from_bytes(&bytes))
        } else {
            None
        };
        self.release_descriptor(id);
        self.publish();
        Some(report)
    }

    /// Returns the next complete pointer report.
    ///
    /// Call this repeatedly until it returns `None`; each call processes at
    /// least one descriptor when input is pending.
    pub fn set_screen(&mut self, width: u16, height: u16) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.x = self.x.min(self.width - 1);
        self.y = self.y.min(self.height - 1);
    }

    /// Returns the next pointer report, if any.
    pub fn poll(&mut self) -> Option<PointerEvent> {
        loop {
            if let Some(report) = self.process_next()? { return Some(report) }
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
    fn a_tablet_reports_the_wheel() {
        // DRV-5: EV_REL was only accepted from relative pointers, and QEMU's
        // default pointer on this machine is a tablet.
        let mut device = VirtioInput::new();
        device.width = 100;
        device.height = 100;
        for (value, wheel) in [(1, 1), (u32::MAX, -1)] {
            let report = device
                .handle_event(RawEvent {
                    kind: input::EV_REL,
                    code: input::REL_WHEEL,
                    value,
                })
                .expect("wheel report");
            assert_eq!(report.wheel, wheel);
        }
    }

    #[test]
    fn a_tablet_ignores_relative_motion_but_a_mouse_uses_it() {
        let mut device = VirtioInput::new();
        device.width = 100;
        device.height = 100;
        let motion = RawEvent {
            kind: input::EV_REL,
            code: input::REL_X,
            value: 5,
        };
        assert!(device.handle_event(motion).is_none());
        assert_eq!(device.pending_x, None);

        device.kind = DeviceKind::Pointer(PointerKind::Relative);
        assert!(device.handle_event(motion).is_none());
        assert_eq!(device.pending_x, Some(5));
    }

    #[test]
    fn gear_buttons_report_as_the_wheel() {
        let mut device = VirtioInput::new();
        device.width = 100;
        device.height = 100;
        let press = |code| RawEvent {
            kind: input::EV_KEY,
            code,
            value: 1,
        };
        assert_eq!(device.handle_event(press(input::BTN_GEAR_UP)).unwrap().wheel, 1);
        assert_eq!(device.handle_event(press(input::BTN_GEAR_DOWN)).unwrap().wheel, -1);
    }

    #[test]
    fn a_keyboard_ignores_the_wheel() {
        let mut device = VirtioInput::new();
        device.kind = DeviceKind::Keyboard;
        let wheel = RawEvent {
            kind: input::EV_REL,
            code: input::REL_WHEEL,
            value: 1,
        };
        assert!(device.handle_event(wheel).is_none());
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
