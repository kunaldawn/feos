//! Tries everything a program shouldn't be able to do, and reports on each.
//!
//! Anything that would fault runs in a child of its own, since a fault ends the
//! process: `malicious --probe NAME`. A child that gets past its probe says so and
//! exits with a status the parent counts as a failure. Bad arguments to system calls
//! don't end anything, so the parent makes those calls itself and checks the error.
//!
//! Exits zero only when every probe was blocked.

#![no_std]
#![no_main]

use feos_user::abi::{errno, nr, open as flags, prot, UserStr};
use feos_user::{close, entry, getpid, mmap, open, println, spawn, syscall, wait, Args};

/// Where the kernel's image is, and a device, on this machine.
#[cfg(target_arch = "aarch64")]
const KERNEL: usize = 0x4020_0000;
#[cfg(target_arch = "aarch64")]
const DEVICE: usize = 0x0900_0000;
#[cfg(target_arch = "x86_64")]
const KERNEL: usize = 0x0200_0000;
#[cfg(target_arch = "x86_64")]
const DEVICE: usize = 0xfee0_0000;
/// The top half of the address space.
const HIGH: usize = 0xffff_ff80_0000_0000;
/// Not a canonical address on x86_64, past the translation range on AArch64.
const NONCANONICAL: usize = 0x8000_0000_0000_0000;

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
    "zero-a-cache-line",
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

/// Probes that end the process, apart from `PRIVILEGED`.
const FAULTS: &[&str] = &[
    "kernel-read", "kernel-write", "kernel-exec", "device-read", "device-write", "null-read",
    "null-write", "high-read", "high-write", "noncanonical-read", "own-code-write",
    "own-rodata-write", "data-exec", "stack-exec", "mmap-exec", "use-after-munmap", "past-the-stack",
    "stack-overflow", "sp-in-kernel", "mprotect-read-only-write", "mprotect-none-read", "mmap-none-read",
    "lazy-write-after-read-only",
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

fn run_child(tally: &mut Tally, program: &str, name: &str) {
    let child = spawn(program, &[program, "--probe", name]);
    if child <= 0 {
        tally.report(name, false, child);
        return;
    }
    let status = wait(child as usize);
    tally.report(name, status == FAULTED, status);
}

fn main(args: Args) -> i32 {
    if let Some(name) = args.value_of("--probe") {
        return probe(name);
    }
    if args.has("--sp-syscall") {
        return syscall_on_kernel_sp();
    }

    let program = "/bin/malicious";
    let mut tally = Tally { run: 0, blocked: 0 };
    for name in FAULTS.iter().chain(PRIVILEGED.iter()) {
        run_child(&mut tally, program, name);
    }

    // The kernel takes no notice of where a program's stack pointer was.
    let child = spawn(program, &[program, "--sp-syscall"]);
    let status = wait(child as usize);
    tally.report("syscall-with-kernel-sp", status == 0, status);

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
    tally.call("mprotect-own-code", nr::MPROTECT, [(probe as *const () as usize) & !4095, 4096, prot::READ | prot::WRITE, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-own-stack", nr::MPROTECT, [feos_user::abi::USER_STACK_TOP - 4096, 4096, 0, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-wraps", nr::MPROTECT, [usize::MAX & !4095, 8192, prot::READ, 0, 0, 0], errno::EINVAL);
    let page = mmap(4096, prot::READ | prot::WRITE) as usize;
    tally.call("mprotect-exec", nr::MPROTECT, [page, 4096, prot::READ | 4, 0, 0, 0], errno::EINVAL);
    tally.call("mprotect-unaligned", nr::MPROTECT, [page + 1, 4096, prot::READ, 0, 0, 0], errno::EINVAL);
    tally.call("munmap-kernel", nr::MUNMAP, [KERNEL, 4096, 0, 0, 0, 0], errno::EINVAL);
    tally.call("munmap-own-code", nr::MUNMAP, [(probe as *const () as usize) & !4095, 4096, 0, 0, 0, 0], errno::EINVAL);
    tally.call("munmap-own-stack", nr::MUNMAP, [feos_user::abi::USER_STACK_TOP - 4096, 4096, 0, 0, 0, 0], errno::EINVAL);
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
    tally.call("pipe-into-kernel", nr::PIPE, [KERNEL, 0, 0, 0, 0, 0], errno::EFAULT);
    tally.call("pipe-into-own-code", nr::PIPE, [probe as *const () as usize, 0, 0, 0, 0, 0], errno::EFAULT);
    tally.call("surface-too-big", nr::SURFACE_CREATE, [1 << 20, 1 << 20, 0, 0, 0, 0], errno::EINVAL);
    tally.call("surface-overflow", nr::SURFACE_CREATE, [usize::MAX / 2, 3, 0, 0, 0, 0], errno::EINVAL);
    tally.call("event-with-no-surface", nr::POLL_EVENT, [good, 0, 0, 0, 0, 0], errno::EINVAL);
    tally.call("bad-file-descriptor", nr::READ, [9999, good, 1, 0, 0, 0], errno::EBADF);
    tally.call("negative-file-descriptor", nr::WRITE, [usize::MAX, good, 1, 0, 0, 0], errno::EBADF);
    tally.call("close-nonsense", nr::CLOSE, [usize::MAX, 0, 0, 0, 0, 0], errno::EBADF);
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
