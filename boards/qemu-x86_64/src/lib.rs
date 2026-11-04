//! Board support package for QEMU's x86_64 `q35` machine.
//!
//! The board owns the machine-specific facts: the COM1 port, APIC/IOAPIC
//! addresses, the PCI configuration transport, the scanout buffer, and the
//! wiring between drivers and kernel libraries. The kernel itself never
//! mentions a physical address.

#![no_std]

extern crate alloc;

use feos_console::println;
use feos_driver_apic::ApicController;
use feos_driver_fw_cfg::FwCfg;
use feos_driver_i8042::{PointerKind, I8042};
use feos_driver_intel_hda::{CodecInfo, IntelHda};
use feos_driver_pci::PciPort;
use feos_driver_ramfb::Ramfb;
use feos_driver_uart16550::Uart16550;
use feos_fs::{FileSystem, MemoryBlockDevice, MountTable, StaticFs};
use feos_gfx::Canvas;
use feos_hal::{Audio, Board, Clock, InterruptController, Keyboard, Pointer, Serial};
use feos_sync::SpinLock;

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

/// Vector the PIT is routed to.
///
/// The PIT is connected to GSI 2, so vector `32 + 2 = 34`.
pub const TIMER_IRQ: u32 = 34;

/// Vector of the IPI that stops a CPU for good.
pub const IPI_STOP: u32 = 49;

/// Vector of the IPI that asks a CPU to reschedule.
pub const IPI_RESCHEDULE: u32 = 50;

/// CPUs the board can bring up.
///
/// The current x86_64 port runs on the boot processor; the scheduler still
/// reports its normal per-core accounting for that one CPU.
pub const MAX_CPUS: usize = 1;

/// Address assigned to BAR0 of the Intel HDA controller when firmware left it
/// unprogrammed.
///
/// This lies in the PCI memory hole below 4 GiB, just above QEMU's q35 ECAM
/// window (`0xb000_0000..0xc000_0000`), and is 16 KiB aligned, matching the
/// controller's BAR size. SeaBIOS assigns BARs when it loads the kernel, so
/// its choice is normally kept: it knows about the machine's other devices.
pub const HDA_BAR0: usize = 0xc000_0000;

/// Intel vendor ID.
const PCI_VENDOR_INTEL: u16 = 0x8086;
/// ICH6 HD Audio controller device ID.
const PCI_DEVICE_ICH6_HDA: u16 = 0x2668;

/// Start of guest RAM.
pub const RAM_BASE: usize = 0x0010_0000;

/// Width of the ramfb scanout buffer.
pub const DISPLAY_WIDTH: usize = 1024;

/// Height of the ramfb scanout buffer.
pub const DISPLAY_HEIGHT: usize = 768;

/// The single UART exposed by the machine.
pub static UART0: Uart16550 = Uart16550::new(CONSOLE_BASE as u16);

/// The APIC pair used by the timer tick.
pub static INTERRUPT_CONTROLLER: ApicController = ApicController::new(LAPIC_BASE, IOAPIC_BASE);

/// Initializes the APIC and enables the timer vector.
pub fn init_interrupts() {
    INTERRUPT_CONTROLLER.init();
    INTERRUPT_CONTROLLER.enable(TIMER_IRQ);
}

/// Initializes the calling CPU's interrupt state.
///
/// The current port only runs the boot processor, so this is a no-op retained
/// for the kernel's generic SMP path.
pub fn init_cpu_interrupts() {}

/// Returns the interrupt controller shared with the kernel IRQ handler.
pub fn interrupts() -> &'static dyn InterruptController {
    &INTERRUPT_CONTROLLER
}

/// Firmware CPU affinity for a logical CPU index.
pub fn cpu_mpidr(index: usize) -> u64 {
    index as u64
}

/// The PS/2 controller shared by the keyboard and pointer.
pub static INPUT: I8042 = I8042::new();

