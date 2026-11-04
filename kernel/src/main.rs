//! feos - a tiny bare-metal multi-architecture kernel skeleton.
//!
//! This binary contains only the boot sequence and the wiring between the
//! board, the architecture crate, shared libraries and the shell. Hardware
//! lives in `drivers/`, board facts in `boards/`, reusable code in `libs/`,
//! and commands in `apps/`.

#![no_std]
#![no_main]

mod allocator;
mod fatal;
mod sched;
mod smp;

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
use feos_hal::{Board, Memory, MemoryInfo};

/// The board this kernel was built for. Going through the [`Board`] trait holds
/// every port to the same surface.
#[cfg(target_arch = "aarch64")]
pub(crate) type Machine = board::QemuVirt;
#[cfg(target_arch = "x86_64")]
pub(crate) type Machine = board::QemuX86_64;

/// Kernel memory accounting for the task manager.
struct KernelMemory;

impl Memory for KernelMemory {
    fn info(&self) -> MemoryInfo {
        let heap = allocator::stats();
        let (kernel_start, _) = Machine::kernel_range();
        let (_, bss_end) = Machine::bss_range();
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
    Machine::init_console();
    feos_console::set_waiter(wait_for_console);

    println!();
    println!("feos {} - a tiny {} kernel", env!("CARGO_PKG_VERSION"), Machine::ARCH);
    println!("----------------------------------------");

    // Install IDT/vectors and calibrate the architecture clock before any
    // interrupt can arrive.
    exceptions::set_handler(fatal::on_exception);
    arch::init();
    println!("[boot] exception vectors installed");
    println!(
        "[boot] UART ready at {:#x} ({})",
        Machine::CONSOLE_BASE,
        Machine::CONSOLE_NAME
    );
    println!("[boot] {} at {boot_info:#x}", arch::boot_info_label(boot_info));

    // Bring up the ramfb framebuffer if QEMU provides one.
    let mut display = Machine::init_display();
    let mut back = if display.is_some() {
        Machine::back_buffer()
    } else {
        None
    };
    match display.as_mut() {
        Some(canvas) => {
            feos_apps::ui::splash(canvas, Machine::ARCH);
            println!(
                "[gfx] ramfb {}x{} XRGB8888 at {:#x}",
                canvas.width(),
                canvas.height(),
                Machine::framebuffer_addr()
            );
        }
        None => println!("[gfx] no framebuffer (start QEMU with -device ramfb)"),
    }

    println!();
    let services = &arch::ARCH;

    let audio = Machine::init_audio(&sched::CLOCK);
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

    let pointer = Machine::init_pointer();
    match pointer {
        Some(kind) => println!("[input] {} pointer ready ({kind:?})", Machine::POINTER_NAME),
        None => println!(
            "[input] no {} pointer ({})",
            Machine::POINTER_NAME,
            Machine::POINTER_HINT
        ),
    }

    if Machine::init_keyboard() {
        println!("[input] {} keyboard ready", Machine::KEYBOARD_NAME);
    } else {
        println!(
            "[input] no {} keyboard ({})",
            Machine::KEYBOARD_NAME,
            Machine::KEYBOARD_HINT
        );
    }

    let platform = Platform {
        board: BoardInfo {
            name: Machine::NAME,
            arch: Machine::ARCH,
            console_base: Machine::CONSOLE_BASE,
            framebuffer_addr: Machine::framebuffer_addr(),
            kernel_range: Machine::kernel_range(),
            bss_range: Machine::bss_range(),
        },
        clock: &sched::CLOCK,
        power: services,
        cpu: services,
        audio: audio.as_ref().map(|audio| audio.device),
        pointer: Machine::pointer(),
        keyboard: Machine::keyboard(),
        processes: &sched::PROCESSES,
        memory: Some(&KERNEL_MEMORY),
        apps: &feos_apps::APP_REGISTRY,
        filesystem: Some(Machine::filesystem()),
    };

    // Background processes read the platform through this static.
    sched::set_platform(platform);
    sched::init();

    irq::set_handler(on_irq);
    irq::set_switch_handler(sched::on_switch_request);
    Machine::init_interrupts();
    println!(
        "[sched] {} interrupt controller, timer IRQ {}",
        Machine::INTERRUPT_CONTROLLER_NAME,
        Machine::TIMER_IRQ
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

/// Blocks the serial shell between input polls.
///
/// The scheduler runs something else in the meantime, so a shell waiting for a
/// keypress doesn't appear as a busy core in the task manager's per-core graphs.
fn wait_for_console() {
    sched::sleep_ms(1);
}

/// Handles every hardware interrupt, including IPIs.
fn on_irq(frame: &mut irq::TrapFrame) {
    let controller = Machine::interrupts();
    let claim = controller.claim();
    let id = claim.id;

    if controller.is_spurious(id) {
        return;
    }

    let is_tick = id == Machine::TIMER_IRQ;
    if is_tick {
        timer::tick();
        if cpu::index() == 0 {
            if let Some(audio) = sched::platform().audio {
                audio.service();
            }
        }
    } else if id == Machine::IPI_STOP {
        fatal::stop_this_cpu();
    }

    // Acknowledge before switching stacks so the controller can deliver the
    // next interrupt to whichever process runs next.
    controller.complete(claim);

    // Only the tick and an explicit request reschedule. Anything else, such as
    // a device, has no business rotating the current task or counting as a tick.
    if is_tick {
        sched::on_tick(frame);
    } else if id == Machine::IPI_RESCHEDULE {
        sched::on_reschedule(frame);
    }
}
