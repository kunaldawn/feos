//! The controller: registers, rings, enumeration and transfers.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use feos_hal::dma;
use feos_hal::{BlockIo, BlockIoError, Clock, KeyboardEvent, PointerEvent, UsbStats};
use feos_sync::{futex_wait_ms, futex_wake, SpinLock};

use crate::descriptors::{self, parse_configuration, parse_device, Configuration, Endpoint, Interface};
use crate::hid::{parse_report, KeyboardState, PointerState, ReportLayout};
use crate::msc;

/// TRBs in each ring, one of them the link back to the start.
const RING_TRBS: usize = 256;
/// TRBs in the event ring.
const EVENT_TRBS: usize = 256;
/// Bytes in a page, for the things that must be page-aligned.
const PAGE: usize = 4096;
/// Slots in use, at most.
const MAX_SLOTS: usize = 16;
/// How long a controller has to come out of reset, or a port to reset.
const RESET_WAIT_MS: u64 = 2000;
/// How long a command or a transfer on the default endpoint can take.
const COMMAND_WAIT_MS: u64 = 5000;
/// How long a disk transfer can take.
const BULK_WAIT_MS: u64 = 30_000;
/// How long to wait for an interrupt before looking anyway.
const WAIT_SLICE_MS: u64 = 50;
/// Largest piece a TRB carries, and the boundary it may not cross.
const TRB_MAX: usize = 64 * 1024;
/// Largest disk transfer in one command.
const MAX_TRANSFER: usize = 1024 * 1024;

mod trb {
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    pub const LINK: u32 = 6;
    pub const ENABLE_SLOT: u32 = 9;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    pub const EVALUATE_CONTEXT: u32 = 13;
    pub const RESET_ENDPOINT: u32 = 14;
    pub const SET_TR_DEQUEUE: u32 = 16;
    pub const EVENT_TRANSFER: u32 = 32;
    pub const EVENT_COMMAND: u32 = 33;

    pub const CYCLE: u32 = 1;
    pub const TOGGLE: u32 = 1 << 1;
    pub const ISP: u32 = 1 << 2;
    pub const CHAIN: u32 = 1 << 4;
    pub const IOC: u32 = 1 << 5;
    pub const IDT: u32 = 1 << 6;
    /// Data flows to the host, on a Data or Status stage.
    pub const DIR_IN: u32 = 1 << 16;
    pub const fn kind(kind: u32) -> u32 {
        kind << 10
    }
}

mod code {
    pub const SUCCESS: u8 = 1;
    pub const STALL: u8 = 6;
    pub const SHORT_PACKET: u8 = 13;
}

mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const CRCR: usize = 0x18;
    pub const DCBAAP: usize = 0x30;
    pub const CONFIG: usize = 0x38;
    pub const PORTS: usize = 0x400;

    pub const CMD_RUN: u32 = 1;
    pub const CMD_RESET: u32 = 1 << 1;
    pub const CMD_INTERRUPTS: u32 = 1 << 2;
    pub const STS_HALTED: u32 = 1;
    pub const STS_EINT: u32 = 1 << 3;
    pub const STS_NOT_READY: u32 = 1 << 11;
}

mod port {
    pub const CONNECTED: u32 = 1;
    pub const ENABLED: u32 = 1 << 1;
    pub const RESET: u32 = 1 << 4;
    pub const POWER: u32 = 1 << 9;
    pub const SPEED_SHIFT: u32 = 10;
    pub const SPEED_MASK: u32 = 0xf;
    pub const CHANGES: u32 = 0x7f << 17;
    pub const RESET_CHANGE: u32 = 1 << 21;
}

/// Port speeds, as PORTSC and the slot context name them.
mod speed {
    pub const FULL: u32 = 1;
    pub const LOW: u32 = 2;
    pub const HIGH: u32 = 3;
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

#[repr(C, align(4096))]
struct RingMemory([Trb; RING_TRBS]);

#[repr(C, align(4096))]
struct EventMemory([Trb; EVENT_TRBS]);

#[repr(C, align(4096))]
struct Page([u8; PAGE]);

/// Allocates zeroed memory for a device to use.
fn zeroed<T>() -> Box<T> {
    // SAFETY: only used here on types made of integers, for which zero is valid.
    unsafe { Box::<T>::new_zeroed().assume_init() }
}

/// A ring the driver writes and the controller reads.
struct Ring {
    memory: Box<RingMemory>,
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    fn new() -> Self {
        let memory: Box<RingMemory> = zeroed();
        Self {
            memory,
            enqueue: 0,
            cycle: true,
        }
    }

    fn phys(&self) -> u64 {
        self.memory.0.as_ptr() as u64
    }

    fn slot_address(&self, index: usize) -> u64 {
        self.phys() + (index * core::mem::size_of::<Trb>()) as u64
    }

    /// The address and cycle state to give the controller for where this will next
    /// write.
    fn dequeue_pointer(&self) -> u64 {
        self.slot_address(self.enqueue) | u64::from(self.cycle)
    }

    /// Writes one TRB and returns its address. The cycle bit goes in last: that's what
    /// hands it over.
    fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        let index = self.enqueue;
        let address = self.slot_address(index);
        let entry = &mut self.memory.0[index];
        // SAFETY: the entry is in the ring this owns, and the device reads it concurrently.
        unsafe {
            write_volatile(&mut entry.param, param);
            write_volatile(&mut entry.status, status);
        }
        dma::wmb();
        let with_cycle = (control & !trb::CYCLE) | u32::from(self.cycle);
        // SAFETY: as above.
        unsafe { write_volatile(&mut entry.control, with_cycle) };
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            // The link: back to the start, and the cycle flips. A chain continues
            // through it.
            let link_control = trb::kind(trb::LINK)
                | trb::TOGGLE
                | (control & trb::CHAIN)
                | u32::from(self.cycle);
            let base = self.phys();
            let link = &mut self.memory.0[RING_TRBS - 1];
            // SAFETY: as above.
            unsafe {
                write_volatile(&mut link.param, base);
                write_volatile(&mut link.status, 0);
            }
            dma::wmb();
            // SAFETY: as above.
            unsafe { write_volatile(&mut link.control, link_control) };
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        address
    }
}

/// Offset of a context entry, in units of the controller's context size.
const fn context_offset(index: usize, stride: usize) -> usize {
    index * stride
}

