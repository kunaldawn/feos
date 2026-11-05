//! x86_64 architecture support.
//!
//! The crate owns the 32-bit multiboot entry stub, the transition into long
//! mode, the interrupt descriptor table, the local APIC timer and the HAL system
//! services. Boards choose the machine-specific addresses; the kernel only
//! sees the same `Clock`, `Power`, `Cpu`, `irq` and `smp` surface as it does on
//! AArch64.

#![no_std]

use core::fmt;

use feos_hal::{Arch, Clock, Cpu, FaultDetails, PageBits, Power, SecondaryEntry, TrapContext};

pub mod boot;
pub mod cpu;
pub mod exceptions;
pub mod gdt;
pub mod irq;
pub mod mmu;
mod lapic;
pub mod power;
pub mod smp;
pub mod timer;

/// The x86_64 implementation of the kernel's system services.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86_64;

/// Shared instance of the x86_64 system services.
pub static ARCH: X86_64 = X86_64;

/// Initializes the architecture's one-time boot state.
///
/// Called by the kernel after the console is up. It installs the per-CPU stacks
/// and the IDT and calibrates the TSC against the PM timer, all of which need to
/// happen before interrupts are unmasked. Install the fault handler first.
pub fn init() {
    gdt::init();
    exceptions::init();
    timer::init();
}

/// Describes the boot information pointer for the boot log.
pub fn boot_info_label(boot_info: usize) -> &'static str {
    if boot_info != 0 {
        "multiboot information"
    } else {
        "no multiboot information"
    }
}

impl Clock for X86_64 {
    fn frequency(&self) -> u64 {
        timer::frequency()
    }

    fn name(&self) -> &'static str {
        "TSC"
    }

    fn ticks(&self) -> u64 {
        timer::ticks()
    }

    fn spin_ms(&self, ms: u64) {
        timer::spin_ms(ms);
    }

    fn idle_ms(&self, ms: u64) {
        timer::idle_ms(ms);
    }
}

impl Power for X86_64 {
    fn system_off(&self) -> ! {
        power::system_off()
    }

    fn system_reset(&self) -> ! {
        power::system_reset()
    }
}

impl Cpu for X86_64 {
    fn current_el(&self) -> u64 {
        cpu::current_el()
    }

    fn midr(&self) -> u64 {
        cpu::midr_el1()
    }

    fn halt(&self) -> ! {
        cpu::halt()
    }

    fn privilege_prefix(&self) -> &'static str {
        "ring "
    }

    fn id_name(&self) -> &'static str {
        "CPUID.0:EAX"
    }
}

impl TrapContext for irq::TrapFrame {
    const ZERO: Self = irq::TrapFrame::ZERO;

    fn new_task(entry: usize, arg: usize, stack_top: usize, return_to: usize) -> Self {
        irq::TrapFrame::new_task(entry, arg, stack_top, return_to)
    }

    fn stack_pointer(&self) -> usize {
        irq::TrapFrame::stack_pointer(self)
    }

    fn interrupt_id(&self) -> Option<u32> {
        irq::TrapFrame::interrupt_id(self)
    }

    fn interrupts_were_enabled(&self) -> bool {
        irq::TrapFrame::interrupts_were_enabled(self)
    }

    fn new_user(entry: usize, stack_top: usize, arg0: usize, arg1: usize) -> Self {
        irq::TrapFrame::new_user(entry, stack_top, arg0, arg1)
    }

    fn is_user(&self) -> bool {
        irq::TrapFrame::is_user(self)
    }

    fn syscall_number(&self) -> usize {
        self.rax as usize
    }

    fn syscall_args(&self) -> [usize; 6] {
        [
            self.rdi as usize,
            self.rsi as usize,
            self.rdx as usize,
            self.r10 as usize,
            self.r8 as usize,
            self.r9 as usize,
        ]
    }

    fn set_syscall_result(&mut self, value: usize) {
        self.rax = value as u64;
    }
}

impl FaultDetails for exceptions::FaultInfo {
    fn is_task_fault(&self) -> bool {
        exceptions::FaultInfo::is_task_fault(self)
    }

    fn from_user(&self) -> bool {
        exceptions::FaultInfo::from_user(self)
    }

    fn fault_address(&self) -> Option<u64> {
        exceptions::FaultInfo::fault_address(self)
    }

