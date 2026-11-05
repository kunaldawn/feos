//! What the QEMU boards have in common, once the machine-specific facts are
//! behind [`IrqRouter`].

#![no_std]

extern crate alloc;

pub mod audio;
pub mod display;
pub mod usb;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;
use feos_console::println;
use feos_driver_intel_hda::IntelHda;
use feos_driver_pci::{ConfigSpace, MsiMessage, PciDevice};
use feos_driver_virtio_blk::VirtioBlk;
use feos_driver_virtio_net::VirtioNet;
use feos_fs::vfs::{assemble, Embedded};
use feos_fs::FileSystem;
use feos_hal::net::NetBinding;
use feos_sync::{futex_wake, SpinLock};
use feos_virtio_core::{AnyTransport, MmioTransport, PciTransport, Transport};

/// Defines a board's console functions over its one UART, which must be a static with
/// a `write_terminal` method and implement `feos_hal::Serial`.
#[macro_export]
macro_rules! wire_console {
    ($uart:ident) => {
        /// Initializes the UART and registers it as the kernel console.
        pub fn init_console() {
            $uart.init();
            feos_console::set_writer(console_writer);
            feos_console::set_byte_writer(console_byte_writer);
            feos_console::set_reader(console_reader);
        }

        fn console_writer(text: &str) {
            $uart.write_terminal(text);
        }

        fn console_byte_writer(bytes: &[u8]) {
            for &byte in bytes {
                $uart.write_byte(byte);
            }
        }

        fn console_reader() -> Option<u8> {
            $uart.read_byte()
        }
    };
}

extern "C" {
    static __kernel_start: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __kernel_end: u8;
}

/// Address range `[start, end)` of the loaded kernel image.
pub fn kernel_range() -> (usize, usize) {
    (
        core::ptr::addr_of!(__kernel_start) as usize,
        core::ptr::addr_of!(__kernel_end) as usize,
    )
}

/// Address range `[start, end)` of `.bss`.
pub fn bss_range() -> (usize, usize) {
    (
        core::ptr::addr_of!(__bss_start) as usize,
        core::ptr::addr_of!(__bss_end) as usize,
    )
}

/// Address range `[start, end)` of the boot stack.
pub fn stack_range() -> (usize, usize) {
    (
        core::ptr::addr_of!(__stack_bottom) as usize,
        core::ptr::addr_of!(__stack_top) as usize,
    )
}

/// Cached result of probing storage.
static MOUNTED: SpinLock<Option<&'static dyn FileSystem>> = SpinLock::new(None);

/// The filesystem apps browse. The first call probes `disks` and assembles the
/// shared tree: see [`assemble`].
pub fn mount_filesystem(
    embedded: impl FnOnce() -> Embedded,
    disks: impl FnOnce() -> Vec<VirtioBlk<AnyTransport>>,
    clock: fn() -> Option<u64>,
) -> &'static dyn FileSystem {
    if let Some(filesystem) = *MOUNTED.lock() {
        return filesystem;
    }
    let (filesystem, log) = assemble(embedded(), disks(), usb::take_disks(), Some(clock));
    for line in log {
        println!("{line}");
    }
    let filesystem: &'static dyn FileSystem = filesystem;
    *MOUNTED.lock() = Some(filesystem);
    filesystem
}

/// How one machine delivers device interrupts.
pub trait IrqRouter {
    /// A free message-signalled interrupt: what a device writes to raise it, and
    /// the interrupt ID it arrives as. `None` when the machine has no way to take
    /// one or has run out.
    fn alloc_msi(&self) -> Option<(MsiMessage, u32)>;

    /// Sets up the wired line of a PCI function's pin and returns its interrupt
    /// ID. `None` when this board doesn't know where that pin goes.
    fn pci_intx(&self, bus: u8, slot: u8, pin: u8) -> Option<u32>;

    /// Sets up a wired, level-triggered line by its interrupt ID.
    fn enable_line(&self, id: u32);
}

fn audio_interrupt(data: usize) {
    // SAFETY: the data registered with this handler is the board's static controller.
    let audio = unsafe { &*(data as *const IntelHda) };
    audio.handle_interrupt();
}

/// Gives the audio controller a plain MSI, for its codec responses. Playback is
/// polled and goes on regardless. Returns whether it worked.
pub fn wire_audio<C: ConfigSpace>(
    audio: &'static IntelHda,
    function: &PciDevice<C>,
    router: &impl IrqRouter,
) -> bool {
    if !function.has_msi() {
        return false;
    }
    let Some((message, id)) = router.alloc_msi() else {
        return false;
    };
    feos_irq::register(id, "intel-hda msi", audio_interrupt, audio as *const IntelHda as usize)
        && function.enable_msi(message)
        && audio.enable_response_interrupts()
}

/// What a disk's interrupt handler needs. Leaked: devices never leave.
struct DiskInterrupt {
    transport: AnyTransport,
    /// Moved on each interrupt, which is what the driver waits on.
    events: AtomicU32,
}

fn disk_interrupt(data: usize) {
    // SAFETY: the data registered with this handler is a leaked `DiskInterrupt`.
    let interrupt = unsafe { &*(data as *const DiskInterrupt) };
    // A message needs no acknowledgement, and doing it anyway is harmless. A shared
    // line's other devices can land here too: nothing's waiting, so that costs a
    // wake-up that finds no one.
    interrupt.transport.acknowledge_interrupt();
    interrupt.events.fetch_add(1, Ordering::Release);
    futex_wake(&interrupt.events, usize::MAX);
}

