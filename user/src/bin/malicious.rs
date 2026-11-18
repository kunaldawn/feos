//! Tries everything a program shouldn't be able to do, and reports on each.
//!
//! Anything that would fault runs in a child of its own, since a fault ends the
//! process: `malicious --probe NAME`. A child that gets past its probe says so and
//! exits with a status the parent counts as a failure. Bad arguments to system calls
//! don't end anything, so the parent makes those calls itself and checks the error.
//!
//! Exits zero only when every probe was blocked.
//!
//! Three more modes are children for kernel tests to kill, over and over, which a program
//! can't do itself until there's a call for it: `--call-loop` keeps the kernel busy with
//! `spawn`, `write` and `read`, `--fault-loop` with page faults, and `--dirty-storm PATH`
//! dirties every page of a shared mapping of `PATH` in a loop, for `ktest dirty-storm`.
//!
//! Run by `ktest drive /bin/malicious`, it's also told the machine's free frames, and
//! checks that what the mapping probes took comes back.
//!
//! `--thread-bomb --hold` starts threads until it can't, and then waits to be killed: for
//! a boot test to check the shell can still `ps` and `kill` it. With 1024 task slots and
//! 64 kept for the kernel, that's at 960 tasks machine-wide. `--uid-thread-bomb` does the
//! same as a user other than root, and stops at that user's cap instead. `--setuid-race`
//! starts children while another of its threads gives up root, so that some start
//! across the change: each must count against the caps as the user it runs as.
//!
//! `--pipe-exhaustion` makes pipes until it may make no more, handing each to a child
//! (`--pipe-wait`) since it has too few descriptors to keep them, and checks another
//! process can still make one. `--uid-process-bomb` starts programs as a user other than
//! root until that user's process cap, each waiting on one pipe.
//!
//! `--lock-exhaustion` takes as many record locks as a process may, and one more, and has
//! `--lock-one` lock beside it. `--lock-hold PATH` and `--setlkw-wait PATH` are for
//! `ktest setlkw-killed`: one holds a write lock over `PATH` and the other waits for one
//! in `F_SETLKW` until it's killed.

#![no_std]
#![no_main]

use feos_user::abi::{errno, madv, map, nr, open as flags, prot, UserStr, MAX_VMAS, PAGE_SIZE};
use feos_user::abi::INHERIT;
use feos_user::{
    close, entry, free_frames, getpid, mmap, mmap2, open, pipe, println, read, spawn, spawn_io, syscall, thread, wait,
    Args,
};

/// Where the kernel's image is, and a device, on this machine.
#[cfg(target_arch = "aarch64")]
const KERNEL: usize = 0x4020_0000;
#[cfg(target_arch = "aarch64")]
const DEVICE: usize = 0x0900_0000;
#[cfg(target_arch = "x86_64")]
const KERNEL: usize = 0x0200_0000;
#[cfg(target_arch = "x86_64")]
const DEVICE: usize = 0xfee0_0000;
/// PCI configuration space as the kernel maps it: on AArch64 the ECAM moved below the
/// user window, on x86_64 q35's.
#[cfg(target_arch = "aarch64")]
const ECAM: usize = 0x0f_d000_0000;
#[cfg(target_arch = "x86_64")]
const ECAM: usize = 0xb000_0000;
/// The top half of the address space.
const HIGH: usize = 0xffff_ff80_0000_0000;
/// Not a canonical address on x86_64, past the translation range on AArch64.
const NONCANONICAL: usize = 0x8000_0000_0000_0000;
/// The top of the main stack: the bottom of the argument area above it.
const STACK_TOP: usize = feos_user::abi::USER_STACK_TOP - feos_user::abi::USER_ARGS_SIZE;

/// What a child's probe has to end with to count as blocked.
const FAULTED: isize = 139;
/// What a child reports having got past its probe.
const ESCAPED: i32 = 77;

fn read_at(address: usize) -> u64 {
    // SAFETY: not at all, and that's the point.
    unsafe { core::ptr::read_volatile(address as *const u64) }
}

fn write_at(address: usize) {
    // SAFETY: not at all, and that's the point.
    unsafe { core::ptr::write_volatile(address as *mut u64, 0xdead_beef_dead_beef) };
}

fn jump_to(address: usize) {
    // SAFETY: not at all, and that's the point.
    let target: extern "C" fn() = unsafe { core::mem::transmute(address) };
    target();
}

#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(depth: usize) -> usize {
    let mut frame = [depth; 256];
    frame[depth % 256] = depth + 1;
    core::hint::black_box(&mut frame);
    // Not a tail call, as the result is used.
    recurse(depth + 1) + core::hint::black_box(frame[0])
}

/// A word in this program's own writable data, holding code bytes that would do
/// nothing harmful if they ran.
#[cfg(target_arch = "aarch64")]
static DATA_CODE: [u32; 2] = [0xd503_201f, 0xd65f_03c0]; // nop; ret
#[cfg(target_arch = "x86_64")]
static DATA_CODE: [u32; 2] = [0x9090_c390, 0];

// A page of code of its own, page aligned and padded to the next page, that nothing runs
// but the probes that change it: `ret`, and nothing else.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text.victim, \"ax\"",
    ".balign 4096",
    ".global victim_page",
    "victim_page:",
    "    ret",
    ".balign 4096",
);
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text.victim, \"ax\"",
    ".balign 4096",
    ".global victim_page",
    "victim_page:",
    "    ret",
    ".balign 4096",
);

extern "C" {
    fn victim_page();
}

fn victim() -> usize {
    victim_page as *const () as usize
}

/// Code made writable stops being executable, and once written can't be made executable
/// again. Zero if that's how it went.
fn code_rewrite() -> i32 {
    let page = victim();
    // SAFETY: the victim page is `ret` and nothing else.
    unsafe { victim_page() };
    let writable = feos_user::mprotect(page, PAGE_SIZE, prot::READ | prot::WRITE);
    if writable != 0 {
        println!("probe mprotect-own-code: making code writable said {writable}");
        return ESCAPED;
    }
    // SAFETY: just made writable, and nothing runs it.
    unsafe { core::ptr::write_volatile(page as *mut u8, DATA_CODE[0] as u8) };
    let back = feos_user::mprotect(page, PAGE_SIZE, prot::READ | prot::EXEC);
    if back != -errno::EACCES {
        println!("probe mprotect-own-code: making written code executable said {back}");
        return ESCAPED;
    }
    0
}

/// Makes and unmaps one-page mappings until the cap, and says whether it was where it
/// should be, with `ENOMEM`, and everything went again. Zero if so.
fn vma_exhaustion() -> i32 {
    let mut first = 0;
    let mut made = 0usize;
    let result = loop {
        let at = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
        if at < 0 {
            break at;
        }
        if made == 0 {
            first = at as usize;
        }
        made += 1;
        if made > MAX_VMAS {
            break 0;
        }
    };
    // The program's own image and stack are a few of the cap.
    let at_cap = result == -errno::ENOMEM && made + 16 >= MAX_VMAS && made < MAX_VMAS;
    // One page and a guard each, from the first: one call takes them all out.
    let gone = feos_user::munmap(first, made * 2 * PAGE_SIZE);
    let again = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    println!("probe vma-exhaustion: made={made} then={result} munmap={gone} again={again:#x}");
    if at_cap && gone == 0 && again as usize == first {
        0
    } else {
        ESCAPED
    }
}

/// Shared reservations no access can touch, each cut down to a page so that only a
/// remnant keeps its object: the kernel heap the objects hold is budgeted, so this runs
/// out with `ENOMEM` long before the VMAs do, and private mappings still work. Zero if so.
fn shared_remnants() -> i32 {
    const BIG: usize = 2 << 30;
    let mut made = 0usize;
    let result = loop {
        let at = mmap2(0, BIG, prot::NONE, map::SHARED | map::ANONYMOUS);
        if at < 0 {
            break at;
        }
        made += 1;
        if feos_user::munmap(at as usize + PAGE_SIZE, BIG - PAGE_SIZE) != 0 || made >= MAX_VMAS {
            break 0;
        }
    };
    let again = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    println!("probe shared-remnants: made={made} then={result} again={again:#x}");
    if result == -errno::ENOMEM && made < 1024 && again > 0 {
        0
    } else {
        ESCAPED
    }
}

/// One-page shared mappings, each its own object, until the VMAs run out, every one
/// written; then one more shared mapping, which has to be refused rather than stop the
/// machine. Several of these at once mustn't use up the kernel's heap. Zero if it got
/// to the end; running out of frames on the way is `SIGBUS`, which is fine too.
fn shared_flood() -> i32 {
    let mut made = 0usize;
    let result = loop {
        let at = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::SHARED | map::ANONYMOUS);
        if at < 0 {
            break at;
        }
        write_at(at as usize);
        made += 1;
        if made > MAX_VMAS {
            break 0;
        }
    };
    let more = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::SHARED | map::ANONYMOUS);
    println!("probe shared-flood: made={made} then={result} more={more}");
    if result == -errno::ENOMEM && more == -errno::ENOMEM {
        0
    } else {
        ESCAPED
    }
}

fn sp_in_kernel() {
    // Fault with the stack pointer aimed at the kernel: where a trap saves mustn't
    // depend on this.
    // SAFETY: not at all, and that's the point. The fault ends the process.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("mov sp, {0}", "str xzr, [sp]", in(reg) KERNEL + 0x1000);
    }
    // SAFETY: as above.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov rsp, {0}", "push rax", in(reg) KERNEL + 0x1000);
    }
}

/// A system call with the stack pointer aimed at the kernel, which must neither use it nor
/// mind. The call should work, and nothing else should happen.
fn syscall_on_kernel_sp() -> i32 {
    let result: isize;
    // SAFETY: the stack pointer comes straight back, and nothing here uses the stack.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!(
            "mov {saved}, sp", "mov sp, {bad}", "svc #0", "mov sp, {saved}",
            saved = out(reg) _,
            bad = in(reg) KERNEL + 0x1000,
            in("x8") nr::GETPID,
            lateout("x0") result,
        );
    }
    // SAFETY: as above.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "mov {saved}, rsp", "mov rsp, {bad}", "int 0xf0", "mov rsp, {saved}",
            saved = out(reg) _,
            bad = in(reg) KERNEL + 0x1000,
            inlateout("rax") nr::GETPID as isize => result,
        );
    }
    if result == getpid() {
        0
    } else {
        ESCAPED
    }
}

/// Makes the machine's privileged state do something, or tries to.
#[cfg(target_arch = "aarch64")]
fn privileged(name: &str) -> bool {
    // SAFETY: not at all, and that's the point: each of these should fault at EL0.
    unsafe {
        use core::arch::asm;
        match name {
            "mask-interrupts" => asm!("msr daifset, #2"),
            "read-page-table-base" => asm!("mrs {}, ttbr0_el1", out(reg) _),
            "write-page-table-base" => asm!("msr ttbr0_el1, xzr"),
            "write-vector-base" => asm!("msr vbar_el1, xzr"),
            "read-system-control" => asm!("mrs {}, sctlr_el1", out(reg) _),
            "invalidate-tlb" => asm!("tlbi vmalle1"),
            "invalidate-caches" => asm!("ic iallu"),
            "exception-return" => asm!("eret"),
            "wait-for-interrupt" => asm!("wfi"),
            "hypervisor-call" => asm!("hvc #0"),
            "secure-monitor-call" => asm!("smc #0"),
            "read-counter" => asm!("mrs {}, cntpct_el0", out(reg) _),
            "zero-a-cache-line" => asm!("dc zva, {}", in(reg) KERNEL),
            // Cache maintenance by address is the kernel's: nothing generates code.
            "ic-ivau-traps" => asm!("ic ivau, {}", in(reg) victim()),
            _ => return false,
        }
    }
    true
}

#[cfg(target_arch = "aarch64")]
const PRIVILEGED: &[&str] = &[
    "mask-interrupts", "read-page-table-base", "write-page-table-base", "write-vector-base",
    "read-system-control", "invalidate-tlb", "invalidate-caches", "exception-return",
    "wait-for-interrupt", "hypervisor-call", "secure-monitor-call", "read-counter",
    "zero-a-cache-line", "ic-ivau-traps",
];

/// Makes the machine's privileged state do something, or tries to.
#[cfg(target_arch = "x86_64")]
fn privileged(name: &str) -> bool {
    // SAFETY: not at all, and that's the point: each of these should fault at ring 3.
    unsafe {
        use core::arch::asm;
        match name {
            "mask-interrupts" => asm!("cli"),
            "halt" => asm!("hlt"),
            "read-page-table-base" => asm!("mov {}, cr3", out(reg) _),
            "write-page-table-base" => asm!("mov cr3, {}", in(reg) 0usize),
            "write-msr" => asm!("wrmsr", in("ecx") 0xc000_0080u32, in("eax") 0, in("edx") 0),
            "read-msr" => asm!("rdmsr", in("ecx") 0xc000_0080u32, out("eax") _, out("edx") _),
            "port-in" => asm!("in al, dx", in("dx") 0x3f8u16, out("al") _),
            "port-out" => asm!("out dx, al", in("dx") 0xcf9u16, in("al") 6u8),
            "load-gdt" => asm!("lgdt [{}]", in(reg) KERNEL),
            "load-idt" => asm!("lidt [{}]", in(reg) KERNEL),
            "swap-gs" => asm!("swapgs"),
            "invalidate-tlb" => asm!("invlpg [{}]", in(reg) KERNEL),
            "software-switch-gate" => asm!("int 48"),
            "unassigned-gate" => asm!("int 0x80"),
            "interrupt-return" => asm!("iretq"),
            // Only blocked where the CPU has UMIP: see `has_umip`.
            "umip-sgdt" => {
                let mut table = [0u8; 16];
                asm!("sgdt [{}]", in(reg) table.as_mut_ptr());
            }
            _ => return false,
        }
    }
    true
}

#[cfg(target_arch = "x86_64")]
const PRIVILEGED: &[&str] = &[
    "mask-interrupts", "halt", "read-page-table-base", "write-page-table-base", "write-msr",
    "read-msr", "port-in", "port-out", "load-gdt", "load-idt", "swap-gs", "invalidate-tlb",
    "software-switch-gate", "unassigned-gate", "interrupt-return",
];

/// Whether `sgdt` and the like fault at ring 3 on this CPU, as the kernel turns UMIP on
/// wherever there is one.
#[cfg(target_arch = "x86_64")]
fn has_umip() -> bool {
    core::arch::x86_64::__cpuid_count(7, 0).ecx & (1 << 2) != 0
}

/// Probes that end the process, apart from `PRIVILEGED`.
const FAULTS: &[&str] = &[
    "kernel-read", "kernel-write", "kernel-exec", "device-read", "device-write", "null-read",
    "null-write", "high-read", "high-write", "noncanonical-read", "own-code-write",
    "own-rodata-write", "data-exec", "stack-exec", "mmap-exec", "use-after-munmap", "past-the-stack",
    "stack-overflow", "sp-in-kernel", "mprotect-read-only-write", "mprotect-none-read", "mmap-none-read",
    "lazy-write-after-read-only", "munmap-own-code", "munmap-own-stack", "mprotect-own-stack",
    "munmap-own-args", "mprotect-own-args",
    "mmap2-noreserve-exec", "mmap2-none-read", "ecam-read", "stack-guard-read",
];

/// Runs one probe, in a child. Returns only if it wasn't stopped.
fn probe(name: &str) -> i32 {
    match name {
        "kernel-read" => {
            core::hint::black_box(read_at(KERNEL));
        }
        "kernel-write" => write_at(KERNEL),
        "kernel-exec" => jump_to(KERNEL),
        "device-read" => {
            core::hint::black_box(read_at(DEVICE));
        }
        "device-write" => write_at(DEVICE),
        "ecam-read" => {
            core::hint::black_box(read_at(ECAM));
        }
        "stack-guard-read" => {
            // The first byte below the 8 MiB stack, where the guard starts.
            let stack_low = STACK_TOP - feos_user::abi::USER_STACK_SIZE;
            core::hint::black_box(read_at(stack_low - 1));
        }
        "null-read" => {
            core::hint::black_box(read_at(core::hint::black_box(0)));
        }
        "null-write" => write_at(core::hint::black_box(0)),
        "high-read" => {
            core::hint::black_box(read_at(HIGH));
        }
        "high-write" => write_at(HIGH),
        "noncanonical-read" => {
            core::hint::black_box(read_at(NONCANONICAL));
        }
        "own-code-write" => write_at(probe as *const () as usize),
        "own-rodata-write" => write_at(DATA_CODE.as_ptr() as usize),
        "data-exec" => jump_to(DATA_CODE.as_ptr() as usize),
        "stack-exec" => {
            let code = DATA_CODE;
            jump_to(core::hint::black_box(&code).as_ptr() as usize);
        }
        "mmap-exec" => {
            let page = mmap(4096, prot::READ | prot::WRITE) as usize;
            // SAFETY: just mapped, and writable.
            unsafe { (page as *mut [u32; 2]).write_volatile(DATA_CODE) };
            jump_to(page);
        }
        "use-after-munmap" => {
            let page = mmap(4096, prot::READ | prot::WRITE) as usize;
            write_at(page);
            feos_user::munmap(page, 4096);
            write_at(page);
        }
        "mprotect-read-only-write" => {
            let page = mmap(4096, prot::READ | prot::WRITE) as usize;
            write_at(page);
            feos_user::mprotect(page, 4096, prot::READ);
            write_at(page);
        }
        "mprotect-none-read" => {
            let page = mmap(4096, prot::READ | prot::WRITE) as usize;
            write_at(page);
            feos_user::mprotect(page, 4096, 0);
            core::hint::black_box(read_at(page));
        }
        "mmap-none-read" => {
            let page = mmap(4096, 0) as usize;
            core::hint::black_box(read_at(page));
        }
        "lazy-write-after-read-only" => {
            // Read-only, and never touched: the first touch is the write.
            let page = mmap(4096, prot::READ) as usize;
            write_at(page);
        }
        "munmap-own-code" => {
            // It goes, and the next call into it faults: the parent survives.
            let gone = feos_user::munmap(victim(), PAGE_SIZE);
            println!("probe munmap-own-code: munmap={gone}");
            // SAFETY: not at all, and that's the point.
            unsafe { victim_page() };
        }
        "munmap-own-stack" => {
            // The stack's top page, just below the argument area.
            let top = STACK_TOP - PAGE_SIZE;
            feos_user::munmap(top, PAGE_SIZE);
            core::hint::black_box(read_at(top));
        }
        "mprotect-own-stack" => {
            let top = STACK_TOP - PAGE_SIZE;
            feos_user::mprotect(top, PAGE_SIZE, prot::NONE);
            core::hint::black_box(read_at(top));
        }
        "munmap-own-args" => {
            // The argument area's top page, which holds the argument strings.
            let top = feos_user::abi::USER_STACK_TOP - PAGE_SIZE;
            feos_user::munmap(top, PAGE_SIZE);
            core::hint::black_box(read_at(top));
        }
        "mprotect-own-args" => {
            let top = feos_user::abi::USER_STACK_TOP - PAGE_SIZE;
            feos_user::mprotect(top, PAGE_SIZE, prot::NONE);
            core::hint::black_box(read_at(top));
        }
        "mmap2-noreserve-exec" => {
            // Writable without a promise, written, then jumped to: never runnable.
            let page = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE | map::NORESERVE) as usize;
            // SAFETY: just mapped, and writable.
            unsafe { (page as *mut [u32; 2]).write_volatile(DATA_CODE) };
            jump_to(page);
        }
        "mmap2-none-read" => {
            let page = mmap2(0, PAGE_SIZE, prot::NONE, map::PRIVATE) as usize;
            core::hint::black_box(read_at(page));
        }
        "past-the-stack" => {
            core::hint::black_box(read_at(feos_user::abi::USER_STACK_TOP));
        }
        "stack-overflow" => {
            core::hint::black_box(recurse(0));
        }
        "sp-in-kernel" => sp_in_kernel(),
        other => {
            if !privileged(other) {
                println!("malicious: no probe called {other}");
                return 2;
            }
        }
    }
    println!("probe {name}: got through");
    ESCAPED
}

