//! Board support package for QEMU's x86_64 `q35` machine.
//!
//! The board owns the machine-specific facts: the COM1 port, APIC/IOAPIC
//! addresses, the PCI configuration transport, the scanout buffer, and the
//! wiring between drivers and kernel libraries. The kernel itself never
//! mentions a physical address.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_board_common::audio::{self, AudioDevice};
use feos_board_common::usb;
use feos_board_common::display::{self, back_buffer, display_name, display_size, framebuffer_addr};
use feos_board_common::{bind_pci_net, bss_range, kernel_range, mount_filesystem, stack_range, wire_pci_disk, IrqRouter};
use feos_console::println;
use feos_driver_apic::ApicController;
use feos_driver_cmos_rtc::CmosRtc;
use feos_driver_fw_cfg::FwCfg;
use feos_driver_i8042::{PointerKind, I8042};
use feos_driver_pci::{configure_bridges, for_each_function, BarAllocator, MsiMessage, PciPort};
use feos_driver_uart16550::Uart16550;
use feos_driver_virtio_blk::VirtioBlk;
use feos_driver_virtio_input::{DeviceKind, VirtioInput};
use feos_bootinfo::Handoff;
use feos_firmware::{acpi, acpi::Madt, multiboot, PhysMem, Region, RegionList};
use feos_fs::vfs::Embedded;
use feos_fs::{FileSystem, StaticFs};
use feos_gfx::Canvas;
use feos_hal::net::NetBinding;
use feos_hal::{
    Board, Clock, InterruptController, Keyboard, KeyboardEvent, Pointer, PointerEvent, RealTimeClock, Serial,
};
use feos_sync::SpinLock;
use feos_virtio_core::{device, AnyTransport, PciTransport};

/// Human-readable board name, shown by the `info` app.
pub const NAME: &str = "QEMU x86_64 q35";

/// Architecture reported by `uname -m`.
pub const ARCH: &str = "x86_64";

/// Name of the serial device, used by the boot log.
pub const CONSOLE_NAME: &str = "16550";

/// Name of the interrupt controller, used by the boot log.
pub const INTERRUPT_CONTROLLER_NAME: &str = "LAPIC/IOAPIC";

/// Name of the pointer device family, used by the boot log.
pub const POINTER_NAME: &str = "PS/2";
/// Hint shown when no pointer was found.
pub const POINTER_HINT: &str = "QEMU q35 should provide an i8042 mouse";

/// Name of the keyboard device family, used by the boot log.
pub const KEYBOARD_NAME: &str = "PS/2";
/// Hint shown when no keyboard was found.
pub const KEYBOARD_HINT: &str = "serial console is the keyboard";

/// COM1 data port.
pub const CONSOLE_BASE: usize = 0x3f8;

/// fw_cfg I/O port base (selector; data and DMA ports follow).
pub const FW_CFG_IO_BASE: u16 = 0x510;

/// Local APIC MMIO base.
pub const LAPIC_BASE: usize = 0xfee0_0000;

/// IOAPIC MMIO base.
pub const IOAPIC_BASE: usize = 0xfec0_0000;

/// Vector of each CPU's local APIC timer, which is the scheduler's tick.
pub const TIMER_IRQ: u32 = feos_arch_x86_64::timer::TIMER_VECTOR;

/// Vector of the IPI that stops a CPU for good.
pub const IPI_STOP: u32 = 49;

/// Vector of the IPI that asks a CPU to reschedule.
pub const IPI_RESCHEDULE: u32 = 50;

/// CPUs the board can bring up.
pub const MAX_CPUS: usize = feos_arch_x86_64::smp::MAX_CPUS;

