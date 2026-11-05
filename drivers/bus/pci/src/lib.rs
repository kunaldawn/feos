//! Minimal PCI/PCIe enumeration over ECAM or legacy I/O ports.
//!
//! Firmware-less boot leaves PCI BARs unprogrammed, so a bare-metal kernel has
//! to assign them itself. This crate provides just enough of the config-space
//! protocol to find a device by vendor/device ID, size and assign a memory
//! BAR, and enable memory decoding plus bus mastering. It also walks capability
//! lists, hands out BAR addresses ([`BarAllocator`]) and numbers the PCI-PCI
//! bridges firmware left alone ([`configure_bridges`]), which is as much of
//! bridging as QEMU's root ports need. It does not implement interrupts.
//!
//! Two transports are supported:
//!
//! * [`PciEcam`] — memory-mapped config space, used by QEMU's AArch64 `virt`
//!   machine (the high ECAM window starts at `0x40_1000_0000`).
//! * [`PciPort`] — the legacy `0xcf8`/`0xcfc` I/O port pair, used on x86 and
//!   available on both i440fx and q35 without firmware tables.
//!
//! The board chooses the transport and passes the base address (or nothing,
//! for the legacy port pair).

#![cfg_attr(not(test), no_std)]

use core::ptr::{read_volatile, write_volatile};

#[cfg(target_arch = "x86_64")]
use feos_port_io::{inl, outl};
#[cfg(target_arch = "x86_64")]
use feos_sync::IrqSpinLock;

/// Number of buses scanned by `find`.
const SCAN_BUSES: u8 = 8;

/// PCI configuration space registers used by this crate.
mod reg {
    pub const VENDOR_ID: usize = 0x00;
    pub const COMMAND: usize = 0x04;
    pub const STATUS: usize = 0x06;
    pub const HEADER_TYPE: usize = 0x0e;
    pub const BAR0: usize = 0x10;
    pub const CAPABILITIES: usize = 0x34;
    pub const INTERRUPT_LINE: usize = 0x3c;
    pub const INTERRUPT_PIN: usize = 0x3d;

    /// Type 1 (bridge) header.
    pub const BUS_NUMBERS: usize = 0x18;
    pub const MEMORY_WINDOW: usize = 0x20;
    pub const PREFETCH_WINDOW: usize = 0x24;
    pub const PREFETCH_BASE_UPPER: usize = 0x28;
    pub const PREFETCH_LIMIT_UPPER: usize = 0x2c;
}

/// Status register bit: the function has a capability list.
const STATUS_CAPABILITIES: u16 = 1 << 4;
/// Bound on a capability walk: a config space holds at most 48 of them, and a
/// list that loops must not hang the boot.
const MAX_CAPABILITIES: usize = 48;
/// Header type value of a PCI-PCI bridge, without the multi-function bit.
const HEADER_BRIDGE: u8 = 1;
/// Memory window handed to each bridge that firmware didn't program.
const BRIDGE_WINDOW: u64 = 16 * 1024 * 1024;
/// Bridge memory windows have 1 MiB granularity.
const BRIDGE_ALIGN: u64 = 1024 * 1024;

/// BAR type field: bits 2:1 of a memory BAR.
const BAR_TYPE_MASK: u32 = 0b110;
/// BAR type field value for a 64-bit BAR.
const BAR_TYPE_64: u32 = 0b100;

/// PCI command-register bits.
mod command {
    pub const MEMORY_SPACE: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTX_DISABLE: u16 = 1 << 10;
}

/// Access to one PCI configuration space.
pub trait ConfigSpace: Copy {
    /// Reads a 32-bit configuration-space register.
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32;

    /// Writes a 32-bit configuration-space register.
    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32);

    /// Reads a 16-bit configuration-space register.
    fn read16(&self, bus: u8, device: u8, function: u8, offset: usize) -> u16 {
        let aligned = offset & !3;
        let value = self.read32(bus, device, function, aligned);
        (value >> ((offset & 2) * 8)) as u16
    }

    /// Writes a 16-bit configuration-space register.
    fn write16(&self, bus: u8, device: u8, function: u8, offset: usize, value: u16) {
        let aligned = offset & !3;
        let shift = (offset & 2) * 8;
        let mask = !(0xffffu32 << shift);
        let current = self.read32(bus, device, function, aligned);
        self.write32(
            bus,
            device,
            function,
            aligned,
            (current & mask) | ((value as u32) << shift),
        );
    }

    /// Reads an 8-bit configuration-space register.
    fn read8(&self, bus: u8, device: u8, function: u8, offset: usize) -> u8 {
        let aligned = offset & !3;
        let value = self.read32(bus, device, function, aligned);
        (value >> ((offset & 3) * 8)) as u8
    }

    /// Writes an 8-bit configuration-space register.
    fn write8(&self, bus: u8, device: u8, function: u8, offset: usize, value: u8) {
        let aligned = offset & !3;
        let shift = (offset & 3) * 8;
        let mask = !(0xffu32 << shift);
        let current = self.read32(bus, device, function, aligned);
        self.write32(
            bus,
            device,
            function,
            aligned,
            (current & mask) | ((value as u32) << shift),
        );
    }
}