fn attach(disk: &mut VirtioBlk<AnyTransport>, transport: AnyTransport, id: u32, name: &'static str) -> bool {
    let interrupt: &'static DiskInterrupt = Box::leak(Box::new(DiskInterrupt {
        transport,
        events: AtomicU32::new(0),
    }));
    if !feos_irq::register(id, name, disk_interrupt, interrupt as *const DiskInterrupt as usize) {
        return false;
    }
    disk.use_interrupts(&interrupt.events);
    true
}

/// Gives a disk on a virtio-mmio slot its interrupt line, by ID.
pub fn wire_mmio_disk(
    disk: &mut VirtioBlk<AnyTransport>,
    transport: MmioTransport,
    id: u32,
    router: &impl IrqRouter,
) -> &'static str {
    router.enable_line(id);
    if attach(disk, AnyTransport::Mmio(transport), id, "virtio-blk mmio") {
        "line"
    } else {
        "polled"
    }
}

/// Gives a disk on PCI the best interrupt its function and the machine can agree
/// on: MSI-X, then its wired line. Returns which it got. Without one, the disk
/// goes on being polled, which also works.
pub fn wire_pci_disk<C: ConfigSpace>(
    disk: &mut VirtioBlk<AnyTransport>,
    function: &PciDevice<C>,
    transport: PciTransport,
    router: &impl IrqRouter,
) -> &'static str {
    let any = AnyTransport::Pci(transport);
    if let Some(table) = transport.msix_table() {
        if let Some((message, id)) = router.alloc_msi() {
            if function.enable_msix(table, 0, message)
                && transport.set_queue_vector(0, 0)
                && attach(disk, any, id, "virtio-blk msi-x")
            {
                transport.clear_config_vector();
                return "MSI-X";
            }
            function.disable_msix();
        }
    }
    if let Some(pin) = function.interrupt_pin() {
        if let Some(id) = router.pci_intx(function.bus(), function.slot(), pin) {
            function.enable_intx();
            if attach(disk, any, id, "virtio-blk intx") {
                return "INTx";
            }
        }
    }
    "polled"
}

/// What a card's interrupt handler needs. Leaked: devices never leave.
struct NetInterrupt {
    transport: AnyTransport,
    events: AtomicU32,
}

fn net_interrupt(data: usize) {
    // SAFETY: the data registered with this handler is a leaked `NetInterrupt`.
    let interrupt = unsafe { &*(data as *const NetInterrupt) };
    interrupt.transport.acknowledge_interrupt();
    interrupt.events.fetch_add(1, Ordering::Release);
    futex_wake(&interrupt.events, usize::MAX);
}

fn net_events(transport: AnyTransport) -> &'static NetInterrupt {
    Box::leak(Box::new(NetInterrupt {
        transport,
        events: AtomicU32::new(0),
    }))
}

fn bind_net(
    mut net: VirtioNet<AnyTransport>,
    interrupt: &'static NetInterrupt,
    how: &'static str,
) -> NetBinding {
    if how != "polled" {
        net.enable_interrupts();
    }
    NetBinding {
        device: Box::leak(Box::new(net)),
        events: &interrupt.events,
        how,
    }
}

/// Brings up the card in a virtio-mmio slot, with its interrupt line.
pub fn bind_mmio_net(transport: MmioTransport, id: u32, router: &impl IrqRouter) -> Option<NetBinding> {
    let any = AnyTransport::Mmio(transport);
    let net = VirtioNet::new(any).ok()?;
    let interrupt = net_events(any);
    router.enable_line(id);
    let how = if feos_irq::register(id, "virtio-net mmio", net_interrupt, interrupt as *const NetInterrupt as usize) {
        "line"
    } else {
        "polled"
    };
    Some(bind_net(net, interrupt, how))
}

/// Brings up a card on PCI, with the best interrupt it and the machine can agree on:
/// MSI-X, then its wired line. Without one, it's polled.
pub fn bind_pci_net<C: ConfigSpace>(
    function: &PciDevice<C>,
    transport: PciTransport,
    router: &impl IrqRouter,
) -> Option<NetBinding> {
    let any = AnyTransport::Pci(transport);
    let net = VirtioNet::new(any).ok()?;
    let interrupt = net_events(any);
    let data = interrupt as *const NetInterrupt as usize;
    if let Some(table) = transport.msix_table() {
        if let Some((message, id)) = router.alloc_msi() {
            if function.enable_msix(table, 0, message)
                && transport.set_queue_vector(0, 0)
                && transport.set_queue_vector(1, 0)
                && feos_irq::register(id, "virtio-net msi-x", net_interrupt, data)
            {
                transport.clear_config_vector();
                return Some(bind_net(net, interrupt, "MSI-X"));
            }
            function.disable_msix();
        }
    }
    if let Some(pin) = function.interrupt_pin() {
        if let Some(id) = router.pci_intx(function.bus(), function.slot(), pin) {
            function.enable_intx();
            if feos_irq::register(id, "virtio-net intx", net_interrupt, data) {
                return Some(bind_net(net, interrupt, "INTx"));
            }
        }
    }
    Some(bind_net(net, interrupt, "polled"))
}
