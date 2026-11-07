//! Board support package for the QEMU `virt` machine.
//!
//! The board owns the machine-specific facts: MMIO addresses, the RAM map, the
//! scanout buffer, and the wiring between drivers and kernel libraries. The
//! kernel itself never mentions a physical address.
//!
//! To support another machine, copy this crate to `boards/<name>`, adjust the
//! constants and devices, and select it in the kernel's `Cargo.toml`.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use feos_board_common::audio::{self, AudioDevice};
use feos_board_common::usb;
use feos_board_common::display::{self, back_buffer, display_name, display_size, framebuffer_addr};
use feos_board_common::{
    bind_mmio_net, bind_pci_net, bss_range, kernel_range, mount_filesystem, stack_range, wire_mmio_disk,
    wire_pci_disk, IrqRouter,
};
use feos_console::println;
use feos_driver_fw_cfg::FwCfg;
use feos_driver_gicv2::GicV2;
use feos_driver_gicv3::GicV3;
use feos_driver_pci::{configure_bridges, for_each_function, BarAllocator, MsiMessage, PciEcam};
use feos_driver_pl011::Pl011;
use feos_driver_pl031::Pl031;
use feos_driver_virtio_blk::VirtioBlk;
use feos_driver_virtio_input::{PointerKind, VirtioInput};
use feos_firmware::fdt::{Fdt, Gic as FdtGic, PsciMethod};
use feos_bootinfo::Handoff;
use feos_firmware::{Region, RegionList};
use feos_fs::vfs::Embedded;
use feos_fs::{FileSystem, StaticFs};
use feos_gfx::Canvas;
use feos_hal::net::NetBinding;
use feos_hal::{
    Board, Clock, FirmwareConduit, InterruptController, IpiTarget, IrqClaim, IrqTrigger, Keyboard, KeyboardEvent, Pointer,
    PointerEvent, RealTimeClock, Serial,
};
use feos_sync::SpinLock;
use feos_virtio_core::{device, AnyTransport, MmioTransport, PciTransport, Transport};

/// Human-readable board name, shown by the `info` app.
pub const NAME: &str = "QEMU aarch64 virt";

/// Architecture reported by `uname -m`.
pub const ARCH: &str = "aarch64";

/// Name of the serial device, used by the boot log.
pub const CONSOLE_NAME: &str = "PL011";

/// Name of the pointer device family, used by the boot log.
pub const POINTER_NAME: &str = "virtio";
/// Hint shown when no pointer was found.
pub const POINTER_HINT: &str = "start QEMU with -device virtio-tablet-device";

/// Name of the keyboard device family, used by the boot log.
pub const KEYBOARD_NAME: &str = "virtio";
/// Hint shown when no keyboard was found.
pub const KEYBOARD_HINT: &str = "serial console is the keyboard";

/// PL011 UART0 on the `virt` machine.
pub const CONSOLE_BASE: usize = 0x0900_0000;

/// GIC distributor on the `virt` machine, when the device tree doesn't say.
pub const GIC_DIST_BASE: usize = 0x0800_0000;

/// GICv2 CPU interface, when the device tree doesn't say.
pub const GIC_CPU_BASE: usize = 0x0801_0000;

/// Non-secure EL1 physical timer PPI (16 + 14).
///
/// QEMU's default `virt` configuration has no GIC security extensions, so the
/// physical timer is PPI 30 and is acknowledged as a Group 0 interrupt.
pub const TIMER_IRQ: u32 = 30;

/// SGI that stops a CPU for good.
pub const IPI_STOP: u32 = 0;

/// SGI that asks a CPU to reschedule.
pub const IPI_RESCHEDULE: u32 = 1;

/// CPUs the board can bring up.
///
/// QEMU's GICv2 `virt` machine supports up to eight CPUs, and the kernel keeps to
/// that with a GICv3 too; pass `-smp N` to boot fewer. The architecture port's
/// secondary-entry stacks match this.
pub const MAX_CPUS: usize = 8;