/// Writes one dword of a context.
fn context_write(context: &mut [u8; PAGE], entry: usize, stride: usize, dword: usize, value: u32) {
    let at = context_offset(entry, stride) + dword * 4;
    context[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Parts of the controller a HID device keeps going.
struct HidEndpoint {
    slot: u8,
    dci: u8,
    ring: Ring,
    buffer: Box<Page>,
    /// How long a report is, at most.
    length: u32,
    kind: HidKind,
}

enum HidKind {
    Keyboard(KeyboardState),
    Pointer(PointerState),
}

/// A transfer ring waited on by whoever started the transfer.
struct WaitedRing {
    slot: u8,
    dci: u8,
    ring: Ring,
    /// What its last transfer's event said: completion code and bytes left over.
    finished: Option<(u8, u32)>,
}

/// One device's memory.
struct DeviceMemory {
    /// What the controller keeps, which it must always have.
    _output: Box<Page>,
    input: Box<Page>,
    slot: u8,
    port: u8,
    speed: u32,
}

struct Inner {
    op: usize,
    runtime: usize,
    doorbells: usize,
    context_stride: usize,
    command: Ring,
    /// What the last command's completion said: its TRB, code and slot.
    command_done: Option<(u64, u8, u8)>,
    events: Box<EventMemory>,
    _segment_table: Box<Page>,
    event_index: usize,
    event_cycle: bool,
    dcbaa: Box<Page>,
    _scratchpad: Vec<Box<Page>>,
    memories: Vec<DeviceMemory>,
    rings: Vec<WaitedRing>,
    hid: Vec<HidEndpoint>,
    pointer_at: (u16, u16),
    stats: UsbStats,
}

/// One controller, for the life of the machine.
pub struct Xhci {
    inner: SpinLock<Inner>,
    /// Moved by the interrupt handler, and what a waiting disk sleeps on.
    events: AtomicU32,
    /// Whether anything will move `events`.
    interrupts: AtomicBool,
    clock: &'static dyn Clock,
    /// The operational registers, for the interrupt handler, which can't lock.
    op: usize,
    /// The runtime registers, likewise.
    runtime: usize,
}

fn read32(address: usize) -> u32 {
    // SAFETY: an address in the controller's BAR, which the board mapped as device memory.
    unsafe { read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // SAFETY: as in `read32`.
    unsafe { write_volatile(address as *mut u32, value) }
}

fn write64(address: usize, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

impl Inner {
    fn event_at(&self, index: usize) -> Trb {
        // SAFETY: the entry is in the ring this owns, and the device writes it concurrently.
        unsafe { read_volatile(&self.events.0[index]) }
    }

    /// Takes everything on the event ring.
    fn process_events(&mut self) {
        let mut moved = false;
        loop {
            let event = self.event_at(self.event_index);
            if (event.control & trb::CYCLE != 0) != self.event_cycle {
                break;
            }
            dma::rmb();
            moved = true;
            self.event_index += 1;
            if self.event_index == EVENT_TRBS {
                self.event_index = 0;
                self.event_cycle = !self.event_cycle;
            }
            let kind = (event.control >> 10) & 0x3f;
            let completion = (event.status >> 24) as u8;
            let slot = (event.control >> 24) as u8;
            match kind {
                trb::EVENT_COMMAND => self.command_done = Some((event.param, completion, slot)),
                trb::EVENT_TRANSFER => {
                    let dci = ((event.control >> 16) & 0x1f) as u8;
                    let residual = event.status & 0xff_ffff;
                    self.on_transfer(slot, dci, completion, residual);
                }
                _ => {}
            }
        }
        if moved {
            let base = self.events.0.as_ptr() as u64;
            let at = base + (self.event_index * core::mem::size_of::<Trb>()) as u64;
            // Bit 3 clears the Event Handler Busy flag: another interrupt may now come.
            write64(self.runtime + 0x20 + 0x18, at | 1 << 3);
        }
    }

    fn on_transfer(&mut self, slot: u8, dci: u8, completion: u8, residual: u32) {
        if let Some(ring) = self.rings.iter_mut().find(|ring| ring.slot == slot && ring.dci == dci) {
            ring.finished = Some((completion, residual));
            return;
        }
        let Some(index) = self.hid.iter().position(|hid| hid.slot == slot && hid.dci == dci) else {
            return;
        };
        let got = if matches!(completion, code::SUCCESS | code::SHORT_PACKET) {
            let hid = &mut self.hid[index];
            let length = hid.length.saturating_sub(residual) as usize;
            // SAFETY: the device has finished with the buffer, until it's given again.
            let report = unsafe { core::slice::from_raw_parts(hid.buffer.0.as_ptr(), length.min(PAGE)) };
            match &mut hid.kind {
                HidKind::Keyboard(state) => {
                    state.on_report(report);
                    self.stats.keyboard_reports += 1;
                }
                HidKind::Pointer(state) => {
                    if let Some(event) = state.on_report(report) {
                        self.pointer_at = (event.x, event.y);
                        POINTER_QUEUE.lock().push(event);
                    }
                    self.stats.pointer_reports += 1;
                }
            }
            true
        } else {
            false
        };
        // A stalled or failed endpoint isn't tried again: it would only say the same.
        if got {
            self.submit_hid(index);
        }
    }

    fn submit_hid(&mut self, index: usize) {
        let hid = &mut self.hid[index];
        let address = hid.buffer.0.as_ptr() as u64;
        hid.ring.push(address, hid.length, trb::kind(trb::NORMAL) | trb::ISP | trb::IOC);
        let (slot, dci) = (hid.slot, hid.dci);
        self.ring_doorbell(slot, dci);
    }

    fn ring_doorbell(&self, slot: u8, target: u8) {
        dma::wmb();
        write32(self.doorbells + 4 * usize::from(slot), u32::from(target));
    }

    fn port_address(&self, number: u8) -> usize {
        self.op + op::PORTS + 0x10 * (usize::from(number) - 1)
    }
}

/// Pointer events waiting to be read. Kept apart so that decoding needn't hold more.
struct PointerQueue {
    events: [PointerEvent; 64],
    head: usize,
    count: usize,
}

impl PointerQueue {
    const fn new() -> Self {
        Self {
            events: [PointerEvent {
                x: 0,
                y: 0,
                left: None,
                right: None,
                middle: None,
                wheel: 0,
            }; 64],
            head: 0,
            count: 0,
        }
    }

    fn push(&mut self, event: PointerEvent) {
        // Nobody's reading: the oldest position is the one to lose.
        if self.count == self.events.len() {
            self.head = (self.head + 1) % self.events.len();
            self.count -= 1;
        }
        let at = (self.head + self.count) % self.events.len();
        self.events[at] = event;
        self.count += 1;
    }

    fn pop(&mut self) -> Option<PointerEvent> {
        if self.count == 0 {
            return None;
        }
        let event = self.events[self.head];
        self.head = (self.head + 1) % self.events.len();
        self.count -= 1;
        Some(event)
    }
}

static POINTER_QUEUE: SpinLock<PointerQueue> = SpinLock::new(PointerQueue::new());

impl Xhci {
    /// Resets the controller at `base`, and brings up what's connected. `width` and
    /// `height` are the screen, which a pointer maps to. Boot messages go in `log`.
    pub fn init(base: usize, clock: &'static dyn Clock, width: u16, height: u16, log: &mut Vec<String>) -> Option<&'static Self> {
        let cap_length = read32(base) as u8 as usize;
        let hcs1 = read32(base + 0x04);
        let hcs2 = read32(base + 0x08);
        let hcc1 = read32(base + 0x10);
        let doorbells = base + (read32(base + 0x14) & !3) as usize;
        let runtime = base + (read32(base + 0x18) & !0x1f) as usize;
        let op = base + cap_length;
        let max_ports = (hcs1 >> 24) as u8;
        let max_slots = (hcs1 as u8 as usize).min(MAX_SLOTS);
        if cap_length == 0 || max_ports == 0 || max_slots == 0 {
            return None;
        }
        let context_stride = if hcc1 & 1 << 2 != 0 { 64 } else { 32 };

        take_from_firmware(base, hcc1, clock);
        if !reset_controller(op, clock) {
            log.push(String::from("[usb] xHCI: the controller won't come out of reset"));
            return None;
        }

        // Everything the controller needs is in place before it runs.
        write32(op + op::CONFIG, max_slots as u32);
        let mut dcbaa: Box<Page> = zeroed();
        let scratch_count = (((hcs2 >> 21) & 0x1f) << 5 | ((hcs2 >> 27) & 0x1f)) as usize;
        let mut scratchpad = Vec::new();
        if scratch_count != 0 {
            let mut array: Box<Page> = zeroed();
            for index in 0..scratch_count.min(PAGE / 8) {
                let page: Box<Page> = zeroed();
                let at = index * 8;
                array.0[at..at + 8].copy_from_slice(&(page.0.as_ptr() as u64).to_le_bytes());
                scratchpad.push(page);
            }
            dcbaa.0[..8].copy_from_slice(&(array.0.as_ptr() as u64).to_le_bytes());
            scratchpad.push(array);
        }
        dma::wmb();
        write64(op + op::DCBAAP, dcbaa.0.as_ptr() as u64);

        let command = Ring::new();
        write64(op + op::CRCR, command.phys() | 1);

        let events: Box<EventMemory> = zeroed();
        let mut segment_table: Box<Page> = zeroed();
        segment_table.0[..8].copy_from_slice(&(events.0.as_ptr() as u64).to_le_bytes());
        segment_table.0[8..12].copy_from_slice(&(EVENT_TRBS as u32).to_le_bytes());
        dma::wmb();
        let interrupter = runtime + 0x20;
        write32(interrupter + 0x08, 1);
        write64(interrupter + 0x18, events.0.as_ptr() as u64);
        write64(interrupter + 0x10, segment_table.0.as_ptr() as u64);
        // A little moderation: 250 us.
        write32(interrupter + 0x04, 1000);
        // Clears a pending flag, and enables.
        write32(interrupter, 0b11);

        write32(op + op::USBCMD, op::CMD_RUN | op::CMD_INTERRUPTS);

        let controller: &'static Self = Box::leak(Box::new(Self {
            inner: SpinLock::new(Inner {
                op,
                runtime,
                doorbells,
                context_stride,
                command,
                command_done: None,
                events,
                _segment_table: segment_table,
                event_index: 0,
                event_cycle: true,
                dcbaa,
                _scratchpad: scratchpad,
                memories: Vec::new(),
                rings: Vec::new(),
                hid: Vec::new(),
                pointer_at: (width / 2, height / 2),
                stats: UsbStats::default(),
            }),
            events: AtomicU32::new(0),
            interrupts: AtomicBool::new(false),
            clock,
            op,
            runtime,
        }));
        let mut disks = Vec::new();
        for number in 1..=max_ports {
            controller.bring_up_port(number, width, height, log, &mut disks);
        }
        DISKS.lock().extend(disks);
        Some(controller)
    }

    /// Quiets the controller's interrupt, and lets waiters know. For the board's handler.
    pub fn handle_interrupt(&self) {
        // Only registers: the handler can't take the lock, which a task may hold.
        write32(self.op + op::USBSTS, op::STS_EINT);
        write32(self.runtime + 0x20, 0b11);
        self.events.fetch_add(1, Ordering::Release);
        futex_wake(&self.events, usize::MAX);
    }

    /// Says that an interrupt handler is registered, so waiters can sleep.
    pub fn use_interrupts(&self) {
        self.interrupts.store(true, Ordering::Release);
    }

    fn sleep_a_moment(&self, seen: u32) {
        if self.interrupts.load(Ordering::Acquire) && feos_sync::irq::is_enabled() {
            futex_wait_ms(&self.events, seen, WAIT_SLICE_MS);
        } else if feos_sync::irq::is_enabled() {
            feos_sync::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }

    /// Takes the next key transition.
    pub fn poll_key(&self) -> Option<KeyboardEvent> {
        let mut inner = self.inner.lock();
        inner.process_events();
        for hid in &mut inner.hid {
            if let HidKind::Keyboard(state) = &mut hid.kind {
                if let Some(event) = state.next() {
                    return Some(event);
                }
            }
        }
        None
    }

    /// Takes the next pointer change.
    pub fn poll_pointer(&self) -> Option<PointerEvent> {
        self.inner.lock().process_events();
        POINTER_QUEUE.lock().pop()
    }

    /// The screen has changed size.
    pub fn set_screen(&self, width: u16, height: u16) {
        let mut inner = self.inner.lock();
        for hid in inner.hid.iter_mut() {
            if let HidKind::Pointer(state) = &mut hid.kind {
                state.set_screen(width, height);
            }
        }
        let (x, y) = inner.pointer_at;
        inner.pointer_at = (x.min(width.max(1) - 1), y.min(height.max(1) - 1));
    }

    /// Where the cursor is.
    pub fn pointer_position(&self) -> (u16, u16) {
        self.inner.lock().pointer_at
    }

    /// Whether a keyboard is connected.
    pub fn has_keyboard(&self) -> bool {
        self.inner.lock().hid.iter().any(|hid| matches!(hid.kind, HidKind::Keyboard(_)))
    }

    /// Whether a pointer is connected.
    pub fn has_pointer(&self) -> bool {
        self.inner.lock().hid.iter().any(|hid| matches!(hid.kind, HidKind::Pointer(_)))
    }

    /// Counters since boot.
    pub fn stats(&self) -> UsbStats {
        self.inner.lock().stats
    }

    /// Takes the disks found at boot.
    pub fn take_disks() -> Vec<UsbDisk> {
        core::mem::take(&mut *DISKS.lock())
    }

    fn command(&self, param: u64, status: u32, control: u32) -> Result<u8, ()> {
        let address = {
            let mut inner = self.inner.lock();
            inner.command_done = None;
            let address = inner.command.push(param, status, control);
            inner.ring_doorbell(0, 0);
            address
        };
        let deadline = self.clock.uptime_ms() + COMMAND_WAIT_MS;
        loop {
            let seen = self.events.load(Ordering::Acquire);
            {
                let mut inner = self.inner.lock();
                inner.process_events();
                if let Some((done, completion, slot)) = inner.command_done {
                    if done == address {
                        return if completion == code::SUCCESS { Ok(slot) } else { Err(()) };
                    }
                }
            }
            if self.clock.uptime_ms() >= deadline {
                return Err(());
            }
            self.sleep_a_moment(seen);
        }
    }

    /// Waits for the transfer on this ring to finish.
    fn wait_transfer(&self, index: usize, limit_ms: u64) -> Result<(u8, u32), ()> {
        let deadline = self.clock.uptime_ms() + limit_ms;
        loop {
            let seen = self.events.load(Ordering::Acquire);
            {
                let mut inner = self.inner.lock();
                inner.process_events();
                if let Some(finished) = inner.rings[index].finished.take() {
                    return Ok(finished);
                }
            }
            if self.clock.uptime_ms() >= deadline {
                return Err(());
            }
            self.sleep_a_moment(seen);
        }
    }

    /// A control transfer on the default endpoint: `setup` and then any data.
    fn control(&self, ring: usize, setup: [u8; 8], data: Option<(u64, u16, bool)>) -> Result<(), ()> {
        {
            let mut inner = self.inner.lock();
            let inner = &mut *inner;
            let waited = &mut inner.rings[ring];
            waited.finished = None;
            let transfer_type = match data {
                None => 0,
                Some((_, _, true)) => 3,
                Some((_, _, false)) => 2,
            };
            waited.ring.push(
                u64::from_le_bytes(setup),
                8,
                trb::kind(trb::SETUP) | trb::IDT | transfer_type << 16,
            );
            let mut status_in = true;
            if let Some((address, length, is_in)) = data {
                let direction = if is_in { trb::DIR_IN } else { 0 };
                waited.ring.push(address, u32::from(length), trb::kind(trb::DATA) | direction);
                status_in = !is_in;
            }
            waited.ring.push(
                0,
                0,
                trb::kind(trb::STATUS) | trb::IOC | if status_in { trb::DIR_IN } else { 0 },
            );
            let (slot, dci) = (waited.slot, waited.dci);
            inner.ring_doorbell(slot, dci);
        }
        match self.wait_transfer(ring, COMMAND_WAIT_MS)? {
            (code::SUCCESS | code::SHORT_PACKET, _) => Ok(()),
            _ => Err(()),
        }
    }

    fn bring_up_port(&self, number: u8, width: u16, height: u16, log: &mut Vec<String>, disks: &mut Vec<UsbDisk>) {
        let (address, stride) = {
            let inner = self.inner.lock();
            (inner.port_address(number), inner.context_stride)
        };
        let status = read32(address);
        if status & port::POWER == 0 {
            write32(address, port::POWER);
            let until = self.clock.uptime_ms() + 20;
            while self.clock.uptime_ms() < until {
                core::hint::spin_loop();
            }
        }
        let status = read32(address);
        if status & port::CONNECTED == 0 {
            return;
        }
        // A USB 3 port trains by itself and comes up enabled. A USB 2 one needs a reset.
        if status & port::ENABLED == 0 {
            write32(address, port::POWER | port::RESET);
            let deadline = self.clock.uptime_ms() + RESET_WAIT_MS;
            while read32(address) & port::RESET_CHANGE == 0 {
                if self.clock.uptime_ms() >= deadline {
                    log.push(alloc::format!("[usb] port {number}: reset timed out"));
                    return;
                }
                core::hint::spin_loop();
            }
        }
        let status = read32(address);
        // Clears what's changed, which is write-one.
        write32(address, port::POWER | (status & port::CHANGES));
        if status & port::ENABLED == 0 {
            log.push(alloc::format!("[usb] port {number}: wouldn't enable"));
            return;
        }
        let device_speed = (status >> port::SPEED_SHIFT) & port::SPEED_MASK;

        let Ok(slot) = self.command(0, 0, trb::kind(trb::ENABLE_SLOT)) else {
            log.push(alloc::format!("[usb] port {number}: no slot"));
            return;
        };
        if slot == 0 || usize::from(slot) > MAX_SLOTS {
            return;
        }

        // The default endpoint's largest packet, until the device says.
        let mut packet = match device_speed {
            speed::LOW | speed::FULL => 8u16,
            speed::HIGH => 64,
            _ => 512,
        };
        let output: Box<Page> = zeroed();
        let mut input: Box<Page> = zeroed();
        let ep0 = Ring::new();
        // Add flags: the slot and the default endpoint.
        context_write(&mut input.0, 0, stride, 1, 0b11);
        write_slot_context(&mut input.0, stride, device_speed, number, 1);
        write_ep0_context(&mut input.0, stride, packet, ep0.dequeue_pointer());
        dma::wmb();

        let (ring_index, input_address) = {
            let mut inner = self.inner.lock();
            let at = usize::from(slot) * 8;
            inner.dcbaa.0[at..at + 8].copy_from_slice(&(output.0.as_ptr() as u64).to_le_bytes());
            inner.rings.push(WaitedRing {
                slot,
                dci: 1,
                ring: ep0,
                finished: None,
            });
            let index = inner.rings.len() - 1;
            let address = input.0.as_ptr() as u64;
            inner.memories.push(DeviceMemory {
                _output: output,
                input,
                slot,
                port: number,
                speed: device_speed,
            });
            (index, address)
        };
        dma::wmb();
        if self
            .command(input_address, 0, trb::kind(trb::ADDRESS_DEVICE) | u32::from(slot) << 24)
            .is_err()
        {
            log.push(alloc::format!("[usb] port {number}: wouldn't take an address"));
            return;
        }

        let transfer: Box<Page> = zeroed();
        let buffer = transfer.0.as_ptr() as u64;
        // SAFETY: the page outlives the transfers below, which are the only things
        // writing it, each of them finished before it's read.
        let read = |bytes: usize| unsafe { core::slice::from_raw_parts(transfer.0.as_ptr(), bytes) };
        let get = |value: u16, index: u16, length: u16| -> Result<(), ()> {
            let [lo, hi] = value.to_le_bytes();
            let [ilo, ihi] = index.to_le_bytes();
            let [len_lo, len_hi] = length.to_le_bytes();
            self.control(ring_index, [0x80, 6, lo, hi, ilo, ihi, len_lo, len_hi], Some((buffer, length, true)))
        };

        if get(u16::from(descriptors::kind::DEVICE) << 8, 0, 18).is_err() {
            log.push(alloc::format!("[usb] port {number}: no device descriptor"));
            return;
        }
        let Some(device) = parse_device(read(18)) else {
            log.push(alloc::format!("[usb] port {number}: bad device descriptor"));
            return;
        };
        if device.max_packet0 != packet {
            packet = device.max_packet0;
            let input_address = {
                let mut inner = self.inner.lock();
                let dequeue = inner.rings[ring_index].ring.dequeue_pointer();
                let Some(memory) = inner.memories.iter_mut().find(|memory| memory.slot == slot) else {
                    return;
                };
                memory.input.0.fill(0);
                context_write(&mut memory.input.0, 0, stride, 1, 0b10);
                write_ep0_context(&mut memory.input.0, stride, packet, dequeue);
                memory.input.0.as_ptr() as u64
            };
            dma::wmb();
            if self
                .command(input_address, 0, trb::kind(trb::EVALUATE_CONTEXT) | u32::from(slot) << 24)
                .is_err()
            {
                log.push(alloc::format!("[usb] port {number}: wouldn't take its packet size"));
                return;
            }
        }

        let config_value = u16::from(descriptors::kind::CONFIGURATION) << 8;
        if get(config_value, 0, 9).is_err() {
            log.push(alloc::format!("[usb] port {number}: no configuration"));
            return;
        }
        let Some(total) = descriptors::configuration_length(read(9)) else {
            log.push(alloc::format!("[usb] port {number}: bad configuration"));
            return;
        };
        let total = total.min(PAGE) as u16;
        if get(config_value, 0, total).is_err() {
            log.push(alloc::format!("[usb] port {number}: no configuration"));
            return;
        }
        let Some(configuration) = parse_configuration(read(usize::from(total))) else {
            log.push(alloc::format!("[usb] port {number}: bad configuration"));
            return;
        };
        let name = device_name(&device, number);

        let storage = configuration.interfaces.iter().find(|interface| {
            interface.alternate == 0
                && interface.class == msc::CLASS
                && interface.subclass == msc::SUBCLASS_SCSI
                && interface.protocol == msc::PROTOCOL_BOT
        });
        let hid = configuration
            .interfaces
            .iter()
            .find(|interface| interface.alternate == 0 && interface.class == 3);

        if let Some(interface) = storage {
            match self.bring_up_storage(slot, ring_index, device_speed, number, &configuration, interface) {
                Some(disk) => {
                    log.push(alloc::format!(
                        "[usb] {name}: mass storage, {} MiB",
                        disk.block_count() * disk.block_size() as u64 / (1024 * 1024)
                    ));
                    self.inner.lock().stats.devices += 1;
                    disks.push(disk);
                }
                None => log.push(alloc::format!("[usb] {name}: mass storage that won't answer")),
            }
        } else if let Some(interface) = hid {
            let report = if interface.hid_report_length != 0 {
                let length = interface.hid_report_length.min(PAGE as u16);
                let wanted = u16::from(descriptors::kind::REPORT) << 8;
                let [lo, hi] = wanted.to_le_bytes();
                let [ilo, ihi] = u16::from(interface.number).to_le_bytes();
                let [len_lo, len_hi] = length.to_le_bytes();
                self.control(ring_index, [0x81, 6, lo, hi, ilo, ihi, len_lo, len_hi], Some((buffer, length, true)))
                    .ok()
                    .and_then(|()| parse_report(read(usize::from(length))))
            } else {
                None
            };
            match self.bring_up_hid(slot, ring_index, device_speed, number, &configuration, interface, report, width, height) {
                Some(what) => {
                    log.push(alloc::format!("[usb] {name}: {what}"));
                    self.inner.lock().stats.devices += 1;
                }
                None => log.push(alloc::format!("[usb] {name}: HID device with no use here")),
            }
        } else {
            log.push(alloc::format!("[usb] {name}: not a device this driver knows (hubs aren't supported)"));
        }
        drop(transfer);
    }

    /// Adds endpoints to the slot's context and selects the configuration.
    fn configure(
        &self,
        slot: u8,
        ring_index: usize,
        device_speed: u32,
        port_number: u8,
        configuration: &Configuration,
        endpoints: &[(&Endpoint, u64)],
    ) -> Result<(), ()> {
        let input_address = {
            let mut inner = self.inner.lock();
            let stride = inner.context_stride;
            let memory = inner.memories.iter_mut().find(|memory| memory.slot == slot).ok_or(())?;
            debug_assert_eq!(memory.port, port_number);
            debug_assert_eq!(memory.speed, device_speed);
            memory.input.0.fill(0);
            let mut add = 1u32;
            let mut last = 1;
            for (endpoint, dequeue) in endpoints {
                let dci = endpoint_dci(endpoint);
                add |= 1 << dci;
                last = last.max(usize::from(dci));
                write_endpoint_context(&mut memory.input.0, stride, endpoint, *dequeue, device_speed);
            }
            context_write(&mut memory.input.0, 0, stride, 1, add);
            write_slot_context(&mut memory.input.0, stride, device_speed, port_number, last as u32);
            memory.input.0.as_ptr() as u64
        };
        dma::wmb();
        self.command(input_address, 0, trb::kind(trb::CONFIGURE_ENDPOINT) | u32::from(slot) << 24)
            .map(|_| ())?;
        self.control(ring_index, [0, 9, configuration.value, 0, 0, 0, 0, 0], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn bring_up_hid(
        &self,
        slot: u8,
        ring_index: usize,
        device_speed: u32,
        port_number: u8,
        configuration: &Configuration,
        interface: &Interface,
        report: Option<ReportLayout>,
        width: u16,
        height: u16,
    ) -> Option<&'static str> {
        let endpoint = *interface
            .endpoints
            .iter()
            .find(|endpoint| endpoint.transfer == Endpoint::INTERRUPT && endpoint.is_in())?;
        let boot_keyboard = interface.subclass == 1 && interface.protocol == 1;
        let boot_mouse = interface.subclass == 1 && interface.protocol == 2;
        // Report protocol's layout where one was read; boot protocol's where that's
        // what the interface offers.
        let (kind, what, protocol): (HidKind, &'static str, Option<u8>) = if boot_keyboard {
            (HidKind::Keyboard(KeyboardState::default()), "keyboard", Some(0))
        } else if let Some(layout) = report {
            let absolute = layout.x.is_some_and(|field| field.absolute);
            let protocol = if boot_mouse { Some(1) } else { None };
            (
                HidKind::Pointer(PointerState::new(layout, width, height)),
                if absolute { "tablet" } else { "mouse" },
                protocol,
            )
        } else if boot_mouse {
            (
                HidKind::Pointer(PointerState::new(ReportLayout::BOOT_MOUSE, width, height)),
                "mouse (boot protocol)",
                Some(0),
            )
        } else {
            return None;
        };

        let ring = Ring::new();
        let dequeue = ring.dequeue_pointer();
        self.configure(slot, ring_index, device_speed, port_number, configuration, &[(&endpoint, dequeue)])
            .ok()?;
        let interface_number = u16::from(interface.number).to_le_bytes();
        if let Some(protocol) = protocol {
            // SET_PROTOCOL, which a device may refuse if it only has the one.
            let _ = self.control(ring_index, [0x21, 0x0b, protocol, 0, interface_number[0], interface_number[1], 0, 0], None);
        }
        // SET_IDLE: only report changes. Optional for a pointer, so failing doesn't matter.
        let _ = self.control(ring_index, [0x21, 0x0a, 0, 0, interface_number[0], interface_number[1], 0, 0], None);

        let mut inner = self.inner.lock();
        inner.hid.push(HidEndpoint {
            slot,
            dci: endpoint_dci(&endpoint),
            ring,
            buffer: zeroed(),
            length: u32::from(endpoint.max_packet.clamp(8, 64)),
            kind,
        });
        let index = inner.hid.len() - 1;
        inner.submit_hid(index);
        Some(what)
    }

    #[allow(clippy::too_many_arguments)]
    fn bring_up_storage(
        &self,
        slot: u8,
        ring_index: usize,
        device_speed: u32,
        port_number: u8,
        configuration: &Configuration,
        interface: &Interface,
    ) -> Option<UsbDisk> {
        let bulk_in = *interface
            .endpoints
            .iter()
            .find(|endpoint| endpoint.transfer == Endpoint::BULK && endpoint.is_in())?;
        let bulk_out = *interface
            .endpoints
            .iter()
            .find(|endpoint| endpoint.transfer == Endpoint::BULK && !endpoint.is_in())?;
        let ring_in = Ring::new();
        let ring_out = Ring::new();
        let (dequeue_in, dequeue_out) = (ring_in.dequeue_pointer(), ring_out.dequeue_pointer());
        self.configure(
            slot,
            ring_index,
            device_speed,
            port_number,
            configuration,
            &[(&bulk_in, dequeue_in), (&bulk_out, dequeue_out)],
        )
        .ok()?;

        let (in_index, out_index) = {
            let mut inner = self.inner.lock();
            inner.rings.push(WaitedRing {
                slot,
                dci: endpoint_dci(&bulk_in),
                ring: ring_in,
                finished: None,
            });
            inner.rings.push(WaitedRing {
                slot,
                dci: endpoint_dci(&bulk_out),
                ring: ring_out,
                finished: None,
            });
            (inner.rings.len() - 2, inner.rings.len() - 1)
        };
        // SAFETY: the controller is leaked, in `init`. This borrows it for ever.
        let controller: &'static Self = unsafe { &*(self as *const Self) };
        let mut disk = UsbDisk {
            controller,
            slot,
            ep0_ring: ring_index,
            in_ring: in_index,
            out_ring: out_index,
            in_address: bulk_in.address,
            out_address: bulk_out.address,
            interface: interface.number,
            in_dci: endpoint_dci(&bulk_in),
            out_dci: endpoint_dci(&bulk_out),
            in_packet: u32::from(bulk_in.max_packet.max(1)),
            out_packet: u32::from(bulk_out.max_packet.max(1)),
            page: zeroed(),
            tag: 1,
            block_size: 512,
            blocks: 0,
            failed: false,
        };
        disk.start().then_some(disk)
    }
}

static DISKS: SpinLock<Vec<UsbDisk>> = SpinLock::new(Vec::new());

fn device_name(device: &descriptors::DeviceDescriptor, port_number: u8) -> String {
    let mut text = String::new();
    let _ = write!(text, "port {port_number} {:04x}:{:04x}", device.vendor, device.product);
    text
}

/// The Device Context Index of an endpoint.
const fn endpoint_dci(endpoint: &Endpoint) -> u8 {
    endpoint.number() * 2 + endpoint.is_in() as u8
}

fn write_slot_context(input: &mut [u8; PAGE], stride: usize, device_speed: u32, port_number: u8, entries: u32) {
    context_write(input, 1, stride, 0, device_speed << 20 | entries << 27);
    context_write(input, 1, stride, 1, u32::from(port_number) << 16);
}

fn write_ep0_context(input: &mut [u8; PAGE], stride: usize, packet: u16, dequeue: u64) {
    let entry = 2;
    // Three retries on an error, control type.
    context_write(input, entry, stride, 1, 3 << 1 | 4 << 3 | u32::from(packet) << 16);
    context_write(input, entry, stride, 2, dequeue as u32);
    context_write(input, entry, stride, 3, (dequeue >> 32) as u32);
    // Average TRB length: a setup stage's eight bytes.
    context_write(input, entry, stride, 4, 8);
}

/// An endpoint's interval as a context field: the base-2 log of 125 us units.
fn interval_field(endpoint: &Endpoint, device_speed: u32) -> u32 {
    if endpoint.transfer != Endpoint::INTERRUPT {
        return 0;
    }
    let interval = u32::from(endpoint.interval.max(1));
    match device_speed {
        // In frames of a millisecond: eight units each.
        speed::LOW | speed::FULL => (interval * 8).ilog2().clamp(3, 10),
        // Already a power, of the same units.
        _ => interval.saturating_sub(1).min(15),
    }
}

fn write_endpoint_context(input: &mut [u8; PAGE], stride: usize, endpoint: &Endpoint, dequeue: u64, device_speed: u32) {
    let entry = 1 + usize::from(endpoint_dci(endpoint));
    let kind = match (endpoint.transfer, endpoint.is_in()) {
        (Endpoint::BULK, false) => 2,
        (Endpoint::BULK, true) => 6,
        (Endpoint::INTERRUPT, false) => 3,
        _ => 7,
    };
    context_write(input, entry, stride, 0, interval_field(endpoint, device_speed) << 16);
    context_write(
        input,
        entry,
        stride,
        1,
        3 << 1 | kind << 3 | u32::from(endpoint.max_burst) << 8 | u32::from(endpoint.max_packet) << 16,
    );
    context_write(input, entry, stride, 2, dequeue as u32);
    context_write(input, entry, stride, 3, (dequeue >> 32) as u32);
    let average = if endpoint.transfer == Endpoint::BULK { 3072 } else { u32::from(endpoint.max_packet) };
    let esit = if endpoint.transfer == Endpoint::INTERRUPT { u32::from(endpoint.max_packet) } else { 0 };
    context_write(input, entry, stride, 4, average | esit << 16);
}

fn reset_controller(op: usize, clock: &dyn Clock) -> bool {
    let deadline = clock.uptime_ms() + RESET_WAIT_MS;
    write32(op + op::USBCMD, read32(op + op::USBCMD) & !op::CMD_RUN);
    while read32(op + op::USBSTS) & op::STS_HALTED == 0 {
        if clock.uptime_ms() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    write32(op + op::USBCMD, op::CMD_RESET);
    while read32(op + op::USBCMD) & op::CMD_RESET != 0 || read32(op + op::USBSTS) & op::STS_NOT_READY != 0 {
        if clock.uptime_ms() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Takes the controller from firmware that's holding on to it (xHCI 7.1). QEMU's
/// doesn't, but a firmware's USB keyboard support on real hardware would.
fn take_from_firmware(base: usize, hcc1: u32, clock: &dyn Clock) {
    const CAP_LEGACY: u32 = 1;
    const BIOS_OWNED: u32 = 1 << 16;
    const OS_OWNED: u32 = 1 << 24;
    let mut offset = ((hcc1 >> 16) * 4) as usize;
    let mut steps = 0;
    while offset != 0 && steps < 64 {
        steps += 1;
        let at = base + offset;
        let capability = read32(at);
        if capability & 0xff == CAP_LEGACY {
            if capability & BIOS_OWNED != 0 {
                write32(at, capability | OS_OWNED);
                let deadline = clock.uptime_ms() + 1000;
                while read32(at) & BIOS_OWNED != 0 && clock.uptime_ms() < deadline {
                    core::hint::spin_loop();
                }
            }
            return;
        }
        let next = ((capability >> 8) & 0xff) as usize * 4;
        if next == 0 {
            return;
        }
        offset += next;
    }
}

/// A mass storage device, as a block device.
pub struct UsbDisk {
    controller: &'static Xhci,
    slot: u8,
    ep0_ring: usize,
    in_ring: usize,
    out_ring: usize,
    in_address: u8,
    out_address: u8,
    interface: u8,
    in_dci: u8,
    out_dci: u8,
    in_packet: u32,
    out_packet: u32,
    /// Commands and answers small enough to live here.
    page: Box<Page>,
    tag: u32,
    block_size: usize,
    blocks: u64,
    /// A transfer was lost: the device may yet write to its buffer, so nothing more
    /// can safely be sent.
    failed: bool,
}

// SAFETY: the page is reached only through this value, which the caller owns.
unsafe impl Send for UsbDisk {}

impl UsbDisk {
    /// Inquires, waits until it's ready, and reads its size.
    fn start(&mut self) -> bool {
        let page = self.page.0.as_ptr() as u64;
        // Everything here answers into the second half of the page.
        let reply = page + 1024;
        if self.scsi(&msc::inquiry(36), msc::Direction::In, Some((reply, 36))).is_err() {
            return false;
        }
        // Peripheral type: direct access.
        if self.page.0[1024] & 0x1f != 0 {
            return false;
        }
        // A fresh device always has something to say first (a "unit attention").
        let mut ready = false;
        for _ in 0..10 {
            if self.scsi(&msc::test_unit_ready(), msc::Direction::In, None) == Ok(0) {
                ready = true;
                break;
            }
            let _ = self.scsi(&msc::request_sense(18), msc::Direction::In, Some((reply, 18)));
        }
        if !ready {
            return false;
        }
        if self.scsi(&msc::read_capacity(), msc::Direction::In, Some((reply, 8))) != Ok(0) {
            return false;
        }
        let Some((last, size)) = msc::parse_capacity(&self.page.0[1024..1032]) else {
            return false;
        };
        // All ones means it's too big to say in this command.
        if last == u32::MAX {
            return false;
        }
        self.block_size = size as usize;
        self.blocks = u64::from(last) + 1;
        true
    }

    /// Sends `count` bytes on a bulk ring, in TRBs that don't cross a 64 KiB boundary.
    fn bulk(&mut self, is_in: bool, address: u64, count: usize) -> Result<(u8, u32), ()> {
        let ring_index = if is_in { self.in_ring } else { self.out_ring };
        let dci = if is_in { self.in_dci } else { self.out_dci };
        let packet = if is_in { self.in_packet } else { self.out_packet };
        {
            let mut inner = self.controller.inner.lock();
            let inner = &mut *inner;
            let waited = &mut inner.rings[ring_index];
            waited.finished = None;
            let mut at = address;
            let mut left = count;
            loop {
                let room = TRB_MAX - (at as usize % TRB_MAX);
                let piece = room.min(left);
                left -= piece;
                let td_size = (left as u32).div_ceil(packet).min(31);
                let flags = if left == 0 {
                    trb::IOC | if is_in { trb::ISP } else { 0 }
                } else {
                    trb::CHAIN
                };
                waited.ring.push(at, piece as u32 | td_size << 17, trb::kind(trb::NORMAL) | flags);
                at += piece as u64;
                if left == 0 {
                    break;
                }
            }
            inner.ring_doorbell(self.slot, dci);
        }
        self.controller.wait_transfer(ring_index, BULK_WAIT_MS)
    }

    /// Clears a halted endpoint on both ends: the controller, then the device.
    fn clear_halt(&mut self, is_in: bool) -> Result<(), ()> {
        let dci = if is_in { self.in_dci } else { self.out_dci };
        let ring_index = if is_in { self.in_ring } else { self.out_ring };
        let address = if is_in { self.in_address } else { self.out_address };
        let slot = u32::from(self.slot);
        self.controller
            .command(0, 0, trb::kind(trb::RESET_ENDPOINT) | slot << 24 | u32::from(dci) << 16)?;
        let dequeue = self.controller.inner.lock().rings[ring_index].ring.dequeue_pointer();
        self.controller
            .command(dequeue, 0, trb::kind(trb::SET_TR_DEQUEUE) | slot << 24 | u32::from(dci) << 16)?;
        // CLEAR_FEATURE (ENDPOINT_HALT).
        self.controller.control(self.ep0_ring, [0x02, 1, 0, 0, address, 0, 0, 0], None)
    }

    /// Runs one command. The answer is SCSI's status: zero for fine. An error here
    /// means the transport itself failed.
    fn scsi(&mut self, command: &[u8], direction: msc::Direction, data: Option<(u64, usize)>) -> Result<u8, ()> {
        if self.failed {
            return Err(());
        }
        let result = self.scsi_inner(command, direction, data);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn scsi_inner(&mut self, command: &[u8], direction: msc::Direction, data: Option<(u64, usize)>) -> Result<u8, ()> {
        self.controller.inner.lock().stats.storage_commands += 1;
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let length = data.map_or(0, |(_, length)| length);
        let wrapper = msc::build_cbw(tag, length as u32, direction, 0, command);
        self.page.0[..msc::CBW_LEN].copy_from_slice(&wrapper);
        dma::wmb();
        let page = self.page.0.as_ptr() as u64;

        match self.bulk(false, page, msc::CBW_LEN)? {
            (code::SUCCESS, _) => {}
            (code::STALL, _) => {
                self.reset_recovery()?;
                return Err(());
            }
            _ => return Err(()),
        }
        if let Some((address, count)) = data {
            let is_in = direction == msc::Direction::In;
            match self.bulk(is_in, address, count)? {
                (code::SUCCESS | code::SHORT_PACKET, _) => {}
                // The device had enough, and the status says why.
                (code::STALL, _) => self.clear_halt(is_in)?,
                _ => return Err(()),
            }
        }
        let status_at = page + 512;
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.bulk(true, status_at, msc::CSW_LEN)? {
                (code::SUCCESS | code::SHORT_PACKET, _) => break,
                (code::STALL, _) if attempts == 1 => self.clear_halt(true)?,
                _ => {
                    self.reset_recovery()?;
                    return Err(());
                }
            }
        }
        dma::rmb();
        let Some(status) = msc::parse_csw(&self.page.0[512..512 + msc::CSW_LEN], tag) else {
            self.reset_recovery()?;
            return Err(());
        };
        if status.code == 2 {
            // A phase error: the device has lost track, and only a reset will do.
            self.reset_recovery()?;
            return Err(());
        }
        Ok(status.code)
    }

    /// The Bulk-Only Mass Storage Reset, and clearing both endpoints after it.
    fn reset_recovery(&mut self) -> Result<(), ()> {
        self.controller
            .control(self.ep0_ring, [0x21, 0xff, 0, 0, self.interface, 0, 0, 0], None)?;
        self.clear_halt(true)?;
        self.clear_halt(false)
    }

    fn check_range(&self, start: u64, len: usize) -> Result<(), BlockIoError> {
        if len % self.block_size != 0 {
            return Err(BlockIoError::OutOfRange);
        }
        match start.checked_add((len / self.block_size) as u64) {
            Some(end) if end <= self.blocks => Ok(()),
            _ => Err(BlockIoError::OutOfRange),
        }
    }

    /// READ or WRITE, split into what one command can carry.
    fn transfer(&mut self, write: bool, start: u64, address: usize, len: usize) -> Result<(), BlockIoError> {
        self.check_range(start, len)?;
        let mut done = 0;
        while done < len {
            let take = (len - done).min(MAX_TRANSFER);
            let lba = start + (done / self.block_size) as u64;
            let blocks = (take / self.block_size) as u16;
            let lba = u32::try_from(lba).map_err(|_| BlockIoError::OutOfRange)?;
            let command = if write { msc::write10(lba, blocks) } else { msc::read10(lba, blocks) };
            let direction = if write { msc::Direction::Out } else { msc::Direction::In };
            match self.scsi(&command, direction, Some(((address + done) as u64, take))) {
                Ok(0) => {}
                _ => return Err(BlockIoError::Io),
            }
            done += take;
        }
        Ok(())
    }
}

impl BlockIo for UsbDisk {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), BlockIoError> {
        self.transfer(false, start, out.as_mut_ptr() as usize, out.len())
    }

    fn write(&mut self, start: u64, data: &[u8]) -> Result<(), BlockIoError> {
        self.transfer(true, start, data.as_ptr() as usize, data.len())
    }

    fn flush(&mut self) -> Result<(), BlockIoError> {
        match self.scsi(&msc::synchronize_cache(), msc::Direction::In, None) {
            Ok(_) => Ok(()),
            Err(()) => Err(BlockIoError::Io),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_wraps_through_its_link_and_flips_the_cycle() {
        let mut ring = Ring::new();
        assert_eq!(ring.dequeue_pointer() & 1, 1);
        let first = ring.push(1, 2, trb::kind(trb::NORMAL));
        assert_eq!(first, ring.phys());
        assert_eq!(ring.memory.0[0].control & trb::CYCLE, 1);
        for _ in 1..RING_TRBS - 1 {
            ring.push(0, 0, trb::kind(trb::NORMAL) | trb::CHAIN);
        }
        // The last entry is the link, and points back.
        let link = ring.memory.0[RING_TRBS - 1];
        assert_eq!(link.param, ring.phys());
        assert_eq!((link.control >> 10) & 0x3f, trb::LINK);
        assert_ne!(link.control & trb::TOGGLE, 0);
        assert_ne!(link.control & trb::CHAIN, 0, "a chain goes through it");
        assert_eq!(link.control & trb::CYCLE, 1);
        assert_eq!(ring.enqueue, 0);
        assert_eq!(ring.dequeue_pointer() & 1, 0);
        ring.push(0, 0, trb::kind(trb::NORMAL));
        assert_eq!(ring.memory.0[0].control & trb::CYCLE, 0);
    }

    #[test]
    fn endpoints_have_the_numbers_xhci_gives_them() {
        let endpoint = |address| Endpoint { address, ..Endpoint::default() };
        assert_eq!(endpoint_dci(&endpoint(0x81)), 3);
        assert_eq!(endpoint_dci(&endpoint(0x01)), 2);
        assert_eq!(endpoint_dci(&endpoint(0x8f)), 31);
    }

    #[test]
    fn intervals_become_powers_of_125_us() {
        let endpoint = |interval| Endpoint { transfer: Endpoint::INTERRUPT, interval, ..Endpoint::default() };
        // Full speed, 8 ms: 64 units.
        assert_eq!(interval_field(&endpoint(8), speed::FULL), 6);
        assert_eq!(interval_field(&endpoint(1), speed::FULL), 3);
        // High speed: the field is already that power, plus one.
        assert_eq!(interval_field(&endpoint(7), speed::HIGH), 6);
        assert_eq!(interval_field(&endpoint(0), speed::HIGH), 0);
        let bulk = Endpoint { transfer: Endpoint::BULK, interval: 9, ..Endpoint::default() };
        assert_eq!(interval_field(&bulk, speed::HIGH), 0);
    }

    #[test]
    fn contexts_land_at_their_stride() {
        let mut input = [0u8; PAGE];
        write_slot_context(&mut input, 32, speed::HIGH, 4, 3);
        let dword = u32::from_le_bytes([input[32], input[33], input[34], input[35]]);
        assert_eq!(dword, speed::HIGH << 20 | 3 << 27);
        let dword = u32::from_le_bytes([input[36], input[37], input[38], input[39]]);
        assert_eq!(dword, 4 << 16);
        let mut input = [0u8; PAGE];
        write_ep0_context(&mut input, 64, 64, 0x1000_0001);
        let dword = u32::from_le_bytes([input[128 + 4], input[128 + 5], input[128 + 6], input[128 + 7]]);
        assert_eq!(dword, 3 << 1 | 4 << 3 | 64 << 16);
    }
}
