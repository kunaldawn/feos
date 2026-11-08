//! What the kernel needs of a CPU architecture.
//!
//! [`Board`](crate::Board) holds each machine to one surface. [`Arch`] does the same
//! for each CPU port, so a change to one that the other doesn't match stops the
//! build, not just a `make check` of the other target.

use core::fmt;

use crate::{Clock, Cpu, Power};

/// A task's registers, as a trap saved them or as a new task starts.
pub trait TrapContext: Copy + 'static {
    /// A frame with every register cleared. Not something to resume.
    const ZERO: Self;

    /// The frame of a task that has never run: `entry` gets `arg` as its first
    /// argument, on a stack ending at `stack_top`, and returning from it goes to
    /// `return_to`.
    fn new_task(entry: usize, arg: usize, stack_top: usize, return_to: usize) -> Self;

    /// Stack pointer of the trapped context.
    fn stack_pointer(&self) -> usize;

    /// The interrupt that caused this trap, when the CPU says without having to ask
    /// the interrupt controller.
    fn interrupt_id(&self) -> Option<u32>;

    /// Whether the trapped code could take interrupts. If it could, it wasn't an
    /// interrupt handler, and held no lock that masks them.
    fn interrupts_were_enabled(&self) -> bool;

    /// The frame of a user program that has never run, starting at `entry` with
    /// `stack_top` as its stack and `arg0` and `arg1` as its first two arguments.
    fn new_user(entry: usize, stack_top: usize, arg0: usize, arg1: usize) -> Self;

    /// Whether the trapped code was a user program.
    fn is_user(&self) -> bool;

    /// A system call's number, in a frame a system call saved.
    fn syscall_number(&self) -> usize;

    /// A system call's six arguments.
    fn syscall_args(&self) -> [usize; 6];

    /// Sets what a system call returns.
    fn set_syscall_result(&mut self, value: usize);
}

/// Who a page belongs to and what can be done with it, for [`PageBits::leaf`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageAccess {
    pub write: bool,
    pub execute: bool,
}

/// How a port writes a user page table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageBits {
    /// Set in any entry that's in use.
    pub present: u64,
    /// Everything but the address in an entry that points at a table.
    pub table: u64,
    /// Everything but the address and access bits in an entry for a page.
    pub leaf: u64,
    /// Set in a writable page's entry.
    pub writable: u64,
    /// Set in a read-only page's entry.
    pub read_only: u64,
    /// Set in a page's entry if code there mustn't run.
    pub no_execute: u64,
    /// The address bits of an entry.
    pub address: u64,
}

impl PageBits {
    /// What goes with the address in an entry for a page.
    pub const fn leaf(&self, access: PageAccess) -> u64 {
        self.leaf
            | if access.write { self.writable } else { self.read_only }
            | if access.execute { 0 } else { self.no_execute }
    }

    /// Whether a page's entry lets a program write.
    pub const fn is_writable(&self, entry: u64) -> bool {
        if self.read_only != 0 {
            entry & self.read_only == 0
        } else {
            entry & self.writable != 0
        }
    }
}

/// What a CPU fault can say about itself.
pub trait FaultDetails {
    /// Whether this is something a task did, as opposed to the machine failing, so
    /// that ending the task could be an answer to it.
    fn is_task_fault(&self) -> bool;
    /// Whether a user program caused this, as opposed to kernel code.
    // "From" as in where the fault came from, not a conversion.
    #[allow(clippy::wrong_self_convention)]
    fn from_user(&self) -> bool;
    /// The address a memory fault was on, if this is one.
    fn fault_address(&self) -> Option<u64>;
    /// Human-readable name of the fault.
    fn syndrome_name(&self) -> &'static str;
    /// The address of the instruction that faulted.
    fn instruction(&self) -> u64;
    /// Writes everything known, for a report that stops the machine.
    fn write_report(&self, out: &mut dyn fmt::Write) -> fmt::Result;
}

/// How to reach firmware for calls such as starting a CPU. Only AArch64 has a choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FirmwareConduit {
    /// Nobody's said.
    #[default]
    Unknown,
    /// A hypervisor call.
    Hvc,
    /// A secure monitor call.
    Smc,
    /// There's no such firmware.
    None,
}

/// The handler a CPU runs when it starts: its logical index, and it doesn't return.
pub type SecondaryEntry = fn(usize) -> !;