/// fw_cfg MMIO window on the `virt` machine.
pub const FW_CFG_BASE: usize = 0x0902_0000;

/// PCIe ECAM (configuration space) window on the `virt` machine.
///
/// QEMU places the high ECAM region at 256 GiB + 16 MiB.
pub const PCIE_ECAM_BASE: usize = 0x40_1000_0000;

/// First virtio MMIO slot on the `virt` machine.
pub const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;

/// Distance between virtio MMIO slots.
pub const VIRTIO_MMIO_STRIDE: usize = 0x200;

/// Number of virtio MMIO slots exposed by QEMU's `virt` machine.
pub const VIRTIO_MMIO_SLOTS: usize = 32;

/// Address assigned to BAR0 of the Intel HDA controller.
///
/// This lies in the low PCIe MMIO window (`0x1000_0000..0x3eff_0000`) and is
/// 16 KiB aligned, matching the controller's BAR size. Firmware-less boots
/// leave BARs unprogrammed, so the board assigns this address itself.
pub const HDA_BAR0: usize = 0x1000_0000;

/// The range the kernel hands out for PCI BARs: the rest of the low PCIe MMIO
/// window, past the HDA controller's fixed address.
pub const PCI_MEMORY_START: u64 = 0x1001_0000;
/// End of the low PCIe MMIO window.
pub const PCI_MEMORY_END: u64 = 0x3eff_0000;

/// PL031 real-time clock.
pub const RTC_BASE: usize = 0x0901_0000;

/// Intel vendor ID.
const PCI_VENDOR_INTEL: u16 = 0x8086;
/// ICH6 HD Audio controller device ID.
const PCI_DEVICE_ICH6_HDA: u16 = 0x2668;

/// Start of guest RAM.
pub const RAM_BASE: usize = 0x4000_0000;

/// Where the linker script loads the kernel image, past the device tree.
pub const KERNEL_LOAD_ADDR: usize = 0x4020_0000;

/// The single UART exposed by the machine.
pub static UART0: Pl011 = Pl011::new(CONSOLE_BASE);

/// Whichever GIC the machine has: `gic-version=3` is QEMU's option, and the device
/// tree says which it was started with.
pub struct Gic {
    v3: AtomicBool,
    v2: GicV2,
    gicv3: GicV3,
}

impl Gic {
    fn active(&self) -> &dyn InterruptController {
        if self.is_v3() {
            &self.gicv3
        } else {
            &self.v2
        }
    }

    fn is_v3(&self) -> bool {
        self.v3.load(Ordering::Relaxed)
    }

    fn init(&self) {
        if self.is_v3() {
            self.gicv3.init();
        } else {
            self.v2.init();
        }
    }

    fn init_cpu(&self) {
        if self.is_v3() {
            self.gicv3.init_cpu();
        } else {
            self.v2.init_cpu();
        }
    }
}

impl InterruptController for Gic {
    fn enable(&self, irq: u32) {
        self.active().enable(irq);
    }
    fn disable(&self, irq: u32) {
        self.active().disable(irq);
    }
    fn acknowledge(&self) -> u32 {
        self.active().acknowledge()
    }
    fn end(&self, irq: u32) {
        self.active().end(irq);
    }
    fn claim(&self) -> IrqClaim {
        self.active().claim()
    }
    fn complete(&self, claim: IrqClaim) {
        self.active().complete(claim);
    }
    fn is_spurious(&self, id: u32) -> bool {
        self.active().is_spurious(id)
    }
    fn configure(&self, irq: u32, trigger: IrqTrigger, cpu: usize) {
        self.active().configure(irq, trigger, cpu);
    }
    fn send_ipi(&self, target: IpiTarget, id: u32) {
        self.active().send_ipi(target, id);
    }
    fn send_stop_nmi(&self) -> bool {
        self.active().send_stop_nmi()
    }
}

