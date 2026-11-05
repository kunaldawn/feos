//! CPU-wide operations available from Rust.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::smp::MAX_CPUS;

/// Stops the CPU until an event occurs, forever.
///
/// This is the kernel's idle loop and the final state of every fatal path.
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are available at ring 0.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

/// Current privilege level: the kernel's own, as this only runs in the kernel.
pub fn current_el() -> u64 {
    let cs: u16;
    // SAFETY: reading a segment register has no side effects.
    unsafe { asm!("mov {:x}, cs", out(reg) cs, options(nomem, nostack)) };
    (cs & 0x3) as u64
}

/// One CPU's private data, reached through the GS base.
#[repr(C, align(64))]
pub struct CpuArea {
    /// Logical index of this CPU. Must stay the first field: see [`index`].
    index: AtomicU64,
    /// Top of this CPU's interrupt stack. `interrupts.S` reads this at offset 8.
    interrupt_stack: AtomicU64,
    /// The frame a handler asked to resume, or zero for the one it interrupted.
    resume: AtomicU64,
}

/// `boot.S` points the boot CPU's GS base here before calling any Rust.
#[no_mangle]
static CPU_AREAS: [CpuArea; MAX_CPUS] = [const {
    CpuArea {
        index: AtomicU64::new(0),
        interrupt_stack: AtomicU64::new(0),
        resume: AtomicU64::new(0),
    }
}; MAX_CPUS];

const MSR_GS_BASE: u32 = 0xc000_0101;

/// Logical index of the CPU running this code.
///
/// A task can migrate between any two instructions unless interrupts are masked,
/// so an answer is only still true with them masked. This read has side effects
/// as far as the compiler knows, so it isn't cached across a preemption.
pub fn index() -> usize {
    let value: u64;
    // SAFETY: GS points at this CPU's `CpuArea` from before the first Rust code,
    // and `index` is its first field.
    unsafe { asm!("mov {}, gs:[0]", out(reg) value, options(nostack, readonly, preserves_flags)) };
    value as usize
}

/// This CPU's private data. The answer is only still true with interrupts masked,
/// or from a handler.
pub(crate) fn area() -> &'static CpuArea {
    &CPU_AREAS[index().min(MAX_CPUS - 1)]
}

impl CpuArea {
    pub(crate) fn set_interrupt_stack(&self, top: u64) {
        self.interrupt_stack.store(top, Ordering::Relaxed);
    }

    pub(crate) fn set_resume(&self, frame: u64) {
        self.resume.store(frame, Ordering::Relaxed);
    }

    pub(crate) fn take_resume(&self) -> u64 {
        self.resume.swap(0, Ordering::Relaxed)
    }
}

/// Assigns the logical CPU index and points GS at this CPU's area.
pub fn set_index(index: usize) {
    let area = &CPU_AREAS[index.min(MAX_CPUS - 1)];
    area.index.store(index as u64, Ordering::Relaxed);
    let address = area as *const CpuArea as u64;
    // SAFETY: the area is static, and the MSR only affects `gs:` accesses.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") MSR_GS_BASE,
            in("eax") address as u32,
            in("edx") (address >> 32) as u32,
            options(nostack, preserves_flags)
        );
    }
}

/// CPU identification value exposed through the generic HAL `Cpu` trait.
///
/// On x86 this is the maximum basic CPUID leaf, which tells diagnostics that
/// the CPU answered CPUID even when no model name is decoded.
pub fn midr_el1() -> u64 {
    core::arch::x86_64::__cpuid(0).eax as u64
}

const MSR_EFER: u32 = 0xc000_0080;
const EFER_NXE: u32 = 1 << 11;
const CR4_UMIP: u64 = 1 << 11;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

static HAS_NX: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static HAS_SMEP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static HAS_SMAP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Turns on what this CPU has for keeping the kernel and programs apart. Call once on
/// each, before anything runs from ring 3.
///
/// SMAP is safe to leave on all the time: the kernel never touches a user page at
/// its user address, as `feos-vm` explains, so it never needs to lift it.
pub fn protect() {
    let extended = core::arch::x86_64::__cpuid(0x8000_0001);
    let nx = extended.edx & (1 << 20) != 0;
    let highest = core::arch::x86_64::__cpuid(0).eax;
    let features = if highest >= 7 {
        core::arch::x86_64::__cpuid_count(7, 0)
    } else {
        core::arch::x86_64::CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 }
    };
    let smep = features.ebx & (1 << 7) != 0;
    let smap = features.ebx & (1 << 20) != 0;
    let umip = features.ecx & (1 << 2) != 0;

    // SAFETY: at ring 0, these only narrow what's allowed: nothing is mapped for user
    // code yet and no page has NX set until this is done.
    unsafe {
        if nx {
            let (low, high): (u32, u32);
            asm!("rdmsr", in("ecx") MSR_EFER, out("eax") low, out("edx") high, options(nomem, nostack));
            asm!(
                "wrmsr",
                in("ecx") MSR_EFER,
                in("eax") low | EFER_NXE,
                in("edx") high,
                options(nostack, preserves_flags)
            );
        }
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        if smep {
            cr4 |= CR4_SMEP;
        }
        if smap {
            cr4 |= CR4_SMAP;
        }
        if umip {
            cr4 |= CR4_UMIP;
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nostack));
    }
    HAS_NX.store(nx, Ordering::Relaxed);
    HAS_SMEP.store(smep, Ordering::Relaxed);
    HAS_SMAP.store(smap, Ordering::Relaxed);
}

/// Whether page entries may say no-execute.
pub fn has_nx() -> bool {
    HAS_NX.load(Ordering::Relaxed)
}

/// Whether SMEP and SMAP are on.
pub fn has_smep_smap() -> (bool, bool) {
    (HAS_SMEP.load(Ordering::Relaxed), HAS_SMAP.load(Ordering::Relaxed))
}