/// Address assigned to BAR0 of the Intel HDA controller when firmware left it
/// unprogrammed.
///
/// This lies in the PCI memory hole below 4 GiB, just above QEMU's q35 ECAM
/// window (`0xb000_0000..0xc000_0000`), and is 16 KiB aligned, matching the
/// controller's BAR size. SeaBIOS assigns BARs when it loads the kernel, so
/// its choice is normally kept: it knows about the machine's other devices.
pub const HDA_BAR0: usize = 0xc000_0000;

/// Where the kernel can place a BAR that firmware didn't: the PCI hole above the
/// q35 ECAM window and below the APICs. BARs already programmed are skipped.
pub const PCI_MEMORY_START: u64 = 0xc000_0000;
/// End of that range.
pub const PCI_MEMORY_END: u64 = 0xfe00_0000;

/// Intel vendor ID.
const PCI_VENDOR_INTEL: u16 = 0x8086;
/// ICH6 HD Audio controller device ID.
const PCI_DEVICE_ICH6_HDA: u16 = 0x2668;

/// Start of guest RAM.
pub const RAM_BASE: usize = 0x0010_0000;

/// The single UART exposed by the machine.
pub static UART0: Uart16550 = Uart16550::new(CONSOLE_BASE as u16);

/// The APIC pair used by the timer tick.
pub static INTERRUPT_CONTROLLER: ApicController = ApicController::new(LAPIC_BASE, IOAPIC_BASE);

/// Physical memory through the boot identity map, which covers the first 4 GiB.
struct IdentityMap;

impl PhysMem for IdentityMap {
    fn read(&self, address: u64, out: &mut [u8]) -> bool {
        match address.checked_add(out.len() as u64) {
            Some(end) if end <= 0x1_0000_0000 => {}
            _ => return false,
        }
        for (offset, byte) in out.iter_mut().enumerate() {
            // SAFETY: the whole range is mapped, and read-only access to it has
            // no effect: firmware tables are in RAM, and a bad pointer into a
            // device window at worst reads something meaningless.
            *byte = unsafe { core::ptr::read_volatile((address as usize + offset) as *const u8) };
        }
        true
    }
}

/// What firmware told the boot CPU. Written once, before anything else runs.
struct BootInfo {
    ram: SpinLock<RegionList>,
    madt: SpinLock<Option<Madt>>,
}

static BOOT_INFO: BootInfo = BootInfo {
    ram: SpinLock::new(RegionList::new()),
    madt: SpinLock::new(None),
};

/// Logical CPUs found, once the boot CPU's APIC ID has sorted them.
static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);

/// Reads what firmware says: from the EFI loader if it came that way, and otherwise
/// the multiboot memory map and the ACPI tables in the BIOS area.
pub fn init_boot_info(boot_info: usize) {
    // SAFETY: the pointer is the loader's or multiboot's, and in the boot identity map.
    let handoff = unsafe { Handoff::at(boot_info) };
    let rsdp = if let Some(handoff) = handoff {
        let mut ram = RegionList::new();
        for range in handoff.ram() {
            ram.push(Region { base: range.base, size: range.size });
        }
        *BOOT_INFO.ram.lock() = ram;
        display::set_firmware_framebuffer(handoff.framebuffer);
        acpi::rsdp_at(&IdentityMap, handoff.rsdp)
    } else {
        if let Some(ram) = multiboot::usable_ram(&IdentityMap, boot_info as u64) {
            *BOOT_INFO.ram.lock() = ram;
        }
        acpi::find_rsdp(&IdentityMap)
    };
    let madt = rsdp.and_then(|rsdp| acpi::parse_madt(&IdentityMap, &rsdp));
    match &madt {
        Some(madt) => println!(
            "[acpi] MADT: {} CPU(s), {} IOAPIC(s), {} override(s)",
            madt.apic_ids().len(),
            madt.ioapics().len(),
            madt.overrides().len()
        ),
        None => println!("[acpi] no MADT, staying on the boot CPU"),
    }
    *BOOT_INFO.madt.lock() = madt;
}