/// The machine's GIC, a GICv2 until the device tree says otherwise.
pub static INTERRUPT_CONTROLLER: Gic = Gic {
    v3: AtomicBool::new(false),
    v2: GicV2::new(GIC_DIST_BASE, GIC_CPU_BASE),
    gicv3: GicV3::new(),
};

/// What the device tree told the boot CPU. Written once, before anything else runs.
static RAM: SpinLock<RegionList> = SpinLock::new(RegionList::new());

/// CPUs in the device tree, or as many as can be asked for without one: PSCI says
/// which of those don't exist.
static CPU_COUNT: AtomicUsize = AtomicUsize::new(MAX_CPUS);

/// Where QEMU leaves the tree when it starts a bare ELF rather than a Linux image.
const FDT_FALLBACK: usize = RAM_BASE;

/// The device tree at `address`, if there's a good one, with a limit on where it can
/// be. This runs before the kernel knows where RAM ends, so the address must be
/// somewhere known to be RAM.
fn device_tree_within(address: usize, limit: usize) -> Option<Fdt<'static>> {
    if !(RAM_BASE..limit).contains(&address) || address % 8 != 0 {
        return None;
    }
    // SAFETY: the address is in RAM, by the limit, and these eight bytes are inside it.
    let start: [u8; 8] = unsafe { core::ptr::read_volatile(address as *const [u8; 8]) };
    let size = feos_firmware::fdt::header(&start)?.total_size as usize;
    if size < 8 {
        return None;
    }
    // SAFETY: the tree is what QEMU or firmware put there. The kernel never writes below
    // its own image and takes nothing firmware didn't report free, so it stays put. The
    // first word was just read, and the size limit in `header` bounds this.
    let blob = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
    Fdt::new(blob)
}

/// The device tree at `address`, in the block QEMU boots from: the first RAM it has
/// to have for this code to be running.
fn device_tree(address: usize) -> Option<Fdt<'static>> {
    device_tree_within(address, KERNEL_LOAD_ADDR)
}

/// How the device tree says to reach PSCI.
static CONDUIT: AtomicU8 = AtomicU8::new(FirmwareConduit::Unknown as u8);

/// Addresses the boot page tables map as Normal memory.
const MAPPED_RAM_END: usize = 0x1_0000_0000;

/// Reads the device tree QEMU passed, or left at the start of RAM. The EFI loader
/// passes a handoff instead. Firmware gives a device tree there if it has one, and
/// with ACPI it doesn't, but QEMU's is still at the start of RAM: this is a QEMU board.
pub fn init_boot_info(boot_info: usize) {
    // SAFETY: only read if it's in the RAM the boot tables map as such. What's at the
    // address is either a handoff, whole, or something else that's still readable.
    let handoff = (RAM_BASE..MAPPED_RAM_END - 8)
        .contains(&boot_info)
        .then(|| unsafe { Handoff::at(boot_info) })
        .flatten();
    let tree = match handoff {
        Some(handoff) => {
            display::set_firmware_framebuffer(handoff.framebuffer);
            let mut ram = RegionList::new();
            for range in handoff.ram() {
                ram.push(Region { base: range.base, size: range.size });
            }
            *RAM.lock() = ram;
            if handoff.device_tree == 0 {
                println!("[boot] firmware gave ACPI ({:#x}) and no device tree", handoff.rsdp);
            }
            device_tree_within(handoff.device_tree as usize, MAPPED_RAM_END).or_else(|| device_tree(FDT_FALLBACK))
        }
        None => device_tree(boot_info).or_else(|| device_tree(FDT_FALLBACK)),
    };
    let Some(tree) = tree else {
        println!("[fdt] no device tree");
        return;
    };
    if handoff.is_none() {
        *RAM.lock() = tree.memory();
    }
    match tree.gic() {
        Some(FdtGic::V2 { v2m: Some(frame), .. }) => {
            V2M_FRAME.store(frame.base as usize, Ordering::Release);
        }
        Some(FdtGic::V3 { distributor, redistributor }) => {
            INTERRUPT_CONTROLLER
                .gicv3
                .set_bases(distributor.base as usize, redistributor.base as usize);
            INTERRUPT_CONTROLLER.v3.store(true, Ordering::Relaxed);
        }
        _ => {}
    }
    let conduit = match tree.psci_method() {
        Some(PsciMethod::Hvc) => FirmwareConduit::Hvc,
        Some(PsciMethod::Smc) => FirmwareConduit::Smc,
        None => FirmwareConduit::None,
    };
    CONDUIT.store(conduit as u8, Ordering::Release);
    let cpus = tree.cpu_count();
    if cpus != 0 {
        CPU_COUNT.store(cpus.min(MAX_CPUS), Ordering::Release);
    }
    println!(
        "[fdt] device tree: {} CPU(s), {} MiB of RAM",
        cpus,
        tree.memory().total() / (1024 * 1024)
    );
}

