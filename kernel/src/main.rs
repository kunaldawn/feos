//! feos - a tiny bare-metal AArch64 kernel skeleton.
//!
//! This binary contains only the boot sequence and the wiring between the
//! board, the architecture crate, shared libraries and the shell. Hardware
//! lives in `drivers/`, board facts in `boards/`, reusable code in `libs/`,
//! and commands in `apps/`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use feos_app::{BoardInfo, Platform};
use feos_arch_aarch64::{cpu, exceptions};
use feos_board_qemu_virt as board;
use feos_console::println;

/// Kernel entry point.
///
/// Called from `_start` in `arch/aarch64/src/boot.S` once a stack exists and
/// `.bss` has been zeroed. `dtb` is the device tree address QEMU passes in
/// `x0`.
#[no_mangle]
pub extern "C" fn kmain(dtb: usize) -> ! {
    board::init_console();

    println!();
    println!("feos {} - a tiny aarch64 kernel", env!("CARGO_PKG_VERSION"));
    println!("----------------------------------------");

    exceptions::set_handler(report_exception);
    exceptions::init();
    println!("[boot] exception vectors installed (VBAR_EL1)");
    println!("[boot] UART ready at {:#x} (PL011)", board::CONSOLE_BASE);

    if dtb != 0 && unsafe { core::ptr::read_volatile(dtb as *const u32) } == 0xd00d_feed {
        println!("[boot] device tree blob at {dtb:#x}");
    } else {
        println!("[boot] no DTB passed by QEMU (x0 = {dtb:#x})");
    }

    // Bring up the ramfb framebuffer if QEMU provides one.
    let mut display = board::init_display();
    match display.as_mut() {
        Some(canvas) => {
            feos_apps::ui::splash(canvas);
            println!(
                "[gfx] ramfb {}x{} XRGB8888 at {:#x}",
                canvas.width(),
                canvas.height(),
                board::framebuffer_addr()
            );
        }
        None => println!("[gfx] no framebuffer (start QEMU with -device ramfb)"),
    }

    println!();
    let services = &feos_arch_aarch64::AARCH64;

    let audio = board::init_audio(services);
    match audio.as_ref() {
        Some(audio) => println!(
            "[audio] Intel HDA ready: {} Hz, {} channel(s), codec {:#x} (DAC {}, pin {})",
            audio.device.sample_rate(),
            audio.device.channels(),
            audio.codec.vendor_id,
            audio.codec.dac,
            audio.codec.pin
        ),
        None => println!(
            "[audio] no Intel HDA codec (start QEMU with -device intel-hda -device hda-output)"
        ),
    }

    let platform = Platform {
        board: BoardInfo {
            name: board::NAME,
            console_base: board::CONSOLE_BASE,
            framebuffer_addr: board::framebuffer_addr(),
            kernel_range: board::kernel_range(),
            bss_range: board::bss_range(),
        },
        clock: services,
        power: services,
        cpu: services,
        audio: audio.as_ref().map(|audio| audio.device),
    };
    feos_shell::run(display, &platform)
}

/// Reports a synchronous exception or interrupt and lets the arch crate halt.
fn report_exception(info: &exceptions::FaultInfo) {
    feos_console::_print_unlocked(format_args!(
        "\n[exception] {} / {} (vector {})\n\
         [exception] ESR_EL1={:#018x} EC={:#04x} ({})\n\
         [exception] ELR_EL1={:#018x} FAR_EL1={:#018x}\n\
         [exception] SPSR_EL1={:#018x} SP={:#018x}\n\
         [exception] system halted\n",
        info.vector_group(),
        info.vector_type(),
        info.index,
        info.esr,
        info.exception_class(),
        info.syndrome_name(),
        info.elr,
        info.far,
        info.spsr,
        info.sp,
    ));
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Bypass the console lock: a panic may have happened while it was held.
    feos_console::_print_unlocked(format_args!("\n[panic] {info}\n"));
    cpu::halt()
}