/// Initializes the APICs, from what the MADT says about them.
pub fn init_interrupts() {
    let boot_apic_id = INTERRUPT_CONTROLLER.cpu_id();
    INTERRUPT_CONTROLLER.set_apic_id(0, boot_apic_id);

    if let Some(madt) = &*BOOT_INFO.madt.lock() {
        if let Some(ioapic) = madt.ioapics().iter().find(|ioapic| ioapic.gsi_base == 0) {
            INTERRUPT_CONTROLLER.set_ioapic_base(ioapic.address as usize);
        }
        // The boot CPU is logical CPU 0, wherever the firmware lists it.
        let mut count = 1;
        for &id in madt.apic_ids() {
            if id != boot_apic_id && count < MAX_CPUS {
                INTERRUPT_CONTROLLER.set_apic_id(count, id);
                count += 1;
            }
        }
        CPU_COUNT.store(count, Ordering::Release);
    }
    INTERRUPT_CONTROLLER.init();
}

/// Initializes the calling CPU's local APIC.
pub fn init_cpu_interrupts() {
    INTERRUPT_CONTROLLER.init_cpu();
}

/// Returns the interrupt controller shared with the kernel IRQ handler.
pub fn interrupts() -> &'static dyn InterruptController {
    &INTERRUPT_CONTROLLER
}

/// APIC ID for a logical CPU index.
pub fn cpu_mpidr(index: usize) -> u64 {
    u64::from(INTERRUPT_CONTROLLER.apic_id(index))
}

/// Vectors handed out for devices, past the fixed ones.
const FIRST_DEVICE_VECTOR: u32 = 64;
const LAST_DEVICE_VECTOR: u32 = 239;
static NEXT_VECTOR: AtomicU32 = AtomicU32::new(FIRST_DEVICE_VECTOR);

fn alloc_vector() -> Option<u32> {
    let vector = NEXT_VECTOR.fetch_add(1, Ordering::Relaxed);
    (vector <= LAST_DEVICE_VECTOR).then_some(vector)
}

/// The GSIs q35 gives PCI's wired interrupts, and the vector each has taken.
const FIRST_PIRQ_GSI: u32 = 20;
static PIRQ_VECTORS: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

struct Router;

impl IrqRouter for Router {
    fn alloc_msi(&self) -> Option<(MsiMessage, u32)> {
        let vector = alloc_vector()?;
        // Fixed delivery, edge triggered, to the boot CPU.
        let address = 0xfee0_0000 | u64::from(INTERRUPT_CONTROLLER.apic_id(0)) << 12;
        Some((MsiMessage { address, data: vector }, vector))
    }

    fn pci_intx(&self, bus: u8, slot: u8, pin: u8) -> Option<u32> {
        // Only the root bus: behind a bridge the pin is rotated by a slot number
        // up the tree. Those devices have MSI-X on this machine.
        if bus != 0 || slot > 24 {
            return None;
        }
        // QEMU's table: slots up to 24 rotate through PIRQ E to H, which the
        // ICH9's IOAPIC mode wires to GSI 20 to 23.
        let index = (u32::from(slot) + u32::from(pin) - 1) % 4;
        let slot = PIRQ_VECTORS.get(index as usize)?;
        let known = slot.load(Ordering::Acquire);
        if known != 0 {
            return Some(known);
        }
        let vector = alloc_vector()?;
        slot.store(vector, Ordering::Release);
        INTERRUPT_CONTROLLER.route_line(FIRST_PIRQ_GSI + index, vector as u8, true);
        Some(vector)
    }

    fn enable_line(&self, _id: u32) {}
}

/// The PS/2 controller shared by the keyboard and pointer.
pub static INPUT: I8042 = I8042::new();

/// The virtio pointer on PCI, when there is one. What's added last gets QEMU's events,
/// so with one of these the PS/2 devices don't hear anything.
static VIRTIO_POINTER: SpinLock<VirtioInput> = SpinLock::new(VirtioInput::new());
/// The virtio keyboard on PCI.
static VIRTIO_KEYBOARD: SpinLock<VirtioInput> = SpinLock::new(VirtioInput::new());