/// Finds and initialises the PS/2 pointer.
pub fn init_pointer() -> Option<PointerKind> {
    if INPUT.init(DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16) {
        Some(INPUT.pointer_kind())
    } else {
        None
    }
}

/// Returns the pointer shared with apps, when one was initialised.
pub fn pointer() -> Option<&'static dyn Pointer> {
    if INPUT.is_ready() {
        Some(&INPUT)
    } else {
        None
    }
}

/// Finds and initialises the PS/2 keyboard.
pub fn init_keyboard() -> bool {
    INPUT.init(DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16)
}

/// Returns the keyboard shared with apps, when one was initialised.
pub fn keyboard() -> Option<&'static dyn Keyboard> {
    if INPUT.is_ready() {
        Some(&INPUT)
    } else {
        None
    }
}

/// The ramfb scanout buffer. Page aligned as required by the device.
#[repr(align(4096))]
struct ScanoutBuffer([u32; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

static mut SCANOUT: ScanoutBuffer = ScanoutBuffer([0; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

/// Off-screen buffer used by the desktop to compose a frame before presenting.
#[repr(align(4096))]
struct BackBuffer([u32; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

static mut BACK: BackBuffer = BackBuffer([0; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

/// Physical address of the scanout buffer.
pub fn framebuffer_addr() -> usize {
    // SAFETY: taking the address of a `static mut` does not create a
    // reference; the buffer is only exposed once through `init_display`.
    unsafe { core::ptr::addr_of_mut!(SCANOUT.0) as *mut u32 as usize }
}

/// Returns a canvas over the off-screen composition buffer.
pub fn back_buffer() -> Option<Canvas<'static>> {
    // SAFETY: `BACK` is a static, page-aligned buffer that is only handed out
    // here. The returned canvas owns the only reference for the rest of the
    // program; the desktop is the sole foreground consumer.
    let ptr = unsafe { core::ptr::addr_of_mut!(BACK.0).cast::<u32>() };
    let pixels: &'static mut [u32] =
        unsafe { core::slice::from_raw_parts_mut(ptr, DISPLAY_WIDTH * DISPLAY_HEIGHT) };
    Canvas::new(pixels, DISPLAY_WIDTH, DISPLAY_HEIGHT, DISPLAY_WIDTH)
}

/// Initializes the UART and registers it as the kernel console.
pub fn init_console() {
    UART0.init();
    feos_console::set_writer(console_writer);
    feos_console::set_byte_writer(console_byte_writer);
    feos_console::set_reader(console_reader);
}

fn console_writer(text: &str) {
    UART0.write_terminal(text);
}

fn console_byte_writer(bytes: &[u8]) {
    for &byte in bytes {
        UART0.write_byte(byte);
    }
}

fn console_reader() -> Option<u8> {
    UART0.read_byte()
}

/// Brings up ramfb and returns a canvas over the static scanout buffer.
pub fn init_display() -> Option<Canvas<'static>> {
    let ramfb = Ramfb::new(FwCfg::new_io(FW_CFG_IO_BASE));
    if !ramfb.configure(
        framebuffer_addr() as u64,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
        DISPLAY_WIDTH,
    ) {
        return None;
    }

    // SAFETY: taking the address of a `static mut` does not create a
    // reference; the buffer is only exposed once, here.
    let ptr = unsafe { core::ptr::addr_of_mut!(SCANOUT.0).cast::<u32>() };
    // SAFETY: `SCANOUT` is a static, page-aligned buffer that is only handed
    // out here. The returned canvas owns the only reference for the rest of
    // the program; only the foreground session draws to it.
    let pixels: &'static mut [u32] =
        unsafe { core::slice::from_raw_parts_mut(ptr, DISPLAY_WIDTH * DISPLAY_HEIGHT) };

    Canvas::new(pixels, DISPLAY_WIDTH, DISPLAY_HEIGHT, DISPLAY_WIDTH)
}

mod fs_image {
    include!(concat!(env!("OUT_DIR"), "/fs_image.rs"));
}

/// The compiled-in read-only filesystem.
static FILESYSTEM: StaticFs = StaticFs::new(fs_image::FS_IMAGE);

/// Cached result of the optional disk-image probe.
static MOUNTED_DISK: SpinLock<Option<&'static dyn FileSystem>> =
    SpinLock::new(None);

/// Returns the filesystem apps browse through `Platform::filesystem`.
pub fn filesystem() -> &'static dyn FileSystem {
    if let Some(filesystem) = *MOUNTED_DISK.lock() {
        return filesystem;
    }

    let mut table = MountTable::new();
    if let Some(image) = fs_image::DISK_FAT32_IMAGE {
        match feos_fs::mount(MemoryBlockDevice::new(image)) {
            Ok(fs) => {
                if table.mount("fat32", fs).is_ok() {
                    println!("[fs] FAT32 volume mounted at /fat32");
                }
            }
            Err(error) => println!("[fs] FAT32 image rejected: {error}"),
        }
    }
    if let Some(image) = fs_image::DISK_EXFAT_IMAGE {
        match feos_fs::mount(MemoryBlockDevice::new(image)) {
            Ok(fs) => {
                if table.mount("exfat", fs).is_ok() {
                    println!("[fs] exFAT volume mounted at /exfat");
                }
            }
            Err(error) => println!("[fs] exFAT image rejected: {error}"),
        }
    }

    let filesystem: &'static dyn FileSystem = if !table.is_empty() {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(table))
    } else if let Some(image) = fs_image::DISK_IMAGE {
        match feos_fs::mount(MemoryBlockDevice::new(image)) {
            Ok(filesystem) => {
                println!("[fs] disk image mounted at /");
                alloc::boxed::Box::leak(filesystem)
            }
            Err(error) => {
                println!("[fs] disk image rejected: {error}");
                &FILESYSTEM
            }
        }
    } else {
        &FILESYSTEM
    };
    *MOUNTED_DISK.lock() = Some(filesystem);
    filesystem
}

/// The board's Intel HDA controller.
pub static AUDIO: IntelHda = IntelHda::new();

/// A discovered audio device and its codec details.
pub struct AudioDevice {
    /// Playback interface handed to apps.
    pub device: &'static dyn Audio,
    /// Codec nodes found during initialisation.
    pub codec: CodecInfo,
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

    let codec = AUDIO.init(bar, clock)?;
    Some(AudioDevice {
        device: &AUDIO,
        codec,
    })
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

/// This board, as the kernel sees it.
pub struct QemuX86_64;

impl Board for QemuX86_64 {
    const NAME: &'static str = NAME;
    const ARCH: &'static str = ARCH;
    const CONSOLE_NAME: &'static str = CONSOLE_NAME;
    const CONSOLE_BASE: usize = CONSOLE_BASE;
    const INTERRUPT_CONTROLLER_NAME: &'static str = INTERRUPT_CONTROLLER_NAME;
    const POINTER_NAME: &'static str = POINTER_NAME;
    const POINTER_HINT: &'static str = POINTER_HINT;
    const KEYBOARD_NAME: &'static str = KEYBOARD_NAME;
    const KEYBOARD_HINT: &'static str = KEYBOARD_HINT;
    const TIMER_IRQ: u32 = TIMER_IRQ;
    const IPI_STOP: u32 = IPI_STOP;
    const IPI_RESCHEDULE: u32 = IPI_RESCHEDULE;
    const MAX_CPUS: usize = MAX_CPUS;

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

    fn init_interrupts() {
        init_interrupts();
    }

    fn init_cpu_interrupts() {
        init_cpu_interrupts();
    }

    fn interrupts() -> &'static dyn InterruptController {
        interrupts()
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

    fn filesystem() -> &'static dyn FileSystem {
        filesystem()
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