/// Makes `path` `pages` pages long, of `byte`.
fn make_file(path: &str, pages: usize, byte: u8) -> bool {
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        return false;
    }
    let chunk = [byte; 512];
    let mut good = true;
    for _ in 0..pages * PAGE_SIZE / chunk.len() {
        good &= feos_user::write(fd as usize, &chunk) == chunk.len() as isize;
    }
    close(fd as usize);
    good
}

/// Shared writable mappings of `path`: only from a file open for writing, made so at the
/// start or later, and never beside a mapping that runs it.
fn shared_write_probes(tally: &mut Tally, path: &str) {
    const RW: usize = prot::READ | prot::WRITE;
    let reader = open(path, flags::READ) as usize;
    tally.call("mmap-file-shared-write", nr::MMAP2, [0, 4096, RW, map::SHARED, reader, 0], errno::EACCES);
    let at = feos_user::mmap2_file(0, PAGE_SIZE, prot::READ, map::SHARED, reader, 0);
    let result = if at > 0 { feos_user::mprotect(at as usize, PAGE_SIZE, RW) } else { at };
    tally.report("mprotect-shared-readonly-fd-write", result == -errno::EACCES, result);
    if at > 0 {
        feos_user::munmap(at as usize, PAGE_SIZE);
    }
    // A writable shared mapping counts as a writable open, after its fd is closed too:
    // the file can't be mapped to run meanwhile.
    let both = open(path, flags::READ | flags::WRITE);
    let writable = feos_user::mmap2_file(0, PAGE_SIZE, RW, map::SHARED, both as usize, 0);
    close(both as usize);
    let exec = feos_user::mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, reader, 0);
    tally.report("exec-map-of-shared-writable", writable > 0 && exec == -errno::ETXTBSY, exec);
    for at in [exec, writable] {
        if at > 0 {
            feos_user::munmap(at as usize, PAGE_SIZE);
        }
    }
    // And the other way round: a file mapped to run can't be made writable through a
    // shared mapping of it. That mapping's fd was open for writing, then closed, which
    // is the only way to have both: open for writing, the file couldn't be mapped to run.
    let both = open(path, flags::READ | flags::WRITE);
    let shared = feos_user::mmap2_file(0, PAGE_SIZE, prot::READ, map::SHARED, both as usize, 0);
    close(both as usize);
    let exec = feos_user::mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, reader, 0);
    let result = if shared > 0 && exec > 0 { feos_user::mprotect(shared as usize, PAGE_SIZE, RW) } else { -1 };
    tally.report("mmap-shared-write-of-exec-mapped", result == -errno::ETXTBSY, result);
    for at in [exec, shared] {
        if at > 0 {
            feos_user::munmap(at as usize, PAGE_SIZE);
        }
    }
    close(reader);
}

/// File mappings every way they shouldn't be made, and running what's been written.
fn file_probes(tally: &mut Tally, program: &str) {
    const RW: usize = prot::READ | prot::WRITE;
    let path = "/tmp/.malicious-map";
    if !make_file(path, 2, 0x5a) {
        tally.report("mmap-file-setup", false, -1);
        return;
    }
    shared_write_probes(tally, path);
    let write_only = open(path, flags::WRITE);
    tally.call("mmap-writeonly-fd", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, write_only as usize, 0], errno::EACCES);
    close(write_only as usize);
    let reader = open(path, flags::READ) as usize;
    tally.call("mmap-unaligned-offset", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, reader, 100], errno::EINVAL);
    tally.call(
        "mmap-offset-overflow",
        nr::MMAP2,
        [0, 2 * 4096, prot::READ, map::PRIVATE, reader, usize::MAX & !4095],
        errno::EOVERFLOW,
    );
    // Code written into a private mapping of a file can't then be run.
    let at = feos_user::mmap2_file(0, PAGE_SIZE, RW, map::PRIVATE, reader, 0);
    let result = if at > 0 {
        // SAFETY: just mapped, writable and private.
        unsafe { core::ptr::write_volatile(at as *mut u32, DATA_CODE[1]) };
        let result = feos_user::mprotect(at as usize, PAGE_SIZE, prot::READ | prot::EXEC);
        feos_user::munmap(at as usize, PAGE_SIZE);
        result
    } else {
        at
    };
    tally.report("exec-own-written-page", result == -errno::EACCES, result);
    close(reader);

    let pipe = feos_user::pipe();
    if let Ok((reader, writer)) = pipe {
        tally.call("mmap-pipe", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, reader, 0], errno::ENODEV);
        close(reader);
        close(writer);
    } else {
        tally.report("mmap-pipe", false, -1);
    }

    // An fd whose path has become a directory since it was opened. The description holds
    // the file by identity (1c.2), so it maps the unlinked file's own pages, never the
    // directory now at its path.
    let dir = "/tmp/.malicious-dir";
    const MARK: u8 = 0x6d;
    let made = make_file(dir, 1, MARK);
    let fd = open(dir, flags::READ);
    let turned = made && fd >= 0 && feos_user::unlink(dir) == 0 && feos_user::mkdir(dir) == 0;
    if turned {
        let at = feos_user::mmap2_file(0, PAGE_SIZE, prot::READ, map::PRIVATE, fd as usize, 0);
        let byte = if at > 0 {
            // SAFETY: just mapped, readable, one page.
            let byte = unsafe { core::ptr::read_volatile(at as *const u8) };
            feos_user::munmap(at as usize, PAGE_SIZE);
            byte as isize
        } else {
            at
        };
        tally.report("mmap-dir", byte == MARK as isize, byte);
    } else {
        tally.report("mmap-dir", false, fd);
    }
    if fd >= 0 {
        close(fd as usize);
    }
    feos_user::unlink(dir);

    // Devices by paths that don't spell "/dev/": what the node is decides. `/dev/null`
    // isn't a file of pages, and `/dev/zero` is anonymous memory, never code.
    let null = open("//dev/./null", flags::READ);
    if null >= 0 {
        tally.call("mmap-dev-null-alias", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, null as usize, 0], errno::ENODEV);
        close(null as usize);
    } else {
        tally.report("mmap-dev-null-alias", false, null);
    }
    let zero = open("/tmp/../dev/zero", flags::READ);
    if zero >= 0 {
        let exec = prot::READ | prot::EXEC;
        tally.call("mmap-dev-zero-alias-exec", nr::MMAP2, [0, 4096, exec, map::PRIVATE, zero as usize, 0], errno::EACCES);
        close(zero as usize);
    } else {
        tally.report("mmap-dev-zero-alias-exec", false, zero);
    }

    // This program is running from its file: it can't be opened to be written.
    tally.call("write-running-binary", nr::OPEN, [program.as_ptr() as usize, program.len(), flags::WRITE, 0, 0, 0], errno::ETXTBSY);
    feos_user::unlink(path);
}