struct BoardPointer;

impl Pointer for BoardPointer {
    fn poll(&self) -> Option<PointerEvent> {
        VIRTIO_POINTER.lock().poll()
    }

    fn position(&self) -> (u16, u16) {
        VIRTIO_POINTER.lock().position()
    }
}

static BOARD_POINTER: BoardPointer = BoardPointer;

struct BoardKeyboard;

impl Keyboard for BoardKeyboard {
    fn poll(&self) -> Option<KeyboardEvent> {
        VIRTIO_KEYBOARD.lock().poll_key()
    }
}

static BOARD_KEYBOARD: BoardKeyboard = BoardKeyboard;

/// Brings up whatever virtio input devices are on the PCI bus.
fn init_virtio_input() {
    let pci = PciPort::new();
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    let (width, height) = display_size();
    for_each_function(pci, |function| {
        if !PciTransport::matches(&function, device::INPUT) {
            return;
        }
        let Some(transport) = PciTransport::new(pci, function, &mut allocator) else {
            println!("[input] virtio-pci {:02x}:{:02x}: unusable", function.bus(), function.slot());
            return;
        };
        let any = AnyTransport::Pci(transport);
        match feos_driver_virtio_input::probe_transport(&any) {
            Some(DeviceKind::Pointer(kind)) => {
                if VIRTIO_POINTER.lock().init_transport(any, width as u16, height as u16) {
                    println!("[input] virtio-pci pointer ready ({kind:?})");
                }
            }
            Some(DeviceKind::Keyboard) => {
                if VIRTIO_KEYBOARD.lock().init_transport(any, 1, 1) {
                    println!("[input] virtio-pci keyboard ready");
                }
            }
            None => {}
        }
    });
}

/// Finds and initialises the pointers: PS/2, and virtio if there is one.
pub fn init_pointer() -> Option<PointerKind> {
    init_virtio_input();
    INPUT.set_clock(feos_arch_x86_64::timer::uptime_ms);
    let (width, height) = display_size();
    if INPUT.init(width as u16, height as u16) {
        Some(INPUT.pointer_kind())
    } else {
        None
    }
}

/// Returns the pointer shared with apps, when one was initialised.
pub fn pointer() -> Option<&'static dyn Pointer> {
    let own: Option<&'static dyn Pointer> = if VIRTIO_POINTER.lock().is_ready() {
        Some(&BOARD_POINTER)
    } else if INPUT.is_ready() {
        Some(&INPUT)
    } else {
        None
    };
    usb::pointer(own)
}

/// Finds and initialises the PS/2 keyboard.
pub fn init_keyboard() -> bool {
    let (width, height) = display_size();
    INPUT.init(width as u16, height as u16)
}

/// Returns the keyboard shared with apps, when one was initialised.
pub fn keyboard() -> Option<&'static dyn Keyboard> {
    let own: Option<&'static dyn Keyboard> = if VIRTIO_KEYBOARD.lock().is_ready() {
        Some(&BOARD_KEYBOARD)
    } else if INPUT.is_ready() {
        Some(&INPUT)
    } else {
        None
    };
    usb::keyboard(own)
}

feos_board_common::wire_console!(UART0);

/// Brings up a virtio-gpu if the machine has one, and ramfb otherwise.
pub fn init_display() -> Option<Canvas<'static>> {
    display::init(FwCfg::new_io(FW_CFG_IO_BASE), find_gpu)
}

/// Wall-clock seconds since the Unix epoch, from the CMOS clock.
pub fn unix_time() -> Option<u64> {
    CmosRtc::new().unix_time()
}

