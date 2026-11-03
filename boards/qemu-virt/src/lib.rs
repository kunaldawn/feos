//! Board support package for the QEMU `virt` machine.
//!
//! The board owns the machine-specific facts: MMIO addresses, the RAM map, the
//! scanout buffer, and the wiring between drivers and kernel libraries. The
//! kernel itself never mentions a physical address.
//!
//! To support another machine, copy this crate to `boards/<name>`, adjust the
//! constants and devices, and select it in the kernel's `Cargo.toml`.

#![no_std]

use feos_driver_fw_cfg::FwCfg;
use feos_driver_gicv2::GicV2;
use feos_driver_intel_hda::{CodecInfo, IntelHda};
use feos_driver_pci::PciEcam;
use feos_driver_pl011::Pl011;
use feos_driver_ramfb::Ramfb;
use feos_driver_virtio_input::{PointerKind, VirtioInput};
use feos_fs::{FileSystem, StaticFs};
use feos_gfx::Canvas;
use feos_hal::{
    Audio, Clock, InterruptController, Keyboard, KeyboardEvent, Pointer, PointerEvent, Serial,
};
use feos_sync::SpinLock;

/// Human-readable board name, shown by the `info` app.
pub const NAME: &str = "QEMU aarch64 virt";

/// PL011 UART0 on the `virt` machine.
pub const CONSOLE_BASE: usize = 0x0900_0000;

/// GICv2 distributor on the `virt` machine.
pub const GIC_DIST_BASE: usize = 0x0800_0000;

/// GICv2 CPU interface for the boot CPU.
pub const GIC_CPU_BASE: usize = 0x0801_0000;

/// Non-secure EL1 physical timer PPI (16 + 14).
///
/// QEMU's default `virt` configuration has no GIC security extensions, so the
/// physical timer is PPI 30 and is acknowledged as a Group 0 interrupt.
pub const TIMER_IRQ: u32 = 30;

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

/// Intel vendor ID.
const PCI_VENDOR_INTEL: u16 = 0x8086;
/// ICH6 HD Audio controller device ID.
const PCI_DEVICE_ICH6_HDA: u16 = 0x2668;

/// Start of guest RAM.
pub const RAM_BASE: usize = 0x4000_0000;

/// Address QEMU's arm64 loader uses for the kernel image.
pub const KERNEL_LOAD_ADDR: usize = 0x4008_0000;

/// Width of the ramfb scanout buffer.
pub const DISPLAY_WIDTH: usize = 1024;

/// Height of the ramfb scanout buffer.
pub const DISPLAY_HEIGHT: usize = 768;

/// The single UART exposed by the machine.
pub static UART0: Pl011 = Pl011::new(CONSOLE_BASE);

/// The GICv2 controller used by the timer tick.
pub static INTERRUPT_CONTROLLER: GicV2 = GicV2::new(GIC_DIST_BASE, GIC_CPU_BASE);

/// Initializes the GIC and enables the timer interrupt.
pub fn init_interrupts() {
    INTERRUPT_CONTROLLER.init();
    INTERRUPT_CONTROLLER.enable(TIMER_IRQ);
}

/// Returns the interrupt controller shared with the kernel IRQ handler.
pub fn interrupts() -> &'static dyn InterruptController {
    &INTERRUPT_CONTROLLER
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
}

static BOARD_POINTER: BoardPointer = BoardPointer;

/// Finds and initialises the virtio pointer, if QEMU provided one.
pub fn init_pointer() -> Option<PointerKind> {
    let (base, kind) =
        feos_driver_virtio_input::find(VIRTIO_MMIO_BASE, VIRTIO_MMIO_STRIDE, VIRTIO_MMIO_SLOTS)?;
    let mut pointer = POINTER.lock();
    if pointer.init(base, DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16) {
        Some(kind)
    } else {
        None
    }
}

/// Returns the pointer shared with apps, when one was initialised.
pub fn pointer() -> Option<&'static dyn Pointer> {
    if POINTER.lock().is_ready() {
        Some(&BOARD_POINTER)
    } else {
        None
    }
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
    if KEYBOARD.lock().is_ready() {
        Some(&BOARD_KEYBOARD)
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
///
/// The desktop renders into this buffer and then copies it to the scanout
/// buffer in one pass, which avoids showing a half-drawn frame.
pub fn back_buffer() -> Option<Canvas<'static>> {
    // SAFETY: `BACK` is a static, page-aligned buffer that is only handed out
    // here. The kernel is single-core and the returned canvas owns the only
    // reference for the rest of the program.
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
        if byte == b'\n' {
            UART0.write_byte(b'\r');
        }
        UART0.write_byte(byte);
    }
}

fn console_reader() -> Option<u8> {
    UART0.read_byte()
}

/// Brings up ramfb and returns a canvas over the static scanout buffer.
///
/// Returns `None` when QEMU was started without `-device ramfb`. The canvas
/// borrows the scanout buffer for the rest of the kernel's lifetime.
pub fn init_display() -> Option<Canvas<'static>> {
    let ramfb = Ramfb::new(FwCfg::new(FW_CFG_BASE));
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
    // out here. The kernel is single-core and the returned canvas owns the
    // only reference for the rest of the program.
    let pixels: &'static mut [u32] =
        unsafe { core::slice::from_raw_parts_mut(ptr, DISPLAY_WIDTH * DISPLAY_HEIGHT) };

    Canvas::new(pixels, DISPLAY_WIDTH, DISPLAY_HEIGHT, DISPLAY_WIDTH)
}

mod fs_image {
    include!(concat!(env!("OUT_DIR"), "/fs_image.rs"));
}

/// The compiled-in read-only filesystem.
static FILESYSTEM: StaticFs = StaticFs::new(fs_image::FS_IMAGE);

/// Returns the filesystem apps browse through `Platform::filesystem`.
pub fn filesystem() -> &'static dyn FileSystem {
    &FILESYSTEM
}

/// The board's Intel HDA controller.
///
/// Its command rings and PCM buffer live in this static. The controller is
/// initialised by [`init_audio`] and then shared with apps through
/// `feos-hal`'s [`Audio`] trait.
pub static AUDIO: IntelHda = IntelHda::new();

/// A discovered audio device and its codec details.
pub struct AudioDevice {
    /// Playback interface handed to apps.
    pub device: &'static dyn Audio,
    /// Codec nodes found during initialisation.
    pub codec: CodecInfo,
}

/// Finds the Intel HDA controller on the PCI bus and brings it up.
///
/// Returns `None` when QEMU was started without `-device intel-hda` (or the
/// codec does not answer). The board assigns BAR0 itself because the kernel
/// boots without PCI firmware.
pub fn init_audio(clock: &'static dyn Clock) -> Option<AudioDevice> {
    let pci = PciEcam::new(PCIE_ECAM_BASE);
    let controller = pci.find(PCI_VENDOR_INTEL, PCI_DEVICE_ICH6_HDA)?;

    if !controller.assign_memory_bar(0, HDA_BAR0 as u64) {
        return None;
    }
    controller.enable();

    let codec = AUDIO.init(HDA_BAR0, clock)?;
    Some(AudioDevice {
        device: &AUDIO,
        codec,
    })
}

extern "C" {
    static __kernel_start: u8;
    static __bss_start: u8;
    static __bss_end: u8;
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
