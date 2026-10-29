//! feos - a tiny bare-metal AArch64 kernel skeleton.
//!
//! Boots on `qemu-system-aarch64 -machine virt` at EL1, sets up a stack and
//! exposes a small serial shell on the PL011 UART. No heap, no MMU, no
//! interrupts yet: deliberately easy to read and extend.

#![no_std]
#![no_main]

#[macro_use]
mod macros;

mod arch;
mod console;
mod drivers;
mod graphics;
mod shell;

use core::panic::PanicInfo;

/// Kernel entry point.
///
/// Called from `_start` in `arch/boot.S` once a stack exists and `.bss` has
/// been zeroed. `dtb` is the device tree address QEMU passes in `x0`.
#[no_mangle]
pub extern "C" fn kmain(dtb: usize) -> ! {
    drivers::pl011::init();

    println!();
    println!("feos {} - a tiny aarch64 kernel", env!("CARGO_PKG_VERSION"));
    println!("----------------------------------------");

    arch::exceptions::init();
    println!("[boot] exception vectors installed (VBAR_EL1)");
    println!("[boot] UART ready at 0x0900_0000 (PL011)");

    if dtb != 0 && unsafe { core::ptr::read_volatile(dtb as *const u32) } == 0xd00d_feed {
        println!("[boot] device tree blob at {dtb:#x}");
    } else {
        println!("[boot] no DTB passed by QEMU (x0 = {dtb:#x})");
    }

    // Bring up the ramfb framebuffer if QEMU provides one.
    let mut display = graphics::init();
    match display.as_mut() {
        Some(canvas) => {
            graphics::demo::splash(canvas);
            println!(
                "[gfx] ramfb {}x{} XRGB8888 at {:#x}",
                canvas.width(),
                canvas.height(),
                graphics::framebuffer_addr()
            );
        }
        None => println!("[gfx] no framebuffer (start QEMU with -device ramfb)"),
    }

    println!();
    shell::run(display)
}

/// Stops the CPU until an event occurs.
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Bypass the console lock: a panic may have happened while it was held.
    console::_print_unlocked(format_args!("\n[panic] {info}\n"));
    halt()
}