/// Finds a virtio-gpu on the PCI bus.
fn find_gpu() -> Option<AnyTransport> {
    let pci = PciPort::new();
    let mut found = None;
    for_each_function(pci, |function| {
        if found.is_none() && PciTransport::matches(&function, device::GPU) {
            found = Some(function);
        }
    });
    let function = found?;
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    PciTransport::new(pci, function, &mut allocator).map(AnyTransport::Pci)
}

/// Brings up every virtio block device on the PCI bus.
fn block_devices() -> alloc::vec::Vec<VirtioBlk<AnyTransport>> {
    let mut disks = alloc::vec::Vec::new();
    let pci = PciPort::new();
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    for_each_function(pci, |function| {
        if !PciTransport::matches(&function, device::BLOCK) {
            return;
        }
        let Some(transport) = PciTransport::new(pci, function, &mut allocator) else {
            println!("[disk] virtio-pci {:02x}:{:02x}: unusable", function.bus(), function.slot());
            return;
        };
        match VirtioBlk::new(AnyTransport::Pci(transport), feos_sync::yield_now) {
            Ok(mut disk) => {
                let how = wire_pci_disk(&mut disk, &function, transport, &Router);
                println!("[disk] virtio-pci {:02x}:{:02x}: {how}", function.bus(), function.slot());
                disks.push(disk);
            }
            Err(error) => println!("[disk] virtio-pci: {error:?}"),
        }
    });
    disks
}

/// Brings up the USB host controller, if there is one.
pub fn init_usb(clock: &'static dyn Clock) {
    let pci = PciPort::new();
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    let (width, height) = display_size();
    usb::init(pci, &mut allocator, &Router, clock, (width as u16, height as u16));
}

/// Finds the first virtio network card on the PCI bus.
pub fn init_network() -> Option<NetBinding> {
    let pci = PciPort::new();
    let mut found = None;
    for_each_function(pci, |function| {
        if found.is_none() && PciTransport::matches(&function, device::NET) {
            found = Some(function);
        }
    });
    let function = found?;
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    let transport = PciTransport::new(pci, function, &mut allocator)?;
    bind_pci_net(&function, transport, &Router)
}

mod fs_image {
    include!(concat!(env!("OUT_DIR"), "/fs_image.rs"));
}

/// The compiled-in read-only filesystem.
static FILESYSTEM: StaticFs = StaticFs::new(fs_image::FS_IMAGE);

/// Returns the filesystem apps browse through `Platform::filesystem`.
///
/// The first call probes for disks and assembles the shared tree: a disk's
/// filesystem at the root when there is one, a writable RAM copy of the embedded
/// image otherwise. See [`feos_fs::vfs::assemble`].
pub fn filesystem() -> &'static dyn FileSystem {
    static IMAGES: [(&str, Option<&[u8]>); 2] = [
        ("fat32", fs_image::DISK_FAT32_IMAGE),
        ("exfat", fs_image::DISK_EXFAT_IMAGE),
    ];
    mount_filesystem(
        || Embedded {
            tree: &FILESYSTEM,
            root_image: fs_image::DISK_IMAGE,
            images: &IMAGES,
        },
        block_devices,
        unix_time,
    )
}

/// Finds the Intel HDA controller on the PCI bus and brings it up.
pub fn init_audio(clock: &'static dyn Clock) -> Option<AudioDevice> {
    let pci = PciPort::new();
    let controller = pci.find(PCI_VENDOR_INTEL, PCI_DEVICE_ICH6_HDA)?;

    // Keep a BAR from firmware if it's below 4 GiB, which is all this board maps.
    let bar = match controller.memory_bar_address(0) {
        Some(address) if address != 0 && address < 0x1_0000_0000 => address as usize,
        _ => {
            if !controller.assign_memory_bar(0, HDA_BAR0 as u64) {
                return None;
            }
            HDA_BAR0
        }
    };
    controller.enable();

    audio::bring_up(&controller, bar, clock, &Router)
}

/// This board, as the kernel sees it.
pub struct QemuX86_64;

