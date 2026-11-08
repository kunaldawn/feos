//! feos - a bare-metal multi-architecture kernel.
//!
//! This binary contains only the boot sequence and the wiring between the
//! board, the architecture crate, shared libraries and the shell. Hardware
//! lives in `drivers/`, board facts in `boards/`, reusable code in `libs/`,
//! and commands in `apps/`.

#![no_std]
#![no_main]

mod allocator;
mod fatal;
mod ktest;
mod memory;
mod sched;
mod smp;
mod surface;
mod user;
mod user_window;

extern crate alloc;

use feos_app::{BoardInfo, Platform, Processes};
use feos_console::println;
use feos_hal::net::Network;
use feos_net::service::{Hooks, Service};

#[cfg(target_arch = "aarch64")]
use feos_board_qemu_virt as board;
#[cfg(target_arch = "x86_64")]
use feos_board_qemu_x86_64 as board;

use feos_hal::{Arch, Board, Clock, IrqClaim, Memory, MemoryInfo, Power};

/// The CPU architecture this kernel was built for. Going through the [`Arch`] trait
/// holds every port to the same surface.
#[cfg(target_arch = "aarch64")]
pub(crate) type Port = feos_arch_aarch64::Aarch64;
#[cfg(target_arch = "x86_64")]
pub(crate) type Port = feos_arch_x86_64::X86_64;

/// A task's saved registers.
pub(crate) type Frame = <Port as Arch>::Frame;
/// What a CPU fault reports.
pub(crate) type Fault = <Port as Arch>::Fault;


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
        let total_bytes = memory::total_bytes();
        let free_bytes = memory::free_bytes() + heap.free_bytes;
        MemoryInfo {
            total_bytes,
            used_bytes: total_bytes.saturating_sub(free_bytes),
            free_bytes,
            heap_total_bytes: heap.total_bytes,
            heap_used_bytes: heap.used_bytes,
            heap_free_bytes: heap.free_bytes,
        }
    }
}

static KERNEL_MEMORY: KernelMemory = KernelMemory;

/// The architecture's power controls, with the disk written out first.
struct SyncingPower;

impl Power for SyncingPower {
    fn system_off(&self) -> ! {
        feos_apps::fsflush::sync_all(sched::platform());
        Port::services().system_off()
    }

    fn system_reset(&self) -> ! {
        feos_apps::fsflush::sync_all(sched::platform());
        Port::services().system_reset()
    }
}

static SYNCING_POWER: SyncingPower = SyncingPower;

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
    println!("feos {} - {} kernel", env!("CARGO_PKG_VERSION"), Machine::ARCH);
    println!("----------------------------------------");

    // Install IDT/vectors and calibrate the architecture clock before any
    // interrupt can arrive.
    Port::set_fault_handler(fatal::on_exception);
    Port::init();
    println!("[boot] exception vectors installed");
    println!(
        "[boot] UART ready at {:#x} ({})",
        Machine::CONSOLE_BASE,
        Machine::CONSOLE_NAME
    );
    println!("[boot] {} at {boot_info:#x}", Port::boot_info_label(boot_info));
    Machine::init_boot_info(boot_info);
    Port::set_firmware_conduit(Machine::firmware_conduit());
    memory::init();

    // Drivers register their interrupts as they come up, so the controller goes
    // first. Nothing is delivered until the scheduler unmasks interrupts.
    Port::set_irq_handler(on_irq);
    Machine::init_interrupts();

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
                "[gfx] {} {}x{} XRGB8888 at {:#x}",
                Machine::display_name(),
                canvas.width(),
                canvas.height(),
                Machine::framebuffer_addr()
            );
        }
        None => println!("[gfx] no framebuffer (start QEMU with -device ramfb)"),
    }

    println!();
    let services = Port::services();

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

    Machine::init_usb(&sched::CLOCK);

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
        power: &SYNCING_POWER,
        cpu: services,
        audio: audio.as_ref().map(|audio| audio.device),
        pointer: Machine::pointer(),
        keyboard: Machine::keyboard(),
        processes: &sched::PROCESSES,
        memory: Some(&KERNEL_MEMORY),
        apps: &ktest::REGISTRY,
        filesystem: Some(Machine::filesystem()),
        net: start_network(),
    };

    // Background processes read the platform through this static.
    sched::set_platform(platform);
    sched::init();
    feos_apps::fsflush::install(sched::platform());

    Port::set_switch_handler(sched::on_switch_request);
    Port::set_syscall_handler(sched::on_syscall);
    println!("[user] protection: {}", Port::protection_summary());
    println!(
        "[sched] {} interrupt controller, timer IRQ {}",
        Machine::interrupt_controller_name(),
        Machine::TIMER_IRQ
    );
    println!(
        "[sched] preemptive, priorities 0-{}, {} ms slices on a one-shot timer per CPU, {} process slots",
        feos_app::PRIORITY_MAX,
        1000 / smp::TICK_HZ,
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
    feos_apps::displayd::install(sched::platform());
    feos_apps::netd::install(sched::platform());

    // Leave the splash on screen for a moment, then hand the framebuffer to
    // the desktop. Logging off (or Ctrl-\ in the serial terminal) returns to
    // the shell.
    if display.is_some() {
        Port::services().spin_ms(1_500);
        println!("[boot] starting the desktop session (Ctrl-\\ returns to the shell)");
        feos_desktop_app::run_session(&mut display, &mut back, sched::platform());
        println!("[boot] desktop session ended, entering the serial shell");
    }

    feos_shell::run(display, sched::platform())
}