/// The GICv2m frame the device tree names, which turns PCI's memory writes into
/// SPIs. Zero when the machine doesn't have one.
static V2M_FRAME: AtomicUsize = AtomicUsize::new(0);
/// `MSI_TYPER`: the frame's first SPI in bits 25:16, its count in bits 9:0.
const V2M_TYPER: usize = 0x008;
/// Where a device writes an SPI number to raise it.
const V2M_SETSPI: u64 = 0x040;
/// SPIs of the frame handed out so far.
static V2M_USED: AtomicU32 = AtomicU32::new(0);

/// virtio-mmio slots' interrupts: SPI 16 up, an INTID 32 above that.
const VIRTIO_MMIO_FIRST_IRQ: u32 = 32 + 16;
/// PCI's four wired interrupts: SPI 3 up, rotated by slot.
const PCI_FIRST_IRQ: u32 = 32 + 3;

struct Router;

impl IrqRouter for Router {
    fn alloc_msi(&self) -> Option<(MsiMessage, u32)> {
        let frame = V2M_FRAME.load(Ordering::Acquire);
        if frame == 0 {
            return None;
        }
        // SAFETY: the device tree put the frame here, in the device-mapped first GiB.
        let typer = unsafe { core::ptr::read_volatile((frame + V2M_TYPER) as *const u32) };
        let first = (typer >> 16) & 0x3ff;
        let count = typer & 0x3ff;
        let index = V2M_USED.fetch_add(1, Ordering::Relaxed);
        if index >= count {
            return None;
        }
        let id = first + index;
        INTERRUPT_CONTROLLER.configure(id, IrqTrigger::Edge, 0);
        INTERRUPT_CONTROLLER.enable(id);
        let address = frame as u64 + V2M_SETSPI;
        Some((MsiMessage { address, data: id }, id))
    }

    fn pci_intx(&self, bus: u8, slot: u8, pin: u8) -> Option<u32> {
        // Only the root bus: behind a bridge the pin is rotated by a slot number
        // up the tree, and MSI-X is what those devices get on this machine.
        if bus != 0 {
            return None;
        }
        let id = PCI_FIRST_IRQ + (u32::from(slot) + u32::from(pin) - 1) % 4;
        self.enable_line(id);
        Some(id)
    }

    fn enable_line(&self, id: u32) {
        INTERRUPT_CONTROLLER.configure(id, IrqTrigger::Level, 0);
        INTERRUPT_CONTROLLER.enable(id);
    }
}

/// Initializes the GIC and enables the timer interrupt on the boot CPU.
pub fn init_interrupts() {
    INTERRUPT_CONTROLLER.init();
    enable_cpu_lines();
}

/// Enables the lines each CPU banks for itself.
fn enable_cpu_lines() {
    INTERRUPT_CONTROLLER.enable(TIMER_IRQ);
    INTERRUPT_CONTROLLER.enable(IPI_STOP);
    INTERRUPT_CONTROLLER.enable(IPI_RESCHEDULE);
}