impl Board for QemuX86_64 {
    const NAME: &'static str = NAME;
    const ARCH: &'static str = ARCH;
    const CONSOLE_NAME: &'static str = CONSOLE_NAME;
    const CONSOLE_BASE: usize = CONSOLE_BASE;
    const POINTER_NAME: &'static str = POINTER_NAME;
    const POINTER_HINT: &'static str = POINTER_HINT;
    const KEYBOARD_NAME: &'static str = KEYBOARD_NAME;
    const KEYBOARD_HINT: &'static str = KEYBOARD_HINT;
    const TIMER_IRQ: u32 = TIMER_IRQ;
    const IPI_STOP: u32 = IPI_STOP;
    const IPI_RESCHEDULE: u32 = IPI_RESCHEDULE;
    const MAX_CPUS: usize = MAX_CPUS;
    /// Where QEMU's PCI hole starts at its lowest. RAM goes up to here, or stops at
    /// 2 GiB and carries on from 4 GiB, where the frame allocator doesn't reach.
    const RAM_LIMIT: u64 = 0xb000_0000;
    /// The BIOS's memory, and the page the other CPUs start from.
    const LOW_RESERVED: u64 = 0x10_0000;

    type Display = Canvas<'static>;
    type AudioDevice = AudioDevice;
    type PointerKind = PointerKind;
    type FileSystem = dyn FileSystem;

    fn init_console() {
        init_console();
    }

    fn write_emergency(bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                UART0.write_byte(b'\r');
            }
            UART0.write_byte(byte);
        }
    }

    fn init_boot_info(boot_info: usize) {
        init_boot_info(boot_info);
    }

    fn cpu_count() -> usize {
        CPU_COUNT.load(Ordering::Acquire)
    }

    fn usable_ram(out: &mut [(u64, u64)]) -> usize {
        let ram = BOOT_INFO.ram.lock();
        let mut count = 0;
        for (slot, region) in out.iter_mut().zip(ram.as_slice()) {
            *slot = (region.base, region.size);
            count += 1;
        }
        count
    }

    fn init_interrupts() {
        init_interrupts();
    }

    fn init_cpu_interrupts() {
        init_cpu_interrupts();
    }

    fn interrupts() -> &'static dyn InterruptController {
        interrupts()
    }

    fn interrupt_controller_name() -> &'static str {
        INTERRUPT_CONTROLLER_NAME
    }

    fn cpu_mpidr(index: usize) -> u64 {
        cpu_mpidr(index)
    }

    fn init_display() -> Option<Canvas<'static>> {
        init_display()
    }

    fn back_buffer() -> Option<Canvas<'static>> {
        back_buffer()
    }

    fn framebuffer_addr() -> usize {
        framebuffer_addr()
    }

    fn init_audio(clock: &'static dyn Clock) -> Option<AudioDevice> {
        init_audio(clock)
    }

    fn init_pointer() -> Option<PointerKind> {
        init_pointer()
    }

    fn pointer() -> Option<&'static dyn Pointer> {
        pointer()
    }

    fn init_keyboard() -> bool {
        init_keyboard()
    }

    fn keyboard() -> Option<&'static dyn Keyboard> {
        keyboard()
    }

    fn init_network() -> Option<NetBinding> {
        init_network()
    }

    fn init_usb(clock: &'static dyn Clock) {
        init_usb(clock);
    }

    fn usb_stats() -> Option<feos_hal::UsbStats> {
        usb::stats()
    }

    fn filesystem() -> &'static dyn FileSystem {
        filesystem()
    }

    fn display_name() -> &'static str {
        display_name()
    }

    fn unix_time() -> Option<u64> {
        unix_time()
    }

    fn kernel_range() -> (usize, usize) {
        kernel_range()
    }

    fn bss_range() -> (usize, usize) {
        bss_range()
    }

    fn stack_range() -> (usize, usize) {
        stack_range()
    }
}