/// The arguments of `mmap2`, `madvise` and `msync`, every way they can be wrong.
fn mmap2_probes(tally: &mut Tally, file: isize, fd: usize) {
    const ANON: usize = map::PRIVATE | map::ANONYMOUS;
    const RW: usize = prot::READ | prot::WRITE;
    let none = usize::MAX;
    let area = feos_user::abi::USER_MMAP_BASE;
    tally.call("mmap2-anon-exec", nr::MMAP2, [0, 4096, prot::READ | prot::EXEC, ANON, none, 0], errno::EACCES);
    tally.call("mmap2-write-exec", nr::MMAP2, [0, 4096, RW | prot::EXEC, ANON, none, 0], errno::EACCES);
    tally.call("mmap2-unknown-prot", nr::MMAP2, [0, 4096, prot::READ | 8, ANON, none, 0], errno::EINVAL);
    tally.call("mmap2-unknown-flag", nr::MMAP2, [0, 4096, RW, ANON | 0x40, none, 0], errno::EINVAL);
    tally.call("mmap2-growsdown", nr::MMAP2, [0, 4096, RW, ANON | 0x100, none, 0], errno::EINVAL);
    tally.call("mmap2-type-bits", nr::MMAP2, [0, 4096, RW, ANON | 0x4, none, 0], errno::EINVAL);
    // `MAP_SHARED | MAP_PRIVATE` is `MAP_SHARED_VALIDATE`, so the refused mixture is neither.
    tally.call("mmap2-neither-shared-nor-private", nr::MMAP2, [0, 4096, RW, map::ANONYMOUS, none, 0], errno::EINVAL);
    tally.call("mmap2-fixed-and-noreplace", nr::MMAP2, [area, 4096, RW, ANON | map::FIXED | map::FIXED_NOREPLACE, none, 0], errno::EINVAL);
    tally.call("mmap2-anon-fd-5", nr::MMAP2, [0, 4096, RW, ANON, 5, 0], errno::EINVAL);
    tally.call("mmap2-anon-fd-high-bits", nr::MMAP2, [0, 4096, RW, ANON, 0x1_ffff_ffff, 0], errno::EINVAL);
    tally.call("mmap2-anon-offset", nr::MMAP2, [0, 4096, RW, ANON, none, 4096], errno::EINVAL);
    tally.call("mmap2-zero-length", nr::MMAP2, [0, 0, RW, ANON, none, 0], errno::EINVAL);
    tally.call("mmap2-huge", nr::MMAP2, [0, usize::MAX, RW, ANON, none, 0], errno::ENOMEM);
    tally.call("mmap2-bigger-than-the-area", nr::MMAP2, [0, 1 << 40, prot::NONE, ANON, none, 0], errno::ENOMEM);
    // Charged, and more than a process may charge. The boot tests' guests can't promise
    // that much either, and that check comes after the limit's but says the same
    // `ENOMEM`, so this can't tell which refused it: `the_page_limit_is_kept` in
    // `feos-vm` checks the limit itself.
    tally.call("mmap2-charged-past-the-limit", nr::MMAP2, [0, feos_user::abi::USER_MEMORY_LIMIT + (1 << 30), RW, ANON, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-unaligned", nr::MMAP2, [area + 1, 4096, RW, ANON | map::FIXED, none, 0], errno::EINVAL);
    tally.call("mmap2-fixed-outside-window", nr::MMAP2, [feos_user::abi::USER_END, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-below-the-area", nr::MMAP2, [feos_user::abi::USER_BASE, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    // The v2 window's edges: its end, just below its base, the interpreter's area, and
    // the stack's guard.
    tally.call("mmap2-fixed-at-user-end", nr::MMAP2, [feos_user::abi::USER_END - 4096, 8192, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-below-base", nr::MMAP2, [feos_user::abi::USER_BASE - 4096, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-interp-area", nr::MMAP2, [feos_user::abi::USER_INTERP_BASE, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-in-stack-guard", nr::MMAP2, [feos_user::abi::USER_MMAP_END, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-kernel", nr::MMAP2, [KERNEL, 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-wraps", nr::MMAP2, [usize::MAX & !4095, 8192, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-fixed-over-own-code", nr::MMAP2, [victim(), 4096, RW, ANON | map::FIXED, none, 0], errno::ENOMEM);
    tally.call("mmap2-bad-fd", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, 99, 0], errno::EBADF);
    tally.call("mmap2-fd-high-bits", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, 0x1_0000_0000 | fd, 0], errno::EBADF);
    if file >= 0 {
        // A file opened only to read can't be mapped to write and run at once.
        tally.call("mmap2-file-write-exec", nr::MMAP2, [0, 4096, RW | prot::EXEC, map::PRIVATE, fd, 0], errno::EACCES);
    }
    tally.call("mmap2-stdin-enodev", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, 0, 0], errno::ENODEV);

    tally.call("madvise-kernel-address", nr::MADVISE, [KERNEL, 4096, madv::DONTNEED, 0, 0, 0], errno::EINVAL);
    tally.call("madvise-wraps", nr::MADVISE, [usize::MAX & !4095, 8192, madv::DONTNEED, 0, 0, 0], errno::EINVAL);
    let page = mmap2(0, 4096, RW, map::PRIVATE) as usize;
    tally.call("madvise-unaligned", nr::MADVISE, [page + 1, 4096, madv::DONTNEED, 0, 0, 0], errno::EINVAL);
    tally.call("madvise-unknown", nr::MADVISE, [page, 4096, 99, 0, 0, 0], errno::EINVAL);
    tally.call("madvise-unmapped", nr::MADVISE, [page, 3 * 4096, madv::DONTNEED, 0, 0, 0], errno::ENOMEM);
    tally.call("msync-kernel-address", nr::MSYNC, [KERNEL, 4096, feos_user::abi::ms::SYNC, 0, 0, 0], errno::EINVAL);
    tally.call("msync-both", nr::MSYNC, [page, 4096, feos_user::abi::ms::SYNC | feos_user::abi::ms::ASYNC, 0, 0, 0], errno::EINVAL);
    tally.call("msync-unmapped", nr::MSYNC, [page, 3 * 4096, feos_user::abi::ms::SYNC, 0, 0, 0], errno::ENOMEM);
    feos_user::munmap(page, 4096);

    // The window surface is the kernel's to lend: nothing may replace or change it.
    let surface = feos_user::surface_create(8, 8);
    if surface > 0 {
        let at = surface as usize;
        tally.call("mmap2-fixed-over-surface", nr::MMAP2, [at, 4096, RW, ANON | map::FIXED, none, 0], errno::EINVAL);
        tally.call("munmap-surface", nr::MUNMAP, [at, 4096, 0, 0, 0, 0], errno::EINVAL);
        tally.call("mprotect-surface", nr::MPROTECT, [at, 4096, prot::READ, 0, 0, 0], errno::EINVAL);
    } else {
        tally.report("mmap2-fixed-over-surface", false, surface);
    }

    // `mmap` and `munmap` across the window, again and again: the tables they need are
    // made and freed each time, so nothing stays behind. Four places a round, spread
    // over the whole of the area up to its last gigabyte, so the sweep reaches the top
    // of the window without taking a call per gigabyte of it.
    let before = free_frames();
    let span = feos_user::abi::USER_MMAP_END - area;
    let step = (span / 4) & !((1 << 30) - 1);
    let mut failed = 0;
    for round in 0..1000usize {
        let mut offset = (round % 512) * (2 << 20);
        while offset + 4096 <= span {
            let at = area + offset;
            let got = mmap2(at, 4096, RW, map::PRIVATE | map::FIXED);
            if got != at as isize {
                failed += 1;
            } else {
                // SAFETY: just mapped, and writable.
                unsafe { core::ptr::write_volatile(at as *mut u8, 1) };
                failed += usize::from(feos_user::munmap(at, 4096) != 0);
            }
            offset += step;
        }
    }
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after == before);
    tally.report("table-sweep", failed == 0 && back, failed as isize);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe table-sweep: frames {before}->{after}");
    }

    // Every number the design reserves and no stage has given a call yet.
    let mut answered = 0isize;
    for number in (nr::FIRST_UNIMPLEMENTED..=nr::LAST_RESERVED).filter(|&number| nr::is_unimplemented(number)) {
        if syscall(number, [0; 6]) != -errno::ENOSYS {
            answered = number as isize;
        }
    }
    tally.report("unimplemented-numbers-enosys", answered == 0, answered);
}

/// Descriptors: a table that ends at `MAX_FDS`, flags and commands that don't exist,
/// descriptors with garbage above their 32 bits, paths past `PATH_MAX`, and ranges the
/// wrong way round.
fn fd_probes(tally: &mut Tally, program: &str) {
    use feos_user::abi::{close_range as range, f, o, MAX_FDS, PATH_MAX};
    tally.call("dup3-to-1024", nr::DUP3, [0, MAX_FDS, 0, 0, 0, 0], errno::EBADF);
    tally.call("dup3-negative", nr::DUP3, [0, usize::MAX, 0, 0, 0, 0], errno::EBADF);
    tally.call("dup3-same", nr::DUP3, [1, 1, 0, 0, 0, 0], errno::EINVAL);
    tally.call("dup3-flags", nr::DUP3, [1, 50, o::NONBLOCK, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-bad-cmd", nr::FCNTL, [1, 9999, 0, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-cmd-upper-bits", nr::FCNTL, [1, 0x1_0000_0000 | f::GETFD, 0, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-setfl-undefined-bit", nr::FCNTL, [1, f::SETFL, 0o40000000, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-setfl-async", nr::FCNTL, [1, f::SETFL, o::ASYNC, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-setfd-bits", nr::FCNTL, [1, f::SETFD, 2, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-dupfd-past-the-table", nr::FCNTL, [1, f::DUPFD, MAX_FDS, 0, 0, 0], errno::EINVAL);
    tally.call("fcntl-setlk-reads-first", nr::FCNTL, [1, f::SETLK, KERNEL, 0, 0, 0], errno::EFAULT);
    tally.call("fcntl-closed-fd", nr::FCNTL, [900, f::GETFD, 0, 0, 0, 0], errno::EBADF);
    // An fd's upper 32 bits must be nothing or its sign: a valid one with garbage above it
    // is no descriptor, to any call.
    let garbage = 0xdead_0000_0000 | 1;
    tally.call("fd-upper-bits", nr::WRITE, [garbage, KERNEL, 1, 0, 0, 0], errno::EBADF);
    tally.call("fd-upper-bits-fcntl", nr::FCNTL, [garbage, f::GETFD, 0, 0, 0, 0], errno::EBADF);
    tally.call("fd-upper-bits-dup3", nr::DUP3, [garbage, 60, 0, 0, 0, 0], errno::EBADF);
    tally.call("fd-upper-bits-close", nr::CLOSE, [0x1_0000_0000 | 1, 0, 0, 0, 0, 0], errno::EBADF);
    let mut fds = [0i32; 2];
    let out = fds.as_mut_ptr() as usize;
    tally.call("pipe2-flags", nr::PIPE2, [out, o::APPEND, 0, 0, 0, 0], errno::EINVAL);
    tally.call("pipe2-into-kernel", nr::PIPE2, [KERNEL, o::CLOEXEC, 0, 0, 0, 0], errno::EFAULT);
    tally.call("close-range-unshare", nr::CLOSE_RANGE, [3, 10, range::UNSHARE, 0, 0, 0], errno::EINVAL);
    tally.call("close-range-unknown-flag", nr::CLOSE_RANGE, [3, 10, 1, 0, 0, 0], errno::EINVAL);
    tally.call("close-range-reversed", nr::CLOSE_RANGE, [10, 3, 0, 0, 0, 0], errno::EINVAL);
    tally.call("close-range-upper-bits", nr::CLOSE_RANGE, [0xdead_0000_0003, 10, 0, 0, 0, 0], errno::EINVAL);
    // One byte past the longest path there is.
    let mut long = [b'/'; PATH_MAX + 1];
    long[PATH_MAX] = b'x';
    tally.call("path-4097", nr::OPEN, [long.as_ptr() as usize, PATH_MAX + 1, flags::READ, 0, 0, 0], errno::ENAMETOOLONG);
    // The limit is on what's given, relative or not.
    let mut relative = [b'.'; PATH_MAX + 1];
    relative[PATH_MAX] = b'x';
    tally.call("path-4097-relative", nr::OPEN, [relative.as_ptr() as usize, PATH_MAX + 1, flags::READ, 0, 0, 0], errno::ENAMETOOLONG);

    // Every descriptor there may be, and one more, in a child: `EMFILE`, and the machine
    // and this process carry on.
    let child = spawn(program, &[program, "--fd-exhaustion"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let after = pipe().map(|(read_end, write_end)| {
        close(read_end);
        close(write_end);
    });
    tally.report("fd-exhaustion", status == 0 && after.is_ok(), status);

    // Descriptions remembering 4000-byte paths, in several children at once, each until
    // it's refused: `ENOMEM` from its heap budget before its table fills, and the machine,
    // and this process, carry on.
    const HEAP_CHILDREN: usize = 8;
    let mut children = [0isize; HEAP_CHILDREN];
    let made = make_long_path();
    for child in &mut children {
        *child = if made { spawn(program, &[program, "--fd-heap"]) } else { -errno::ENOENT };
    }
    let mut worst = 0;
    for child in children {
        let status = if child > 0 { wait(child as usize) } else { child };
        if status != 0 {
            worst = status;
        }
    }
    let after = pipe().map(|(read_end, write_end)| {
        close(read_end);
        close(write_end);
    });
    tally.report("fd-heap-budget", worst == 0 && after.is_ok(), worst);
    // The file, then each directory from the deepest.
    let mut out = [0u8; 4096];
    let path = long_path(&mut out);
    let mut end = path.len();
    while end > "/tmp".len() {
        feos_user::unlink(&path[..end]);
        end = path[..end].rfind('/').unwrap_or(0);
    }
}

/// The path a thread keeps rewriting during `toctou-openat-path`: `/etc/motd`, and the
/// same with a NUL in place of its `t`.
static FLIPPED_PATH: [core::sync::atomic::AtomicU8; 9] = {
    use core::sync::atomic::AtomicU8;
    let bytes = *b"/etc/motd";
    let mut out = [const { AtomicU8::new(0) }; 9];
    let mut index = 0;
    while index < bytes.len() {
        out[index] = AtomicU8::new(bytes[index]);
        index += 1;
    }
    out
};
static PATH_FLIP_STOP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn run_path_flipper(_: usize) -> usize {
    use core::sync::atomic::Ordering;
    let mut flips = 0;
    while PATH_FLIP_STOP.load(Ordering::Relaxed) == 0 {
        FLIPPED_PATH[7].store(if flips % 2 == 1 { 0 } else { b't' }, Ordering::Relaxed);
        flips += 1;
    }
    FLIPPED_PATH[7].store(b't', Ordering::Relaxed);
    flips
}

/// The path calls of stage 1c.2: descriptors that aren't directories, flags that don't
/// exist, paths that aren't UTF-8, buffers too small or in the kernel, cookies that were
/// never given, the root, the orphans, and a path rewritten while the kernel reads it.
/// `/dev` and `/proc` (stage 1c.3): nothing can be made or taken away in either, root or
/// not, `/dev/shm` aside; another process's descriptors aren't there to find; and with no
/// terminal, `/dev/tty` is `ENXIO`.
fn dev_probes(tally: &mut Tally) {
    use feos_user::abi::{at, o};
    let cwd = at::FDCWD as isize as usize;
    let named = |path: &'static str| (path.as_ptr() as usize, path.len());
    let mut stat = feos_user::abi::Stat2::default();
    let out = &mut stat as *mut feos_user::abi::Stat2 as usize;
    let (p, l) = named("/dev/evil");
    tally.call("dev-create", nr::OPENAT, [cwd, p, l, o::WRONLY | o::CREAT, 0o644, 0], errno::EPERM);
    tally.call("dev-create-exclusive", nr::OPENAT, [cwd, p, l, o::WRONLY | o::CREAT | o::EXCL, 0o644, 0], errno::EPERM);
    tally.call("dev-mkdir", nr::MKDIRAT, [cwd, p, l, 0o755, 0, 0], errno::EPERM);
    tally.call("dev-v1-mkdir", nr::MKDIR, [p, l, 0, 0, 0, 0], errno::EPERM);
    let (p, l) = named("/dev/null");
    tally.call("dev-unlink-null", nr::UNLINKAT, [cwd, p, l, 0, 0, 0], errno::EPERM);
    tally.call("dev-v1-unlink-null", nr::UNLINK, [p, l, 0, 0, 0, 0], errno::EPERM);
    tally.call("dev-chmod-null", nr::CHMOD, [p, l, 0o777, 0, 0, 0], errno::EPERM);
    let (p, l) = named("/dev/stdout");
    tally.call("dev-unlink-link", nr::UNLINKAT, [cwd, p, l, 0, 0, 0], errno::EPERM);
    let (p, l) = named("/dev/fd/evil");
    tally.call("dev-fd-create", nr::OPENAT, [cwd, p, l, o::WRONLY | o::CREAT, 0o644, 0], errno::EPERM);
    let (p, l) = named("/proc/evil");
    tally.call("proc-create", nr::OPENAT, [cwd, p, l, o::WRONLY | o::CREAT, 0o644, 0], errno::EPERM);
    let (p, l) = named("/proc/self/evil");
    tally.call("proc-mkdir", nr::MKDIRAT, [cwd, p, l, 0o755, 0, 0], errno::EPERM);
    let (p, l) = named("/proc/self/exe");
    tally.call("proc-unlink-exe", nr::UNLINKAT, [cwd, p, l, 0, 0, 0], errno::EPERM);
    tally.call("proc-write-exe-link", nr::OPENAT, [cwd, p, l, o::WRONLY | o::NOFOLLOW, 0, 0], errno::ELOOP);
    let (p, l) = named("/proc/self/fd");
    tally.call("proc-rmdir-fd", nr::UNLINKAT, [cwd, p, l, at::REMOVEDIR, 0, 0], errno::EPERM);
    let (p, l) = named("/proc/1/fd/0");
    tally.call("proc-fd-of-other-pid", nr::OPENAT, [cwd, p, l, o::RDONLY, 0, 0], errno::ENOENT);
    let (p, l) = named("/proc/1/exe");
    tally.call("proc-exe-of-other-pid", nr::FSTATAT, [cwd, p, l, out, 0, 0], errno::ENOENT);
    let (p, l) = named("/proc/self/fd/999");
    tally.call("proc-fd-closed", nr::OPENAT, [cwd, p, l, o::RDONLY, 0, 0], errno::ENOENT);
    let (p, l) = named("/proc/self/maps");
    tally.call("proc-maps-missing", nr::OPENAT, [cwd, p, l, o::RDONLY, 0, 0], errno::ENOENT);
    let (p, l) = named("/dev/tty");
    tally.call("dev-tty-without-terminal", nr::OPENAT, [cwd, p, l, o::RDWR, 0, 0], errno::ENXIO);
    tally.call("dev-tty-v1", nr::OPEN, [p, l, flags::READ | flags::WRITE, 0, 0, 0], errno::ENXIO);
    let (p, l) = named("/dev/urandom");
    tally.call("dev-urandom-not-yet", nr::OPENAT, [cwd, p, l, o::RDONLY, 0, 0], errno::ENOENT);
    // `/dev/full` can't be mapped: no device but `/dev/zero` is memory.
    let full = feos_user::openat(at::FDCWD, "/dev/full", o::RDWR, 0);
    let fd = if full >= 0 { full as usize } else { 900 };
    tally.call("dev-full-mmap", nr::MMAP2, [0, PAGE_SIZE, prot::READ, map::PRIVATE, fd, 0], errno::ENODEV);
    tally.call("dev-full-write", nr::WRITE, [fd, out, 8, 0, 0, 0], errno::ENOSPC);
    if full >= 0 {
        close(full as usize);
    }
    // A pipe's end, opened again through its link, can't do what the description can't.
    if let Ok((reader, writer)) = pipe() {
        let link = fd_link_path(reader);
        tally.call("proc-pipe-read-end-for-writing", nr::OPENAT, [cwd, link.0.as_ptr() as usize, link.1, o::WRONLY, 0, 0], errno::EACCES);
        close(reader);
        close(writer);
    }
    long_path_fd_link(tally);
}

/// A file whose canonical path is longer than a link may be (directories of long names
/// in `/tmp`, made relative to each other), opened read-only: its `/proc/self/fd` link
/// must still be a link, one that reaches the description, so opening it for writing is
/// `EACCES`. Were it taken for a file of `/proc`'s own, the open would give a writable
/// descriptor of that.
fn long_path_fd_link(tally: &mut Tally) {
    use feos_user::abi::{at, o};
    const DEPTH: usize = 20;
    let name_bytes = [b'm'; 250];
    let name = core::str::from_utf8(&name_bytes).unwrap_or("m");
    let mut dirs = [0usize; DEPTH + 1];
    let tmp = feos_user::openat(at::FDCWD, "/tmp", o::RDONLY | o::DIRECTORY, 0);
    if tmp < 0 {
        tally.report("proc-fd-long-path-write", false, tmp);
        return;
    }
    dirs[0] = tmp as usize;
    let mut made = 0;
    while made < DEPTH {
        let parent = dirs[made] as i32;
        let created = feos_user::mkdirat(parent, name, 0o700);
        let opened = if created == 0 { feos_user::openat(parent, name, o::RDONLY | o::DIRECTORY, 0) } else { created };
        if opened < 0 {
            break;
        }
        made += 1;
        dirs[made] = opened as usize;
    }
    let file = if made == DEPTH { feos_user::openat(dirs[made] as i32, "f", o::RDONLY | o::CREAT, 0o600) } else { -1 };
    let result = if file >= 0 {
        let link = fd_link_path(file as usize);
        let result = syscall(nr::OPENAT, [at::FDCWD as isize as usize, link.0.as_ptr() as usize, link.1, o::RDWR, 0, 0]);
        if result >= 0 {
            close(result as usize);
        }
        close(file as usize);
        feos_user::unlinkat(dirs[made] as i32, "f", 0);
        result
    } else {
        file
    };
    while made > 0 {
        close(dirs[made]);
        made -= 1;
        feos_user::unlinkat(dirs[made] as i32, name, at::REMOVEDIR);
    }
    close(dirs[0]);
    tally.report("proc-fd-long-path-write", result == -errno::EACCES, result);
}

/// `/proc/self/fd/<fd>`, in a buffer of its own: the bytes and their length.
fn fd_link_path(fd: usize) -> ([u8; 32], usize) {
    let mut out = [0u8; 32];
    let prefix = b"/proc/self/fd/";
    out[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut value = fd;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..count {
        out[prefix.len() + index] = digits[count - 1 - index];
    }
    (out, prefix.len() + count)
}

fn path_probes(tally: &mut Tally) {
    use feos_user::abi::{at, o, seek};
    let cwd = at::FDCWD as isize as usize;
    let name = b"x";
    let motd = "/etc/motd";
    let motd_at = |path: &str| [cwd, path.as_ptr() as usize, path.len()];
    let [_, mp, ml] = motd_at(motd);
    tally.call("openat-bad-dirfd", nr::OPENAT, [900, name.as_ptr() as usize, 1, o::RDONLY, 0, 0], errno::EBADF);
    tally.call("openat-dirfd-upper-bits", nr::OPENAT, [0xdead_0000_0000 | 1, name.as_ptr() as usize, 1, o::RDONLY, 0, 0], errno::EBADF);
    let file = open(motd, flags::READ);
    let file_fd = if file >= 0 { file as usize } else { 0 };
    tally.call("openat-file-as-dirfd", nr::OPENAT, [file_fd, name.as_ptr() as usize, 1, o::RDONLY, 0, 0], errno::ENOTDIR);
    tally.call("openat-flags", nr::OPENAT, [cwd, mp, ml, o::RDONLY | 0o40000000, 0, 0], errno::EINVAL);
    tally.call("openat-async", nr::OPENAT, [cwd, mp, ml, o::RDONLY | o::ASYNC, 0, 0], errno::EINVAL);
    tally.call("openat-access-3", nr::OPENAT, [cwd, mp, ml, 3, 0, 0], errno::EINVAL);
    tally.call("openat-tmpfile", nr::OPENAT, [cwd, "/tmp".as_ptr() as usize, 4, o::RDWR | o::TMPFILE, 0o600, 0], errno::EOPNOTSUPP);
    tally.call("openat-flags-upper-bits", nr::OPENAT, [cwd, mp, ml, 0x1_0000_0000, 0, 0], errno::EINVAL);
    tally.call("openat-mode-upper-bits", nr::OPENAT, [cwd, mp, ml, o::RDONLY, 0x1_0000_0000, 0], errno::EINVAL);
    tally.call("openat-empty-path", nr::OPENAT, [cwd, mp, 0, o::RDONLY, 0, 0], errno::ENOENT);
    tally.call("openat-kernel-path", nr::OPENAT, [cwd, KERNEL, 8, o::RDONLY, 0, 0], errno::EFAULT);
    let invalid = [b'/', 0xff, 0xfe];
    tally.call("path-invalid-utf8", nr::OPENAT, [cwd, invalid.as_ptr() as usize, invalid.len(), o::RDONLY, 0, 0], errno::EILSEQ);
    let with_nul = *b"/etc\0motd";
    tally.call("path-with-nul", nr::OPENAT, [cwd, with_nul.as_ptr() as usize, with_nul.len(), o::RDONLY, 0, 0], errno::EINVAL);
    tally.call("fstatat-kernel-out", nr::FSTATAT, [cwd, mp, ml, KERNEL, 0, 0], errno::EFAULT);
    let mut stat = feos_user::abi::Stat2::default();
    let out = &mut stat as *mut feos_user::abi::Stat2 as usize;
    tally.call("fstatat-flags", nr::FSTATAT, [cwd, mp, ml, out, at::REMOVEDIR, 0], errno::EINVAL);
    tally.call("fstatat-empty-path-bad-fd", nr::FSTATAT, [900, mp, 0, out, at::EMPTY_PATH, 0], errno::EBADF);

    let root = feos_user::openat(at::FDCWD, "/", o::RDONLY | o::DIRECTORY, 0);
    let root_fd = if root >= 0 { root as usize } else { 0 };
    let mut tiny = [0u8; 8];
    tally.call("getdents-tiny-buffer", nr::GETDENTS, [root_fd, tiny.as_mut_ptr() as usize, tiny.len(), 0, 0, 0], errno::EINVAL);
    tally.call("getdents-on-file", nr::GETDENTS, [file_fd, tiny.as_mut_ptr() as usize, tiny.len(), 0, 0, 0], errno::ENOTDIR);
    tally.call("getdents-into-kernel", nr::GETDENTS, [root_fd, KERNEL, 4096, 0, 0, 0], errno::EFAULT);
    tally.call("getdents-bad-cookie", nr::SEEK, [root_fd, 1 << 20, seek::SET, 0, 0, 0], errno::EINVAL);
    tally.call("getdents-negative-cookie", nr::SEEK, [root_fd, usize::MAX, seek::SET, 0, 0, 0], errno::EINVAL);
    tally.call("getdents-seek-cur-moved", nr::SEEK, [root_fd, 1, seek::CURRENT, 0, 0, 0], errno::EINVAL);
    tally.call("getdents-seek-end", nr::SEEK, [root_fd, 0, seek::END, 0, 0, 0], errno::EINVAL);
    let reference = feos_user::openat(at::FDCWD, motd, o::PATH, 0);
    let reference_fd = if reference >= 0 { reference as usize } else { 0 };
    tally.call("o-path-read", nr::READ, [reference_fd, tiny.as_mut_ptr() as usize, 1, 0, 0, 0], errno::EBADF);
    tally.call("o-path-mmap", nr::MMAP2, [0, 4096, prot::READ, map::PRIVATE, reference_fd, 0], errno::EBADF);
    tally.call("o-path-pread", nr::PREAD, [reference_fd, tiny.as_mut_ptr() as usize, 1, 0, 0, 0], errno::EBADF);
    tally.call("o-path-fsync", nr::FSYNC, [reference_fd, 0, 0, 0, 0, 0], errno::EBADF);
    tally.call("unlinkat-root", nr::UNLINKAT, [cwd, "/".as_ptr() as usize, 1, at::REMOVEDIR, 0, 0], errno::EBUSY);
    tally.call("unlinkat-flags", nr::UNLINKAT, [cwd, mp, ml, 1, 0, 0], errno::EINVAL);
    tally.call("umask-high-bits", nr::UMASK, [0o1777, 0, 0, 0, 0, 0], errno::EINVAL);
    orphan_probes(tally, out);
    long_name_probes(tally);
    let made = "/tmp/.malicious-creat-dir";
    tally.call(
        "openat-creat-directory",
        nr::OPENAT,
        [cwd, made.as_ptr() as usize, made.len(), o::RDWR | o::CREAT | o::DIRECTORY, 0o600, 0],
        errno::EINVAL,
    );
    let left = feos_user::fstatat(at::FDCWD, made, at::SYMLINK_NOFOLLOW).map(|_| 0);
    tally.report("openat-creat-directory-makes-nothing", left == Err(-errno::ENOENT), left.unwrap_or_else(|error| error));
    tally.call("ftruncate-pipe", nr::FTRUNCATE, [0, 0, 0, 0, 0, 0], errno::EINVAL);
    tally.call("ftruncate-read-only", nr::FTRUNCATE, [file_fd, 0, 0, 0, 0, 0], errno::EBADF);
    tally.call("pwrite-negative-offset", nr::PWRITE, [1, tiny.as_ptr() as usize, 1, usize::MAX, 0, 0], errno::EINVAL);
    tally.call("pread-pipe", nr::PREAD, [0, tiny.as_mut_ptr() as usize, 1, 0, 0, 0], errno::ESPIPE);
    tally.call("fchdir-file", nr::FCHDIR, [file_fd, 0, 0, 0, 0, 0], errno::ENOTDIR);
    tally.call("getcwd-into-kernel", nr::GETCWD, [KERNEL, 4096, 0, 0, 0, 0], errno::EFAULT);
    tally.call("current-exe-into-kernel", nr::CURRENT_EXE, [KERNEL, 4096, 0, 0, 0, 0], errno::EFAULT);
    tally.call("fd-path-closed", nr::FD_PATH, [900, tiny.as_mut_ptr() as usize, tiny.len(), 0, 0, 0], errno::EBADF);
    for fd in [file, root, reference] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    // The path rewritten as the kernel reads it: each call sees one version, so it either
    // opens the file or says `EINVAL` (a NUL), and never anything else.
    use core::sync::atomic::Ordering;
    PATH_FLIP_STOP.store(0, Ordering::Relaxed);
    let flipper = thread::spawn(run_path_flipper, 0);
    let path = FLIPPED_PATH.as_ptr() as usize;
    let (mut opened, mut refused, mut other) = (0isize, 0isize, 0isize);
    for _ in 0..2_000 {
        match syscall(nr::OPENAT, [cwd, path, FLIPPED_PATH.len(), o::RDONLY | o::CLOEXEC, 0, 0]) {
            fd if fd >= 0 => {
                opened += 1;
                close(fd as usize);
            }
            result if result == -errno::EINVAL => refused += 1,
            result => other = result,
        }
    }
    PATH_FLIP_STOP.store(1, Ordering::Relaxed);
    let flips = flipper.map(thread::Thread::join);
    println!("probe toctou-openat-path: opened={opened} refused={refused} flips={flips:?}");
    tally.report("toctou-openat-path", other == 0 && opened + refused == 2_000 && flips.is_ok(), other);
}


/// The `PathAt` a thread keeps changing during `toctou-renameat`: its length flips
/// between the path's own and far past `PATH_MAX`.
static FLIPPED_LENGTH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LENGTH_FLIP_STOP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The path the flipped `PathAt` names.
const TOCTOU_RENAME: &str = "/tmp/.malicious-toctou-rename";

fn run_length_flipper(address: usize) -> usize {
    use core::sync::atomic::Ordering;
    let mut flips = 0;
    // SAFETY: the parent's `PathAt`, which it keeps until this thread is joined; written
    // as a whole word, as another thread's plain store would be.
    let length = unsafe { &*((address + 16) as *const core::sync::atomic::AtomicU64) };
    while LENGTH_FLIP_STOP.load(Ordering::Relaxed) == 0 {
        length.store(if flips % 2 == 1 { 1 << 20 } else { TOCTOU_RENAME.len() as u64 }, Ordering::Relaxed);
        flips += 1;
    }
    FLIPPED_LENGTH.store(flips as u64, Ordering::Relaxed);
    flips
}

/// `--fchown-nonroot`: as a user other than root, `FCHOWNAT` of anything is `EPERM`.
fn fchown_nonroot() -> i32 {
    use feos_user::abi::at;
    if feos_user::setuid(1000, 100) != 0 {
        return 2;
    }
    let path = "/etc/motd";
    let result = syscall(nr::FCHOWNAT, [at::FDCWD as isize as usize, path.as_ptr() as usize, path.len(), 0, 0, 0]);
    println!("probe fchownat-nonroot: as 1000, {result}");
    if result == -errno::EPERM { 0 } else { ESCAPED }
}

/// Root's directory, which `--tree-swap-race` keeps putting a link to in place of its own.
const SWAP_ROOT: &str = "/tmp/.malicious-root";
/// `--tree-swap-race`'s own directory, and the one in it that's swapped.
const SWAP_HOME: &str = "/tmp/.malicious-swap";
const SWAP_DIR: &str = "/tmp/.malicious-swap/d";
const SWAP_AWAY: &str = "/tmp/.malicious-swap/away";
const SWAP_MINE: &str = "/tmp/.malicious-swap/mine";
const SWAP_VICTIM: &str = "/tmp/.malicious-swap/d/victim";
const SWAP_PLANTED: &str = "/tmp/.malicious-swap/d/planted";
static SWAP_STOP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Swaps `SWAP_DIR` for a link to root's directory and back, again and again: what a
/// thread does to have a call checked on its own directory and carried out in root's.
fn run_swapper(_: usize) -> usize {
    use core::sync::atomic::Ordering;
    use feos_user::abi::at;
    let mut swaps = 0;
    while SWAP_STOP.load(Ordering::Relaxed) == 0 {
        if feos_user::renameat(at::FDCWD, SWAP_DIR, at::FDCWD, SWAP_AWAY, 0) == 0 {
            feos_user::symlinkat(SWAP_ROOT, at::FDCWD, SWAP_DIR);
            feos_user::unlinkat(at::FDCWD, SWAP_DIR, 0);
            feos_user::renameat(at::FDCWD, SWAP_AWAY, at::FDCWD, SWAP_DIR, 0);
            swaps += 1;
        }
    }
    swaps
}

/// `--tree-swap-race`: as user 1000, renames, mode and time changes and links aimed into
/// its own directory while another thread keeps swapping that for a link to root's. Each
/// is judged on what's changed, so none may land in root's directory; the parent looks.
fn tree_swap_race() -> i32 {
    use core::sync::atomic::Ordering;
    use feos_user::abi::{at, o, Timespec};
    if feos_user::setuid(1000, 100) != 0 {
        return 2;
    }
    if feos_user::mkdirat(at::FDCWD, SWAP_HOME, 0o755) != 0 || feos_user::mkdirat(at::FDCWD, SWAP_DIR, 0o755) != 0 {
        return 3;
    }
    SWAP_STOP.store(0, Ordering::Relaxed);
    let swapper = thread::spawn(run_swapper, 0);
    let long_ago = [Timespec { sec: 1, nsec: 0 }, Timespec { sec: 1, nsec: 0 }];
    for _ in 0..2_000 {
        let made = feos_user::openat(at::FDCWD, SWAP_MINE, o::WRONLY | o::CREAT, 0o644);
        if made >= 0 {
            close(made as usize);
        }
        feos_user::renameat(at::FDCWD, SWAP_MINE, at::FDCWD, SWAP_VICTIM, 0);
        feos_user::fchmodat(at::FDCWD, SWAP_VICTIM, 0o4777, 0);
        feos_user::utimensat(at::FDCWD, SWAP_VICTIM, Some(&long_ago), 0);
        if feos_user::symlinkat("/x", at::FDCWD, SWAP_PLANTED) == 0 {
            feos_user::unlinkat(at::FDCWD, SWAP_PLANTED, 0);
        }
    }
    SWAP_STOP.store(1, Ordering::Relaxed);
    let swaps = swapper.map(thread::Thread::join);
    println!("probe tree-swap-race: swaps={swaps:?}");
    if swaps.is_ok() { 0 } else { 4 }
}

/// `tree-swap-race`: root's file and directory are as they were after the child's race.
fn tree_swap_probe(tally: &mut Tally, program: &str) {
    use feos_user::abi::{at, o};
    let victim = "/tmp/.malicious-root/victim";
    let planted = "/tmp/.malicious-root/planted";
    let made = feos_user::mkdirat(at::FDCWD, SWAP_ROOT, 0o755);
    let file = feos_user::openat(at::FDCWD, victim, o::WRONLY | o::CREAT | o::TRUNC, 0o644);
    if made != 0 || file < 0 {
        tally.report("tree-swap-race", false, if made != 0 { made } else { file });
        return;
    }
    feos_user::write(file as usize, b"root");
    close(file as usize);
    let before = feos_user::fstatat(at::FDCWD, victim, 0);
    let child = spawn(program, &[program, "--tree-swap-race"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let after = feos_user::fstatat(at::FDCWD, victim, 0);
    let unchanged = match (before, after) {
        (Ok(before), Ok(after)) => {
            (after.ino, after.size, after.mode, after.uid, after.mtime) == (before.ino, 4, before.mode, 0, before.mtime)
        }
        _ => false,
    };
    let nothing_planted = feos_user::fstatat(at::FDCWD, planted, at::SYMLINK_NOFOLLOW).map(|_| 0) == Err(-errno::ENOENT);
    println!("probe tree-swap-race: status={status} unchanged={unchanged} nothing-planted={nothing_planted}");
    tally.report("tree-swap-race", status == 0 && unchanged && nothing_planted, status);
    for path in [SWAP_VICTIM, SWAP_PLANTED, SWAP_MINE, "/tmp/.malicious-swap/away/victim", "/tmp/.malicious-swap/away/planted", SWAP_DIR] {
        feos_user::unlinkat(at::FDCWD, path, 0);
    }
    for path in [SWAP_DIR, SWAP_AWAY, SWAP_HOME] {
        feos_user::unlinkat(at::FDCWD, path, at::REMOVEDIR);
    }
    feos_user::unlinkat(at::FDCWD, victim, 0);
    feos_user::unlinkat(at::FDCWD, planted, 0);
    feos_user::unlinkat(at::FDCWD, SWAP_ROOT, at::REMOVEDIR);
}

/// Changing the tree (stage 1c.4): `PathAt`s in the kernel or with reserved bits, flags
/// that aren't offered, link targets too long or not UTF-8, owners given away by someone
/// who isn't root, times that aren't times, a link's mode, and a `PathAt` rewritten while
/// the kernel reads it.
/// The file `lock-exhaustion` and `toctou-setlk` lock.
const LOCK_FILE: &str = "/tmp/.malicious-locks";

/// Opens [`LOCK_FILE`] for reading and writing, made if it isn't there.
fn open_lock_file() -> isize {
    use feos_user::abi::{at, o};
    feos_user::openat(at::FDCWD, LOCK_FILE, o::RDWR | o::CREAT, 0o600)
}

/// `--lock-exhaustion`: as many record locks as a process may hold, one byte apart so none
/// merge, then one more: `ENOLCK`, and another process can still lock.
fn lock_exhaustion() -> i32 {
    use feos_user::abi::{f, MAX_PROCESS_LOCKS};
    let fd = open_lock_file();
    if fd < 0 {
        return 2;
    }
    let fd = fd as usize;
    let mut failed = 0;
    for index in 0..MAX_PROCESS_LOCKS as i64 {
        failed += usize::from(feos_user::fcntl_lock(fd, f::SETLK, &mut feos_user::record(f::WRLCK, index * 2, 1)) != 0);
    }
    let over = feos_user::fcntl_lock(fd, f::SETLK, &mut feos_user::record(f::WRLCK, 10_000, 1));
    // What doesn't add a lock is still fine: one grown onto its neighbour's byte.
    let merged = feos_user::fcntl_lock(fd, f::SETLK, &mut feos_user::record(f::WRLCK, 1, 1));
    let program = "/bin/malicious";
    let other = spawn(program, &[program, "--lock-one"]);
    let other = if other > 0 { wait(other as usize) } else { other };
    println!("probe lock-exhaustion: refused={failed} over={over} merge={merged} other-process={other}");
    if failed == 0 && over == -errno::ENOLCK && merged == 0 && other == 0 {
        0
    } else {
        ESCAPED
    }
}

/// `--lock-one`: one record lock of its own on [`LOCK_FILE`], past the other's.
fn lock_one() -> i32 {
    use feos_user::abi::f;
    let fd = open_lock_file();
    if fd < 0 {
        return 2;
    }
    feos_user::fcntl_lock(fd as usize, f::SETLK, &mut feos_user::record(f::WRLCK, 1 << 20, 1)) as i32
}

/// `--lock-hold PATH`: a write lock over all of `PATH`, then waits on its standard input:
/// for `ktest setlkw-killed`, which kills it.
fn lock_hold(path: &str) -> i32 {
    use feos_user::abi::{at, f, o};
    let fd = feos_user::openat(at::FDCWD, path, o::RDWR | o::CREAT, 0o600);
    if fd < 0 || feos_user::fcntl_lock(fd as usize, f::SETLK, &mut feos_user::record(f::WRLCK, 0, 0)) != 0 {
        return 2;
    }
    let mut byte = [0u8; 1];
    while read(0, &mut byte) > 0 {}
    0
}

/// `--setlkw-wait PATH`: waits in `F_SETLKW` for a write lock over all of `PATH`, which
/// `ktest setlkw-killed` makes sure it never gets before it's killed.
fn setlkw_wait(path: &str) -> i32 {
    use feos_user::abi::{at, f, o};
    let fd = feos_user::openat(at::FDCWD, path, o::RDWR, 0);
    if fd < 0 {
        return 2;
    }
    let got = feos_user::fcntl_lock(fd as usize, f::SETLKW, &mut feos_user::record(f::WRLCK, 0, 0));
    println!("malicious setlkw-wait: got {got}, which it shouldn't have");
    ESCAPED
}

static KIND_FLIP_STOP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Flips the `kind` and `whence` of the `struct flock` at `address` between a good write
/// lock and nonsense, for `toctou-setlk`.
fn run_kind_flipper(address: usize) -> usize {
    use core::sync::atomic::Ordering;
    use feos_user::abi::f;
    let mut flips = 0;
    // SAFETY: the parent's `Flock`, kept until this thread is joined, 8-aligned; written
    // a word at a time, as another thread's plain store would be.
    let word = unsafe { &*(address as *const core::sync::atomic::AtomicU32) };
    while KIND_FLIP_STOP.load(Ordering::Relaxed) == 0 {
        word.store(if flips % 2 == 1 { 0x0009_0009 } else { f::WRLCK as u16 as u32 }, Ordering::Relaxed);
        flips += 1;
    }
    flips
}

/// Locks (stage 1c.5): operations and commands that don't exist, `struct flock`s in the
/// kernel or naming no range, locks a descriptor's access doesn't allow, a process at its
/// cap, and a `struct flock` rewritten while the kernel reads it.
fn lock_probes(tally: &mut Tally, program: &str) {
    use feos_user::abi::{f, lock, Flock};
    let fd = open_lock_file();
    if fd < 0 {
        tally.report("lock-file", false, fd);
        return;
    }
    let fd = fd as usize;
    tally.call("flock-bad-op", nr::FLOCK, [fd, 3, 0, 0, 0, 0], errno::EINVAL);
    tally.call("flock-no-op", nr::FLOCK, [fd, lock::NB, 0, 0, 0, 0], errno::EINVAL);
    tally.call("flock-unknown-bit", nr::FLOCK, [fd, lock::SH | 16, 0, 0, 0, 0], errno::EINVAL);
    tally.call("flock-op-upper-bits", nr::FLOCK, [fd, 0x1_0000_0000 | lock::SH, 0, 0, 0, 0], errno::EINVAL);
    tally.call("flock-fd-upper-bits", nr::FLOCK, [0xdead_0000_0000 | fd, lock::SH, 0, 0, 0, 0], errno::EBADF);
    tally.call("flock-closed-fd", nr::FLOCK, [900, lock::SH, 0, 0, 0, 0], errno::EBADF);
    tally.call("flock-console", nr::FLOCK, [1, lock::SH, 0, 0, 0, 0], errno::EINVAL);
    tally.call("setlk-kernel-pointer", nr::FCNTL, [fd, f::SETLK, KERNEL, 0, 0, 0], errno::EFAULT);
    tally.call("getlk-kernel-pointer", nr::FCNTL, [fd, f::GETLK, KERNEL, 0, 0, 0], errno::EFAULT);
    tally.call("setlk-null", nr::FCNTL, [fd, f::SETLK, 0, 0, 0, 0], errno::EFAULT);
    let bad = |kind: i16, whence: i16, start: i64, length: i64| Flock { kind, whence, start, length, ..Flock::default() };
    let probe_lock = |tally: &mut Tally, name: &str, command: usize, mut lock: Flock, expected: isize| {
        let result = feos_user::fcntl_lock(fd, command, &mut lock);
        tally.report(name, result == -expected, result);
    };
    probe_lock(tally, "setlk-bad-kind", f::SETLK, bad(3, 0, 0, 0), errno::EINVAL);
    probe_lock(tally, "setlk-bad-whence", f::SETLK, bad(f::WRLCK, 3, 0, 0), errno::EINVAL);
    probe_lock(tally, "setlk-negative-whence", f::SETLK, bad(f::WRLCK, -1, 0, 0), errno::EINVAL);
    probe_lock(tally, "setlk-before-the-file", f::SETLK, bad(f::WRLCK, 0, -1, 1), errno::EINVAL);
    probe_lock(tally, "setlk-negative-length-past-start", f::SETLK, bad(f::WRLCK, 0, 4, -5), errno::EINVAL);
    probe_lock(tally, "setlk-length-overflows", f::SETLK, bad(f::WRLCK, 0, i64::MAX, 2), errno::EOVERFLOW);
    probe_lock(tally, "getlk-unlck", f::GETLK, bad(f::UNLCK, 0, 0, 0), errno::EINVAL);
    // A `struct flock` the kernel can read but not write back: `F_GETLK`'s answer faults
    // on the way out, not on the way in.
    let page = mmap(PAGE_SIZE, prot::READ | prot::WRITE);
    if page > 0 {
        let page = page as usize;
        // SAFETY: just mapped, writable, and page-aligned, so aligned for a `Flock`.
        unsafe { (page as *mut Flock).write_volatile(feos_user::record(f::WRLCK, 0, 0)) };
        let protected = feos_user::mprotect(page, PAGE_SIZE, prot::READ);
        let result = feos_user::fcntl(fd, f::GETLK, page);
        tally.report("getlk-into-read-only", protected == 0 && result == -errno::EFAULT, result);
        feos_user::munmap(page, PAGE_SIZE);
    } else {
        tally.report("getlk-into-read-only", false, page);
    }
    // Each of these needs a descriptor made first; one that can't be made fails its
    // probes rather than leave them out, since the boot test only counts what reports.
    let motd = open("/etc/motd", flags::READ);
    if motd >= 0 {
        let mut write_lock = feos_user::record(f::WRLCK, 0, 0);
        let result = feos_user::fcntl_lock(motd as usize, f::SETLK, &mut write_lock);
        tally.report("setlk-write-on-readonly-fd", result == -errno::EBADF, result);
        close(motd as usize);
    } else {
        tally.report("setlk-write-on-readonly-fd", false, motd);
    }
    match pipe() {
        Ok((reader, writer)) => {
            let mut read_lock = feos_user::record(f::RDLCK, 0, 0);
            let result = feos_user::fcntl_lock(reader, f::SETLK, &mut read_lock);
            tally.report("setlk-pipe", result == -errno::EINVAL, result);
            let mut write_lock = feos_user::record(f::WRLCK, 0, 0);
            let result = feos_user::fcntl_lock(writer, f::SETLK, &mut write_lock);
            tally.report("setlk-pipe-write-end", result == -errno::EINVAL, result);
            close(reader);
            close(writer);
        }
        Err(error) => {
            tally.report("setlk-pipe", false, error);
            tally.report("setlk-pipe-write-end", false, error);
        }
    }
    let path_only = feos_user::openat(feos_user::abi::at::FDCWD, LOCK_FILE, feos_user::abi::o::PATH, 0);
    if path_only >= 0 {
        tally.call("flock-o-path", nr::FLOCK, [path_only as usize, lock::SH, 0, 0, 0, 0], errno::EBADF);
        let mut read_lock = feos_user::record(f::RDLCK, 0, 0);
        let result = feos_user::fcntl_lock(path_only as usize, f::SETLK, &mut read_lock);
        tally.report("setlk-o-path", result == -errno::EBADF, result);
        close(path_only as usize);
    } else {
        tally.report("flock-o-path", false, path_only);
        tally.report("setlk-o-path", false, path_only);
    }
    // A directory takes a `flock` but no record lock, which would outlive it (its
    // description holds no claim on its inode number).
    let directory = feos_user::openat(feos_user::abi::at::FDCWD, "/tmp", feos_user::abi::o::RDONLY | feos_user::abi::o::DIRECTORY, 0);
    if directory >= 0 {
        let mut read_lock = feos_user::record(f::RDLCK, 0, 0);
        let result = feos_user::fcntl_lock(directory as usize, f::SETLK, &mut read_lock);
        tally.report("setlk-directory", result == -errno::EINVAL, result);
        close(directory as usize);
    } else {
        tally.report("setlk-directory", false, directory);
    }

    // A process at its cap of record locks: `ENOLCK`, and others still lock.
    let child = spawn(program, &[program, "--lock-exhaustion"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("lock-exhaustion", status == 0, status);

    // A `struct flock` whose kind another thread keeps changing, read once by the kernel:
    // each call locks or says `EINVAL`, never anything else, nor faults.
    use core::sync::atomic::Ordering;
    #[repr(C, align(8))]
    struct Shared(Flock);
    let mut shared = Shared(feos_user::record(f::WRLCK, 0, 1));
    let at = &mut shared.0 as *mut Flock as usize;
    KIND_FLIP_STOP.store(0, Ordering::Relaxed);
    let flipper = thread::spawn(run_kind_flipper, at);
    let (mut locked, mut refused, mut other) = (0isize, 0isize, 0isize);
    for _ in 0..2_000 {
        match feos_user::fcntl(fd, f::SETLK, at) {
            0 => locked += 1,
            result if result == -errno::EINVAL => refused += 1,
            result => other = result,
        }
    }
    KIND_FLIP_STOP.store(1, Ordering::Relaxed);
    let flips = flipper.map(thread::Thread::join);
    println!("probe toctou-setlk: locked={locked} refused={refused} flips={flips:?}");
    tally.report("toctou-setlk", other == 0 && locked + refused == 2_000 && flips.is_ok(), other);
    close(fd);
    feos_user::unlink(LOCK_FILE);
}

fn tree_probes(tally: &mut Tally, program: &str) {
    use feos_user::abi::{at, rename, Timespec};
    let cwd = at::FDCWD as isize as usize;
    let motd = feos_user::path_at(at::FDCWD, "/etc/motd");
    let elsewhere = feos_user::path_at(at::FDCWD, "/tmp/.malicious-renamed");
    let motd_at = &motd as *const feos_user::abi::PathAt as usize;
    let elsewhere_at = &elsewhere as *const feos_user::abi::PathAt as usize;
    tally.call("renameat-kernel-pathat", nr::RENAMEAT, [KERNEL, elsewhere_at, 0, 0, 0, 0], errno::EFAULT);
    tally.call("renameat-kernel-pathat-to", nr::RENAMEAT, [motd_at, KERNEL, 0, 0, 0, 0], errno::EFAULT);
    tally.call("renameat-exchange", nr::RENAMEAT, [motd_at, elsewhere_at, rename::EXCHANGE, 0, 0, 0], errno::EINVAL);
    tally.call("renameat-whiteout", nr::RENAMEAT, [motd_at, elsewhere_at, rename::WHITEOUT, 0, 0, 0], errno::EINVAL);
    tally.call("renameat-flags-upper-bits", nr::RENAMEAT, [motd_at, elsewhere_at, 0x1_0000_0000, 0, 0, 0], errno::EINVAL);
    let mut reserved = motd;
    reserved.reserved = 1;
    let reserved_at = &reserved as *const feos_user::abi::PathAt as usize;
    tally.call("pathat-reserved-nonzero", nr::RENAMEAT, [reserved_at, elsewhere_at, 0, 0, 0, 0], errno::EINVAL);
    tally.call("pathat-reserved-nonzero-linkat", nr::LINKAT, [reserved_at, elsewhere_at, 0, 0, 0, 0], errno::EINVAL);
    let mut kernel_path = motd;
    kernel_path.pointer = KERNEL as u64;
    let kernel_path_at = &kernel_path as *const feos_user::abi::PathAt as usize;
    tally.call("pathat-kernel-path", nr::RENAMEAT, [kernel_path_at, elsewhere_at, 0, 0, 0, 0], errno::EFAULT);
    tally.call("linkat-flags", nr::LINKAT, [motd_at, elsewhere_at, at::REMOVEDIR, 0, 0, 0], errno::EINVAL);
    tally.call("rename-root", nr::RENAMEAT, [&feos_user::path_at(at::FDCWD, "/") as *const _ as usize, elsewhere_at, 0, 0, 0, 0], errno::EBUSY);

    let link = "/tmp/.malicious-link";
    let long = [b'a'; feos_user::abi::PATH_MAX + 1];
    tally.call("symlinkat-long-target", nr::SYMLINKAT, [long.as_ptr() as usize, long.len(), cwd, link.as_ptr() as usize, link.len(), 0], errno::ENAMETOOLONG);
    let invalid = [b'/', 0xff, 0xfe];
    tally.call("symlinkat-invalid-utf8", nr::SYMLINKAT, [invalid.as_ptr() as usize, invalid.len(), cwd, link.as_ptr() as usize, link.len(), 0], errno::EILSEQ);
    let with_nul = *b"/etc\0motd";
    tally.call("symlinkat-target-nul", nr::SYMLINKAT, [with_nul.as_ptr() as usize, with_nul.len(), cwd, link.as_ptr() as usize, link.len(), 0], errno::EINVAL);
    tally.call("symlinkat-kernel-target", nr::SYMLINKAT, [KERNEL, 8, cwd, link.as_ptr() as usize, link.len(), 0], errno::EFAULT);
    let made = feos_user::fstatat(at::FDCWD, link, at::SYMLINK_NOFOLLOW).map(|_| 0);
    tally.report("symlinkat-refused-makes-nothing", made == Err(-errno::ENOENT), made.unwrap_or_else(|error| error));
    let (mp, ml) = ("/etc/motd".as_ptr() as usize, "/etc/motd".len());
    let mut out = [0u8; 8];
    tally.call("readlinkat-not-a-link", nr::READLINKAT, [cwd, mp, ml, out.as_mut_ptr() as usize, out.len(), 0], errno::EINVAL);
    let stdout_link = "/dev/stdout";
    tally.call("readlinkat-into-kernel", nr::READLINKAT, [cwd, stdout_link.as_ptr() as usize, stdout_link.len(), KERNEL, 8, 0], errno::EFAULT);

    // Owners, given away by someone who isn't root, from a child that has stopped being.
    let child = spawn(program, &[program, "--fchown-nonroot"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("fchownat-nonroot", status == 0, status);
    tally.call("fchownat-uid-upper-bits", nr::FCHOWNAT, [cwd, mp, ml, 0x1_0000_0000, 0, 0], errno::EINVAL);
    tally.call("fchownat-flags", nr::FCHOWNAT, [cwd, mp, ml, 0, 0, at::REMOVEDIR], errno::EINVAL);

    let bad = [Timespec { sec: 0, nsec: 1_000_000_000 }, Timespec { sec: 0, nsec: 0 }];
    tally.call("utimensat-bad-nsec", nr::UTIMENSAT, [cwd, mp, ml, bad.as_ptr() as usize, 0, 0], errno::EINVAL);
    let negative = [Timespec { sec: 0, nsec: -1 }, Timespec { sec: 0, nsec: 0 }];
    tally.call("utimensat-negative-nsec", nr::UTIMENSAT, [cwd, mp, ml, negative.as_ptr() as usize, 0, 0], errno::EINVAL);
    tally.call("utimensat-kernel-times", nr::UTIMENSAT, [cwd, mp, ml, KERNEL, 0, 0], errno::EFAULT);
    tally.call("utimensat-flags", nr::UTIMENSAT, [cwd, mp, ml, 0, at::REMOVEDIR, 0], errno::EINVAL);
    tally.call("fchmodat-nofollow", nr::FCHMODAT, [cwd, mp, ml, 0o644, at::SYMLINK_NOFOLLOW, 0], errno::EOPNOTSUPP);
    tally.call("fchmodat-mode-bits", nr::FCHMODAT, [cwd, mp, ml, 0o10000, 0, 0], errno::EINVAL);
    tally.call("faccessat-mode", nr::FACCESSAT, [cwd, mp, ml, 8, 0, 0], errno::EINVAL);
    tally.call("faccessat-flags", nr::FACCESSAT, [cwd, mp, ml, 0, 0x8000, 0], errno::EINVAL);
    let mut room = [0u8; 88];
    tally.call("fstatvfs-stream", nr::FSTATVFS, [0, room.as_mut_ptr() as usize, 0, 0, 0, 0], errno::EINVAL);
    tally.call("fstatvfs-closed-fd", nr::FSTATVFS, [900, room.as_mut_ptr() as usize, 0, 0, 0, 0], errno::EBADF);
    let file = open("/etc/motd", flags::READ);
    if file >= 0 {
        tally.call("fstatvfs-into-kernel-file", nr::FSTATVFS, [file as usize, KERNEL, 0, 0, 0, 0], errno::EFAULT);
        close(file as usize);
    }

    // A `PathAt` whose length another thread keeps changing, read once by the kernel:
    // each call renames the file onto itself or says `ENAMETOOLONG`, and never anything
    // else, nor faults.
    use core::sync::atomic::Ordering;
    let made = feos_user::openat(at::FDCWD, TOCTOU_RENAME, feos_user::abi::o::WRONLY | feos_user::abi::o::CREAT, 0o600);
    if made >= 0 {
        close(made as usize);
    }
    let from = feos_user::path_at(at::FDCWD, TOCTOU_RENAME);
    let flipped = feos_user::path_at(at::FDCWD, TOCTOU_RENAME);
    let flipped_at = &flipped as *const feos_user::abi::PathAt as usize;
    LENGTH_FLIP_STOP.store(0, Ordering::Relaxed);
    let flipper = thread::spawn(run_length_flipper, flipped_at);
    let (mut renamed, mut refused, mut other) = (0isize, 0isize, 0isize);
    for _ in 0..2_000 {
        match syscall(nr::RENAMEAT, [&from as *const _ as usize, flipped_at, 0, 0, 0, 0]) {
            0 => renamed += 1,
            result if result == -errno::ENAMETOOLONG => refused += 1,
            result => other = result,
        }
    }
    LENGTH_FLIP_STOP.store(1, Ordering::Relaxed);
    let flips = flipper.map(thread::Thread::join);
    feos_user::unlinkat(at::FDCWD, TOCTOU_RENAME, 0);
    println!("probe toctou-renameat: renamed={renamed} refused={refused} flips={flips:?}");
    tally.report("toctou-renameat", other == 0 && renamed + refused == 2_000 && made >= 0 && flips.is_ok(), other);
    tree_swap_probe(tally, program);
}

/// A few bytes of text, made on the stack.
struct Text {
    bytes: [u8; 320],
    length: usize,
}

impl Text {
    const fn new() -> Self {
        Self { bytes: [0; 320], length: 0 }
    }

    fn at(&self) -> (usize, usize) {
        (self.bytes.as_ptr() as usize, self.length)
    }
}

impl core::fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// The orphan directory, which no call of a program's reaches, v1's or v2's. An orphan is
/// made first, a root mount file held open as its name goes, so the directory is there:
/// otherwise `ENOENT` says nothing of hiding. The file is closed at the end, and swept.
fn orphan_probes(tally: &mut Tally, out: usize) {
    use core::fmt::Write as _;
    use feos_user::abi::{at, o};
    let cwd = at::FDCWD as isize as usize;
    let name = "/.malicious-orphan";
    let _ = feos_user::unlinkat(at::FDCWD, name, 0);
    let held = feos_user::openat(at::FDCWD, name, o::RDWR | o::CREAT | o::EXCL, 0o600);
    let identity = if held >= 0 { feos_user::fstat(held as usize).map(|stat| (stat.dev, stat.ino)) } else { Err(held) };
    let unlinked = if held >= 0 { feos_user::unlinkat(at::FDCWD, name, 0) } else { held };
    let made = identity.is_ok() && unlinked == 0;
    tally.report("orphan-made", made, if made { 0 } else { unlinked });
    let (dev, ino) = identity.unwrap_or((0, 0));
    let mut orphan = Text::new();
    let _ = write!(orphan, "/.feos-orphans/{dev}-{ino}");
    let (orphan_pointer, orphan_length) = orphan.at();
    let directory = "/.feos-orphans";
    let (dp, dl) = (directory.as_ptr() as usize, directory.len());
    let inside = "/tmp/.feos-orphans";
    let (ip, il) = (inside.as_ptr() as usize, inside.len());
    let made_in = "/.feos-orphans/x";
    let (mp, ml) = (made_in.as_ptr() as usize, made_in.len());
    let revived = "/tmp/.malicious-revived";
    let mut v1_stat = feos_user::abi::Stat::default();
    let v1_out = &mut v1_stat as *mut feos_user::abi::Stat as usize;

    tally.call("orphan-dir-lookup", nr::FSTATAT, [cwd, dp, dl, out, 0, 0], errno::ENOENT);
    tally.call("orphan-lookup", nr::FSTATAT, [cwd, orphan_pointer, orphan_length, out, 0, 0], errno::ENOENT);
    tally.call("orphan-dir-open", nr::OPENAT, [cwd, dp, dl, o::RDONLY | o::DIRECTORY, 0, 0], errno::ENOENT);
    tally.call("orphan-open", nr::OPENAT, [cwd, orphan_pointer, orphan_length, o::RDONLY, 0, 0], errno::ENOENT);
    tally.call("orphan-dir-make", nr::MKDIRAT, [cwd, ip, il, 0o777, 0, 0], errno::ENOENT);
    tally.call("orphan-dir-make-inside", nr::MKDIRAT, [cwd, mp, ml, 0o777, 0, 0], errno::ENOENT);
    tally.call("orphan-unlinkat", nr::UNLINKAT, [cwd, orphan_pointer, orphan_length, 0, 0, 0], errno::ENOENT);
    // v1's path calls hide them too.
    tally.call("orphan-v1-stat-dir", nr::STAT, [dp, dl, v1_out, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-stat", nr::STAT, [orphan_pointer, orphan_length, v1_out, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-open", nr::OPEN, [orphan_pointer, orphan_length, flags::READ, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-mkdir", nr::MKDIR, [ip, il, 0, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-mkdir-inside", nr::MKDIR, [mp, ml, 0, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-unlink", nr::UNLINK, [orphan_pointer, orphan_length, 0, 0, 0, 0], errno::ENOENT);
    tally.call("orphan-v1-chmod", nr::CHMOD, [orphan_pointer, orphan_length, 0o777, 0, 0, 0], errno::ENOENT);
    tally.call(
        "orphan-v1-link",
        nr::LINK,
        [orphan_pointer, orphan_length, revived.as_ptr() as usize, revived.len(), 0, 0],
        errno::ENOENT,
    );
    let args = [UserStr { pointer: orphan_pointer, length: orphan_length }];
    let argv = args.as_ptr() as usize;
    tally.call("orphan-v1-spawn", nr::SPAWN, [orphan_pointer, orphan_length, argv, 1, INHERIT, INHERIT], errno::ENOENT);
    // And the file still works for whoever holds it.
    let wrote = if held >= 0 { feos_user::pwrite(held as usize, b"kept", 0) } else { held };
    let mut back = [0u8; 4];
    let read = if held >= 0 { feos_user::pread(held as usize, &mut back, 0) } else { held };
    tally.report("orphan-still-works", wrote == 4 && read == 4 && &back == b"kept", read);
    let _ = feos_user::unlink(revived);
    if held >= 0 {
        close(held as usize);
    }
}

/// A name longer than `NAME_MAX`, which no listing could carry: never made, by any call.
fn long_name_probes(tally: &mut Tally) {
    use core::fmt::Write as _;
    use feos_user::abi::{at, o};
    let cwd = at::FDCWD as isize as usize;
    let mut long = Text::new();
    let _ = long.write_str("/tmp/");
    for _ in 0..256 {
        let _ = long.write_str("n");
    }
    let (lp, ll) = long.at();
    tally.call("name-too-long-mkdirat", nr::MKDIRAT, [cwd, lp, ll, 0o755, 0, 0], errno::ENAMETOOLONG);
    tally.call("name-too-long-openat", nr::OPENAT, [cwd, lp, ll, o::RDWR | o::CREAT, 0o600, 0], errno::ENAMETOOLONG);
    tally.call("name-too-long-v1-mkdir", nr::MKDIR, [lp, ll, 0, 0, 0, 0], errno::ENAMETOOLONG);
    tally.call("name-too-long-v1-open", nr::OPEN, [lp, ll, flags::READ | flags::WRITE | flags::CREATE, 0, 0, 0], errno::ENAMETOOLONG);
}

/// Where `fd-heap-budget`'s file is: under fifteen directories of 250-byte names in
/// `/tmp`, so the canonical path a description remembers of it is nearly 4 KiB.
fn long_path(out: &mut [u8; 4096]) -> &str {
    let mut length = 0;
    let mut put = |bytes: &[u8], out: &mut [u8; 4096]| {
        out[length..length + bytes.len()].copy_from_slice(bytes);
        length += bytes.len();
    };
    put(b"/tmp/.malicious-long", out);
    for level in 0..15u8 {
        put(b"/", out);
        put(&[b'a' + level; 250], out);
    }
    put(b"/motd", out);
    core::str::from_utf8(&out[..length]).unwrap_or("/")
}

/// Makes `long_path`'s directories and file. Whether it could.
fn make_long_path() -> bool {
    let mut out = [0u8; 4096];
    let path = long_path(&mut out);
    let mut at = 0;
    // Each directory on the way, then the file.
    while let Some(next) = path[at + 1..].find('/') {
        at += 1 + next;
        let made = feos_user::mkdir(&path[..at]);
        if made != 0 && made != -errno::EEXIST {
            return false;
        }
    }
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        return false;
    }
    close(fd as usize);
    true
}

/// `--fd-heap`: opens `long_path`'s file until a call is refused, which must be `ENOMEM`
/// (the process's kernel-heap budget, which charges each description its canonical
/// path's length) with the table nowhere near full; once they're closed, it opens again.
/// Zero if so.
fn fd_heap() -> i32 {
    use feos_user::abi::MAX_FDS;
    let mut out = [0u8; 4096];
    let long = long_path(&mut out);
    let mut made = 0;
    let error = loop {
        let fd = open(long, flags::READ);
        if fd < 0 {
            break fd;
        }
        made += 1;
    };
    let closed = feos_user::close_range(3, u32::MAX as usize, 0);
    let again = open(long, flags::READ);
    println!("fd-heap: made={made} then={error} closed={closed} again={again}");
    if error == -errno::ENOMEM && made > 100 && made + 3 < MAX_FDS && closed == 0 && again == 3 {
        0
    } else {
        ESCAPED
    }
}

/// `--fd-exhaustion`: duplicates standard output until the table is full, which must be
/// at exactly `MAX_FDS` descriptors; then every way of adding one says `EMFILE`, and
/// closing one makes room for one again. Zero if so.
fn fd_exhaustion() -> i32 {
    use feos_user::abi::{f, MAX_FDS};
    let mut made = 0;
    let error = loop {
        let fd = syscall(nr::FCNTL, [1, f::DUPFD, 0, 0, 0, 0]);
        if fd < 0 {
            break fd;
        }
        made += 1;
    };
    let file = open("/etc/motd", flags::READ);
    let piped = pipe().err().unwrap_or(0);
    let dup3_top = syscall(nr::DUP3, [1, MAX_FDS - 1, 0, 0, 0, 0]);
    close(500);
    let again = syscall(nr::FCNTL, [1, f::DUPFD, 0, 0, 0, 0]);
    println!("fd-exhaustion: made={made} then={error} open={file} pipe={piped} dup3-over-the-top={dup3_top} again={again}");
    let full = error == -errno::EMFILE && made + 3 == MAX_FDS;
    if full && file == -errno::EMFILE && piped == -errno::EMFILE && dup3_top == (MAX_FDS - 1) as isize && again == 500 {
        0
    } else {
        ESCAPED
    }
}

/// Futex words the kernel has to find through the space, which aren't all there: gone,
/// `PROT_NONE`, misaligned for a wake, in the window's unmapped top page or past the window, and a
/// read-only word that doesn't hold what's expected, whose frame is pinned for the check
/// and must be let go. Every frame the probes' pages had must come back.
fn futex_probes(tally: &mut Tally) {
    const RW: usize = prot::READ | prot::WRITE;
    let before = free_frames();
    let gone = mmap2(0, PAGE_SIZE, RW, map::PRIVATE);
    let none = mmap2(0, PAGE_SIZE, prot::NONE, map::PRIVATE);
    // Its own frame, not the zero frame, so a pin left on it would keep it from coming back.
    let read_only = mmap2(0, PAGE_SIZE, RW, map::PRIVATE);
    if gone <= 0 || none <= 0 || read_only <= 0 {
        tally.report("futex-probe-pages", false, gone.min(none).min(read_only));
        return;
    }
    let (gone, none, read_only) = (gone as usize, none as usize, read_only as usize);
    write_at(gone);
    feos_user::munmap(gone, PAGE_SIZE);
    write_at(read_only);
    feos_user::mprotect(read_only, PAGE_SIZE, prot::READ);
    let top = feos_user::abi::USER_END;
    tally.call("futex-wait-unmapped", nr::FUTEX_WAIT, [gone, 0, 1, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wake-unmapped", nr::FUTEX_WAKE, [gone, 1, 0, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wait-prot-none", nr::FUTEX_WAIT, [none, 0, 1, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wake-prot-none", nr::FUTEX_WAKE, [none, 1, 0, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wake-unaligned", nr::FUTEX_WAKE, [read_only + 1, 1, 0, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wait-read-only-mismatch", nr::FUTEX_WAIT, [read_only, 0, 1, 0, 0, 0], errno::EAGAIN);
    // Above the stack's top, the window's last page has nothing mapped.
    tally.call("futex-wait-window-top", nr::FUTEX_WAIT, [top - 4, 0, 1, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wake-window-top", nr::FUTEX_WAKE, [top - 4, 1, 0, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wait-past-window", nr::FUTEX_WAIT, [top, 0, 1, 0, 0, 0], errno::EFAULT);
    feos_user::munmap(none, PAGE_SIZE);
    feos_user::munmap(read_only, PAGE_SIZE);
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after == before);
    tally.report("futex-probes-leave-no-pin", back, 0);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe futex-probes-leave-no-pin: frames {before}->{after}");
    }
}

/// A `Timespec` another thread keeps rewriting, between a valid one-microsecond interval
/// and one whose `nsec` is out of range, for `toctou-nanosleep`.
static FLIPPING: [core::sync::atomic::AtomicI64; 2] = [const { core::sync::atomic::AtomicI64::new(0) }; 2];
static FLIP_STOP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn run_flipper(_: usize) -> usize {
    use core::sync::atomic::Ordering;
    let mut flips = 0;
    while FLIP_STOP.load(Ordering::Relaxed) == 0 {
        let bad = flips % 2 == 1;
        FLIPPING[1].store(if bad { 2_000_000_000 } else { 1_000 }, Ordering::Relaxed);
        // `sec` goes negative as `nsec` goes out of range: a kernel that read the two
        // halves at different times could see a valid pair the user never wrote, but
        // never one that sleeps long.
        FLIPPING[0].store(if bad { -1 } else { 0 }, Ordering::Relaxed);
        flips += 1;
    }
    flips
}

/// The time calls' arguments, every way they can be wrong, and a `Timespec` rewritten
/// while the kernel reads it.
fn time_probes(tally: &mut Tally) {
    use core::sync::atomic::{AtomicU32, Ordering};
    use feos_user::abi::{clock, timer, Timespec};
    static WORD: AtomicU32 = AtomicU32::new(0);
    let word = &WORD as *const AtomicU32 as usize;
    let negative = Timespec { sec: -1, nsec: 0 };
    let overflow = Timespec { sec: 0, nsec: 1_000_000_000 };
    let upper = Timespec { sec: 0, nsec: 1 << 32 };
    let short = Timespec { sec: 0, nsec: 1_000 };
    let at = |time: &Timespec| time as *const Timespec as usize;
    let mut out = Timespec::default();
    let out = &mut out as *mut Timespec as usize;
    let ns = nr::NANOSLEEP;
    let mono = clock::MONOTONIC;
    tally.call("nanosleep-negative", ns, [mono, 0, at(&negative), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-nsec-overflow", ns, [mono, 0, at(&overflow), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-nsec-upper-half", ns, [mono, 0, at(&upper), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-absolute-negative", ns, [mono, timer::ABSTIME, at(&negative), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-process-cputime", ns, [clock::PROCESS_CPUTIME_ID, 0, at(&short), 0, 0, 0], errno::EOPNOTSUPP);
    tally.call("nanosleep-thread-cputime", ns, [clock::THREAD_CPUTIME_ID, 0, at(&short), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-bad-clock", ns, [99, 0, at(&short), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-coarse-clock", ns, [clock::MONOTONIC_COARSE, 0, at(&short), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-clock-upper-bits", ns, [0x1_0000_0001, 0, at(&short), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-bad-flags", ns, [mono, 2, at(&short), 0, 0, 0], errno::EINVAL);
    tally.call("nanosleep-kernel-request", ns, [mono, 0, KERNEL, 0, 0, 0], errno::EFAULT);
    tally.call("nanosleep-null-request", ns, [mono, 0, 0, 0, 0, 0], errno::EFAULT);
    tally.call("nanosleep-kernel-remain", ns, [mono, 0, at(&short), KERNEL, 0, 0], errno::EFAULT);
    tally.call("clock-bad-id", nr::CLOCK_GETTIME, [8, out, 0, 0, 0, 0], errno::EINVAL);
    tally.call("clock-id-upper-bits", nr::CLOCK_GETTIME, [0x1_0000_0001, out, 0, 0, 0, 0], errno::EINVAL);
    tally.call("clock-getres-bad-id", nr::CLOCK_GETRES, [usize::MAX, out, 0, 0, 0, 0], errno::EINVAL);
    tally.call("clock-gettime-kernel-out", nr::CLOCK_GETTIME, [mono, KERNEL, 0, 0, 0, 0], errno::EFAULT);
    tally.call("clock-gettime-code-out", nr::CLOCK_GETTIME, [mono, victim(), 0, 0, 0, 0], errno::EFAULT);
    tally.call("clock-getres-kernel-out", nr::CLOCK_GETRES, [mono, KERNEL, 0, 0, 0, 0], errno::EFAULT);
    let fw = nr::FUTEX_WAIT2;
    tally.call("futex-wait2-kernel-deadline-pointer", fw, [word, 0, KERNEL, mono, 0, 0], errno::EFAULT);
    tally.call("futex-wait2-flags", fw, [word, 0, 0, mono, 1, 0], errno::EINVAL);
    tally.call("futex-wait2-expected-upper-bits", fw, [word, 0x1_0000_0000, 0, mono, 0, 0], errno::EINVAL);
    tally.call("futex-wait2-bad-clock", fw, [word, 0, at(&short), clock::PROCESS_CPUTIME_ID, 0, 0], errno::EINVAL);
    // With no deadline too: the word holds 0, so a kernel that let this one through
    // would block here for good rather than pass.
    tally.call("futex-wait2-bad-clock-no-deadline", fw, [word, 0, 0, clock::PROCESS_CPUTIME_ID, 0, 0], errno::EINVAL);
    tally.call("futex-wait2-nsec-overflow", fw, [word, 0, at(&overflow), mono, 0, 0], errno::EINVAL);
    tally.call("futex-wait2-negative-deadline", fw, [word, 0, at(&negative), mono, 0, 0], errno::EINVAL);
    tally.call("futex-wait2-kernel-word", fw, [KERNEL, 0, 0, mono, 0, 0], errno::EFAULT);

    // The request rewritten as the kernel reads it: each call sees one version, so it
    // either sleeps a microsecond or says `EINVAL`, and never anything else.
    FLIP_STOP.store(0, Ordering::Relaxed);
    let flipper = thread::spawn(run_flipper, 0);
    let request = FLIPPING.as_ptr() as usize;
    let (mut slept, mut refused, mut other) = (0isize, 0isize, 0isize);
    for _ in 0..2_000 {
        match syscall(ns, [mono, 0, request, 0, 0, 0]) {
            0 => slept += 1,
            result if result == -errno::EINVAL => refused += 1,
            result => other = result,
        }
    }
    FLIP_STOP.store(1, Ordering::Relaxed);
    let flips = flipper.map(thread::Thread::join);
    println!("probe toctou-nanosleep: slept={slept} refused={refused} flips={flips:?}");
    tally.report("toctou-nanosleep", other == 0 && slept + refused == 2_000 && flips.is_ok(), other);
}

struct Tally {
    run: usize,
    blocked: usize,
}

impl Tally {
    fn report(&mut self, name: &str, blocked: bool, detail: isize) {
        self.run += 1;
        if blocked {
            self.blocked += 1;
            println!("probe {name}: blocked ({detail})");
        } else {
            println!("probe {name}: NOT BLOCKED ({detail})");
        }
    }

    /// A system call that must fail with `expected`.
    fn call(&mut self, name: &str, number: usize, args: [usize; 6], expected: isize) {
        let result = syscall(number, args);
        self.report(name, result == -expected, result);
    }
}

/// `SPAWN2` (stage 1c.6): requests from places a program can't hand the kernel, reserved
/// fields, counts and strings past their limits, actions that make no sense, groups out of
/// reach, and a request changed while the kernel reads it. Then the caps: children nobody
/// reaps, and programs until there are no slots, with everything back after.
fn spawn2_probes(tally: &mut Tally, program: &str) {
    use feos_user::abi::{spawn, spawn_action, SpawnAction, SpawnRequest};
    use feos_user::{spawn2, spawn_request, user_strs};
    let mut argv = [UserStr::default(); 4];
    let base = spawn_request(program, user_strs(&[program, "--quiet-exit"], &mut argv), &[], &[]);
    let call = |tally: &mut Tally, name: &str, request: &SpawnRequest, expected: isize| {
        let result = spawn2(request);
        if result > 0 {
            wait(result as usize);
        }
        tally.report(name, result == -expected, result);
    };
    tally.call("spawn2-kernel-request", nr::SPAWN2, [KERNEL, 0, 0, 0, 0, 0], errno::EFAULT);
    call(tally, "spawn2-reserved-nonzero", &SpawnRequest { reserved: [0, 1, 0], ..base }, errno::EINVAL);
    call(tally, "spawn2-argc-zero", &SpawnRequest { argc: 0, ..base }, errno::EINVAL);
    call(tally, "spawn2-schedparam", &SpawnRequest { flags: spawn::SETSCHEDPARAM, ..base }, errno::EINVAL);

    // One string past 128 KiB, and nine of 128 KiB together past `ARG_MAX`: refused
    // before anything is read from them.
    let mut big = [UserStr::default(); 10];
    user_strs(&[program, "--quiet-exit"], &mut big);
    let area = mmap2(0, spawn::STRING_MAX + PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE | map::ANONYMOUS);
    let area = if area > 0 { area as usize } else { KERNEL };
    big[2] = UserStr { pointer: area, length: spawn::STRING_MAX + 1 };
    let one = call_result(&SpawnRequest { argv: big.as_ptr() as u64, argc: 3, ..base });
    for slot in big[1..].iter_mut() {
        *slot = UserStr { pointer: area, length: spawn::STRING_MAX };
    }
    let nine = call_result(&SpawnRequest { argv: big.as_ptr() as u64, argc: 10, ..base });
    tally.report("spawn2-args-too-big", one == -errno::E2BIG && nine == -errno::E2BIG, one.min(nine));
    let wild = [UserStr { pointer: program.as_ptr() as usize, length: program.len() }, UserStr { pointer: KERNEL, length: 8 }];
    call(tally, "spawn2-strings-in-kernel", &SpawnRequest { argv: wild.as_ptr() as u64, argc: 2, ..base }, errno::EFAULT);
    let nul = "--quiet\0exit";
    let with_nul = [UserStr { pointer: program.as_ptr() as usize, length: program.len() }, UserStr { pointer: nul.as_ptr() as usize, length: nul.len() }];
    call(tally, "spawn2-string-with-nul", &SpawnRequest { argv: with_nul.as_ptr() as u64, argc: 2, ..base }, errno::EINVAL);

    let with = |actions: &[SpawnAction]| SpawnRequest { actions: actions.as_ptr() as u64, action_count: actions.len() as u64, ..base };
    let bad_kind = [SpawnAction { kind: 99, ..SpawnAction::default() }];
    call(tally, "spawn2-action-bad-kind", &with(&bad_kind), errno::EINVAL);
    let dup_closed = [SpawnAction { kind: spawn_action::DUP2, fd: 700, to: 3, ..SpawnAction::default() }];
    call(tally, "spawn2-dup2-from-closed", &with(&dup_closed), errno::EBADF);
    let close_1024 = [SpawnAction { kind: spawn_action::CLOSE, fd: 1024, ..SpawnAction::default() }];
    call(tally, "spawn2-close-fd-1024", &with(&close_1024), errno::EBADF);

    // A child in a session of its own is out of this one's reach, and so is its group.
    let mut waiter = [UserStr::default(); 4];
    let (other, release) = match feos_user::pipe2(feos_user::abi::o::CLOEXEC) {
        Ok((read_end, write_end)) => {
            let actions = [SpawnAction { kind: spawn_action::DUP2, fd: read_end as i32, to: 0, ..SpawnAction::default() }];
            let request = spawn_request(program, user_strs(&[program, "--pipe-wait"], &mut waiter), &[], &actions);
            let pid = spawn2(&SpawnRequest { flags: spawn::SETSID, ..request });
            close(read_end);
            (pid, write_end)
        }
        Err(error) => (error, usize::MAX),
    };
    let moved = feos_user::setpgid(other as usize, other as usize);
    let joined = feos_user::setpgid(0, other as usize);
    if release != usize::MAX {
        close(release);
    }
    if other > 0 {
        wait(other as usize);
    }
    tally.report("setpgid-other-session", other > 0 && moved == -errno::EPERM && joined == -errno::EPERM, moved.max(joined));

    toctou_spawn_lengths(tally, program);

    let before = free_frames();
    let child = spawn(program, &[program, "--concurrent-1mib"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("spawn2-concurrent-1mib", status == 0 && back, status);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe spawn2-concurrent-1mib: frames {before}->{after}");
    }

    let child = spawn(program, &[program, "--children-cap"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("children-cap", status == 0, status);

    let before = free_frames();
    let mut made = [0isize; 2];
    for count in made.iter_mut() {
        let child = spawn(program, &[program, "--spawn-storm"]);
        *count = if child > 0 { wait(child as usize) } else { child };
    }
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("spawn-storm", made == [0, 0] && back, made[0].min(made[1]));
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe spawn-storm: frames {before}->{after}");
    }
    if area != KERNEL {
        feos_user::munmap(area, spawn::STRING_MAX + PAGE_SIZE);
    }
}

/// A `SPAWN2` that may start a child: its result, the child waited for.
fn call_result(request: &feos_user::abi::SpawnRequest) -> isize {
    let result = feos_user::spawn2(request);
    if result > 0 {
        wait(result as usize);
    }
    result
}

/// A `UserStr` array of three, the last of which another thread keeps changing while
/// `SPAWN2` reads it: words, as the kernel reads them.
static FLIPPED: [core::sync::atomic::AtomicUsize; 6] = [const { core::sync::atomic::AtomicUsize::new(0) }; 6];
static SPAWN_FLIPPING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn flip(text: usize) -> usize {
    use core::sync::atomic::Ordering;
    let mut round = 0usize;
    while SPAWN_FLIPPING.load(Ordering::Acquire) {
        let (pointer, length) = match round % 3 {
            0 => (text, 4),
            1 => (text, 1 << 40),
            _ => (KERNEL, 4),
        };
        FLIPPED[4].store(pointer, Ordering::Relaxed);
        FLIPPED[5].store(length, Ordering::Relaxed);
        round += 1;
    }
    round
}

/// A thread flips an argument's pointer and length while the other spawns with it, over
/// and over: each spawn starts a child or is `E2BIG` or `EFAULT`, never anything else, and
/// the machine stays up.
fn toctou_spawn_lengths(tally: &mut Tally, program: &str) {
    use core::sync::atomic::Ordering;
    let quiet = "--quiet-exit";
    let text = "abcd";
    let words = [program.as_ptr() as usize, program.len(), quiet.as_ptr() as usize, quiet.len(), text.as_ptr() as usize, 4];
    for (word, value) in FLIPPED.iter().zip(words) {
        word.store(value, Ordering::Relaxed);
    }
    SPAWN_FLIPPING.store(true, Ordering::Release);
    let flipper = thread::spawn(flip, text.as_ptr() as usize);
    let request = feos_user::abi::SpawnRequest {
        argv: FLIPPED.as_ptr() as u64,
        argc: 3,
        ..feos_user::spawn_request(program, &[], &[], &[])
    };
    let mut counts = [0usize; 4];
    for _ in 0..300 {
        match feos_user::spawn2(&request) {
            pid if pid > 0 => {
                wait(pid as usize);
                counts[0] += 1;
            }
            error if error == -errno::E2BIG => counts[1] += 1,
            error if error == -errno::EFAULT => counts[2] += 1,
            _ => counts[3] += 1,
        }
    }
    SPAWN_FLIPPING.store(false, Ordering::Release);
    let rounds = flipper.map(thread::Thread::join).unwrap_or(0);
    println!("probe toctou-spawn-lengths: started {} E2BIG {} EFAULT {} other {} ({rounds} flips)", counts[0], counts[1], counts[2], counts[3]);
    tally.report("toctou-spawn-lengths", counts[3] == 0 && rounds > 0, counts[3] as isize);
}

/// Started with 1 MiB of arguments, the most there may be.
const BIG_ARG: usize = 128 * 1024;
/// Threads spawning at once.
const SPAWNERS: usize = 64;
/// Where the big arguments' bytes are, shared by every spawning thread.
static BIG_AREA: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// One thread's spawn of this program with seven 128 KiB arguments and most of an eighth:
/// 0 if it started and ended well or there was no memory for it, else what it got.
fn spawn_big(_: usize) -> usize {
    let program = "/bin/malicious";
    let area = BIG_AREA.load(core::sync::atomic::Ordering::Acquire);
    let mut argv = [UserStr::default(); 10];
    argv[0] = UserStr { pointer: program.as_ptr() as usize, length: program.len() };
    let quiet = "--quiet-exit";
    argv[1] = UserStr { pointer: quiet.as_ptr() as usize, length: quiet.len() };
    let fixed = program.len() + 1 + quiet.len() + 1 + 10 * 8;
    let last = feos_user::abi::spawn::ARG_MAX - fixed - 7 * (BIG_ARG + 1) - 1;
    for (index, slot) in argv[2..].iter_mut().enumerate() {
        *slot = UserStr { pointer: area, length: if index == 7 { last } else { BIG_ARG } };
    }
    let request = feos_user::spawn_request(program, &argv, &[], &[]);
    match feos_user::spawn2(&request) {
        pid if pid > 0 => match wait(pid as usize) {
            0 => 0,
            status => status as usize,
        },
        error if error == -errno::ENOMEM => 0,
        error => error as usize,
    }
}

/// `SPAWNERS` threads spawning with 1 MiB of arguments each, at once: each starts its
/// child or is `ENOMEM`, nothing else. Zero if so.
fn concurrent_1mib() -> i32 {
    let area = mmap2(0, BIG_ARG, prot::READ | prot::WRITE, map::PRIVATE | map::ANONYMOUS);
    if area < 0 {
        return 2;
    }
    // SAFETY: just mapped, writable and this long.
    unsafe { core::ptr::write_bytes(area as *mut u8, b'a', BIG_ARG) };
    BIG_AREA.store(area as usize, core::sync::atomic::Ordering::Release);
    let mut threads = [const { None }; SPAWNERS];
    for slot in threads.iter_mut() {
        *slot = thread::spawn(spawn_big, 0).ok();
    }
    let mut worst = 0;
    let mut started = 0;
    for slot in threads.iter_mut() {
        if let Some(thread) = slot.take() {
            started += 1;
            let result = thread.join();
            if result != 0 {
                worst = result;
            }
        }
    }
    println!("concurrent-1mib: {started} threads, worst {}", worst as isize);
    if worst == 0 && started == SPAWNERS {
        0
    } else {
        ESCAPED
    }
}

/// Children started and never reaped, up to the cap: one more is `EAGAIN` (`EBUSY` to
/// v1's `SPAWN`), and once one is reaped there's room again. Zero if so.
fn children_cap(program: &str) -> i32 {
    use feos_user::abi::MAX_CHILDREN;
    let mut argv = [UserStr::default(); 4];
    let request = feos_user::spawn_request(program, feos_user::user_strs(&[program, "--quiet-exit"], &mut argv), &[], &[]);
    // A slot may be short while children end: tried again, a while, until they have.
    let start = |patience: usize| {
        let mut result = feos_user::spawn2(&request);
        for _ in 0..patience {
            if result != -errno::EAGAIN {
                break;
            }
            feos_user::sleep_ms(10);
            result = feos_user::spawn2(&request);
        }
        result
    };
    let mut first = 0;
    for made in 0..MAX_CHILDREN {
        let pid = start(500);
        if pid <= 0 {
            println!("children-cap: child {made} refused: {pid}");
            return ESCAPED;
        }
        if made == 0 {
            first = pid;
        }
    }
    // Every one of them has ended, so it's the cap that's met, not the slots.
    feos_user::sleep_ms(200);
    let over = start(5);
    let v1 = spawn(program, &[program, "--quiet-exit"]);
    let reaped = wait(first as usize);
    let again = start(500);
    println!("children-cap: {MAX_CHILDREN} made, then {over} and v1 {v1}; reaped {first} ({reaped}), then {again}");
    if over == -errno::EAGAIN && v1 == -errno::EBUSY && again > 0 {
        0
    } else {
        ESCAPED
    }
}

/// Programs until there's no slot (or memory) for another, each waiting on a pipe this
/// then closes, so they all end; twice, the second as far as the first. Zero if so.
fn spawn_storm(program: &str) -> i32 {
    use feos_user::abi::{spawn_action, SpawnAction};
    static PIDS: [core::sync::atomic::AtomicUsize; 1024] = [const { core::sync::atomic::AtomicUsize::new(0) }; 1024];
    let Ok((read_end, write_end)) = feos_user::pipe2(feos_user::abi::o::CLOEXEC) else {
        return 2;
    };
    let actions = [SpawnAction { kind: spawn_action::DUP2, fd: read_end as i32, to: 0, ..SpawnAction::default() }];
    let mut argv = [UserStr::default(); 4];
    let request = feos_user::spawn_request(program, feos_user::user_strs(&[program, "--pipe-wait"], &mut argv), &[], &actions);
    let mut made = 0;
    let error = loop {
        let pid = feos_user::spawn2(&request);
        if pid <= 0 {
            break pid;
        }
        PIDS[made].store(pid as usize, core::sync::atomic::Ordering::Relaxed);
        made += 1;
        if made == PIDS.len() {
            break 0;
        }
    };
    close(write_end);
    close(read_end);
    let failed = PIDS[..made].iter().filter(|pid| !ended_well(pid.load(core::sync::atomic::Ordering::Relaxed) as isize)).count();
    println!("spawn-storm: made={made} then={error} failed={failed}");
    if (error == -errno::EAGAIN || error == -errno::ENOMEM) && made > 0 && failed == 0 {
        0
    } else {
        ESCAPED
    }
}

fn run_child(tally: &mut Tally, program: &str, name: &str) {
    let child = spawn(program, &[program, "--probe", name]);
    if child <= 0 {
        tally.report(name, false, child);
        return;
    }
    let status = wait(child as usize);
    tally.report(name, status == FAULTED, status);
}

/// Spawns, writes and reads for as long as it lives: whenever a kill lands, it's
/// likely to be in one of those, holding a pipe, a slot or a buffer.
fn call_loop() -> i32 {
    let mut buffer = [0u8; 256];
    loop {
        let child = spawn("/bin/hello", &["/bin/hello", "--quiet"]);
        if let Ok((reader, writer)) = feos_user::pipe() {
            feos_user::write(writer, &buffer);
            feos_user::read(reader, &mut buffer);
            close(reader);
            close(writer);
        }
        if child > 0 {
            wait(child as usize);
        }
    }
}

/// Takes page faults for as long as it lives: a mapping's worth, given back, again.
fn fault_loop() -> i32 {
    let mut length = 16 * 1024 * 1024;
    loop {
        let area = mmap(length, prot::READ | prot::WRITE);
        if area < 0 {
            length = (length / 2).max(4096);
            continue;
        }
        let mut page = 0;
        while page < length {
            write_at(area as usize + page);
            page += 4096;
        }
        feos_user::munmap(area as usize, length);
    }
}

/// Dirties every page of a shared mapping of `path`, the whole file, over and over: each
/// pass writes its number into the first word of every page, so whatever is written back
/// when it's killed, each page holds one pass's number and nothing else.
fn dirty_storm(path: &str) -> i32 {
    let Ok(stat) = feos_user::stat(path) else {
        println!("dirty-storm: no {path}");
        return 2;
    };
    let length = stat.size as usize & !(PAGE_SIZE - 1);
    let fd = open(path, flags::READ | flags::WRITE);
    let at = if fd >= 0 { feos_user::mmap2_file(0, length, prot::READ | prot::WRITE, map::SHARED, fd as usize, 0) } else { fd };
    if at < 0 || length == 0 {
        println!("dirty-storm: open={fd} map={at}");
        return 2;
    }
    close(fd as usize);
    let mut pass = 1u64;
    loop {
        let mut page = 0;
        while page < length {
            // SAFETY: the mapping just made, `length` bytes, writable.
            unsafe { core::ptr::write_volatile((at as usize + page) as *mut u64, pass) };
            page += PAGE_SIZE;
        }
        pass += 1;
    }
}

fn wait_forever(_: usize) -> usize {
    static NEVER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    loop {
        feos_user::futex_wait(&NEVER, 0, 0);
    }
}

/// Starts threads until it can't: `EAGAIN`, at the thread cap or with no task free to
/// programs. Says how many, and then either waits to be killed (`hold`) or ends, and
/// every thread with it. Zero if it stopped at `EAGAIN`.
fn thread_bomb(hold: bool) -> i32 {
    let mut made = 0;
    let error = loop {
        match thread::spawn(wait_forever, 0) {
            Ok(_) => made += 1,
            Err(error) => break error,
        }
        if made > 1000 {
            break 0;
        }
    };
    println!("thread-bomb: made={made} then={error}");
    if hold {
        wait_forever(0);
    }
    if error == -errno::EAGAIN {
        0
    } else {
        ESCAPED
    }
}

/// A thread whose `THREAD_EXIT` unmaps part of the image, a page nothing runs: allowed,
/// as `MUNMAP` of it is, and the first thread carries on. Zero if the page went.
fn exit_unmapping_image() -> i32 {
    fn run(_: usize) -> usize {
        feos_user::thread_exit(victim(), PAGE_SIZE)
    }
    let Ok(thread) = thread::spawn(run, 0) else {
        return 2;
    };
    let mut waited = 0;
    while !thread.finished() && waited < 5000 {
        feos_user::sleep_ms(1);
        waited += 1;
    }
    // The page is gone: a futex wake on it finds nothing mapped. (It can't be mapped
    // again to show it: `mmap2` places nothing below its area.)
    let gone = syscall(nr::FUTEX_WAKE, [victim(), 1, 0, 0, 0, 0]);
    println!("probe thread-exit-unmap-image-only: waited={waited}ms wake-on-page={gone}");
    if thread.finished() && gone == -errno::EFAULT {
        0
    } else {
        ESCAPED
    }
}

/// `THREAD_EXIT` with ranges `MUNMAP` would refuse: each comes back `EINVAL`, with the
/// thread still running. Zero if so.
fn exit_bad_ranges() -> i32 {
    let ranges = [
        (victim() + 1, PAGE_SIZE),
        (KERNEL, PAGE_SIZE),
        (feos_user::abi::USER_END - PAGE_SIZE, 2 * PAGE_SIZE),
        (usize::MAX & !(PAGE_SIZE - 1), 2 * PAGE_SIZE),
        (victim(), usize::MAX),
    ];
    for (address, length) in ranges {
        let result = feos_user::thread_exit_raw(address, length);
        if result != -errno::EINVAL {
            println!("probe thread-exit-bad-range: {address:#x}+{length:#x} said {result}");
            return ESCAPED;
        }
    }
    0
}

/// The thread calls' arguments, every way they can be wrong, and the children that end
/// threads in ways a process should survive, or be ended by.
fn thread_probes(tally: &mut Tally, program: &str) {
    let entry = thread::entry();
    let stack = STACK_TOP - 4096;
    let good = 0;
    let tc = nr::THREAD_CREATE;
    tally.call("thread-create-kernel-entry", tc, [KERNEL, stack, good, 0, good, 0], errno::EFAULT);
    tally.call("thread-create-data-entry", tc, [DATA_CODE.as_ptr() as usize, stack, good, 0, good, 0], errno::EFAULT);
    tally.call("thread-create-null-entry", tc, [0, stack, good, 0, good, 0], errno::EFAULT);
    tally.call("thread-create-unaligned-stack", tc, [entry, stack - 8, good, 0, good, 0], errno::EINVAL);
    tally.call("thread-create-kernel-stack", tc, [entry, KERNEL + 4096, good, 0, good, 0], errno::EINVAL);
    tally.call("thread-create-stack-past-window", tc, [entry, feos_user::abi::USER_END + 16, good, 0, good, 0], errno::EINVAL);
    tally.call("thread-create-noncanonical-tls", tc, [entry, stack, NONCANONICAL, 0, good, 0], errno::EINVAL);
    tally.call("thread-create-kernel-tls", tc, [entry, stack, KERNEL, 0, good, 0], errno::EINVAL);
    tally.call("clear-word-kernel-address", tc, [entry, stack, good, 0, KERNEL, 0], errno::EINVAL);
    tally.call("clear-word-unaligned", tc, [entry, stack, good, 0, stack - 4096 + 2, 0], errno::EINVAL);
    tally.call("set-tls-kernel-address", nr::SET_TLS, [KERNEL, 0, 0, 0, 0, 0], errno::EINVAL);
    tally.call("set-tls-noncanonical", nr::SET_TLS, [NONCANONICAL, 0, 0, 0, 0, 0], errno::EINVAL);
    tally.call("set-tls-high", nr::SET_TLS, [HIGH, 0, 0, 0, 0, 0], errno::EINVAL);

    let child = spawn(program, &[program, "--exit-unmap-image"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("thread-exit-unmap-image-only", status == 0, status);
    let child = spawn(program, &[program, "--exit-bad-range"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("thread-exit-bad-range", status == 0, status);

    // Threads until there's no more room, twice: the second makes as many as the first,
    // so every slot the first took came back when it ended.
    let before = free_frames();
    let mut counts = [0isize; 2];
    for count in counts.iter_mut() {
        let child = spawn(program, &[program, "--thread-bomb"]);
        *count = if child > 0 { wait(child as usize) } else { child };
    }
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("thread-bomb", counts == [0, 0] && back, counts[0].min(counts[1]));
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe thread-bomb: frames {before}->{after}");
    }

    // The same as a user other than root, which has a cap of its own well short of the
    // machine's. (Its process cap is `uid-process-cap`, below.)
    let child = spawn(program, &[program, "--uid-thread-bomb"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    tally.report("uid-thread-cap", status == 0, status);

    // As many processes as a user other than root may have, and one more: refused, with
    // the machine up and every one of them ending when its pipe does. (`SPAWN`, v1's,
    // says `EBUSY` for a full table, and for a cap too; `SPAWN2` will say `EAGAIN`.)
    let before = free_frames();
    let child = spawn(program, &[program, "--uid-process-bomb"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("uid-process-cap", status == 0 && back, status);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe uid-process-cap: frames {before}->{after}");
    }

    // Pipes until there may be no more: `ENFILE` at the process's cap, and another
    // process can still make one.
    let before = free_frames();
    let child = spawn(program, &[program, "--pipe-exhaustion"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("pipe-exhaustion", status == 0 && back, status);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe pipe-exhaustion: frames {before}->{after}");
    }

    // A child started as its parent's `SETUID` lands, a few times over.
    let mut worst = 0;
    for _ in 0..3 {
        let child = spawn(program, &[program, "--setuid-race"]);
        let status = if child > 0 { wait(child as usize) } else { child };
        if status != 0 {
            worst = status;
        }
    }
    tally.report("setuid-spawn-race", worst == 0, worst);
}

/// The per-user cap on processes for users other than root (`USER_LIMITS` in
/// `kernel/src/sched.rs`).
const UID_PROCESSES: usize = 256;

/// Pipes a process may have alive of the ones it made (`PIPES_PER_PROCESS` in
/// `libs/app/src/pipe.rs`).
const PIPES_PER_PROCESS: usize = 64;

/// Whether a child ended with status 0. v1 keeps only the last 64 exits' statuses, so
/// one of many children ending together may have gone with none left (`ECHILD`): it has
/// ended, and nothing says it failed. Exit records for every child are 1c.8's.
fn ended_well(child: isize) -> bool {
    let status = wait(child as usize);
    status == 0 || status == -errno::ECHILD
}

/// A child holding pipe ends for its parent: reads its standard input until it ends.
fn pipe_wait() -> i32 {
    let mut buffer = [0u8; 16];
    while read(0, &mut buffer) > 0 {}
    0
}

/// As uid 4242, programs until the process cap: exactly as many as it leaves beside
/// this one, each reading a pipe this one then closes, so they all end. Zero if so.
fn uid_process_bomb(program: &str) -> i32 {
    if feos_user::setuid(4242, 4242) != 0 {
        return 2;
    }
    let Ok((read_end, write_end)) = pipe() else {
        return 2;
    };
    let mut children = [0isize; UID_PROCESSES];
    let mut made = 0;
    let error = loop {
        let child = spawn_io(program, &[program, "--pipe-wait"], read_end, INHERIT);
        if child <= 0 {
            break child;
        }
        children[made] = child;
        made += 1;
        if made == UID_PROCESSES {
            break 0;
        }
    };
    close(write_end);
    close(read_end);
    let failed = children[..made].iter().filter(|&&child| !ended_well(child)).count();
    println!("uid-process-bomb: made={made} then={error} failed={failed}");
    if error == -errno::EBUSY && made + 1 == UID_PROCESSES && failed == 0 {
        0
    } else {
        ESCAPED
    }
}

/// Pipes until `ENFILE`: exactly [`PIPES_PER_PROCESS`]. Each is handed on, as when this
/// process had only 16 descriptors to keep them in: the `n`th child reads pipe `n` and
/// holds pipe `n + 1`'s writer, and this process keeps only the first writer and the
/// newest reader. Pipes count against their maker wherever their ends have gone. While
/// at the cap, another process makes one; then closing the first writer ends the chain,
/// child by child, and every pipe can be made again. Zero if all is so.
fn pipe_exhaustion(program: &str) -> i32 {
    let Ok((mut reader, first_writer)) = pipe() else {
        return 2;
    };
    let mut made = 1;
    let mut children = [0isize; PIPES_PER_PROCESS];
    let mut started = 0;
    let error = loop {
        let (next_reader, next_writer) = match pipe() {
            Ok(ends) => ends,
            Err(error) => break error,
        };
        made += 1;
        let child = spawn_io(program, &[program, "--pipe-wait"], reader, next_writer);
        close(reader);
        close(next_writer);
        reader = next_reader;
        if child <= 0 {
            break child;
        }
        children[started] = child;
        started += 1;
        if made > PIPES_PER_PROCESS {
            break 0;
        }
    };
    // Another process isn't held to this one's cap.
    let other = spawn(program, &[program, "--pipe-once"]);
    let other = if other > 0 { wait(other as usize) } else { other };
    close(first_writer);
    close(reader);
    let failed = children[..started].iter().filter(|&&child| !ended_well(child)).count();
    // Each child's pipes closed before its wait returned: all of them can be made again.
    let mut again = 0;
    let mut held = [0usize; 2 * 4];
    while again < 4 {
        let Ok((read_end, write_end)) = pipe() else {
            break;
        };
        held[2 * again] = read_end;
        held[2 * again + 1] = write_end;
        again += 1;
    }
    for &fd in &held[..2 * again] {
        close(fd);
    }
    println!("pipe-exhaustion: made={made} then={error} children={started} failed={failed} other={other} again={again}");
    if error == -errno::ENFILE && made == PIPES_PER_PROCESS && started + 1 == made && failed == 0 && other == 0 && again == 4
    {
        0
    } else {
        ESCAPED
    }
}

/// What `--uid-check` ends with as uid 4242, its threads stopped at the cap.
const CAPPED: i32 = 3;
/// How many children `--setuid-race` may have at once.
const RACERS: usize = 16;

static GAVE_UP_ROOT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn give_up_root(_: usize) -> usize {
    let result = feos_user::setuid(4242, 4242);
    GAVE_UP_ROOT.store(true, core::sync::atomic::Ordering::Release);
    result as usize
}

/// As root, starts `--uid-check` children while a thread becomes uid 4242: each child is
/// root or 4242 through and through, so one that runs as 4242 is capped as 4242. The
/// kernel once took the uid the caps count by and the one the child runs as at two
/// different moments, and a child started between them was 4242 with no cap. Zero if
/// every child kept to its user's.
fn setuid_race(program: &str) -> i32 {
    if feos_user::getuid() != 0 {
        return 2;
    }
    let Ok(_thread) = thread::spawn(give_up_root, 0) else {
        return 2;
    };
    let mut children = [0isize; RACERS];
    let mut started = 0;
    while started < RACERS {
        let done = GAVE_UP_ROOT.load(core::sync::atomic::Ordering::Acquire);
        let child = spawn(program, &[program, "--uid-check"]);
        if child <= 0 {
            break;
        }
        children[started] = child;
        started += 1;
        if done {
            break;
        }
    }
    let (mut root, mut capped, mut escaped) = (0, 0, 0);
    for &child in &children[..started] {
        match wait(child as usize) {
            0 => root += 1,
            status if status == CAPPED as isize => capped += 1,
            _ => escaped += 1,
        }
    }
    println!("setuid-race: children={started} root={root} capped={capped} escaped={escaped}");
    if escaped == 0 && started > 0 && feos_user::getuid() == 4242 {
        0
    } else {
        ESCAPED
    }
}

/// A child of `--setuid-race`: zero as root; as uid 4242, threads until `EAGAIN`, which
/// must come within the cap, and then [`CAPPED`].
fn uid_check() -> i32 {
    match feos_user::getuid() {
        0 => 0,
        4242 => {
            let mut made = 0;
            let error = loop {
                match thread::spawn(wait_forever, 0) {
                    Ok(_) => made += 1,
                    Err(error) => break error,
                }
                if made >= UID_TASKS {
                    break 0;
                }
            };
            if error == -errno::EAGAIN && made < UID_TASKS {
                CAPPED
            } else {
                println!("uid-check: made={made} threads as 4242, then {error}");
                ESCAPED
            }
        }
        _ => ESCAPED,
    }
}

/// The per-user cap on tasks, threads included, the kernel sets for users other than
/// root (`USER_LIMITS` in `kernel/src/sched.rs`).
const UID_TASKS: usize = 512;

/// As uid 4242, threads until `EAGAIN`: exactly as many as the task cap leaves beside
/// the first, however many the machine could still make.
fn uid_thread_bomb() -> i32 {
    if feos_user::setuid(4242, 4242) != 0 {
        return 2;
    }
    let mut made = 0;
    let error = loop {
        match thread::spawn(wait_forever, 0) {
            Ok(_) => made += 1,
            Err(error) => break error,
        }
        if made > UID_TASKS {
            break 0;
        }
    };
    println!("uid-thread-bomb: made={made} then={error}");
    if error == -errno::EAGAIN && made + 1 == UID_TASKS {
        0
    } else {
        ESCAPED
    }
}



fn main(args: Args) -> i32 {
    if args.has("--quiet-exit") {
        return 0;
    }
    if args.has("--concurrent-1mib") {
        return concurrent_1mib();
    }
    if args.has("--children-cap") {
        return children_cap("/bin/malicious");
    }
    if args.has("--spawn-storm") {
        return spawn_storm("/bin/malicious");
    }
    if let Some(name) = args.value_of("--probe") {
        return probe(name);
    }
    if args.has("--thread-bomb") {
        return thread_bomb(args.has("--hold"));
    }
    if args.has("--uid-thread-bomb") {
        return uid_thread_bomb();
    }
    if args.has("--setuid-race") {
        return setuid_race("/bin/malicious");
    }
    if args.has("--uid-check") {
        return uid_check();
    }
    if args.has("--uid-process-bomb") {
        return uid_process_bomb("/bin/malicious");
    }
    if args.has("--pipe-exhaustion") {
        return pipe_exhaustion("/bin/malicious");
    }
    if args.has("--pipe-wait") {
        return pipe_wait();
    }
    if args.has("--fd-exhaustion") {
        return fd_exhaustion();
    }
    if args.has("--fd-heap") {
        return fd_heap();
    }
    if args.has("--lock-exhaustion") {
        return lock_exhaustion();
    }
    if args.has("--lock-one") {
        return lock_one();
    }
    if let Some(path) = args.value_of("--lock-hold") {
        return lock_hold(path);
    }
    if let Some(path) = args.value_of("--setlkw-wait") {
        return setlkw_wait(path);
    }
    if args.has("--fchown-nonroot") {
        return fchown_nonroot();
    }
    if args.has("--tree-swap-race") {
        return tree_swap_race();
    }
    if args.has("--pipe-once") {
        return match pipe() {
            Ok((read_end, write_end)) => {
                close(read_end);
                close(write_end);
                0
            }
            Err(error) => error as i32,
        };
    }

    if args.has("--exit-unmap-image") {
        return exit_unmapping_image();
    }
    if args.has("--exit-bad-range") {
        return exit_bad_ranges();
    }
    if let Some(path) = args.value_of("--dirty-storm") {
        return dirty_storm(path);
    }
    if args.has("--call-loop") {
        return call_loop();
    }
    if args.has("--fault-loop") {
        return fault_loop();
    }
    if args.has("--sp-syscall") {
        return syscall_on_kernel_sp();
    }
    if args.has("--code-rewrite") {
        return code_rewrite();
    }
    if args.has("--vma-exhaustion") {
        return vma_exhaustion();
    }
    if args.has("--shared-remnants") {
        return shared_remnants();
    }
    if args.has("--shared-flood") {
        return shared_flood();
    }
    feos_user::set_driven(args.has("--frames"));

    let program = "/bin/malicious";
    let mut tally = Tally { run: 0, blocked: 0 };
    for name in FAULTS.iter().chain(PRIVILEGED.iter()) {
        run_child(&mut tally, program, name);
    }
    #[cfg(target_arch = "x86_64")]
    if has_umip() {
        run_child(&mut tally, program, "umip-sgdt");
    } else {
        println!("probe umip-sgdt: skipped (this CPU has no UMIP)");
    }
    thread_probes(&mut tally, program);

    // The kernel takes no notice of where a program's stack pointer was.
    let child = spawn(program, &[program, "--sp-syscall"]);
    let status = wait(child as usize);
    tally.report("syscall-with-kernel-sp", status == 0, status);

    // Code made writable, written, and made executable again: refused.
    let child = spawn(program, &[program, "--code-rewrite"]);
    let status = wait(child as usize);
    tally.report("mprotect-own-code", status == 0, status);

    // Every mapping there may be, and one more: `ENOMEM`, the machine up, and the
    // frames back once the child has gone.
    let before = free_frames();
    let child = spawn(program, &[program, "--vma-exhaustion"]);
    let status = wait(child as usize);
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("vma-exhaustion", status == 0 && back, status);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe vma-exhaustion: frames {before}->{after}");
    }

    // Shared objects' heap: remnants of reservations, and floods of small objects from
    // several programs at once. `ENOMEM` (or `SIGBUS`), the machine up, and the frames
    // back after.
    let before = free_frames();
    let child = spawn(program, &[program, "--shared-remnants"]);
    let status = wait(child as usize);
    tally.report("shared-remnants", status == 0, status);
    let mut floods = [0isize; 3];
    for flood in floods.iter_mut() {
        *flood = spawn(program, &[program, "--shared-flood"]);
    }
    let mut worst = 0;
    for &flood in &floods {
        let status = if flood > 0 { wait(flood as usize) } else { flood };
        if status != 0 && status != 135 {
            worst = status;
        }
    }
    let shared = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::SHARED | map::ANONYMOUS);
    if shared > 0 {
        write_at(shared as usize);
        feos_user::munmap(shared as usize, PAGE_SIZE);
    }
    let after = free_frames();
    let back = before.zip(after).is_none_or(|(before, after)| after >= before);
    tally.report("shared-flood", worst == 0 && shared > 0 && back, worst);
    if let (Some(before), Some(after)) = (before, after) {
        println!("probe shared-flood: frames {before}->{after}");
    }

    // Wild pointers and lengths, given to the kernel to use on the program's behalf.
    let scratch = [0u8; 16];
    let good = scratch.as_ptr() as usize;
    let file = open("/etc/motd", flags::READ);
    let fd = if file >= 0 { file as usize } else { 0 };

    tally.call("write-from-kernel", nr::WRITE, [1, KERNEL, 64, 0, 0, 0], errno::EFAULT);
    tally.call("write-from-device", nr::WRITE, [1, DEVICE, 64, 0, 0, 0], errno::EFAULT);
    tally.call("write-from-null", nr::WRITE, [1, 0, 64, 0, 0, 0], errno::EFAULT);
    tally.call("write-from-high", nr::WRITE, [1, HIGH, 64, 0, 0, 0], errno::EFAULT);
    tally.call("write-length-wraps", nr::WRITE, [1, good, usize::MAX, 0, 0, 0], errno::EFAULT);
    tally.call("write-address-wraps", nr::WRITE, [1, usize::MAX - 16, 64, 0, 0, 0], errno::EFAULT);
    tally.call("write-length-huge", nr::WRITE, [1, good, 1 << 48, 0, 0, 0], errno::EFAULT);
    tally.call("write-runs-off-the-stack", nr::WRITE, [1, feos_user::abi::USER_STACK_TOP - 8, 4096, 0, 0, 0], errno::EFAULT);
    tally.call("read-into-kernel", nr::READ, [fd, KERNEL, 64, 0, 0, 0], errno::EFAULT);
    tally.call("read-into-device", nr::READ, [fd, DEVICE, 64, 0, 0, 0], errno::EFAULT);
    tally.call("read-into-own-code", nr::READ, [fd, probe as *const () as usize, 64, 0, 0, 0], errno::EFAULT);
    tally.call("read-length-wraps", nr::READ, [fd, good, usize::MAX, 0, 0, 0], errno::EFAULT);
    tally.call("open-kernel-path", nr::OPEN, [KERNEL, 8, flags::READ, 0, 0, 0], errno::EFAULT);
    tally.call("open-path-wraps", nr::OPEN, [usize::MAX - 2, 8, flags::READ, 0, 0, 0], errno::EFAULT);
    tally.call("open-path-long", nr::OPEN, [good, 1 << 20, flags::READ, 0, 0, 0], errno::ENAMETOOLONG);
    tally.call("mmap-huge", nr::MMAP, [usize::MAX, prot::READ, 0, 0, 0, 0], errno::ENOMEM);
    tally.call("mmap-nothing", nr::MMAP, [0, prot::READ, 0, 0, 0, 0], errno::EINVAL);
    tally.call("mmap-exec-flag", nr::MMAP, [4096, 4, 0, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-kernel", nr::MPROTECT, [KERNEL, 4096, prot::READ, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-wraps", nr::MPROTECT, [usize::MAX & !4095, 8192, prot::READ, 0, 0, 0], errno::EINVAL);
    let page = mmap(4096, prot::READ | prot::WRITE) as usize;
    tally.call("mprotect-anon-exec", nr::MPROTECT, [page, 4096, prot::READ | prot::EXEC, 0, 0, 0], errno::EACCES);
    tally.call("mprotect-write-exec", nr::MPROTECT, [page, 4096, prot::READ | prot::WRITE | prot::EXEC, 0, 0, 0], errno::EACCES);
    tally.call("mprotect-unknown-bit", nr::MPROTECT, [page, 4096, prot::READ | 8, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-hole", nr::MPROTECT, [page, 3 * 4096, prot::READ, 0, 0, 0], errno::ENOMEM);
    tally.call("mprotect-unaligned", nr::MPROTECT, [page + 1, 4096, prot::READ, 0, 0, 0], errno::EINVAL);
    tally.call("munmap-kernel", nr::MUNMAP, [KERNEL, 4096, 0, 0, 0, 0], errno::EINVAL);
    tally.call("munmap-wraps", nr::MUNMAP, [usize::MAX & !4095, 8192, 0, 0, 0, 0], errno::EINVAL);
    tally.call("spawn-kernel-path", nr::SPAWN, [KERNEL, 8, 0, 0, usize::MAX, usize::MAX], errno::EFAULT);
    tally.call("spawn-kernel-argv", nr::SPAWN, [program.as_ptr() as usize, program.len(), KERNEL, 1, usize::MAX, usize::MAX], errno::EFAULT);
    tally.call("spawn-huge-argc", nr::SPAWN, [program.as_ptr() as usize, program.len(), good, usize::MAX, usize::MAX, usize::MAX], errno::ENAMETOOLONG);
    let wild = [UserStr { pointer: KERNEL, length: 16 }];
    tally.call("spawn-kernel-arg-string", nr::SPAWN, [program.as_ptr() as usize, program.len(), wild.as_ptr() as usize, 1, usize::MAX, usize::MAX], errno::EFAULT);
    let wrap = [UserStr { pointer: good, length: usize::MAX }];
    tally.call("spawn-arg-length-wraps", nr::SPAWN, [program.as_ptr() as usize, program.len(), wrap.as_ptr() as usize, 1, usize::MAX, usize::MAX], errno::EFAULT);
    tally.call("spawn-bad-stream", nr::SPAWN, [program.as_ptr() as usize, program.len(), good, 0, 99, usize::MAX], errno::EBADF);
    tally.call("futex-wait-kernel", nr::FUTEX_WAIT, [KERNEL, 0, 1, 0, 0, 0], errno::EFAULT);
    tally.call("futex-wake-kernel", nr::FUTEX_WAKE, [KERNEL, 1, 0, 0, 0, 0], errno::EFAULT);
    tally.call("futex-unaligned", nr::FUTEX_WAIT, [good + 1, 0, 1, 0, 0, 0], errno::EFAULT);
    futex_probes(&mut tally);
    time_probes(&mut tally);
    tally.call("pipe-into-kernel", nr::PIPE, [KERNEL, 0, 0, 0, 0, 0], errno::EFAULT);
    tally.call("pipe-into-own-code", nr::PIPE, [probe as *const () as usize, 0, 0, 0, 0, 0], errno::EFAULT);
    tally.call("surface-too-big", nr::SURFACE_CREATE, [1 << 20, 1 << 20, 0, 0, 0, 0], errno::EINVAL);
    tally.call("surface-overflow", nr::SURFACE_CREATE, [usize::MAX / 2, 3, 0, 0, 0, 0], errno::EINVAL);
    tally.call("event-with-no-surface", nr::POLL_EVENT, [good, 0, 0, 0, 0, 0], errno::EINVAL);
    tally.call("bad-file-descriptor", nr::READ, [9999, good, 1, 0, 0, 0], errno::EBADF);
    tally.call("negative-file-descriptor", nr::WRITE, [usize::MAX, good, 1, 0, 0, 0], errno::EBADF);
    tally.call("close-nonsense", nr::CLOSE, [usize::MAX, 0, 0, 0, 0, 0], errno::EBADF);
    mmap2_probes(&mut tally, file, fd);
    file_probes(&mut tally, program);
    fd_probes(&mut tally, program);
    path_probes(&mut tally);
    dev_probes(&mut tally);
    tree_probes(&mut tally, program);
    lock_probes(&mut tally, program);
    spawn2_probes(&mut tally, program);
    tally.call("no-such-call", 0xdead, [0; 6], errno::ENOSYS);
    tally.call("call-number-huge", usize::MAX, [0; 6], errno::ENOSYS);
    if file >= 0 {
        close(fd);
    }

    println!("malicious: {} of {} probes blocked", tally.blocked, tally.run);
    if tally.blocked == tally.run {
        0
    } else {
        1
    }
}

entry!(main);