/// Initializes the calling CPU's GIC interface and timer PPI.
pub fn init_cpu_interrupts() {
    INTERRUPT_CONTROLLER.init_cpu();
    enable_cpu_lines();
}

/// Returns the interrupt controller shared with the kernel IRQ handler.
pub fn interrupts() -> &'static dyn InterruptController {
    &INTERRUPT_CONTROLLER
}

/// PSCI target affinity for a logical CPU index.
///
/// QEMU's GICv2 `virt` machine places all CPUs in affinity level 0, so the
/// MPIDR value is simply the index. A board with clustered CPUs would encode
/// its cluster/core split here instead.
pub fn cpu_mpidr(index: usize) -> u64 {
    index as u64
}

/// The virtio-input pointer discovered at boot.
///
/// QEMU exposes the tablet as a virtio MMIO device. The driver keeps its
/// virtqueue in this static so the session loop can poll it without
/// allocation.
pub static POINTER: SpinLock<VirtioInput> = SpinLock::new(VirtioInput::new());

struct BoardPointer;

impl Pointer for BoardPointer {
    fn poll(&self) -> Option<PointerEvent> {
        POINTER.lock().poll()
    }

    fn position(&self) -> (u16, u16) {
        POINTER.lock().position()
    }

    fn set_screen(&self, width: u16, height: u16) {
        POINTER.lock().set_screen(width, height);
    }
}

static BOARD_POINTER: BoardPointer = BoardPointer;

/// Finds and initialises the virtio pointer, if QEMU provided one.
pub fn init_pointer() -> Option<PointerKind> {
    let (base, kind) =
        feos_driver_virtio_input::find(VIRTIO_MMIO_BASE, VIRTIO_MMIO_STRIDE, VIRTIO_MMIO_SLOTS)?;
    let mut pointer = POINTER.lock();
    let (width, height) = display_size();
    if pointer.init(base, width as u16, height as u16) {
        Some(kind)
    } else {
        None
    }
}

/// Returns the pointer shared with apps, when one was initialised.
pub fn pointer() -> Option<&'static dyn Pointer> {
    let own: Option<&'static dyn Pointer> = if POINTER.lock().is_ready() { Some(&BOARD_POINTER) } else { None };
    usb::pointer(own)
}

/// The virtio keyboard discovered at boot.
pub static KEYBOARD: SpinLock<VirtioInput> = SpinLock::new(VirtioInput::new());

struct BoardKeyboard;

impl Keyboard for BoardKeyboard {
    fn poll(&self) -> Option<KeyboardEvent> {
        KEYBOARD.lock().poll_key()
    }
}

static BOARD_KEYBOARD: BoardKeyboard = BoardKeyboard;

/// Finds and initialises the virtio keyboard, if QEMU provided one.
pub fn init_keyboard() -> bool {
    let Some(base) = feos_driver_virtio_input::find_keyboard(
        VIRTIO_MMIO_BASE,
        VIRTIO_MMIO_STRIDE,
        VIRTIO_MMIO_SLOTS,
    ) else {
        return false;
    };
    KEYBOARD.lock().init(base, 1, 1)
}

/// Returns the keyboard shared with apps, when one was initialised.
pub fn keyboard() -> Option<&'static dyn Keyboard> {
    let own: Option<&'static dyn Keyboard> = if KEYBOARD.lock().is_ready() { Some(&BOARD_KEYBOARD) } else { None };
    usb::keyboard(own)
}

feos_board_common::wire_console!(UART0);

/// Brings up a virtio-gpu if the machine has one, and ramfb otherwise.
pub fn init_display() -> Option<Canvas<'static>> {
    display::init(FwCfg::new(FW_CFG_BASE), find_gpu)
}

/// Wall-clock seconds since the Unix epoch, from the PL031.
pub fn unix_time() -> Option<u64> {
    Pl031::new(RTC_BASE).unix_time()
}