/// Calls `visit` for each function on buses 0..8 until it returns `Some`.
fn walk_functions<C: ConfigSpace, R>(
    config: C,
    mut visit: impl FnMut(PciDevice<C>) -> Option<R>,
) -> Option<R> {
    for bus in 0..SCAN_BUSES {
        for slot in 0..32 {
            for function in 0..8 {
                let id = config.read32(bus, slot, function, reg::VENDOR_ID);
                if id == 0xffff_ffff {
                    // No function present (and no further functions on this
                    // slot for a single-function device).
                    if function == 0 {
                        break;
                    }
                    continue;
                }

                let found = visit(PciDevice {
                    config,
                    bus,
                    slot,
                    function,
                });
                if found.is_some() {
                    return found;
                }

                // Only function 0 is guaranteed to exist; functions 1..7 may
                // be present on a multi-function device.
                let header = config.read8(bus, slot, function, reg::HEADER_TYPE);
                if function == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
    }

    None
}

/// Finds the first device with `vendor`/`device` on buses 0..8.
fn find_device<C: ConfigSpace>(config: C, vendor: u16, device: u16) -> Option<PciDevice<C>> {
    walk_functions(config, |function| {
        (function.vendor_id() == vendor && function.device_id() == device).then_some(function)
    })
}

/// Calls `visit` for every function on buses 0..8, bridges included.
pub fn for_each_function<C: ConfigSpace>(config: C, mut visit: impl FnMut(PciDevice<C>)) {
    walk_functions(config, |function| -> Option<()> {
        visit(function);
        None
    });
}

/// Hands out naturally aligned addresses from one window of bus address space.
///
/// Ranges already in use (a BAR firmware programmed, or one the board fixed)
/// are [reserved](Self::reserve) and skipped, so nothing ends up decoding two
/// devices at the same address.
#[derive(Clone, Copy, Debug)]
pub struct BarAllocator {
    next: u64,
    end: u64,
    reserved: [(u64, u64); Self::MAX_RESERVED],
    reserved_count: usize,
}

impl BarAllocator {
    /// Most ranges one allocator can keep out of its window.
    pub const MAX_RESERVED: usize = 32;

    /// An allocator for the window `start..end`.
    pub const fn new(start: u64, end: u64) -> Self {
        Self {
            next: start,
            end,
            reserved: [(0, 0); Self::MAX_RESERVED],
            reserved_count: 0,
        }
    }

    /// Marks `start..start + size` as used. Returns `false` when the list is full.
    pub fn reserve(&mut self, start: u64, size: u64) -> bool {
        if self.reserved_count == Self::MAX_RESERVED {
            return false;
        }
        self.reserved[self.reserved_count] = (start, start.saturating_add(size));
        self.reserved_count += 1;
        true
    }

    /// Reserves every memory BAR already programmed on the bus.
    pub fn reserve_programmed<C: ConfigSpace>(&mut self, config: C) {
        for_each_function(config, |function| {
            if function.header_kind() != 0 {
                return;
            }
            let mut index = 0;
            while index < 6 {
                let wide = function.is_memory_bar_64(index);
                if let (Some(address), Some(size)) =
                    (function.memory_bar_address(index), function.bar_size(index))
                {
                    if address != 0 {
                        self.reserve(address, size);
                    }
                }
                index += if wide { 2 } else { 1 };
            }
        });
    }

    /// Returns an address for a power-of-two `size`, aligned to `align`.
    pub fn allocate(&mut self, size: u64, align: u64) -> Option<u64> {
        if size == 0 || !align.is_power_of_two() {
            return None;
        }
        let mut start = self.next.checked_next_multiple_of(align)?;
        loop {
            let end = start.checked_add(size)?;
            if end > self.end {
                return None;
            }
            let conflict = self.reserved[..self.reserved_count]
                .iter()
                .find(|(low, high)| start < *high && *low < end);
            match conflict {
                Some((_, high)) => start = high.checked_next_multiple_of(align)?,
                None => {
                    self.next = end;
                    return Some(start);
                }
            }
        }
    }
}

/// Numbers and opens every PCI-PCI bridge that firmware left unprogrammed.
///
/// A firmware-less boot finds bridges with bus numbers of zero, so nothing
/// behind them answers. Each one gets the next free bus and a 16 MiB memory
/// window from `allocator`. Returns the highest bus number in use. Devices
/// behind a bridge then get their BARs from [`bridge_window`].
pub fn configure_bridges<C: ConfigSpace>(config: C, allocator: &mut BarAllocator) -> u8 {
    let mut highest = 0u8;
    // Firmware's numbering stands: never hand out a bus it has used.
    for_each_function(config, |function| {
        if let Some((_, subordinate)) = function.bridge_buses() {
            highest = highest.max(subordinate);
        }
    });
    for bus in 0..SCAN_BUSES {
        for_each_function_on(config, bus, |function| {
            if function.bridge_buses() != Some((0, 0)) || highest + 1 >= SCAN_BUSES {
                return;
            }
            highest += 1;
            let secondary = highest;
            let buses = (function.bus as u32) | ((secondary as u32) << 8) | ((secondary as u32) << 16);
            config.write32(function.bus, function.slot, function.function, reg::BUS_NUMBERS, buses);
            if let Some(window) = allocator.allocate(BRIDGE_WINDOW, BRIDGE_ALIGN) {
                function.set_bridge_window(window, BRIDGE_WINDOW);
                function.enable();
            }
        });
    }
    highest
}

fn for_each_function_on<C: ConfigSpace>(config: C, bus: u8, mut visit: impl FnMut(PciDevice<C>)) {
    for slot in 0..32 {
        for function in 0..8 {
            let id = config.read32(bus, slot, function, reg::VENDOR_ID);
            if id == 0xffff_ffff {
                if function == 0 {
                    break;
                }
                continue;
            }
            visit(PciDevice {
                config,
                bus,
                slot,
                function,
            });
            let header = config.read8(bus, slot, function, reg::HEADER_TYPE);
            if function == 0 && header & 0x80 == 0 {
                break;
            }
        }
    }
}

/// The memory window of the bridge in front of `bus`, if one is, as
/// `(base, size)`. A device on that bus must take its BARs from this window.
pub fn bridge_window<C: ConfigSpace>(config: C, bus: u8) -> Option<(u64, u64)> {
    walk_functions(config, |function| {
        let (secondary, _) = function.bridge_buses()?;
        if secondary != bus {
            return None;
        }
        function.read_bridge_window()
    })
}

/// What a device writes, and where, to raise a message-signalled interrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MsiMessage {
    /// Bus address of the write.
    pub address: u64,
    /// The 32-bit value written.
    pub data: u32,
}

/// Capability IDs for message-signalled interrupts.
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;
/// MSI control register bits, at capability + 2.
const MSI_ENABLE: u16 = 1 << 0;
const MSI_64_BIT: u16 = 1 << 7;
const MSI_MULTIPLE_ENABLE: u16 = 0b111 << 4;
/// MSI-X control register bits, at capability + 2.
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
const MSIX_TABLE_SIZE: u16 = 0x7ff;
/// One MSI-X table entry: address low, address high, data, vector control.
const MSIX_ENTRY_SIZE: usize = 16;

/// One entry in a function's capability list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capability {
    /// Capability ID (`0x09` is vendor specific, `0x05` MSI, `0x11` MSI-X).
    pub id: u8,
    /// Offset of the capability in configuration space.
    pub offset: usize,
}

