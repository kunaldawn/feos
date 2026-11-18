//! AArch64 architecture support.
//!
//! This crate contains only code that is specific to the AArch64 CPU: the
//! early boot entry point, exception vectors, PSCI calls and the generic
//! timer. It knows nothing about boards, drivers, apps or the console.
//!
//! [`ARCH`] implements the system-service traits from `feos-hal`, so the
//! kernel can hand one static implementation to every app without apps
//! depending on this crate.
//!
//! A port to another architecture adds a sibling crate (`arch/riscv64`,
//! `arch/x86_64`, ...) with the same surface and a board crate that selects
//! it.

#![no_std]

use core::fmt;

use feos_hal::{
    Arch, Clock, Cpu, FaultDetails, FirmwareConduit, PageBits, Power, SecondaryEntry, TrapContext,
};

pub mod boot;
pub mod cpu;
pub mod exceptions;
pub mod irq;
pub mod mmu;
pub mod psci;
pub mod smp;
pub mod timer;

/// The AArch64 implementation of the kernel's system services.
#[derive(Clone, Copy, Debug, Default)]
pub struct Aarch64;

/// Shared instance of the AArch64 system services.
pub static ARCH: Aarch64 = Aarch64;

/// One-time architecture initialization.
///
/// The AArch64 boot stub already set up the CPU and stacks. This installs the
/// exception vectors, so install the fault handler first.
pub fn init() {
    exceptions::init();
    cpu::protect();
}

/// Describes the boot information pointer for the boot log.
pub fn boot_info_label(boot_info: usize) -> &'static str {
    // SAFETY: QEMU passes the address of the device tree in RAM, or zero.
    if boot_info != 0 && unsafe { core::ptr::read_volatile(boot_info as *const u32) } == 0xedfe_0dd0
    {
        "device tree blob"
    } else {
        "no device tree pointer"
    }
}

impl Clock for Aarch64 {
    fn frequency(&self) -> u64 {
        timer::frequency()
    }

    fn name(&self) -> &'static str {
        "generic timer"
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

impl Power for Aarch64 {
    fn system_off(&self) -> ! {
        psci::system_off()
    }

    fn system_reset(&self) -> ! {
        psci::system_reset()
    }
}

impl Cpu for Aarch64 {
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
        "EL"
    }

    fn id_name(&self) -> &'static str {
        "MIDR_EL1"
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
        self.x[8] as usize
    }

    fn syscall_args(&self) -> [usize; 6] {
        [
            self.x[0] as usize,
            self.x[1] as usize,
            self.x[2] as usize,
            self.x[3] as usize,
            self.x[4] as usize,
            self.x[5] as usize,
        ]
    }

    fn set_syscall_result(&mut self, value: usize) {
        self.x[0] = value as u64;
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

    fn memory_fault(&self) -> Option<feos_hal::MemoryFault> {
        exceptions::FaultInfo::memory_fault(self)
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

impl Arch for Aarch64 {
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

    fn set_firmware_conduit(conduit: FirmwareConduit) {
        psci::set_conduit(conduit);
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

    unsafe fn unmap_range(
        base: usize,
        size: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> bool {
        // SAFETY: passed on from the caller.
        unsafe { mmu::unmap_range(base, size, alloc_table) }
    }

    unsafe fn reserve_kernel_window(
        base: usize,
        size: usize,
        alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> bool {
        // SAFETY: passed on from the caller.
        unsafe { mmu::reserve_kernel_window(base, size, alloc_table) }
    }

    unsafe fn map_kernel_page(address: usize, frame: usize) {
        // SAFETY: passed on from the caller.
        unsafe { mmu::map_kernel_page(address, frame) }
    }

    unsafe fn unmap_kernel_page(address: usize) {
        // SAFETY: passed on from the caller.
        unsafe { mmu::unmap_kernel_page(address) }
    }

    fn user_page_bits() -> PageBits {
        mmu::USER_BITS
    }

    fn kernel_map_end() -> usize {
        mmu::kernel_map_end()
    }

    unsafe fn prepare_user_tables(
        tables: usize,
        _alloc_table: &mut dyn FnMut() -> Option<usize>,
    ) -> Option<usize> {
        // SAFETY: passed on from the caller.
        Some(unsafe { mmu::prepare_user_tables(tables) })
    }

    fn activate_user_tables(root: usize, asid: u16) {
        mmu::activate(root, asid);
    }

    fn user_asid_bits() -> u32 {
        mmu::asid_bits()
    }

    // Every invalidation is the inner shareable kind, which every CPU carries out.
    const USER_FLUSH_BROADCAST: bool = true;

    fn flush_user_space(asid: u16) {
        mmu::flush_space(asid);
    }

    fn flush_user_page(address: usize, asid: u16) {
        mmu::flush_page(address, asid);
    }

    fn flush_user_local() {
        mmu::flush_local();
    }

    fn flush_user_all() {
        mmu::flush_all();
    }

    unsafe fn read_user_word(address: usize) -> u64 {
        // SAFETY: passed on from the caller.
        unsafe { mmu::read_user_word(address) }
    }

    fn sync_user_tables() {
        mmu::sync_tables();
    }

    fn sync_user_code(address: usize, length: usize) {
        mmu::sync_code(address, length);
    }

    fn set_user_tls(value: usize) {
        cpu::set_user_tls(value);
    }

    fn user_tls() -> usize {
        cpu::user_tls()
    }

    fn user_hwcaps() -> (u64, u64) {
        cpu::user_hwcaps()
    }

    // `msr tpidr_el0` is an EL0 instruction.
    const USER_TLS_WRITABLE: bool = true;

    fn protection_summary() -> &'static str {
        match (cpu::has_pan(), mmu::asid_bits()) {
            (true, 16) => "execute-never, 16-bit ASIDs, PAN",
            (true, _) => "execute-never, 8-bit ASIDs, PAN",
            (false, 16) => "execute-never, 16-bit ASIDs (this CPU has no PAN)",
            (false, _) => "execute-never, 8-bit ASIDs (this CPU has no PAN)",
        }
    }
}