/// One CPU architecture, as the kernel sees it. Everything is a function of the
/// type, because everything is a fact about the CPU the code's running on.
pub trait Arch: Clock + Cpu + Power + Sized + 'static {
    /// CPUs this port can run on.
    const MAX_CPUS: usize;
    /// The saved registers of a task.
    type Frame: TrapContext;
    /// What a CPU fault reports.
    type Fault: FaultDetails;

    /// This port's system services, for handing to apps.
    fn services() -> &'static Self;

    /// One-time setup on the boot CPU, before interrupts can arrive. Install the
    /// fault handler first.
    fn init();
    /// Says how firmware is reached, for a port that has more than one way. Call it
    /// once the board knows, before starting any CPU.
    fn set_firmware_conduit(_conduit: FirmwareConduit) {}
    /// Describes the boot information pointer for the boot log.
    fn boot_info_label(boot_info: usize) -> &'static str;
    /// Starts CPUs `1..limit`, where `firmware_id` maps a logical index to the
    /// firmware's number for it. Returns how many started, and a status if one
    /// didn't.
    fn start_secondaries(
        limit: usize,
        firmware_id: fn(usize) -> u64,
        entry: SecondaryEntry,
    ) -> (usize, Option<i64>);

    /// Logical index of the CPU running this code. Only still true by the time it's
    /// used with interrupts masked.
    fn cpu_index() -> usize;
    /// Masks everything and stops this CPU for good.
    fn halt() -> !;

    /// Installs the function called for every interrupt.
    fn set_irq_handler(handler: fn(&mut Self::Frame));
    /// Installs the function called on [`request_switch`](Self::request_switch).
    fn set_switch_handler(handler: fn(&mut Self::Frame));
    /// Installs the function called when a user program makes a system call.
    fn set_syscall_handler(handler: fn(&mut Self::Frame));
    /// Installs the function called for every CPU fault. It can return `true` having
    /// called [`resume`](Self::resume), to carry on with something else.
    fn set_fault_handler(handler: fn(&Self::Fault, &mut Self::Frame) -> bool);

    /// Unmasks interrupts on this CPU.
    fn irq_enable();
    /// Masks interrupts on this CPU.
    fn irq_disable();
    /// Masks interrupts on this CPU and returns the previous state.
    fn irq_save_and_disable() -> u64;
    /// Restores a state from [`irq_save_and_disable`](Self::irq_save_and_disable).
    fn irq_restore(flags: u64);
    /// Whether interrupts are unmasked on this CPU.
    fn irqs_enabled() -> bool;
    /// Waits for an interrupt.
    fn wait_for_interrupt();
    /// Traps into the switch handler, saving this thread's whole state. It resumes
    /// after this call when it's next chosen.
    fn request_switch();
    /// Has traps on this CPU save their state in `frame` from now on. Call with
    /// interrupts masked, for the frame of what's about to run.
    fn set_trap_frame(frame: *mut Self::Frame);
    /// From a handler: returns to `frame` instead of the frame the trap saved, and
    /// has the next trap on this CPU save there.
    fn resume(frame: *mut Self::Frame);

    /// Starts this CPU's timer, first firing one tick from now at `hz` hertz.
    fn timer_start(hz: u64);
    /// Counter units per scheduler tick.
    fn timer_period() -> u64;
    /// Has this CPU's timer interrupt fire at counter value `deadline`.
    fn timer_arm(deadline: u64);
    /// Returns and clears the idle hint recorded by [`Clock::idle_ms`] on this CPU.
    fn take_idle_hint() -> bool;

    /// Makes sure RAM at `[base, base + size)` is mapped as ordinary cached memory, for
    /// a port whose boot map might have it otherwise.
    ///
    /// # Safety
    /// The range must be RAM, and no other CPU may be running.
    unsafe fn map_ram_cached(_base: usize, _size: usize) {}

    /// Takes `[base, base + size)` out of the kernel's page tables, so that touching
    /// any of it faults. `alloc_table` provides a page for each new table. Returns
    /// `false` when the range is unaligned or unmapped, or tables run out.
    ///
    /// # Safety
    /// Nothing may use the range again, and no other CPU may be running.
    unsafe fn unmap_range(
        base: usize,
        size: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> bool;

    /// How this port writes a user page table entry.
    fn user_page_bits() -> PageBits;

    /// Readies `tables`, a user top table from `feos-vm`, to be loaded: the kernel has
    /// to stay mapped, for the supervisor only, in every process. Returns what to give
    /// [`activate_user_tables`](Self::activate_user_tables): `tables` itself, or a page
    /// more from `alloc_table` for the caller to free if it isn't `tables`.
    ///
    /// # Safety
    /// `tables` must be a table, given over to this, on a machine past early boot: the
    /// kernel's own top-level mappings don't change after that.
    unsafe fn prepare_user_tables(
        tables: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> Option<usize>;

    /// Loads a process's tables on this CPU, or the kernel's own with `root` zero.
    /// `asid` is unique among live spaces and below 256. Call with interrupts masked.
    fn activate_user_tables(root: usize, asid: u16);

    /// Forgets anything cached about a space that's going away. On every CPU.
    fn flush_user_space(asid: u16);

    /// Forgets anything cached about one user page. On every CPU.
    fn flush_user_page(address: usize, asid: u16);

    /// Makes changes to user page tables visible to the table walker.
    fn sync_user_tables();

    /// Makes code just written to memory at `[address, address + length)`, the
    /// kernel's address for it, safe to run.
    fn sync_user_code(address: usize, length: usize);

    /// What memory protection this CPU has turned on, for the boot log.
    fn protection_summary() -> &'static str;
}