/// Walks a function's capability list. See [`PciDevice::capabilities`].
pub struct Capabilities<C: ConfigSpace> {
    device: PciDevice<C>,
    next: usize,
    remaining: usize,
}

impl<C: ConfigSpace> Iterator for Capabilities<C> {
    type Item = Capability;

    fn next(&mut self) -> Option<Capability> {
        // The low two bits of a pointer are reserved, and offsets below 0x40
        // are the standard header.
        if self.next < 0x40 || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let offset = self.next;
        let id = self.device.config_read8(offset);
        self.next = (self.device.config_read8(offset + 1) & 0xfc) as usize;
        Some(Capability { id, offset })
    }
}

/// A PCIe ECAM window.
#[derive(Clone, Copy, Debug)]
pub struct PciEcam {
    base: usize,
}

impl PciEcam {
    /// Creates an ECAM accessor for the window at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    /// Base address of the ECAM window.
    pub const fn base(&self) -> usize {
        self.base
    }

    fn address(&self, bus: u8, device: u8, function: u8, offset: usize) -> usize {
        self.base
            + ((bus as usize) << 20)
            + ((device as usize) << 15)
            + ((function as usize) << 12)
            + offset
    }

    /// Finds the first device with `vendor`/`device` on buses 0..8.
    pub fn find(&self, vendor: u16, device: u16) -> Option<PciDevice<Self>> {
        find_device(*self, vendor, device)
    }
}

impl ConfigSpace for PciEcam {
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        // SAFETY: `base` is an MMIO window supplied by the board and the
        // callers pass a device/function address inside it.
        unsafe { read_volatile(self.address(bus, device, function, offset) as *const u32) }
    }

    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
        // SAFETY: see `read32`.
        unsafe {
            write_volatile(
                self.address(bus, device, function, offset) as *mut u32,
                value,
            )
        }
    }
}

/// Legacy PCI configuration access through I/O ports `0xcf8`/`0xcfc`.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug)]
pub struct PciPort;

#[cfg(target_arch = "x86_64")]
impl PciPort {
    /// Creates a legacy-port accessor.
    pub const fn new() -> Self {
        Self
    }

    fn address(bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        0x8000_0000
            | ((bus as u32) << 16)
            | ((device as u32) << 11)
            | ((function as u32) << 8)
            | (offset as u32 & 0xfc)
    }

    /// Finds the first device with `vendor`/`device` on buses 0..8.
    pub fn find(&self, vendor: u16, device: u16) -> Option<PciDevice<Self>> {
        find_device(*self, vendor, device)
    }
}

/// The two ports are an address and its data, so one access must not interleave
/// with another, on this CPU or a second one.
#[cfg(target_arch = "x86_64")]
static PORT_LOCK: IrqSpinLock<()> = IrqSpinLock::new(());

#[cfg(target_arch = "x86_64")]
impl ConfigSpace for PciPort {
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        let address = Self::address(bus, device, function, offset);
        let _guard = PORT_LOCK.lock();
        // SAFETY: `0xcf8`/`0xcfc` are the standard PCI configuration ports.
        unsafe {
            outl(0xcf8, address);
            inl(0xcfc)
        }
    }

    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
        let address = Self::address(bus, device, function, offset);
        let _guard = PORT_LOCK.lock();
        // SAFETY: `0xcf8`/`0xcfc` are the standard PCI configuration ports.
        unsafe {
            outl(0xcf8, address);
            outl(0xcfc, value);
        }
    }
}