    fn syndrome_name(&self) -> &'static str {
        exceptions::FaultInfo::syndrome_name(self)
    }

    fn instruction(&self) -> u64 {
        self.elr
    }

    fn write_report(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(
            out,
            "[fatal] {} / {} (vector {})\n\
             [fatal] error={:#018x} class={:#04x} ({})\n\
             [fatal] ip={:#018x} fault address={:#018x}\n\
             [fatal] flags={:#018x} sp={:#018x}\n",
            self.vector_group(),
            self.vector_type(),
            self.index,
            self.esr,
            self.exception_class(),
            self.syndrome_name(),
            self.elr,
            self.far,
            self.spsr,
            self.sp,
        )?;
        self.write_registers(out)
    }
}

impl Arch for X86_64 {
    const MAX_CPUS: usize = smp::MAX_CPUS;
    type Frame = irq::TrapFrame;
    type Fault = exceptions::FaultInfo;

    fn services() -> &'static Self {
        &ARCH
    }

    fn init() {
        init();
    }

    fn boot_info_label(boot_info: usize) -> &'static str {
        boot_info_label(boot_info)
    }

    fn start_secondaries(
        limit: usize,
        firmware_id: fn(usize) -> u64,
        entry: SecondaryEntry,
    ) -> (usize, Option<i64>) {
        smp::start_secondaries(limit, firmware_id, entry)
    }

    fn cpu_index() -> usize {
        cpu::index()
    }

    fn halt() -> ! {
        cpu::halt()
    }

    fn set_irq_handler(handler: fn(&mut Self::Frame)) {
        irq::set_handler(handler);
    }

    fn set_switch_handler(handler: fn(&mut Self::Frame)) {
        irq::set_switch_handler(handler);
    }

    fn set_syscall_handler(handler: fn(&mut Self::Frame)) {
        irq::set_syscall_handler(handler);
    }

    fn set_fault_handler(handler: fn(&Self::Fault, &mut Self::Frame) -> bool) {
        exceptions::set_handler(handler);
    }

    fn irq_enable() {
        irq::enable();
    }

    fn irq_disable() {
        irq::disable();
    }

    fn irq_save_and_disable() -> u64 {
        irq::save_and_disable()
    }

    fn irq_restore(flags: u64) {
        irq::restore(flags);
    }

    fn irqs_enabled() -> bool {
        irq::is_enabled()
    }

    fn wait_for_interrupt() {
        irq::wait_for_interrupt();
    }

    fn request_switch() {
        irq::request_switch();
    }

    fn set_trap_frame(frame: *mut Self::Frame) {
        irq::set_trap_frame(frame);
    }

    fn resume(frame: *mut Self::Frame) {
        irq::resume(frame);
    }

    fn timer_start(hz: u64) {
        timer::start(hz);
    }

    fn timer_period() -> u64 {
        timer::period()
    }

    fn timer_arm(deadline: u64) {
        timer::arm(deadline);
    }

    fn take_idle_hint() -> bool {
        timer::take_idle_hint()
    }

    unsafe fn map_ram_cached(base: usize, size: usize) {
        // SAFETY: passed on from the caller.
        unsafe { mmu::map_ram_cached(base, size) }
    }

    unsafe fn unmap_range(
        base: usize,
        size: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> bool {
        // SAFETY: passed on from the caller.
        unsafe { mmu::unmap_range(base, size, alloc_table) }
    }

    fn user_page_bits() -> PageBits {
        mmu::user_bits()
    }

    unsafe fn prepare_user_tables(
        tables: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> Option<usize> {
        // SAFETY: passed on from the caller.
        unsafe { mmu::prepare_user_tables(tables, alloc_table) }
    }

    fn activate_user_tables(root: usize, _asid: u16) {
        mmu::activate(root);
    }

    fn flush_user_space(_asid: u16) {
        // Every change of tables flushes: see `mmu::activate`.
    }

    fn flush_user_page(address: usize, _asid: u16) {
        mmu::flush_page(address);
    }

    fn sync_user_tables() {}

    fn sync_user_code(_address: usize, _length: usize) {}

    fn protection_summary() -> &'static str {
        match (cpu::has_nx(), cpu::has_smep_smap()) {
            (true, (true, true)) => "NX, SMEP, SMAP",
            (true, (true, false)) => "NX, SMEP (no SMAP on this CPU)",
            (true, _) => "NX (no SMEP or SMAP on this CPU)",
            (false, _) => "no NX, SMEP or SMAP on this CPU",
        }
    }
}
