//! Secondary-CPU startup.
//!
//! The boot CPU copies the trampoline from `boot.S` to low memory and walks each
//! other CPU through INIT, then two startup IPIs, the sequence Intel and AMD both
//! specify. CPUs start one at a time, sharing one word for the stack and index, so
//! each is waited for before the words change. A CPU that doesn't answer is
//! reported and left alone.

use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{cpu, exceptions, gdt, lapic, timer};

/// Maximum number of CPUs this architecture port can bring up.
pub const MAX_CPUS: usize = 8;

/// Callback invoked on a secondary CPU once its stack and vectors are ready.
pub type SecondaryHandler = fn(usize) -> !;

/// Must match `AP_TRAMPOLINE` in `boot.S`. It's a free page by the multiboot map:
/// QEMU's loader keeps its structures from 0x9000 up.
const TRAMPOLINE: usize = 0x8000;
/// How long a CPU may take to reach Rust, in milliseconds. Emulated CPUs share a
/// host, so this is generous.
const START_TIMEOUT_MS: u64 = 2000;
/// A stack until the CPU's first tick, as on AArch64.
const STACK_SIZE: usize = 32 * 1024;

#[repr(C, align(16))]
struct Stacks([[u8; STACK_SIZE]; MAX_CPUS]);

static mut STACKS: Stacks = Stacks([[0; STACK_SIZE]; MAX_CPUS]);

static HANDLER: AtomicUsize = AtomicUsize::new(0);
/// The logical index of the last CPU to reach Rust.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static mut ap_boot_stack: u64;
    static mut ap_boot_index: u64;
}

/// Starts CPUs `1..limit`, where `apic_id` maps a logical index to the firmware's
/// APIC ID. Returns how many answered and, when one didn't, its logical index.
pub fn start_secondaries(
    limit: usize,
    apic_id: fn(usize) -> u64,
    handler: SecondaryHandler,
) -> (usize, Option<i64>) {
    HANDLER.store(handler as usize, Ordering::Release);

    let start = core::ptr::addr_of!(ap_trampoline_start);
    let length = core::ptr::addr_of!(ap_trampoline_end) as usize - start as usize;
    // SAFETY: the page is free, nothing else uses it, and the first 2 MiB are
    // identity mapped. The stub is smaller than a page.
    unsafe { core::ptr::copy_nonoverlapping(start, TRAMPOLINE as *mut u8, length.min(4096)) };

    let mut started = 0;
    let mut failed = None;
    for index in 1..limit.min(MAX_CPUS) {
        // SAFETY: nothing else is reading these: the last CPU to start has
        // finished with them, and any CPU that timed out is reported below.
        unsafe {
            let stack = core::ptr::addr_of_mut!(STACKS.0[index]) as u64 + STACK_SIZE as u64;
            core::ptr::addr_of_mut!(ap_boot_stack).write_volatile(stack);
            core::ptr::addr_of_mut!(ap_boot_index).write_volatile(index as u64);
        }

        let id = apic_id(index) as u32;
        lapic::send_init(id);
        timer::spin_ms(10);
        lapic::send_startup(id, (TRAMPOLINE >> 12) as u8);
        timer::spin_ms(1);
        if ARRIVED.load(Ordering::Acquire) != index {
            lapic::send_startup(id, (TRAMPOLINE >> 12) as u8);
        }

        let deadline = timer::ticks() + timer::frequency() / 1000 * START_TIMEOUT_MS;
        while ARRIVED.load(Ordering::Acquire) != index && timer::ticks() < deadline {
            core::hint::spin_loop();
        }
        if ARRIVED.load(Ordering::Acquire) == index {
            started += 1;
        } else {
            // It may yet start, on these same words, so nothing more can be tried.
            failed = Some(index as i64);
            break;
        }
    }
    (started, failed)
}

/// Called from `ap_long_mode` in `boot.S`, on the stack this module gave the CPU.
#[no_mangle]
extern "C" fn x86_ap_entry(index: usize) -> ! {
    cpu::set_index(index);
    gdt::init();
    exceptions::load();
    ARRIVED.store(index, Ordering::Release);

    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        cpu::halt();
    }
    // SAFETY: only `start_secondaries` writes this atomic, with a valid function
    // pointer, and a function pointer is `usize`-sized on this target.
    let handler = unsafe { transmute::<usize, SecondaryHandler>(address) };
    handler(index)
}