/// Finds a virtio-gpu: MMIO slots first, then PCI functions.
fn find_gpu() -> Option<AnyTransport> {
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let base = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        if let Some(transport) = MmioTransport::probe(base) {
            // virtio-gpu has no legacy form, so a legacy slot can't be one that works.
            if transport.device_id() == device::GPU && transport.version() == 2 {
                return Some(AnyTransport::Mmio(transport));
            }
        }
    }
    let pci = PciEcam::new(PCIE_ECAM_BASE);
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

/// Brings up every virtio block device: MMIO slots first, then PCI functions.
fn block_devices() -> alloc::vec::Vec<VirtioBlk<AnyTransport>> {
    let mut disks = alloc::vec::Vec::new();
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let Some(transport) = MmioTransport::probe(VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE)
        else {
            continue;
        };
        if transport.device_id() != device::BLOCK {
            continue;
        }
        match VirtioBlk::new(AnyTransport::Mmio(transport), feos_sync::yield_now) {
            Ok(mut disk) => {
                let id = VIRTIO_MMIO_FIRST_IRQ + slot as u32;
                let how = wire_mmio_disk(&mut disk, transport, id, &Router);
                println!("[disk] virtio-mmio slot {slot}: {how}");
                disks.push(disk);
            }
            Err(error) => println!("[disk] virtio-mmio slot {slot}: {error:?}"),
        }
    }

    let pci = PciEcam::new(PCIE_ECAM_BASE);
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
    let pci = PciEcam::new(PCIE_ECAM_BASE);
    let mut allocator = BarAllocator::new(PCI_MEMORY_START, PCI_MEMORY_END);
    allocator.reserve_programmed(pci);
    configure_bridges(pci, &mut allocator);
    let (width, height) = display_size();
    usb::init(pci, &mut allocator, &Router, clock, (width as u16, height as u16));
}

/// Finds the first virtio network card: MMIO slots first, then PCI functions.
pub fn init_network() -> Option<NetBinding> {
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let Some(transport) = MmioTransport::probe(VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE) else {
            continue;
        };
        if transport.device_id() == device::NET {
            return bind_mmio_net(transport, VIRTIO_MMIO_FIRST_IRQ + slot as u32, &Router);
        }
    }
    let pci = PciEcam::new(PCIE_ECAM_BASE);
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
///
/// Returns `None` when QEMU was started without `-device intel-hda` (or the
/// codec does not answer). The board assigns BAR0 itself because the kernel
/// boots without PCI firmware.
pub fn init_audio(clock: &'static dyn Clock) -> Option<AudioDevice> {
    let pci = PciEcam::new(PCIE_ECAM_BASE);
    let controller = pci.find(PCI_VENDOR_INTEL, PCI_DEVICE_ICH6_HDA)?;

    // Firmware may have been here before: its address stands, with other devices
    // placed around it. Without firmware, nothing's been placed anywhere.
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
pub struct QemuVirt;

impl Board for QemuVirt {
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
    /// The boot page tables map RAM as Normal memory up to here.
    const RAM_LIMIT: u64 = 0x1_0000_0000;

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

    fn firmware_conduit() -> FirmwareConduit {
        match CONDUIT.load(Ordering::Acquire) {
            value if value == FirmwareConduit::Hvc as u8 => FirmwareConduit::Hvc,
            value if value == FirmwareConduit::Smc as u8 => FirmwareConduit::Smc,
            value if value == FirmwareConduit::None as u8 => FirmwareConduit::None,
            _ => FirmwareConduit::Unknown,
        }
    }

    fn cpu_count() -> usize {
        CPU_COUNT.load(Ordering::Acquire)
    }

    fn usable_ram(out: &mut [(u64, u64)]) -> usize {
        let ram = RAM.lock();
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
        if INTERRUPT_CONTROLLER.is_v3() {
            "GICv3"
        } else {
            "GICv2"
        }
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
