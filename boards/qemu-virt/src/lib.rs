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
use feos_driver_pl011::Pl011;
use feos_driver_ramfb::Ramfb;
use feos_gfx::Canvas;
use feos_hal::Serial;

/// Human-readable board name, shown by the `info` app.
pub const NAME: &str = "QEMU aarch64 virt";

/// PL011 UART0 on the `virt` machine.
pub const CONSOLE_BASE: usize = 0x0900_0000;

/// fw_cfg MMIO window on the `virt` machine.
pub const FW_CFG_BASE: usize = 0x0902_0000;

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

/// The ramfb scanout buffer. Page aligned as required by the device.
#[repr(align(4096))]
struct ScanoutBuffer([u32; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

static mut SCANOUT: ScanoutBuffer = ScanoutBuffer([0; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

/// Physical address of the scanout buffer.
pub fn framebuffer_addr() -> usize {
    // SAFETY: taking the address of a `static mut` does not create a
    // reference; the buffer is only exposed once through `init_display`.
    unsafe { core::ptr::addr_of_mut!(SCANOUT.0) as *mut u32 as usize }
}

/// Initializes the UART and registers it as the kernel console.
pub fn init_console() {
    UART0.init();
    feos_console::set_writer(console_writer);
    feos_console::set_reader(console_reader);
}

fn console_writer(text: &str) {
    UART0.write_terminal(text);
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
