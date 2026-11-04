//! feos - a tiny bare-metal multi-architecture kernel skeleton.
//!
//! This binary contains only the boot sequence and the wiring between the
//! board, the architecture crate, shared libraries and the shell. Hardware
//! lives in `drivers/`, board facts in `boards/`, reusable code in `libs/`,
//! and commands in `apps/`.

#![no_std]
#![no_main]

mod allocator;
mod sched;
mod smp;

use core::panic::PanicInfo;

use feos_app::{BoardInfo, Platform};
use feos_console::println;

#[cfg(target_arch = "aarch64")]
use feos_arch_aarch64 as arch;
#[cfg(target_arch = "x86_64")]
use feos_arch_x86_64 as arch;
#[cfg(target_arch = "aarch64")]
use feos_board_qemu_virt as board;
#[cfg(target_arch = "x86_64")]
use feos_board_qemu_x86_64 as board;

use arch::{cpu, exceptions, irq, timer};
use feos_hal::{Memory, MemoryInfo};

/// Kernel memory accounting for the task manager.
struct KernelMemory;

impl Memory for KernelMemory {
    fn info(&self) -> MemoryInfo {
        let heap = allocator::stats();
        let (kernel_start, _) = board::kernel_range();
        let (_, bss_end) = board::bss_range();
        let total_bytes = bss_end.saturating_sub(kernel_start);
        MemoryInfo {
            total_bytes,
            used_bytes: total_bytes.saturating_sub(heap.free_bytes),
            free_bytes: heap.free_bytes,
            heap_total_bytes: heap.total_bytes,
            heap_used_bytes: heap.used_bytes,
            heap_free_bytes: heap.free_bytes,
        }
    }
}

static KERNEL_MEMORY: KernelMemory = KernelMemory;

/// Kernel entry point.
///
/// On AArch64 this is called from `_start` in `arch/aarch64/src/boot.S` with
/// the device tree address QEMU passes in `x0`. On x86_64 the long-mode entry
/// passes the multiboot information pointer in `rdi`.
#[no_mangle]
pub extern "C" fn kmain(boot_info: usize) -> ! {
    board::init_console();
    feos_console::set_waiter(wait_for_console);

    println!();
    println!("feos {} - a tiny {} kernel", env!("CARGO_PKG_VERSION"), board::ARCH);
    println!("----------------------------------------");

    // Install IDT/vectors and calibrate the architecture clock before any
    // interrupt can arrive.
    arch::init();

    exceptions::set_handler(report_exception);
    exceptions::init();
    println!("[boot] exception vectors installed");
    println!(
        "[boot] UART ready at {:#x} ({})",
        board::CONSOLE_BASE,
        board::CONSOLE_NAME
    );

    #[cfg(target_arch = "aarch64")]
    {
        if boot_info != 0
            && unsafe { core::ptr::read_volatile(boot_info as *const u32) } == 0xd00d_feed
        {
            println!("[boot] device tree blob at {boot_info:#x}");
        } else {
            println!("[boot] no DTB passed by QEMU (x0 = {boot_info:#x})");
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        if boot_info != 0 {
            println!("[boot] multiboot information at {boot_info:#x}");
        } else {
            println!("[boot] no multiboot information pointer");
        }
    }

    // Bring up the ramfb framebuffer if QEMU provides one.
    let mut display = board::init_display();
    let mut back = if display.is_some() {
        board::back_buffer()
    } else {
        None
    };
    match display.as_mut() {
        Some(canvas) => {
            feos_apps::ui::splash(canvas, board::ARCH);
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
    let services = &arch::ARCH;

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

    let pointer = board::init_pointer();
    match pointer {
        Some(kind) => println!("[input] {} pointer ready ({kind:?})", board::POINTER_NAME),
        None => println!(
            "[input] no {} pointer ({})",
            board::POINTER_NAME,
            board::POINTER_HINT
        ),
    }

    if board::init_keyboard() {
        println!("[input] {} keyboard ready", board::KEYBOARD_NAME);
    } else {
        println!(
            "[input] no {} keyboard ({})",
            board::KEYBOARD_NAME,
            board::KEYBOARD_HINT
        );
    }

    let platform = Platform {
        board: BoardInfo {
            name: board::NAME,
            arch: board::ARCH,
            console_base: board::CONSOLE_BASE,
            framebuffer_addr: board::framebuffer_addr(),
            kernel_range: board::kernel_range(),
            bss_range: board::bss_range(),
        },
        clock: services,
        power: services,
        cpu: services,
        audio: audio.as_ref().map(|audio| audio.device),
        pointer: board::pointer(),
        keyboard: board::keyboard(),
        processes: &sched::PROCESSES,
        memory: Some(&KERNEL_MEMORY),
        apps: &feos_apps::APP_REGISTRY,
        filesystem: Some(board::filesystem()),
    };

    // Background processes read the platform through this static.
    sched::set_platform(platform);
    sched::init();

    irq::set_handler(on_irq);
    board::init_interrupts();
    println!(
        "[sched] {} interrupt controller, timer IRQ {}",
        board::INTERRUPT_CONTROLLER_NAME,
        board::TIMER_IRQ
    );
    println!(
        "[sched] preemptive round-robin at {} Hz, {} process slots",
        smp::TICK_HZ,
        feos_app::MAX_PROCESSES
    );

    // Bring up the remaining CPUs before the boot CPU starts ticking. Each
    // secondary configures its own interrupt-controller interface and timer,
    // then joins the shared run queue.
    let (started, error) = smp::start_secondaries();
    match error {
        Some(code) => {
            println!(
                "[smp] started {started} secondary CPU(s); no more CPUs (firmware status {code})"
            )
        }
        None => println!("[smp] started {started} secondary CPU(s)"),
    }

    // Timer ticks and IRQ delivery begin here; the shell is PID 1 and the
    // boot CPU's currently running process.
    sched::start(smp::TICK_HZ);

    // Leave the splash on screen for a moment, then hand the framebuffer to
    // the desktop. Logging off (or Ctrl-\ in the serial terminal) returns to
    // the shell.
    if display.is_some() {
        arch::timer::spin_ms(1_500);
        println!("[boot] starting the desktop session (Ctrl-\\ returns to the shell)");
        feos_desktop_app::run_session(&mut display, &mut back, sched::platform());
        println!("[boot] desktop session ended, entering the serial shell");
    }

    feos_shell::run(display, sched::platform())
}

/// Blocks the serial shell in the architecture's idle instruction between
/// input polls.
///
/// Using the timer's idle helper also marks the tick as idle, so a shell
/// waiting for a keypress no longer appears as a busy core in the task
/// manager's per-core graphs.
fn wait_for_console() {
    timer::idle_ms(1);
}

/// Handles the periodic timer interrupt and lets the scheduler choose a
/// process.
fn on_irq(frame: &mut irq::TrapFrame) {
    let controller = board::interrupts();
    let id = controller.acknowledge();

    if id == board::TIMER_IRQ {
        timer::tick();
    }

    // Acknowledge before switching stacks so the controller can deliver the
    // next interrupt to whichever process runs next.
    if id < 1020 {
        controller.end(id);
    }

    sched::preempt(frame);
}

/// Reports a synchronous exception or interrupt and lets the arch crate halt.
fn report_exception(info: &exceptions::FaultInfo) {
    feos_console::_print_unlocked(format_args!(
        "\n[exception] {} / {} (vector {})\n\
         [exception] error={:#018x} class={:#04x} ({})\n\
         [exception] ip={:#018x} fault address={:#018x}\n\
         [exception] flags={:#018x} sp={:#018x}\n\
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