/// One PCI function located by a scan.
#[derive(Clone, Copy, Debug)]
pub struct PciDevice<C: ConfigSpace> {
    config: C,
    bus: u8,
    slot: u8,
    function: u8,
}

impl<C: ConfigSpace> PciDevice<C> {
    /// Bus number of this function.
    pub const fn bus(&self) -> u8 {
        self.bus
    }

    /// Slot (device) number of this function.
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    /// Function number.
    pub const fn function(&self) -> u8 {
        self.function
    }

    /// Vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.config
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID) as u16
    }

    /// Device ID.
    pub fn device_id(&self) -> u16 {
        (self
            .config
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID)
            >> 16) as u16
    }

    /// Class code (bits 31:8 of the class/revision register).
    pub fn class_code(&self) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, 0x08)
            >> 8
    }

    /// Reads a byte of this function's configuration space.
    pub fn config_read8(&self, offset: usize) -> u8 {
        self.config.read8(self.bus, self.slot, self.function, offset)
    }

    /// Reads a 32-bit register of this function's configuration space.
    pub fn config_read32(&self, offset: usize) -> u32 {
        self.config.read32(self.bus, self.slot, self.function, offset)
    }

    /// The capabilities this function lists.
    pub fn capabilities(&self) -> Capabilities<C> {
        let status = self
            .config
            .read16(self.bus, self.slot, self.function, reg::STATUS);
        let next = if status & STATUS_CAPABILITIES != 0 {
            (self.config_read8(reg::CAPABILITIES) & 0xfc) as usize
        } else {
            0
        };
        Capabilities {
            device: *self,
            next,
            remaining: MAX_CAPABILITIES,
        }
    }

    fn capability(&self, id: u8) -> Option<usize> {
        self.capabilities()
            .find(|capability| capability.id == id)
            .map(|capability| capability.offset)
    }

    fn write16(&self, offset: usize, value: u16) {
        self.config.write16(self.bus, self.slot, self.function, offset, value);
    }

    fn read16(&self, offset: usize) -> u16 {
        self.config.read16(self.bus, self.slot, self.function, offset)
    }

    fn write32(&self, offset: usize, value: u32) {
        self.config.write32(self.bus, self.slot, self.function, offset, value);
    }

    /// Finds the function's MSI-X table, giving its BAR an address if it needs
    /// one. `None` when it has no such capability or the capability is nonsense.
    pub fn msix_table(&self, allocator: &mut BarAllocator) -> Option<usize> {
        let capability = self.capability(CAP_MSIX)?;
        let location = self.config_read32(capability + 4);
        let bar = (location & 7) as usize;
        if bar > 5 {
            return None;
        }
        let offset = u64::from(location & !7);
        let entries = u64::from(self.read16(capability + 2) & MSIX_TABLE_SIZE) + 1;
        let end = offset.checked_add(entries * MSIX_ENTRY_SIZE as u64)?;
        if end > self.bar_size(bar)? {
            return None;
        }
        let base = self.ensure_memory_bar(bar, allocator)?;
        usize::try_from(base.checked_add(offset)?).ok()
    }

    /// Points one entry of the MSI-X table at `table` to `message`, turns off the
    /// function's other interrupt forms and turns MSI-X on. The caller has found
    /// `table` with [`msix_table`](Self::msix_table). Returns `false` when the
    /// function has no such entry.
    ///
    /// Memory decoding has to be on for this, which is why `table` isn't looked up
    /// here, before the caller enables the function.
    pub fn enable_msix(&self, table: usize, entry: u16, message: MsiMessage) -> bool {
        let Some(capability) = self.capability(CAP_MSIX) else {
            return false;
        };
        let control = self.read16(capability + 2);
        if entry > control & MSIX_TABLE_SIZE {
            return false;
        }
        // The entries are only writable with the function's interrupts held off.
        self.write16(capability + 2, control | MSIX_ENABLE | MSIX_FUNCTION_MASK);
        let slot = table + entry as usize * MSIX_ENTRY_SIZE;
        // SAFETY: `msix_table` checked that the table fits its BAR, and that the
        // BAR has an address the board has mapped as device memory.
        unsafe {
            core::ptr::write_volatile(slot as *mut u32, message.address as u32);
            core::ptr::write_volatile((slot + 4) as *mut u32, (message.address >> 32) as u32);
            core::ptr::write_volatile((slot + 8) as *mut u32, message.data);
            core::ptr::write_volatile((slot + 12) as *mut u32, 0);
        }
        self.disable_intx();
        self.write16(capability + 2, (control | MSIX_ENABLE) & !MSIX_FUNCTION_MASK);
        true
    }

    /// Turns MSI-X off. Entries are masked at reset, so the table needn't be.
    pub fn disable_msix(&self) {
        if let Some(capability) = self.capability(CAP_MSIX) {
            let control = self.read16(capability + 2);
            self.write16(capability + 2, control & !(MSIX_ENABLE | MSIX_FUNCTION_MASK));
        }
    }

    /// Whether the function can use plain MSI.
    pub fn has_msi(&self) -> bool {
        self.capability(CAP_MSI).is_some()
    }

    /// Points the function's MSI capability at `message`, asks for one vector and
    /// turns off its wired line. Returns `false` when it has no such capability.
    pub fn enable_msi(&self, message: MsiMessage) -> bool {
        let Some(capability) = self.capability(CAP_MSI) else {
            return false;
        };
        let control = self.read16(capability + 2);
        // The data register moves when the address is 64 bits wide.
        let data_at = if control & MSI_64_BIT != 0 {
            self.write32(capability + 4, message.address as u32);
            self.write32(capability + 8, (message.address >> 32) as u32);
            capability + 12
        } else {
            if message.address >> 32 != 0 {
                return false;
            }
            self.write32(capability + 4, message.address as u32);
            capability + 8
        };
        self.write16(data_at, message.data as u16);
        self.disable_intx();
        self.write16(capability + 2, (control & !MSI_MULTIPLE_ENABLE) | MSI_ENABLE);
        true
    }

    /// Lets the function drive its wired interrupt pin.
    pub fn enable_intx(&self) {
        let command = self.read16(reg::COMMAND);
        self.write16(reg::COMMAND, command & !command::INTX_DISABLE);
    }

    /// Stops the function driving its wired interrupt pin.
    pub fn disable_intx(&self) {
        let command = self.read16(reg::COMMAND);
        self.write16(reg::COMMAND, command | command::INTX_DISABLE);
    }

    /// The interrupt pin (1 to 4 for INTA to INTD) the function drives, if any.
    pub fn interrupt_pin(&self) -> Option<u8> {
        match self.config_read8(reg::INTERRUPT_PIN) {
            pin @ 1..=4 => Some(pin),
            _ => None,
        }
    }

    /// The interrupt line firmware routed the pin to; 0xff means unknown.
    pub fn interrupt_line(&self) -> u8 {
        self.config_read8(reg::INTERRUPT_LINE)
    }

    /// Header layout: 0 for an endpoint, 1 for a PCI-PCI bridge.
    pub fn header_kind(&self) -> u8 {
        self.config_read8(reg::HEADER_TYPE) & 0x7f
    }

    /// For a bridge, its `(secondary, subordinate)` bus numbers.
    pub fn bridge_buses(&self) -> Option<(u8, u8)> {
        if self.header_kind() != HEADER_BRIDGE {
            return None;
        }
        let buses = self.config_read32(reg::BUS_NUMBERS);
        Some(((buses >> 8) as u8, (buses >> 16) as u8))
    }

    /// Programs a bridge's memory window, and closes its prefetchable one.
    fn set_bridge_window(&self, base: u64, size: u64) {
        let limit = base + size - 1;
        let window = ((base >> 16) as u32 & 0xfff0) | ((limit >> 16) as u32 & 0xfff0) << 16;
        let write = |offset, value| {
            self.config
                .write32(self.bus, self.slot, self.function, offset, value)
        };
        write(reg::MEMORY_WINDOW, window);
        // A base above the limit closes it, as firmware does for an unused one.
        write(reg::PREFETCH_WINDOW, 0x0000_fff0);
        write(reg::PREFETCH_BASE_UPPER, 0);
        write(reg::PREFETCH_LIMIT_UPPER, 0);
    }

    /// A bridge's memory window as `(base, size)`, when it's open.
    fn read_bridge_window(&self) -> Option<(u64, u64)> {
        let window = self.config_read32(reg::MEMORY_WINDOW);
        let base = ((window & 0xfff0) as u64) << 16;
        let limit = (((window >> 16) & 0xfff0) as u64) << 16 | 0xf_ffff;
        (window != 0 && base < limit).then_some((base, limit + 1 - base))
    }

    /// Whether memory BAR `index` takes two registers.
    pub fn is_memory_bar_64(&self, index: usize) -> bool {
        let low = self.bar(index);
        low & 1 == 0 && Self::is_64_bit(low)
    }

    /// Returns a memory BAR's address, assigning one from `allocator` if
    /// firmware left it unprogrammed or put it above the 4 GiB the boards map.
    pub fn ensure_memory_bar(&self, index: usize, allocator: &mut BarAllocator) -> Option<u64> {
        let current = self.memory_bar_address(index)?;
        if current != 0 && current < 0x1_0000_0000 {
            return Some(current);
        }
        let size = self.bar_size(index)?;
        let address = allocator.allocate(size, size.max(4096))?;
        self.assign_memory_bar(index, address).then_some(address)
    }

    /// Reads a 32-bit BAR register (index 0..5).
    pub fn bar(&self, index: usize) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, reg::BAR0 + index * 4)
    }

    fn read_bar(&self, offset: usize) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, offset)
    }

    fn write_bar(&self, offset: usize, value: u32) {
        self.config
            .write32(self.bus, self.slot, self.function, offset, value);
    }

    /// Returns `true` when a memory BAR's type field says it is 64 bits wide.
    fn is_64_bit(low: u32) -> bool {
        low & BAR_TYPE_MASK == BAR_TYPE_64
    }

    /// Runs `body` with memory decoding off.
    ///
    /// While a BAR is being sized or moved it can hold any address, and the
    /// device would answer accesses meant for something else.
    fn with_decode_disabled<T>(&self, body: impl FnOnce() -> T) -> T {
        let current = self
            .config
            .read16(self.bus, self.slot, self.function, reg::COMMAND);
        self.config.write16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
            current & !command::MEMORY_SPACE,
        );
        let result = body();
        self.config
            .write16(self.bus, self.slot, self.function, reg::COMMAND, current);
        result
    }

    /// Returns the address programmed into a memory BAR.
    ///
    /// Returns `None` for an I/O BAR. An address of zero means firmware left the
    /// BAR unassigned.
    pub fn memory_bar_address(&self, index: usize) -> Option<u64> {
        let offset = reg::BAR0 + index * 4;
        let low = self.read_bar(offset);
        if low & 1 != 0 {
            return None;
        }
        let high = if Self::is_64_bit(low) {
            self.read_bar(offset + 4)
        } else {
            0
        };
        Some(((high as u64) << 32) | (low & !0xf) as u64)
    }

    /// Sizes a BAR and returns its length in bytes.
    ///
    /// Returns `None` for I/O BARs or if the size cannot be probed.
    pub fn bar_size(&self, index: usize) -> Option<u64> {
        let offset = reg::BAR0 + index * 4;
        let original = self.read_bar(offset);

        if original & 1 != 0 {
            return None; // I/O space BAR
        }

        let wide = Self::is_64_bit(original);
        let mask = self.with_decode_disabled(|| {
            self.write_bar(offset, 0xffff_ffff);
            let low = self.read_bar(offset);
            self.write_bar(offset, original);
            let high = if wide {
                let high_original = self.read_bar(offset + 4);
                self.write_bar(offset + 4, 0xffff_ffff);
                let high = self.read_bar(offset + 4);
                self.write_bar(offset + 4, high_original);
                high
            } else {
                // A 32-bit BAR decodes nothing above bit 31.
                0xffff_ffff
            };
            ((high as u64) << 32) | (low & !0xf) as u64
        });

        if mask as u32 == 0 && mask >> 32 == if wide { 0 } else { 0xffff_ffff } {
            return None;
        }

        Some((!mask).wrapping_add(1))
    }

    /// Assigns `address` to a memory BAR.
    ///
    /// Handles both 32-bit and 64-bit memory BARs. Returns `false` if the BAR
    /// is not memory-mapped, has an unknown size, `address` is misaligned, or
    /// `address` doesn't fit a 32-bit BAR.
    pub fn assign_memory_bar(&self, index: usize, address: u64) -> bool {
        let offset = reg::BAR0 + index * 4;
        let original = self.read_bar(offset);

        if original & 1 != 0 {
            return false;
        }

        let Some(size) = self.bar_size(index) else {
            return false;
        };

        let wide = Self::is_64_bit(original);
        if !address.is_multiple_of(size) || (!wide && address > u32::MAX as u64) {
            return false;
        }

        self.with_decode_disabled(|| {
            // Preserve the read-only type bits (bit 0 is 0 for memory, bits 2:1
            // encode 32/64-bit addressing and bit 3 prefetchability).
            self.write_bar(offset, (address as u32) | (original & 0xf));
            if wide {
                // 64-bit BAR: the upper half lives in the next register.
                self.write_bar(offset + 4, (address >> 32) as u32);
            }
        });

        true
    }

    /// Enables memory-space decoding and bus mastering.
    pub fn enable(&self) {
        let current = self.config.read16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
        );
        self.config.write16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
            current | command::MEMORY_SPACE | command::BUS_MASTER,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A config space backed by a map, with a sizable BAR per function.
    struct Bus {
        registers: RefCell<HashMap<(u8, u8, u8, usize), u32>>,
        /// `(bus, slot, function, bar offset) -> writable address mask`.
        masks: RefCell<HashMap<(u8, u8, u8, usize), u32>>,
        /// Each write to a BAR while memory decoding is on.
        writes_while_decoding: RefCell<usize>,
    }

    impl Bus {
        fn new() -> &'static Self {
            Box::leak(Box::new(Self {
                registers: RefCell::new(HashMap::new()),
                masks: RefCell::new(HashMap::new()),
                writes_while_decoding: RefCell::new(0),
            }))
        }

        fn add(&self, slot: u8, function: u8, id: u32, multi_function: bool) {
            let mut registers = self.registers.borrow_mut();
            registers.insert((0, slot, function, reg::VENDOR_ID), id);
            let header = if multi_function { 0x80u32 << 16 } else { 0 };
            registers.insert((0, slot, function, 0x0c), header);
            // Cache line size in the byte the wrong offset used to read.
            registers.entry((0, slot, function, 0x0c)).and_modify(|value| *value |= 0x10);
        }

        fn bar(&self, slot: u8, function: u8, offset: usize, value: u32, mask: u32) {
            self.registers
                .borrow_mut()
                .insert((0, slot, function, offset), value);
            self.masks
                .borrow_mut()
                .insert((0, slot, function, offset), mask);
        }
    }

    impl ConfigSpace for &'static Bus {
        fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
            *self
                .registers
                .borrow()
                .get(&(bus, device, function, offset))
                .unwrap_or(&if offset == reg::VENDOR_ID {
                    0xffff_ffff
                } else {
                    0
                })
        }

        fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
            let key = (bus, device, function, offset);
            let value = match self.masks.borrow().get(&key) {
                Some(mask) => {
                    let command = self.read32(bus, device, function, reg::COMMAND);
                    if command as u16 & command::MEMORY_SPACE != 0 {
                        *self.writes_while_decoding.borrow_mut() += 1;
                    }
                    let current = self.read32(bus, device, function, offset);
                    (value & mask) | (current & !mask)
                }
                None => value,
            };
            self.registers.borrow_mut().insert(key, value);
        }
    }

    const ID: u32 = 0x2668_8086;

    fn found(bus: &'static Bus) -> Option<PciDevice<&'static Bus>> {
        find_device(bus, 0x8086, 0x2668)
    }

    #[test]
    fn scan_finds_a_device_at_function_zero() {
        let bus = Bus::new();
        bus.add(3, 0, ID, false);
        let device = found(bus).expect("device");
        assert_eq!((device.slot(), device.function()), (3, 0));
    }

    #[test]
    fn scan_reaches_higher_functions_of_a_multi_function_device() {
        // DRV-6: the header type was read from 0x0c, the cache line size, so
        // the multi-function bit was never seen. q35's AHCI is at 00:1f.2.
        let bus = Bus::new();
        bus.add(0x1f, 0, 0x2918_8086, true);
        bus.add(0x1f, 2, ID, false);
        let device = found(bus).expect("device");
        assert_eq!((device.slot(), device.function()), (0x1f, 2));
    }

    #[test]
    fn scan_skips_functions_of_a_single_function_device() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_5678, false);
        bus.add(1, 2, ID, false);
        assert!(found(bus).is_none());
    }

    #[test]
    fn read8_and_read16_select_the_right_lane() {
        let bus = Bus::new();
        bus.registers.borrow_mut().insert((0, 0, 0, 0x0c), 0x0080_0010);
        assert_eq!(bus.read8(0, 0, 0, 0x0c), 0x10);
        assert_eq!(bus.read8(0, 0, 0, 0x0e), 0x80);
        assert_eq!(bus.read16(0, 0, 0, 0x0e), 0x0080);
    }

    #[test]
    fn a_32_bit_bar_sizes_and_assigns() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0, 0xffff_c000);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), Some(0x4000));
        assert!(device.assign_memory_bar(0, 0xc000_0000));
        assert_eq!(device.memory_bar_address(0), Some(0xc000_0000));

        // A 32-bit BAR can't hold a 64-bit address, and alignment is enforced.
        assert!(!device.assign_memory_bar(0, 0x1_0000_0000));
        assert!(!device.assign_memory_bar(0, 0xc000_1000));
    }

    #[test]
    fn a_64_bit_bar_takes_an_address_above_4_gib() {
        // DRV-17: the docs claimed 64-bit BARs while the code refused them.
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, BAR_TYPE_64, 0xffff_c000);
        bus.bar(1, 0, reg::BAR0 + 4, 0, 0xffff_ffff);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), Some(0x4000));
        assert!(device.assign_memory_bar(0, 0x2_c000_0000));
        assert_eq!(device.memory_bar_address(0), Some(0x2_c000_0000));
    }

    #[test]
    fn sizing_restores_the_bar_and_decode_is_off_meanwhile() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0xc000_0000, 0xffff_c000);
        let device = found(bus).unwrap();
        device.enable();

        assert_eq!(device.bar_size(0), Some(0x4000));
        assert_eq!(device.memory_bar_address(0), Some(0xc000_0000));
        assert!(device.assign_memory_bar(0, 0xd000_0000));
        assert_eq!(*bus.writes_while_decoding.borrow(), 0);

        // Decoding is back on afterwards.
        let command = bus.read16(0, 1, 0, reg::COMMAND);
        assert_ne!(command & command::MEMORY_SPACE, 0);
    }

    #[test]
    fn an_io_bar_is_refused() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0xc001, 0xffff_fff0);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), None);
        assert_eq!(device.memory_bar_address(0), None);
        assert!(!device.assign_memory_bar(0, 0xc000_0000));
    }

    #[test]
    fn an_unimplemented_bar_has_no_size() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0, 0);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), None);
    }

    #[test]
    fn capabilities_are_walked_and_a_loop_ends() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_abcd, false);
        {
            let mut registers = bus.registers.borrow_mut();
            registers.insert((0, 1, 0, 0x04), 1 << 20); // Status: list present.
            registers.insert((0, 1, 0, 0x34), 0x40);
            registers.insert((0, 1, 0, 0x40), 0x0000_5009);
            registers.insert((0, 1, 0, 0x50), 0x0000_5011); // Points at itself.
        }
        let device = found_id(bus, 0x1234_abcd);
        let mut walked = device.capabilities();
        assert_eq!(walked.next(), Some(Capability { id: 0x09, offset: 0x40 }));
        assert_eq!(walked.next(), Some(Capability { id: 0x11, offset: 0x50 }));
        assert_eq!(device.capabilities().count(), MAX_CAPABILITIES);
    }

    /// A function with an MSI capability at 0x50 and nothing else.
    fn msi_function(bus: &'static Bus, flags: u16) -> PciDevice<&'static Bus> {
        bus.add(1, 0, 0x1234_abcd, false);
        {
            let mut registers = bus.registers.borrow_mut();
            registers.insert((0, 1, 0, 0x04), 1 << 20); // Status: list present.
            registers.insert((0, 1, 0, 0x34), 0x50);
            registers.insert((0, 1, 0, 0x50), u32::from(CAP_MSI) | u32::from(flags) << 16);
        }
        found_id(bus, 0x1234_abcd)
    }

    #[test]
    fn msi_is_programmed_for_both_address_widths() {
        let message = MsiMessage { address: 0xfee0_1000, data: 0x45 };

        let bus = Bus::new();
        let device = msi_function(bus, MSI_64_BIT);
        assert!(device.has_msi());
        assert!(device.enable_msi(message));
        assert_eq!(bus.read32(0, 1, 0, 0x54), 0xfee0_1000);
        assert_eq!(bus.read32(0, 1, 0, 0x58), 0);
        assert_eq!(bus.read16(0, 1, 0, 0x5c), 0x45);
        assert_eq!(bus.read16(0, 1, 0, 0x52) & MSI_ENABLE, MSI_ENABLE);
        assert_ne!(bus.read16(0, 1, 0, reg::COMMAND) & command::INTX_DISABLE, 0);

        let bus = Bus::new();
        let device = msi_function(bus, 0);
        assert!(device.enable_msi(message));
        assert_eq!(bus.read32(0, 1, 0, 0x54), 0xfee0_1000);
        assert_eq!(bus.read16(0, 1, 0, 0x58), 0x45);

        // A 32-bit capability can't reach a 64-bit address.
        let bus = Bus::new();
        let device = msi_function(bus, 0);
        assert!(!device.enable_msi(MsiMessage { address: 1 << 32, data: 1 }));
        assert_eq!(bus.read16(0, 1, 0, 0x52) & MSI_ENABLE, 0);
    }

    #[test]
    fn a_function_without_the_capability_refuses() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_abcd, false);
        let device = found_id(bus, 0x1234_abcd);
        assert!(!device.has_msi());
        assert!(!device.enable_msi(MsiMessage { address: 0, data: 0 }));
        assert!(!device.enable_msix(0x1000, 0, MsiMessage { address: 0, data: 0 }));
        let mut allocator = BarAllocator::new(0x1000_0000, 0x2000_0000);
        assert_eq!(device.msix_table(&mut allocator), None);
    }

    #[test]
    fn the_wired_pin_can_be_turned_on_and_off() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_abcd, false);
        let device = found_id(bus, 0x1234_abcd);
        device.disable_intx();
        assert_ne!(bus.read16(0, 1, 0, reg::COMMAND) & command::INTX_DISABLE, 0);
        device.enable_intx();
        assert_eq!(bus.read16(0, 1, 0, reg::COMMAND) & command::INTX_DISABLE, 0);
    }

    #[test]
    fn no_capability_bit_means_no_list() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_abcd, false);
        bus.registers.borrow_mut().insert((0, 1, 0, 0x34), 0x40);
        assert_eq!(found_id(bus, 0x1234_abcd).capabilities().count(), 0);
    }

    fn found_id(bus: &'static Bus, id: u32) -> PciDevice<&'static Bus> {
        find_device(bus, id as u16, (id >> 16) as u16).unwrap()
    }

    #[test]
    fn the_allocator_aligns_and_skips_what_is_reserved() {
        let mut allocator = BarAllocator::new(0x1000_0000, 0x1100_0000);
        assert!(allocator.reserve(0x1000_0000, 0x4000));
        assert_eq!(allocator.allocate(0x4000, 0x4000), Some(0x1000_4000));
        assert_eq!(allocator.allocate(0x1000, 0x1000), Some(0x1000_8000));
        // The next 1 MiB boundary is the first address that is aligned.
        assert_eq!(allocator.allocate(0x10_0000, 0x10_0000), Some(0x1010_0000));
        assert_eq!(allocator.allocate(0x100_0000, 0x1000), None);
        assert_eq!(allocator.allocate(0, 0x1000), None);
        assert_eq!(allocator.allocate(0x1000, 3), None);
    }

    #[test]
    fn firmware_bars_are_kept_and_empty_ones_assigned() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_abcd, false);
        bus.bar(1, 0, 0x10, 0x2000_0000, 0xffff_c000);
        bus.bar(1, 0, 0x14, 0, 0xffff_c000);
        let device = found_id(bus, 0x1234_abcd);
        let mut allocator = BarAllocator::new(0x1000_0000, 0x2000_0000);
        assert_eq!(device.ensure_memory_bar(0, &mut allocator), Some(0x2000_0000));
        assert_eq!(device.ensure_memory_bar(1, &mut allocator), Some(0x1000_0000));
        assert_eq!(device.memory_bar_address(1), Some(0x1000_0000));
    }

    #[test]
    fn an_unprogrammed_bridge_gets_a_bus_and_a_window() {
        let bus = Bus::new();
        bus.add(2, 0, 0x000c_1b36, false);
        bus.registers.borrow_mut().insert((0, 2, 0, 0x0c), 0x0001_0000); // Type 1.
        let mut allocator = BarAllocator::new(0x1000_0000, 0x3000_0000);
        assert_eq!(configure_bridges(bus, &mut allocator), 1);
        let bridge = found_id(bus, 0x000c_1b36);
        assert_eq!(bridge.bridge_buses(), Some((1, 1)));
        assert_eq!(bridge_window(bus, 1), Some((0x1000_0000, 0x100_0000)));
        // The window is taken, and bridge numbering is idempotent.
        assert_eq!(allocator.allocate(0x1000, 0x1000), Some(0x1100_0000));
        assert_eq!(configure_bridges(bus, &mut allocator), 1);
        assert_eq!(bridge_window(bus, 2), None);
    }
}