/// Wraps the board's network card, if it has one, in a TCP/IP stack. The stack does
/// nothing until the network task starts running it.
fn start_network() -> Option<&'static dyn Network> {
    let binding = Machine::init_network()?;
    println!("[net] virtio-net ready: {}", binding.how);
    let seed = sched::CLOCK.ticks() ^ Machine::unix_time().unwrap_or(0).rotate_left(32);
    let service: &'static Service = alloc::boxed::Box::leak(alloc::boxed::Box::new(Service::new(binding, seed, NET_HOOKS)));
    Some(service)
}

/// What the stack asks of the kernel.
const NET_HOOKS: Hooks = Hooks {
    now_ms: || sched::CLOCK.uptime_ms(),
    now_us: || {
        let ticks = u128::from(sched::CLOCK.ticks());
        (ticks * 1_000_000 / u128::from(sched::CLOCK.frequency().max(1))) as u64
    },
    current: || sched::PROCESSES.current(),
    alive: |pid| pid == 0 || sched::PROCESSES.detail(pid).is_some(),
    kill_requested: || sched::PROCESSES.kill_requested(),
};

/// Blocks the serial shell between input polls.
///
/// The scheduler runs something else in the meantime, so a shell waiting for a
/// keypress doesn't appear as a busy core in the task manager's per-core graphs.
fn wait_for_console() {
    sched::sleep_ms(1);
}

/// Handles every hardware interrupt, including IPIs.
fn on_irq(frame: &mut Frame) {
    let controller = Machine::interrupts();
    let claim = match frame.interrupt_id() {
        Some(id) => IrqClaim { id, raw: id },
        None => controller.claim(),
    };
    let id = claim.id;

    if controller.is_spurious(id) {
        return;
    }

    let is_tick = id == Machine::TIMER_IRQ;
    let is_reschedule = id == Machine::IPI_RESCHEDULE;
    if is_tick {
        if Port::cpu_index() == 0 {
            if let Some(audio) = sched::platform().audio {
                audio.service();
            }
        }
    } else if id == Machine::IPI_STOP {
        fatal::stop_this_cpu();
    } else if !is_reschedule {
        // The handler quiets its device before the end-of-interrupt below, so a
        // level-triggered line doesn't come straight back.
        feos_irq::dispatch(id);
    }

    // Acknowledge before switching stacks so the controller can deliver the
    // next interrupt to whichever process runs next.
    controller.complete(claim);

    // Only the tick and an explicit request reschedule. A device doesn't get to
    // rotate the current task or count as a tick. It may have woken a task,
    // though, and an idle CPU shouldn't sit on that until its next tick.
    if is_tick {
        sched::on_tick(frame);
    } else if is_reschedule || sched::is_idle() {
        sched::on_reschedule(frame);
    }
}
