//! feos - a tiny bare-metal AArch64 kernel skeleton.
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
use feos_arch_aarch64::{cpu, exceptions, irq, timer};
use feos_board_qemu_virt as board;
use feos_console::println;
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
/// Called from `_start` in `arch/aarch64/src/boot.S` once a stack exists and
/// `.bss` has been zeroed. `dtb` is the device tree address QEMU passes in
/// `x0`.
#[no_mangle]
pub extern "C" fn kmain(dtb: usize) -> ! {
    board::init_console();
    feos_console::set_waiter(wait_for_console);

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
    let mut back = if display.is_some() {
        board::back_buffer()
    } else {
        None
    };
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

    let pointer = board::init_pointer();
    match pointer {
        Some(kind) => println!("[input] virtio pointer ready ({kind:?})"),
        None => println!("[input] no pointer (start QEMU with -device virtio-tablet-device)"),
    }

    if board::init_keyboard() {
        println!("[input] virtio keyboard ready");
    } else {
        println!("[input] no virtio keyboard (serial console is the keyboard)");
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
        "[sched] GICv2 distributor {:#x}, CPU interface {:#x}, timer IRQ {}",
        board::GIC_DIST_BASE,
        board::GIC_CPU_BASE,
        board::TIMER_IRQ
    );
    println!(
        "[sched] preemptive round-robin at {} Hz, {} process slots",
        smp::TICK_HZ,
        feos_app::MAX_PROCESSES
    );

    // Bring up the remaining CPUs before the boot CPU starts ticking. Each
    // secondary configures its own GIC interface and timer, then joins the
    // shared run queue.
    let (started, error) = smp::start_secondaries();
    match error {
        Some(code) => {
            println!("[smp] started {started} secondary CPU(s); no more CPUs (PSCI status {code})")
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
        feos_arch_aarch64::timer::spin_ms(1_500);
        println!("[boot] starting the desktop session (Ctrl-\\ returns to the shell)");
        feos_desktop_app::run_session(&mut display, &mut back, sched::platform());
        println!("[boot] desktop session ended, entering the serial shell");
    }

    feos_shell::run(display, sched::platform())
}

/// Blocks the serial shell in `WFI` between input polls.
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
