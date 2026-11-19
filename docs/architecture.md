# Architecture

`feos` is a monolithic kernel built from small crates. The goal is that adding
a board, a driver, a library, or an app is a local change: create one crate,
wire one dependency edge, and register it at the layer above. There are no
`mod` declarations in a central `main.rs` that need editing for every new
device.

The kernel is also a small multiprocess system: on both ports every online CPU
has a one-shot timer the scheduler arms for the next thing it has to do, and a
fixed-size scheduler in `kernel/src/sched.rs` switches between kernel threads that
share the address space, and [user programs](#user-programs) that don't.

## Layers

| Layer | Location | Depends on | Responsibility |
|-------|----------|------------|----------------|
| Architecture | `arch/<arch>` | `libs/hal` | Boot entry, exception vectors, recoverable IRQ entry, CPU registers, firmware/power calls, timers, page table changes, the `Arch` trait and HAL service impls |
| HAL | `libs/hal` | nothing | Device and system-service traits shared by drivers, architectures and apps |
| Drivers | `drivers/<class>/<name>` | `libs/hal`, other drivers | One device per crate; MMIO base comes from the constructor |
| Libraries | `libs/<name>` | other `libs/*` only | Reusable, hardware-independent code |
| Board | `boards/<board>`, sharing `boards/common` | drivers, libraries | Machine memory map, what firmware reports, device instances, console/display/audio/IRQ wiring |
| Apps | `apps/*` | `libs/*` only | Shell commands and their UI; never hardware access |
| Kernel | `kernel` | board, arch, libraries, apps | Boot sequence, panic handler, scheduler and process table, platform services, shell launch |

The dependency graph is acyclic and points downwards. Diagnostics are the one
place where the kernel and architecture crates meet: `feos-arch-aarch64`
captures a `FaultInfo` and calls a handler registered by the kernel, so the
architecture crate does not need to know about the console. The IRQ path uses
the same inversion: `arch/aarch64` defines the `TrapFrame` and calls a handler
installed by the kernel, while the scheduler and all process policy stay in
`kernel/`.

## Package naming

Every workspace crate uses the `feos-` prefix. Crates that fill a role use
`feos-<role>-<thing>`:

- `feos-arch-aarch64`, `feos-arch-x86_64`
- `feos-board-qemu-virt`, `feos-board-qemu-x86_64`
- `feos-driver-<name>` for each driver: `feos-driver-pl011`, `feos-driver-gicv3`,
  `feos-driver-virtio-net`, `feos-driver-xhci` and so on (the shared virtio transport is
  `feos-virtio-core`)
- `feos-efi`, the UEFI loader, in `boot/efi`
- libraries by what they are: `feos-hal`, `feos-fs`, `feos-net`, `feos-h264`, ...
- apps: `feos-apps` (`apps/commands`), `feos-shell`, `feos-desktop-app`, `feos-dosbox-app`
- `feos-kernel` (binary name: `feos`)

Crate directories mirror the package name, except that trailing role words are
kept out of directory names (`drivers/serial/pl011`, not
`drivers/serial/feos-driver-pl011`).

## Dependency rules

1. **Drivers never print.** They expose byte-level operations; the console and
   apps decide what text to show.
2. **Drivers never hard-code board addresses.** A board passes the base address
   (`Pl011::new(0x0900_0000)`, `FwCfg::new(0x0902_0000)`).
3. **Libraries never depend on a board, driver, or the kernel.**
4. **Apps never touch MMIO or architecture registers directly.** They use
   `feos-console`, `feos-gfx`, `feos-gui`, `feos-fs`, and the `Platform`
   services (`Clock`, `Power`, `Cpu`, `Audio`, `Processes`, `filesystem`,
   board facts) passed through `Context`.
5. **The kernel contains no device logic.** If `kernel/src/main.rs` starts
   growing a driver, it belongs in `drivers/` or `boards/`. The scheduler is
   policy, not a device: it lives in `kernel/src/sched.rs` and reaches the GIC
   through the `InterruptController` HAL trait owned by the board.

## Adding components

### A driver

```
drivers/<class>/<name>/
├── Cargo.toml       # depends on feos-hal (and any other driver crates)
└── src/lib.rs       # pub struct Name { base: usize }
```

Implement the matching HAL trait, or add a new trait to `feos-hal` if this is a
new device class. The root `members = ["drivers/*/*"]` glob picks the crate up
automatically.

### A library

```
libs/<name>/
├── Cargo.toml
└── src/lib.rs
```

Keep it `no_std`, keep host tests in the crate (`#[cfg(test)]`), and add it to
`[workspace.dependencies]` only when more than one crate uses it. Add a
`test-<name>` host target to the `Makefile` when it has tests.

### An app

An app is a unit struct implementing `feos_app::App`:

```rust
pub struct Hello;

impl App for Hello {
    fn name(&self) -> &'static str { "hello" }
    fn usage(&self) -> &'static str { "hello" }
    fn description(&self) -> &'static str { "say hello" }
    fn run(&self, _context: &mut Context<'_>) {
        feos_console::println!("hello");
    }
}
```

Add the module to `apps/commands/src/lib.rs` and its instance to `REGISTRY`.
For a larger app that deserves its own crate, create `apps/<name>/` and have
the kernel (or `feos-apps`) merge its registry.

Apps receive board facts and system services through
`Context::platform`, so they never import `feos-arch-*` or `feos-board-*`.
An app that wants concurrency calls `context.platform.processes.spawn(app,
args)`; it does not need to know that the target runs as a preemptible kernel
thread. Keep spawned apps free of assumptions about exclusive access to
crate-level statics.

### A board

Copy `boards/qemu-virt`, change the constants, and update:

1. `kernel/Cargo.toml` to depend on the new board crate (and the matching
   architecture crate) under `[target.'cfg(target_arch = "...")'.dependencies]`.
2. The kernel's `#[cfg(target_arch = "...")] use ... as board` line.
3. The linker script path (and runner) in `.cargo/config.toml` for that target.

The kernel's `main.rs` should not need any other changes: it only calls boards
through the `feos_hal::Board` trait, which holds every port to the same surface.
What's common to the QEMU boards, such as the display, the disk bring-up and the
build script for the filesystem image, is in `boards/common`.

A new CPU architecture implements `feos_hal::Arch` the same way. The kernel names
one type for it, `Port`, in `main.rs`, and calls nothing else of the architecture
crate.

## The x86_64 port

`arch/x86_64` mirrors the AArch64 surface. QEMU loads a flat multiboot image at
32 MiB, not the traditional 1 MiB because UEFI firmware has memory of its own past that;
what's below the image goes to the frame allocator. `boot.S` keeps the boot CPU, zeroes `.bss`, builds an identity map of 2 MiB
pages covering the first 4 GiB, enables PAE/long mode and jumps to
`kmain(multiboot_info)`.

**The build target** is `arch/x86_64/x86_64-feos.json`, not `x86_64-unknown-none`.
That target is soft-float, and rustc doesn't let that be turned off: its precompiled
`core` and `compiler_builtins` pass floats in integer registers, so hard-float code
linked against them would be silently wrong. A hard-float custom target has to build
`core` from `rust-src`, which takes unstable cargo flags, so `scripts/cargo-x64.sh`
wraps cargo with them and `RUSTC_BOOTSTRAP=1`. Nothing else in the tree uses unstable
features. Every x86_64 build goes through that script, as `make build-x64` does.

**Floating point** works in any Rust, including interrupt handlers, because every
trap saves all of it before calling Rust. `CPU_LONG_MODE_SETUP` in `boot.S` sets
CR0 and CR4 on each CPU before its first call. `interrupts.S` saves the general
registers and then the 512-byte `FXSAVE64` image, MXCSR included, at the start of the
`TrapFrame`. That makes the state per-task: it's part of what a switch changes. A new
task starts with `fninit`'s control word and MXCSR's reset value.

**The other CPUs** come from the ACPI MADT, found by `feos-firmware` from the RSDP in
the BIOS area, or the one the EFI loader passes. The boot CPU copies a small real-mode stub from `boot.S` to 0x8000 and
sends each one INIT and two startup IPIs, one CPU at a time. Each joins the boot CPU's
path in protected mode. A CPU's logical index and interrupt stack are in its
`CpuArea`, at the GS base. Each has its own TSS and interrupt stacks, in one GDT, and
its own local APIC timer in one-shot mode, calibrated once against the TSC. A logical
index reaches its APIC ID through a table in `feos-driver-apic`, since firmware's
numbering needn't be contiguous.

`boards/qemu-x86_64` uses COM1 (`uart16550`), the LAPIC/IOAPIC (`feos-driver-apic`),
legacy PCI config ports (`PciPort`, under a lock now that there's more than one CPU),
I/O-port fw_cfg and ramfb, Intel HDA and the i8042 PS/2 controller. The monotonic
clock is the TSC, calibrated once against the ACPI PM timer. ACPI PM1a control and the
PCI reset register implement `poweroff`/`reboot`.

## Interrupts and scheduling

The interrupt path crosses four layers; secondary CPU startup adds a fifth:

1. The interrupt controller driver owns the hardware. On AArch64 that's
   `feos-driver-gicv2` or `feos-driver-gicv3`, whichever the device tree names
   (`-machine virt,gic-version=3` selects the second); on x86_64 it's
   `feos-driver-apic`. The boot CPU initializes the shared parts once and every core
   then initializes what's private to it, which on a GICv3 is a redistributor found
   by affinity and a CPU interface in system registers.
2. The board exposes the initialized controller as `&dyn InterruptController` from
   the HAL. The kernel's IRQ handler acknowledges, services, and ends each interrupt
   without naming a concrete driver. On x86_64 the vector is already in the trap
   frame, so acknowledging takes no MMIO; a GIC has to be asked.
3. The architecture crate owns the entry mechanics. While a task runs, the CPU's trap
   stack pointer (`SP_EL1` on AArch64, TSS IST entry 1 on x86_64) points just past
   that task's `TrapFrame`, which the frame exactly fills. So a trap saves the task's
   registers right where they belong, the handler then moves to the CPU's private
   interrupt stack, and returning restores whichever frame the handler asked for
   with `Arch::resume`, leaving the trap stack pointer just past that one. Switching
   tasks is a change of pointer, with no copying and nothing left on a stack another
   CPU might be about to use. Faults are on their own stack on x86_64.
4. `kernel/src/sched.rs` installs the handler and decides what runs. See below.
5. Secondary CPUs join through PSCI `CPU_ON` on AArch64 and INIT/SIPI on x86_64.
   `kernel/src/smp.rs` asks the port to start each CPU firmware reported. The
   entry stub gives the core a private stack, records its logical index and installs
   the exception table, and the kernel handler then configures its interrupt controller
   interface and starts its timer.

### Device interrupts

Device handlers are in one table, `feos_irq::TABLE`, keyed by whatever the controller
calls the interrupt: a vector on x86_64, an INTID on a GIC. A board routes a line,
then registers `fn(usize)` and a value to call it with. The kernel's IRQ path calls
`feos_irq::dispatch` and quiets nothing itself, so a handler has to quiet its device
before returning: the end-of-interrupt follows, and a level-triggered line would come
straight back. Lines can be shared. Devices never leave, so handlers are never removed,
and the interrupt path reads the table without a lock. `ktest irqs` lists it.

What a board has to do is the `IrqRouter` trait in `boards/common`:

| | x86_64 | AArch64 |
|-|--------|---------|
| Interrupt IDs | vectors from 64 up | SPIs |
| Wired PCI (INTx) | IOAPIC GSI 20-23, rotated by slot | SPI 3-6, rotated by slot |
| virtio-mmio | none on this machine | SPI 16 up, by slot |
| MSI and MSI-X | written straight to the local APIC | through the GICv2m frame in the device tree |

A GICv3 has no GICv2m, and message-signalled interrupts on it would need an ITS, which
there's no driver for. PCI devices on such a machine use INTx. INTx for a device behind
a bridge is also not handled, because the pin is rotated at each bridge; the machines
here give such devices MSI-X instead.

`virtio-blk` is interrupt-driven, by the best its function offers: MSI-X, then INTx. It has one
request in flight on purpose: every filesystem operation holds the one `Vfs` state mutex across its
disk I/O, so nothing could submit a second, and the cache already joins contiguous writes into one
request, with metadata ordered by flushes. An async `BlockDevice` would have no caller until that
lock is split. A task waiting for a request blocks, with a 100 ms timeout in case the interrupt never
comes, and polls when interrupts are masked, as they are early in boot. An idle CPU
reschedules after a device interrupt, so a woken task doesn't wait for its next timer.
`virtio-net` and xHCI take the same order, with xHCI falling back to plain MSI. Plain MSI
is also what the Intel HDA controller's codec responses use. Its playback is polled from
the timer on CPU 0. Input devices are polled, USB ones included: their interrupt wakes
nobody, and a report waits for whoever polls next.

### The scheduler

Decisions are in `feos-sched`, tested on the host. The kernel owns frames, stacks,
locks and the per-CPU state.

- 1024 slots (`feos_app::MAX_PROCESSES`) in a static `.bss` table. Slot 0 is the shell
  (PID 1) and keeps the linker-provided kernel stack. Every other slot's stack is made from
  frames when the slot is taken, in a window kept for them (`kernel/src/kstack.rs`): 256 KiB
  for a kernel-linked app, 64 KiB for a user thread, which runs on it only in a call or a
  fault. A task starts with an otherwise zero context.
- States are `Free`, `Spawning`, `Ready`, `Running` and `Blocked`. A running task
  blocks to sleep or to wait on a word (a futex, which `Mutex` and the disk use).
- **Per-CPU queues, in policy.** Each task has a home CPU, and a CPU prefers its own
  tasks. It takes another's when it has nothing of its own at that priority or the
  other's queue is longer, and a new task goes to the quietest CPU. They're fields in
  one table, under one lock (a ticket lock: see below), so there are no per-CPU locks
  and no queue to fall out of step.
- **Bitmaps, not scans.** Beside the slots, the table keeps bitmaps of them, changed with
  every change of state (`Table::update`): the ready tasks at each priority, the starved,
  the sleepers with a deadline, the futex waiters by hash bucket (64 for kernel words, 64
  for user words, and a set of every user waiter for an unmap's range wake), and the slots
  that have left the table, which is all `collect_exits` looks at. A pick walks the ready
  bits from the top priority down; a tick's wakes and deadline walk the sleepers; a wake
  walks one bucket. Each goes in slot order, so it decides exactly as the old scan of
  every slot did, which `make test-sched` checks on random traces against that scan.
- **Priorities**, 0 to 15, larger being more urgent, default 8. Highest first,
  round-robin among equals. A task passed over 64 times gets one turn at the top, so a
  low priority means slow, not stopped. `renice` sets it. A wake-up doesn't preempt:
  a task woken with a higher priority than what's running waits for that CPU's next
  reschedule, at most a time slice away, unless a CPU is idle.
- **Affinity**: a bitmask of CPUs a task may use. `taskset` sets it.
- **One-shot timers.** Every reschedule arms this CPU's timer for the end of the time
  slice (10 ms) or an earlier sleeper on its queue. A sleep therefore wakes when it
  asked to, about 1.2 ms for a 1 ms sleep under QEMU, not on a tick. An idle CPU sleeps
  up to a second unless something's due, and is woken by an IPI when there's work. CPU
  0 keeps to 10 ms because the audio stream is looked after on its tick. CPU time is
  accounted from elapsed time, since there's no longer one tick per tick.
- **Kills and stops.** A kill ends a task at a safe point: holding no lock, and either
  owning no heap or past a 200 ms grace period. A stop holds it off the CPU from its next
  tick. That's for kernel-linked apps. A user program in the kernel (a system call, a page
  fault, or loading) has a nonzero *in-call count* (`IN_CALL`, reported through
  `TaskFacts::in_call`), and while it does it's never reaped and never held: a kill sets
  `kill_pending` and wakes it, every wait a call can reach gives up (pipe reads and writes
  return `pipe::Killed`, futex waits and sleeps end, `wait` returns), the call unwinds, and
  `syscall_entry` or `fault_entry` ends the task. A stop takes effect on the way back to the
  program, so a stopped program never holds the VFS mutex or anything else.
  `sched::ATTENTION` is the hint that sends the return through the scheduler. `spawn`
  clears it in the same critical section that reserves the slot, before the PID can be
  found, so a kill that lands while the task is still being set up isn't forgotten.
- **Teardown in task context.** What a task leaves behind (its address space, the pipes it
  held) is given back with interrupts on, never in the trap that takes it off the table. A
  task that ends itself does it on its way out (`teardown_self`). One reaped by a kill or
  failure is moved by `collect_exits` into a fixed queue (`TEARDOWNS`), which the reaper
  task drains; `spawn` drains it too if it's full. The exit is recorded only once teardown
  is done, so a `wait` that returns means the child's pipes are closed and its frames are
  free. The reaper is protected: it can't be killed or stopped.
- **Threads.** A user program's process can have as many threads as there are slots for
  user programs (`sched::MAX_THREADS`, 960),
  each a slot of its own: `feos_sched::Task::pid` is the thread id, `tgid` the process's pid
  (its first thread's id, as for a kernel-linked app, which is its own group). Thread ids come
  from the pid counter, so they never meet a pid. Kill, stop, priority, affinity and
  `find_pid` take the pid and act on every thread (a thread started while its process is
  stopped starts stopped); a process is there while any thread is,
  its first included or not. Its exit is recorded once, when `collect_exits` takes its last
  thread (`Table::last_of_group`, one look at a ring of the process's slots that are
  live or uncollected, so a thousand exits don't scan the table a thousand times), with
  the status `EXIT` or a fault decided for the whole
  process (`group_code`, set in every thread's entry under the scheduler lock) or else the
  last thread's own; a wait also waits for every queued teardown of its threads. User
  programs, threads included, can't take the last `KEEP_FOR_KERNEL` (64) free slots, so a
  thread bomb stops at 960 tasks and leaves the shell room to `kill` it. A user other than
  root also has caps of its own (`USER_LIMITS`): 512 tasks and 256 processes, counted in
  the table (`Task::uid`, kept by `SETUID`); a spawn reads its creator's uid once, under
  the scheduler lock, for both the table and the task. Heap blocks are charged to their thread
  (`SLOT_PID`); pipes are held in the process's name (`SLOT_TGID`), and released when the
  process's last reference drops.
- The process service (`Processes` in `libs/app`) is the only interface apps use. A full
  table is 1024 `ProcessInfo`s, 72 KiB, so what lists processes (`ps`, `taskmgr`, the
  terminal host's job control, `init`, the desktop session) takes a `ProcessSnapshot`, a
  copy on the heap. When the heap can't hold one, that's "don't know", never "nothing
  running": `init` and the desktop leave every service and window as it was until the
  next tick.
- **Pipes** (`feos_app::pipe`) are 1024 slots in `.bss`, each a few words until it's used;
  a pipe's 16 KiB buffer and its list of holders (as many as hold it) come from the heap
  as it's made and go back as its last end closes, both under the slot's lock, so a killed
  kernel app can't strand either. A user program's process may have 64 alive of the pipes
  it made, wherever their ends have gone since, and can't take the last 64 of the pool
  (`ENFILE` either way); kernel apps have the whole pool. A program's own pipes (`PIPE`,
  `PIPE2`) are owned by its open file descriptions, not held in its name
  (`pipe::create_owned_for_program`, `close_owned`): their ends close as the last
  descriptor referring to each goes. A write of at most `PIPE_BUF` (4096) bytes goes in
  whole (`pipe::write_some`), a larger blocking one waits until it's all in, and a read
  returns what's there as soon as there's anything (`pipe::read_some`); `O_NONBLOCK` makes
  either `EAGAIN` instead of waiting.

### Locks

`IrqSpinLock` masks interrupts and is a ticket lock: a waiter can't be preempted or
killed, so its turn always gets used. `SpinLock` is a test-and-set lock that stays
preemptible, because a ticket-holding waiter that was killed or descheduled would hold
up everyone behind it. It counts in the task's lock depth, and a task holding one isn't
reaped. `Mutex` blocks. On AArch64, waiters sleep in `wfe` and releases send `sev`. Both
spin locks record their owner, a CPU or a task, and taking one you already hold panics
instead of hanging.

### When something fails

See `kernel/src/fatal.rs`. A CPU fault or a panic in an ordinary task, holding no lock
and with interrupts unmasked, **ends that task** with status 139 and a one-line report.
Its pipes close, as any exit's do, and anyone waiting for it is told. Anywhere else (the
boot task, an interrupt handler, a lock held, or an NMI or machine check) **stops the
machine**: an IPI to every CPU, then on x86_64 an NMI for any with interrupts masked,
then a full report. GICv2 has no such thing as an NMI, so an AArch64 CPU with interrupts
masked is only waited for, and the report says so.

For a kernel thread, ending a task isn't isolation. They share an address space, so one
that scribbled on someone else's memory before it faulted has done it, and nothing can
tell. Its heap stays allocated, charged as leaked. Only a [user program](#user-programs)
has a wall around it. A fault there ends it with status 139, whatever it did.

**Windows.** The desktop session is the boot task, so `feos_app::isolate` runs each
window's app on a task of the window's, one call at a time, with the desktop waiting:
`open_window`, `frame_window`, `background_tick` and closing. A fault or a panic there ends
the task, the window closes with a notification, and its app is given a fresh task to clean
up on. A call that hasn't returned in 30 seconds has its task killed. That's failure
containment, not isolation: the app still shares kernel memory and privilege, a fault with
a lock held still stops the machine, a stuck app freezes the desktop until it's killed, and
a stuck app holding a lock can't be killed at all. Foreground apps, with the framebuffer to
themselves, still run on the boot task.

**Task stacks and their guards.** Each slot has 512 KiB of the stack window at 62 GiB, its
stack at the top of it and the rest never mapped, so an overflow faults at the time
however large the frame, with a report that says so. `Arch::reserve_kernel_window` makes
the window's tables at boot, before any process copies the kernel's top-level entries, so
`map_kernel_page` and `unmap_kernel_page` never allocate or change a table later. A stack's
frames are contiguous, and `feos_hal::dma::bus_address` gives a driver the frames' address
for a buffer on one (every driver's device addresses go through it). A stack is taken when
its slot is (or the last one reused, if the right size), goes idle once the task's exit is
collected, and is given back by the reaper, or by anything counting free frames: unmapped,
flushed (broadcast on AArch64, a shootdown on x86_64) and freed. Whatever changes a
stack's state or reads another task's (`ps`'s high-water mark, under a reader count that
keeps a trim off) holds `sched::NoKill`, as a spawn does from its reservation to its
commit, so a kernel app reaped part way can't leave a stack being given back, read or
half made for good. A kill that waited for a `NoKill` takes effect as it's dropped (a
yield, which the switch reaps): one that waited for the next tick to find the app outside
one would wait indefinitely for an app that takes one after another. A stack is painted when
it's taken and measured as it goes; `ktest kstack` checks the deepest calls leave half of a
user thread's unused. The 256 KiB below the boot stack is taken out of the identity map at
boot. Interrupt, idle and early secondary stacks don't have guards.
`Arch::unmap_range` splits the identity map as needed, with tables from the frame
allocator, on the boot CPU before the others start. On AArch64 the architecture wants a
live block replaced by way of an invalid entry and a TLB invalidation, which can't be done
to the block the code's running from. `replace_descriptor` in `boot.S` does it with the MMU
off. Canaries and a stack pointer check at each reschedule remain as a backstop.

## User programs

A user program is a separate static ELF binary, built from `user/` by `make user-arm` and
`make user-x64`, at EL0 or ring 3 with its own page tables. It's in `/bin` in the filesystem
image, because the board's build script embeds whatever's in `user/out/<arch>`. The shell
runs one by path, with arguments, exit status, pipes and redirection. Almost nothing else
in the system is one: the apps and shell are linked into the kernel, and unaffected.

| Where | What |
|---|---|
| `libs/abi` | The stable ABI: call numbers, errors, flags, the address space layout |
| `libs/elf` | Parses and validates an ELF64 file: never panics, fuzzed on the host |
| `libs/vm` | A process's address space: VMAs, page tables, promises, shared-anonymous objects, file mappings, and every access to its memory |
| `libs/pagecache` | The page cache: files' pages in frames, shared by every mapping, and each file's lock-table counts (`ETXTBSY`) and reverse map |
| `arch/*` | EL0 entry and return, page table formats, TLB handling, CPU protections |
| `kernel/src/sched.rs` | The second frame, and switching tables |
| `libs/fdtable` | A process's descriptor table: lowest free, growth to `MAX_FDS`, `dup3`, close-on-exec, ranges |
| `libs/locks` | Advisory locks: record locks' ranges (split, merge, convert), `flock`, conflicts, and the machine's (4096) and per-process (256) caps |
| `kernel/src/user/` | `mod.rs`: loading, the system calls, what a process owns; `files.rs`: open file descriptions and the calls on descriptors; `memory_calls.rs`: the memory calls, faults, promises and the zero frame; `lock_calls.rs`: `FLOCK` and `fcntl`'s record locks |
| `kernel/src/pagecache.rs` | The page cache's frames, its fills from the shared tree, write-back of dirty pages, instruction-cache syncs, and the tree's file hook |
| `kernel/src/surface.rs`, `user_window.rs` | Pixels a program draws, and the window that shows them |
| `user/` | The runtime and the programs: its own cargo workspace |

**Address spaces.** Physical RAM stays identity mapped for the kernel, which didn't move.
Each process has a table of its own, and what's in it depends on the port: on AArch64, a top
table holding the kernel's entries and on x86_64 a PML4 and a page directory pointer table,
under which it shares the kernel's directories. From there down, both are the same
3-level tree for the window `0x10_0000_0000..0x80_0000_0000` (64 GiB to 512 GiB, all of what
the tree reaches), which nothing of the kernel's is in. The kernel's mappings stay in every
table, for the supervisor only: `AP[1]` clear and `UXN` set on AArch64, and no user bit in the
upper-level entry on x86_64. A thread with no process runs on the kernel's own tables.
Because a process's top table holds the kernel's entries beside its own, everything the
kernel maps has to end below `USER_BASE`: `memory::init` checks `Arch::kernel_map_end` and the
RAM in use against it at boot and stops the machine if not. On x86_64 the boot map is the
first 4 GiB; on AArch64 it's RAM and the low device gigabyte, plus QEMU's PCIe ECAM, which
`virt` puts at 256 GiB and `boot.S` therefore maps at 63 GiB instead, the one mapping that
isn't identity (`PCIE_ECAM_BASE` in the board).

The window, from `feos-abi` (`ABI_VERSION` 2):

| Range | What |
|---|---|
| `USER_BASE..USER_IMAGE_END` (64..68 GiB) | the program image (`ET_EXEC` links here); `USER_PIE_BASE` for a later `ET_DYN` |
| `USER_INTERP_BASE..USER_MMAP_BASE` (68..69 GiB) | kept for an interpreter |
| `USER_MMAP_BASE..USER_MMAP_END` | where `mmap` and `mmap2` place things, about 446 GiB |
| 1 GiB (`USER_STACK_GUARD`) | unmapped, below the stack |
| 8 MiB (`USER_STACK_SIZE`) | the main stack, reserved whole and charged as it's touched, as `MAP_NORESERVE` memory is |
| 2 MiB (`USER_ARGS_SIZE`) up to `USER_STACK_TOP` | the argument area: the System V initial stack (`argc`, `argv`, `envp`, the aux vector), the arguments' `UserStr`s, `AT_RANDOM`'s bytes and the strings |
| the last page | unmapped |

The argument area is a mapping of its own, so however long the arguments are the whole
8 MiB stack is free at `main`; a program going down from its first stack pointer crosses the
rest of the argument area into the stack with nothing between. Neither is promised when the
program starts: 10 MiB a program promised whole let a 512 MiB machine start about twenty,
whatever they used. A page of either is promised as it's first touched, and one touched
with no frame to be had ends the program with status 135. `USER_MEMORY_LIMIT` is 64 GiB.

| | AArch64 | x86_64 |
|---|---|---|
| Tables loaded by | `TTBR0_EL1` | `CR3` |
| TLB | An ASID per address space (16-bit where the CPU has them, `TCR_EL1.AS` set in `boot.S`; generations recycle them), global kernel mappings, broadcast invalidations, `tlbi aside1is` as a space goes | No PCIDs: a flush with each change of tables, a shootdown IPI (`IPI_TLB`) to the other CPUs that have the space loaded |
| Kernel kept off user pages | PAN, where the CPU has it (not on the Cortex-A53 QEMU runs by default) | SMEP and SMAP, where the CPU has them |
| No execute | `UXN` and `PXN` | `NX` |
| Enter | `eret` | `iretq` |
| System call | `svc` | `int $0xf0`, the only gate with DPL 3 |
| Thread pointer | `TPIDR_EL0`, the program's to write: saved as a thread leaves its CPU (in a call or not), set as one comes | the FS base, set only by `SET_TLS` (`CR4.FSGSBASE` clear): written at every switch in, nothing cached per CPU, so a selector load is never another's |
| Program's own state cleared | `TPIDRRO_EL0` is always 0 | `swapgs` guards the kernel's GS base; AC and DF on entry |
| What EL0 / ring 3 may do itself | `CTR_EL0` (`SCTLR_EL1.UCT`), `dc zva` (`DZE`), `CNTVCT_EL0` and `CNTFRQ_EL0` (`CNTKCTL_EL1.EL0VCTEN`); cache maintenance by address, the physical counter, timers, `wfi` and `msr daif` trap | `rdtsc` (`CR4.TSD` clear), `cpuid`, `rdrand`; `sgdt` and friends fault where there's UMIP, `rdpmc` always, AVX always (`CR4.OSXSAVE` clear, as the frame holds only what `fxsave` saves) |

**Trapping in.** A trap from a program takes the same path as any other. The trap stack
pointer is just past the frame the scheduler last resumed, here the slot's *user* frame, and
the program's stack pointer has no part in it. So a slot has two frames: `USER_FRAMES` and
`TASK_FRAMES`, and `IN_USER` says which it resumes from. On x86_64 the CPU's floating point
control word and MXCSR, and on AArch64 FPCR, are reset before any kernel code runs, as a
program can set them to anything.

A system call doesn't run in the trap. `sched::on_syscall` points the slot's kernel frame at
`syscall_entry`, at the top of the task's kernel stack, and resumes that: an ordinary
kernel thread that can block, take interrupts and be preempted. When it's done, it writes the
result into the user frame and traps to go back. The arch crates know nothing about what a
call means. A fault from the program goes the same way: `sched::on_user_fault` copies its
registers into the user frame (on x86_64 an exception's frame is on the CPU's fault stack)
and starts `fault_entry` on the kernel stack, which resolves it with interrupts on, or
reports it and ends the program. The kernel stack is only free while the task is at EL0 or
ring 3, so a fault from user mode when the slot isn't `IN_USER` stops the machine.

**What a process is.** A slot points at its task's `user::Process` (`kernel/src/user/process.rs`),
an `Arc` holding the pid, the file table (behind its own mutex), a creation lock and an
`Arc<UserMm>`: the address space, a `feos_vm::Mm` behind a sleeping mutex, with the table to
load, its TLB bookkeeping, the window surface and a number of its own for file reverse maps
(not a slot's: a slot can be reused while the space lives on in other threads). Each thread
of a process has a slot and a reference of its own: the process goes with its last reference
and the space with its own last, and since a space's end writes back what its shared file
mappings dirtied, the scheduler only ever moves a dead task's reference into the teardown
queue, and drops it in task context. A slot's `ROOT` and `TLBS` entries say what to load for
it.

**Starting a program.** `kernel/src/user/load.rs` and `proc.rs`. Whoever starts a program
copies its argument and environment strings into frames of their own (`load::Staged`),
promised before they're taken and never from the kernel heap (a program's counted against
its page limit while its call holds them, a kernel app's with kills held off until its
task has them), and hands them, with what else it starts with (`load::UserStart`), to the
new task through its slot. `SPAWN2` does
everything that can fail in the call: the request and its `UserStr` arrays are copied once
and checked, the child's descriptor table is a copy of the caller's (sharing descriptions)
changed by the actions and swept of close-on-exec descriptors, the program is resolved
against that table and working directory (or, for `SPAWN_EMPTY_PATH`, is the file its
descriptor holds) and held by identity before it's checked (`load::Image`), and a record
for the child is taken in the caller's `Process` (at most `MAX_CHILDREN` unreaped, `EAGAIN`).
v1's `SPAWN` and kernel apps (the shell passes its exported variables as the environment)
hand over a path the task resolves itself, so a missing program still ends with 127. The new
task maps its image, its argument area and stack, and copies the System V initial stack to
the top of the argument area: `argc`, the `argv` and `envp` pointers, the aux vector
(`AT_PHDR`, `AT_RANDOM`, `AT_HWCAP` from `Arch::user_hwcaps`, `AT_FEOS_EXE`,
`AT_FEOS_FEATURES` and the rest), and above them the `UserStr`s v1's convention passes in
the first two registers, which `user/`'s runtime still reads; its `_start` passes the stack
pointer on for `environment()` and `auxv()`. `user/link.ld` loads the ELF and program
headers at the start of the read-only segment, so `AT_PHDR` points at them.

**Running another program.** `EXEC` (`kernel/src/user/proc.rs`) checks and stages as
`SPAWN2` does, resolves the program against the caller's own table, and builds the whole new
image in a fresh space nobody runs on yet (`load::build_image`, which a new task's start uses
too), so every way it can fail is the call's error. Then, holding the process's creation lock
(taken through `Process::creating`, which a kill ends), it has every other thread killed and
waits until each has left the table, switches its own tables to the new space, puts the new
space and program path in place of the old in the same `Process` (whose pid's locks and pipes
stay), closes the close-on-exec descriptors with their record locks, drops the old space and
goes to the new program in the same thread, which keeps its tid. A `#!` script
(`feos_elf::shebang`) is followed to its interpreter, four deep at most, with the
interpreter, its argument and the script's path in place of `argv[0]`
(`load::Staged::with_heads`), for every way of starting a program.

**Process groups and sessions.** Each slot's group and session are `sched`'s `SLOT_PGID` and
`SLOT_SID`, the same in every thread of a process and changed only under the scheduler's
lock (`SETPGID`, `SETSID`, a spawn's `SETPGROUP` or `SETSID`, which is refused there, before
the task is committed, if the group it's to join isn't in its session). A task a kernel app
starts gets a group of its own in session 1, the shell's; a program's child inherits its
creator's unless it asks. Only programs' groups can be joined. `GETPPID` says 1 once no task
has the parent's pid; pids come from a counter, so a pid that's there is the one it was.
`SYSINFO` and `GETRUSAGE` report the machine and the process: `GETRUSAGE`'s CPU time is all
`utime` (the scheduler doesn't split a task's ticks between its program and its calls),
`minflt` counts the faults a space resolved and `maxrss` the most pages it has had frames for.

**Exit records, waits and signals.** A program's child has an exit record in it
(`kernel/src/user/children.rs`), allocated before the child exists and listed in the parent's
`Process`. Every thread's slot points at it (`sched::RECORD`, one reference for the thread
group); each thread's CPU time is added as it's collected, and the last thread's end fills it
in under the scheduler's lock, without allocating, and gives the reference up onto a lock-free
list the reaper drops in task context. The child's `Process` adds its memory use and its own
reaped children's as it goes. A slot points at a record only from its task's commit (a
thread's, from its reservation) until its exit is collected: a refused thread, and every new
spawn, clear it. A record's pid and its group's are held (`children::held`, which the table
asks only once its pid counter has wrapped) until the record is dropped; the table also
skips live tasks' ids and, through a scan made once per window of 4096 pids, teardowns under
way and live tasks' groups and sessions, and gives up (`EAGAIN`) after 2^17 pids, so the
search under the scheduler's lock is bounded. `WAIT4` (`proc.rs`) looks
through the records for one that's ended and whose teardowns are all done (so a wait that
returns means the frames and pipes are free), or with a stop or continue not yet reported,
and otherwise waits on the counter every exit, stop and continue moves on; v1's `WAIT` is it
for one pid, reporting the v1 status. Statuses are a `feos_signal::Status` throughout, encoded
per caller: the POSIX word for `WAIT4`, the code or 128 plus the signal for v1, kernel apps
and the shell, whose waits still use the ring of recent exits. `KILL` (and the shell's `kill`,
Ctrl-C and Ctrl-\\ through `Processes::send_signal`) carries out a signal's default action
(`feos_signal::default_action`) in `sched`: a terminating signal ends the process through the
kill path with the signal as its status; a stop (`SIGSTOP`, and job control's Ctrl-Z as
`SIGTSTP`) holds each thread at its next way back to its program, never in a call, and is
recorded for the parent once none is running and each is blocked or held (CPUs running a
thread of it are asked to reschedule); `SIGCONT` lets it go and records that. A background
job's task is a forked shell, so the shell's `kill %N` (or of `$!`) signals what that task runs,
its process tree but itself (`TaskHost::job_members`).

**Signal handlers.** A process's dispositions (`kernel/src/user/signal.rs`, set by
`SIGACTION`) are behind its signal lock, with what the scheduler needs beside them in atomics:
the process-pending set, which signals are caught, which ignored, and `SIGCHLD`'s
`SA_NOCLDSTOP`/`SA_NOCLDWAIT`. Each slot points at its process's (`sched::SIGNALS`, cleared
under the scheduler's lock before the slot lets its process go), so a signal is decided where
it's sent, under that lock: ignored, made pending for a handler (every CPU running a thread
of it asked to reschedule, as one leaving a call may have looked before the signal came), or
its default action. A program whose process isn't made yet (its image still loading) goes by
the ignored set it inherits. Each thread has a mask and
a pending word of its own (`SIGPIPE`). Delivery is only on the way back to a program: the end
of every call and fault delivers what's due (`signal::deliver`) before the `ToUser` switch,
which checks again and, as does a reschedule resuming a thread at its program, diverts into a
kernel thread on the task's kernel stack (`sched::signal_entry`) when something is due, so a
frame is always built in task context, from `USER_FRAMES`, never from live registers. A
process signal is claimed by one thread, by compare-and-swap (`feos_signal::claim`). The
frame builder and the `SIGRETURN` checker are `feos_signal::frame`, byte layouts for both
ports host-tested on any host, which the arch crates' `TrapFrame`s implement
(`UserFrame`): a frame is built in a zeroed buffer and copied out; `SIGRETURN` copies the
context once and rebuilds the frame from a whitelist (x86_64's selectors from the user
frame, `RFLAGS` sanitised, `MXCSR` against the CPU's `MXCSR_MASK`; AArch64's `SPSR` from
`NZCV` and the port's EL0t value), ending the process with `SIGSEGV` if it fails a check. A
fault is its signal (`FaultDetails::user_signal`, refined to `SEGV_ACCERR`/`MAPERR` and
`SIGBUS` for a page with no frame): a handler it doesn't block runs at once, and otherwise
the process ends with the signal, a `SIGSEGV` still 139. A write to a pipe with no reader
raises `SIGPIPE`. `SIGCHLD` is raised as a child's record is filled in or its stop or continue
noted, and with `SIG_IGN` or `SA_NOCLDWAIT` the record is marked detached and dropped once it
has settled; `WAIT4(WNOHANG)` waits for a child that has ended but not settled, so a handler's
reap always finds it. `EXEC` resets handlers, and a spawned program starts ignoring what its
spawner ignores (but for `SETSIGDEF`); either way `SIGCHLD`'s flags are worked out again from
what's kept, so an inherited or kept `SIG_IGN` of it reaps too. Masks beyond a handler's own, alternate stacks,
interrupted calls and restart are stage 1c.10.

**Threads.** `THREAD_CREATE` checks every argument, takes the process's creation lock and
has `sched::spawn_thread` reserve a slot (refused at the thread cap, with too few slots, or
once the process is ending), share the process, copy the creator's streams and credentials,
and start the thread straight at its program from a fresh user frame: there's no kernel half
to unwind until its first call. `THREAD_EXIT` writes the thread's clear word and unmaps its
range under one hold of the space's lock, and wakes the word's waiters after, so a joiner
that maps over the range at once keeps its mapping. `EXIT`, a fault from the program, and a
contained fault or panic in any thread's call end the whole process: the status is decided
for every thread and the others are killed the cooperative way. Floating point is in each
thread's frames already; the thread pointer is per slot (`sched::TLS`).

**Pointers.** The kernel never dereferences a user address. `feos-vm` walks the process's
tables for each one, checks the whole range before anything happens, and uses the frame
through the kernel's mapping. So a wild pointer is an `EFAULT`, not a kernel fault, and
there's no fixup table to get wrong. The tables only change under the space's lock, and a
copy doesn't hold it: `Mm::translate_and_pin` readies a page and pins its frame in one step
under the lock, and the copy runs on the pinned frame with the lock released
(`UserMm::copy_in`, `copy_out`). An `munmap` meanwhile takes the page away, but its frame
stays until the pin goes. A call that waits (a pipe, the console) waits first and pins
after, or pins, copies into a bounce buffer and unpins before it waits: one page is pinned
at a time, never across a wait.

**TLBs.** A space's `user::tlb::Tlb` records the CPUs that have it loaded and, on AArch64,
its ASID, which it keeps for life and gives back, flushed, when it goes; when they run out (8
bits: 255 live spaces, of up to 1024) a generation rolls over with `tlbi vmalle1is`, and the spaces loaded on
a CPU at that moment keep theirs (`feos_vm::asid`); a CPU loads its tables with the ASID
lock still held, so no rollover falls between its claiming an ASID and using it. AArch64's invalidations reach every CPU,
so that's all. x86_64's `invlpg` reaches only its own CPU: a change to a space loaded on
another CPU is a shootdown. Every change runs through a `Gather` that flushes locally and
notes who else has the space; the frames it gives back are held until those CPUs have taken
`IPI_TLB`, flushed and acknowledged, with the initiator waiting with interrupts on and no
spin lock held. A CPU publishes that it's loading a space (`SeqCst`) before loading its
tables and withdraws only after loading others, and the initiator reads who has it after
clearing the entry, so every CPU either walks the new tables or is asked. Loading the tables
a CPU already has (another thread of the same space) reloads nothing. `ktest tlb` and `ktest
asid-rollover` check both.

**Memory that arrives on first touch.** A process's address space is a `feos_vm::Mm`: a
VMA list (`libs/vm/src/vma.rs`, sorted, joined where neighbours match, capped at
`MAX_VMAS`) saying what's mapped and how, and page tables saying only which pages have
frames. `mmap` and `mmap2` add a VMA and no page-table entries: a reservation, a
`PROT_NONE` range or a page never touched has none. The first touch faults, and
`fault_entry` asks `user::resolve_fault`, which asks `Mm::fault` with what was tried (the
arch's `memory_fault`: the address, write or execute, and only for faults a mapping can
answer). The same fault twice from one thread with nothing between flushes the page, and a
third ends the program, so a fault the tables can't explain never loops. "Nothing between"
is none of the thread's own calls and no change from any thread that can take a page away
(`UserMm::revoke`: unmaps, `mprotect`, discards, truncation zaps, fixed maps), counted in
`UserMm::revokes`, so a thread writing a page another thread keeps discarding just faults
again. A private page that's read gets the machine's zero frame, read-only (`user::init`
makes it at boot); one that's written gets a zeroed frame of its own, replacing the zero
frame if it was there; a `MAP_SHARED` page gets its shared-anonymous object's frame
(`libs/vm/src/shared.rs`), which the object holds a count on, so the frame survives the
mapping leaving and every split of the VMA sees the same one. A call that copies to or from
such a page faults it in first, for the access it needs (`UserMm::fault_in`); a futex word in
writable memory is given its own frame, so the word a waiter checks is the one a writer
changes. A user futex's key is what the word is, not where its frame is (`feos_sched::UserKey`,
a wait of its own kind that a kernel key never matches): the space and the address for private
memory, the shared object or file and the offset for shared memory, so two processes mapping
one object agree. The waiter's frame is pinned from the check to the resume, and an unmap
(or an `MADV_DONTNEED`, which leaves the word on another frame) wakes the waiters on its range, after bumping the space's unmap count, which the scheduler
checks with the word, so a wait can't outlive its memory. A
page-table entry that has to point at another frame (the zero frame giving way to a page of
its own) is cleared and flushed before the new one is written, as AArch64's break-before-make
rule wants.

**Clocks and deadlines.** `kernel/src/time.rs` keeps two clocks. Monotonic time is the port's
counter (`Clock::ticks`: the AArch64 generic timer, the TSC) in nanoseconds, clamped by one
atomic to the latest value any CPU has returned, so a thread that moves between CPUs whose
counters disagree slightly never sees it go back. The wall clock is an offset from it: the
board's RTC (PL031, CMOS) is read once at boot, which puts it up to a second behind since the
RTC counts whole seconds, and then the `rtc` task polls it every millisecond until its second
changes and takes the offset again at that edge, keeping it only if it's larger: both readings
take the counter after the RTC, so both put the clock behind, and keeping the larger means it
never steps back. Boot doesn't wait for it. With no RTC the
wall clock starts at the epoch. The desktop's clock (`KernelClock::unix_time`) reads the same
offset. CPU-time clocks are the scheduler's accounting, in ticks (10 ms): a thread's own
`cpu_ticks`, and for a process every live thread's plus `Process::ended_ticks`, which a thread's
count is folded into as it stops being counted live (in `teardown_self`, or in `collect_exits`
for one reaped), both under the scheduler lock the sum is read under. `CLOCK_GETTIME`,
`CLOCK_GETRES`, `NANOSLEEP` and `FUTEX_WAIT2` (`kernel/src/user/time_calls.rs`) turn every
deadline into a counter value, which is what the scheduler waits for (`Wait::Until`,
`Wait::UserKeyUntil`, with the one-shot timer armed for the earliest); a `CLOCK_REALTIME`
deadline is converted as the call starts. A sleep that's woken early sleeps again, unless
it was woken by a kill, when the call unwinds and the thread ends.

**File mappings and the page cache.** `mmap2` of a file open for reading maps its pages from
the page cache (`libs/pagecache`, glued to frames and the tree by `kernel/src/pagecache.rs`),
keyed by the file's identity (`FileId`: the mount's `dev` and the file's `ino`, which the
tree keeps across renames and doesn't give another file while the cache holds it). A first
touch reads the page into a frame the cache owns and maps it read-only, with a software bit
in the entry saying the frame is the cache's: a write to a private mapping copies it (copy
on write). A shared mapping of a file open for writing may be writable, and is the only
way a cache frame is written through an entry: its pages are mapped read-only until the
first write fault marks the page dirty in the cache and makes the entry writable, so
`write`, `read` and every mapping of the file see one copy. Dirty pages are written back
(`VfsTree::write_back`, which takes the page's bytes under the tree's lock and doesn't
call the hook) at `msync(MS_SYNC)` of their range, which also flushes the filesystems
(`EIO` if either fails, the tree refusing included), for the pages unmapped at `munmap`
and a `MAP_FIXED` over the mapping, for the whole file at every v1 `CLOSE` of it, and at
the space's end (dropping the last `UserMm`, in task context, unmaps its shared file mappings,
and private ones of files with dirty pages, one by one and writes back each one's pages).
There's no background write-back. A write through a writable entry doesn't fault, so a
write-back cleans a page only if no entry had its frame when its bytes were taken (a later
write faults and marks it again), or the file had no writable shared mapping then, after or
in between; a page still mapped writable stays dirty and is written at each write-back.
Reclaim never takes a dirty page, promises don't count them, and a write or truncation
through the tree that fails part way keeps them (and any page an entry has), dropping only
the clean, unmapped pages it may have changed. A program's `READ` lays a file's dirty pages over what the tree
returned, so it sees mapped writes; kernel apps reading through the tree see them after the
next write-back. A writable shared mapping counts as a writable open for `ETXTBSY` from
`mmap` (or `mprotect` to writable) until it's unmapped. Two programs running one binary run the same frames: the loader maps code
and read-only data straight from the cache, populated at load, and copies only what's
writable (or a segment whose offset and address disagree within a page). A page wholly past
the end of the file ends the program with status 135. The tree tells the cache of every
change to a file's bytes (`feos_fs::vfs::FileHook`, under the tree's lock): a write updates
the cached pages in place, so a shared mapping sees it; a truncation drops the pages past the
new end, zeroes the tail of the last, and, if it took whole pages off, marks the spaces
mapping the file (its reverse map) to take them out of their tables, private copies
included, at their next call or fault, the truncating program before its call returns; a removal drops every page, and a mapping keeps only what it had. A fill
reads the tree with no lock of the cache's held, and is thrown away if the file changed
meanwhile, so the cache's locks are always inside the tree's; a fault reads a page at most
`FILL_TRIES` times before it gives up with status 135, since it runs with kills held off.
A program's buffer for a write, or a read, is copied through the kernel a page at a time:
it may be a mapping of the cached page being written, or of a dirty page a read through the
tree would overwrite. `mmap2` knows `/dev/zero`
(anonymous memory) and other devices (`ENODEV`) by the node the path reaches
(`Stat::device`), not by the path's text. Pages nothing maps are counted
as frames to be had: promises may count them, an allocation that finds no free frame takes
one back, and the cache only fills from frames free beyond every promise and the spare,
taking back a page nothing maps first (a clock with a second chance). `ktest user-memory`
lets go of every page nothing maps before it counts.

**Promises and charging.** A page is promised (`feos_vm::Promises`, one machine-wide
`PROMISES` in `kernel/src/user/memory_calls.rs`) only if it could need a frame of the
process's own that can't be taken back: a writable private page, a writable shared object,
unless `MAP_NORESERVE`. v1's `MMAP` still promises every page whatever its protection. A
promise is made only when the free frames cover it, every other promise and a spare
(`SPARE_FRAMES`) for the kernel, and every frame for user memory goes promise, allocate,
release, so two CPUs can't both count on one frame. So running out is `ENOMEM` from the call,
not a fault later. A charged range has its page tables made with it, so its faults never
need a frame nobody promised; an uncharged one gets a table when a fault needs it, and a
`MAP_NORESERVE` page with no frame to give ends the program with status 135 (`EXIT_BUS`).
`mprotect` to writable charges then, or fails with `ENOMEM`. `USER_MEMORY_LIMIT` limits what's
charged or owned: pages promised, frames no promise covers, and page tables, so a program can
reserve far more than it may use. `MADV_DONTNEED` gives private frames back and takes their
promise again. `munmap` takes out any of the program's mappings but the window surface, holes
and all, and frees the leaf and middle tables it leaves empty, after flushing their span.
Code is executable only straight from the page cache: a file page no mapping has copied, in
a range that isn't writable, or a segment of the program's image that the loader had to copy
and that's never been writable; anonymous memory never is. A file with pages mapped to run
can't be opened for writing, truncated or written by anyone, kernel apps included
(`ETXTBSY`), and one open for writing can't be mapped to run: each file's counts are one
critical section in its cache entry. The instruction cache is synced once per frame before
a cached page first runs, and again after a write changes it. `mprotect` to executable syncs
the range's frames to the point of unification first, since the loader only synced the
segments that were executable to start with. A window surface is the program's memory too: promised before
its frames are taken, and counted against the limit as frames it owns.

**Kernel heap a program can make the kernel hold** is bounded too. The VMA list is capped
at `MAX_VMAS`. A shared-anonymous object's header and directory, sized by the object
whether it's touched or not, count against `OBJECT_HEAP_LIMIT` for the space that made it,
until the object's last VMA goes; past it, a shared `mmap` is `ENOMEM`. Its leaves are made
by first touches, 256 bytes per 32 pages, so they're at most a 16th of the frames the
object holds, and frames are bounded machine-wide by the promises. The object itself is
made fallibly (`feos_vm::ObjectRef` in place of an `Arc`), since a failed allocation inside
a change to a space, with a kill held off, would stop the machine. What else a process's
calls hold on the kernel heap, from stage 1c.1 its open file descriptions and the paths they
remember, is charged to its budget (`Process::charge_heap`, `PROCESS_HEAP_LIMIT`, 2 MiB) and,
every process's together, to a quarter of the heap, so however many programs there are the
kernel keeps three quarters of it; past either, the call is `ENOMEM`. Those are made
fallibly too (`feos_sync::TryArc`), before anything the call would have to undo.

**Frames** all come from `feos-frames`, one at a time, counted (a map count per page-table
entry or object holding one, a pin count per copy in flight; a frame goes when both reach
zero), and are all reachable from the process's VMAs, so they go back whichever way it ends: exit, fault, kill, or failing
during load. A program that ends itself frees its address space on the way out, off its own
tables; one reaped at EL0 or ring 3 has it taken by `sched::collect_exits` under the lock,
once no CPU has it loaded, and freed by the reaper. A section that has a frame in hand that
isn't yet in a table holds off a kill, as does any call. A descriptor refers to an open
file description (`user::files::Ofd`, in a `TryArc`), which the descriptors `dup3` and
`F_DUPFD` make share, offset and all: a file (by identity, `FileId`, holding its cache
entry, with its canonical path, offset and status flags, and, open for writing, a writer's
count in that entry), a directory (with a snapshot of its listing whose cookies
`GETDENTS` and `SEEK_SET` resume from), an `O_PATH` handle, a pipe end, `/dev/console`, or one of the
task's standard streams. Paths resolve in `libs/fs/src/vfs/path.rs` against the dirfd's or
working directory's remembered canonical path, so `..` is physical. Unlinking a regular
file's last link while a description, a mapping or a running program's text holds it (the
cache entry's count beyond the cache's own) renames it into its mount's `.feos-orphans`
instead, so it still reads, writes and maps; `user::fs_calls::sweep_orphans` removes it
once nothing holds it, and each writable mount clears what's left there when it's made.
Whether a file is held and its removal are one step to every new hold: `pagecache::file_for`
makes holds under `pagecache::removing`, which the removal holds from asking `held` to
removing or orphaning. `RENAMEAT` over a file (stage 1c.4, `user::tree_calls`) replaces it
in one step of the tree's (`VfsTree::rename_replacing`), asking `held` under the tree's
lock: a replaced file still held becomes an orphan in that same step, so the new name
never names nothing, and the old file goes on working for whoever has it. The permission for a program's
rename, link or symbolic link, and the owner check for its `FCHMODAT` and `UTIMENSAT`, are
judged by the tree under its lock (`feos_fs::vfs::Caller`, the `allow` of the by-identity
changes), against what its walk reached, so a directory swapped for a link after the
kernel's first look can't move the change somewhere else. Programs can't see, reach or make anything named `.feos-orphans`, by
any call, v1's included, given or reached through a link; nor any name longer than 255
bytes (`ENAMETOOLONG`; RAM mounts refuse them to kernel apps too). The table (`feos-fdtable`) grows as it's used, to 1024, and
nothing comes out of it, by a close, a `dup3` over an open descriptor or `CLOSE_RANGE`,
but is dropped after its lock has gone; the process's teardown drops the lot in task
context. A description's last reference closes what it has open: a pipe end, a file's
writer count, and a write-back of what the file's shared mappings dirtied. A call holds a
reference of its own, so another thread's close doesn't end a transfer under way. On a
regular file `READ` and `WRITE` are whole, an `IO_CHUNK` at a time with the offset held,
until the end of the file or a full volume, with the file's write lock (in its cache entry)
held for a write, so an `O_APPEND` write's end is still the end when it writes there.
Locks (stage 1c.5, `user::lock_calls` over `feos-locks`) are one machine-wide table keyed
by a file's or directory's identity (a directory takes only `flock`s, since its description
holds no claim on its inode number): a process owns its record locks, which go when it
closes any descriptor for the file (every place a descriptor leaves its table says so) and
when its `Process` drops; a description owns its `flock` lock, which goes with its last
reference. `F_SETLKW` and a blocking `FLOCK` wait on a kernel futex word every release
moves on, holding nothing, and end when killed. The table is a leaf lock, never held
while user memory is copied. The standard streams' pipes are closed by
`release_pid` as for any task. A surface's frames aren't freed with
the program's, as a window may be reading them while it dies: they're counted, and the last
user frees them.

**The boundary ends** at memory and privilege. It doesn't reach:

- **Kernel-linked apps.** Those aren't behind it at all. They run as root, and `id` says so: a shell, its coreutils and every app are uid 0 and nothing checks them. Permissions apply to user programs.
- **The filesystem, except where it has owners.** A program has a user and group, and on the [native filesystem](#the-native-filesystem) its modes are enforced at the system call boundary. FAT, exFAT and RAM mounts have no owners, so programs read and write all of those whoever they are. An open file is only checked when it's opened.
- **Resources other than memory.** A program has a 64 GiB limit and the machine has its promises, but enough programs can promise all the frames (they get `ENOMEM`), or the 1024 task slots but the 64 kept for the kernel, or as a user other than root that user's 512 tasks or 256 processes (they get `EBUSY`, or `EAGAIN` for a thread), and a program can write to the console, to disk or keep spinning. Nothing bills for it.
- **Bugs in the kernel.** A system call handler is kernel code, and a bug in one is a kernel bug. A panic there ends the process if it held no lock, and otherwise stops the machine.
- **Side channels.** Nothing here tries, and there's no page table isolation.
- **The x86_64 NMI window.** An NMI in the few instructions around `swapgs` would run with the wrong GS base. The only NMI is the fatal path's stop.

Other limits: no `fork`, and shared memory only within a process (or through a shared mapping of a file); a program reading a terminal gets the
shell's cooked lines, with no way to ask for raw keys; and
surfaces are a fixed size, with no way to hear about a resize.

Adding a program is a file in `user/src/bin/` and its name in `USER_BINS` in the `Makefile`. A
windowed one also needs a kernel-linked host app, which `user_window.rs` is, and a bundle.

## The GUI path

`feos-gui` is a pure library above `feos-gfx`; it never talks to a device:

1. A board driver supplies a `feos_gfx::Canvas` over the framebuffer. On the
   QEMU boards that's a virtio-gpu when the machine has one, and otherwise the
   ramfb XRGB8888 buffer, or the framebuffer the EFI loader passed
   (`boards/common/src/display.rs`).
2. An app owns its persistent state — `InteractionState`, `TextBuffer`,
   `WindowState`, component state such as `SelectState` or `TableResponse`
   inputs, booleans, sliders — and passes it to widgets each frame.
3. `Ui` clips drawing through a `Painter`, computes deterministic row/column/
   grid layout, and turns pointer and keyboard `Event`s into hover, press,
   click, drag, scroll and focus changes.
4. The app decides what to do with each `Response`; `Ui` never owns a widget
   tree, allocates, or calls `unsafe`.

The library has two layers. `widgets` contains the small primitives: text,
buttons, inputs, panels, tabs, windows, dialogs and charts. `components`
composes those primitives into the pieces an app puts on a screen: selects,
menus, tables, trees, timelines, alerts, toasts, toolbars, pagination,
accordions, split panes, drawers and popovers. It also carries a
Windows/Fluent-inspired set with WinUI naming: `NavigationView`, `TabView`,
`MenuBar`, `CommandBar`, `ListView`, `GridView`, `FlipView`, `PipsPager`,
`InfoBar`, `InfoBadge`, `Persona`, `SettingsCard`, `HeroBanner`,
`TeachingTip`, `Flyout`, `Calendar`, `DatePicker`, `TimePicker`,
`AutoSuggestBox`, `ToggleButton`, `SplitButton`, `DropDownButton` and
`HyperlinkButton`. The advanced set adds password and multiline fields, range
sliders, knobs, tag and mention inputs, hotkey recorders, cascaders, transfer
lists and drop zones. Composition patterns include avatar groups, presence
badges, group boxes, property grids, log/code/diff viewers, chat and comment
threads, result pages, selector bars, pivots, title bars, floating action
buttons, bottom sheets, popconfirms, command palettes, swipe actions,
pull-to-refresh, annotated scrollbars and zoom controls. Additional charts
cover pie/donut, scatter, radar, heatmap, funnel, bullet, treemap and
candlestick views. Both layers are inherent methods on `Ui`, so an app can mix
them with custom widgets. See
[`libs/gui/src/components/README.md`](../libs/gui/src/components/README.md) for
the full component reference.

### The font

Everything's drawn with one 8x8 font, `feos_gfx::font`, so `Ui` text is UTF-8 with one advance per
character. ASCII is `font8x8`. The rest is a generated table, `font_extended.rs` from
`scripts/gen-font.py`: Latin-1, Latin Extended-A, Cyrillic and some punctuation from the VGA8 console
fonts, with Greek drawn for this project and box drawing and block elements generated from what each
one is. About 590 glyphs, 6 KiB. The console fonts' own Unicode tables have most of Greek pointing at
the wrong bitmaps, which is why having a glyph isn't what the tests check: see `LICENSING.md` for the sources.

What has no glyph is an empty box: CJK, and anything else outside those blocks. Combining marks
aren't composed, and right-to-left text isn't reordered. The Terminal carries box lines through the
two pixels between its rows.

Floating surfaces (select popups, menus, popovers, drawers, dialogs and toasts)
paint immediately rather than registering an overlay. Apps therefore draw them
late in the frame; the toolkit keeps no hidden z-order. Component state is
fixed-capacity and caller-owned, matching the rest of the toolkit. Components
that need more room than their container simply clip.

The QEMU boards expose a virtio tablet and keyboard (on PCI on q35 if they're there, and
otherwise PS/2) and any USB ones, and the desktop session also translates serial-console keys into `feos_gui::Input` events:
arrows navigate the Start menu and wallpaper, `Enter` launches, `Space` opens
the menu and `Ctrl-\` returns to the shell. The `gui` app is the dashboard and
component gallery: a window driven by real input on the desktop, and one static frame
when run from the shell.

## Memory

`feos-firmware` reads what firmware reports: the device tree on AArch64, the multiboot
map and ACPI tables on x86_64, or what the EFI loader passes on either. All of its parsers take untrusted input, so they use
checked reads and return nothing rather than panic, and fuzz-style tests run them over
blobs dumped from real QEMU machines. QEMU passes a bare ELF no pointer on AArch64 and
leaves the tree at the start of RAM, but only if all of it (1 MiB) fits below the kernel,
which is why the image loads at 0x4020_0000.

`kernel/src/memory.rs` adds that RAM to `feos-frames`, a bitmap of 4 KiB frames up to
4 GiB, and takes out the kernel image and what the board says is below it: low memory on a
PC, the device tree on `virt`. An image that doesn't fit in the RAM reported is a panic that
says so. The heap's arena comes out of the frames, half of what's free with at least 64 MiB
if that fits and at most 1 GiB, and page tables come from what's left. The x86_64 boot map
has everything above 2 GiB uncached for the device windows, and `Arch::map_ram_cached` puts
right what's RAM. RAM above QEMU's PCI hole, at 4 GiB, isn't used: with 3 GiB on q35, the
kernel sees 2. `ktest mem` exercises it, and the boot test runs from 192 MiB to 3 GiB.

## Booting

There are three ways in, all ending in `kmain` with one pointer:

| | Started by | Firmware's facts |
|---|---|---|
| x86_64 multiboot | QEMU `-kernel`, with SeaBIOS | Multiboot map, ACPI in the BIOS area |
| AArch64 ELF | QEMU `-kernel`, with no firmware | Device tree at the start of RAM |
| UEFI, either | OVMF or AAVMF, through `boot/efi` | A `feos_bootinfo::Handoff` |

**The EFI loader** (`boot/efi`, built with `scripts/cargo-efi.sh` or `make build-efi`) goes on
a FAT ESP as `\EFI\BOOT\BOOTX64.EFI` or `BOOTAA64.EFI`, with a kernel ELF beside it as
`\feos.elf`. It reads that from its own volume and asks GOP for a 1024x768 mode, if there is
one, and for a framebuffer of 32-bit blue-first pixels: a mode like virtio-gpu's with no
linear framebuffer isn't reported, and the kernel falls back to its own driver. It finds the
ACPI RSDP and device tree in the configuration table, and checks that the image's range is
free in the memory map and that nothing it still needs is in the way, printing what is if
not. Then `ExitBootServices`, a copy to the addresses the ELF is linked at, and a jump. On
x86_64 that's `_start_efi`, in long mode, which builds what the 32-bit entry builds and joins
it; the loader finds it from a mark in the image. On AArch64 it's `_start`, with the caches
cleaned and the MMU off. The handoff is in pages below 4 GiB, outside the image, and the kernel
copies out what it keeps before it allocates anything.

The usable RAM is what the map calls free, loader or boot services memory. ACPI reclaim isn't
reclaimed, since the kernel doesn't read those tables late, and it isn't much.

**ACPI and device trees on AArch64.** AAVMF gives ACPI by default and no device tree, unless the
machine is started with `acpi=off`, and the kernel doesn't parse ACPI there. It reads the tree
QEMU leaves at the start of RAM, which firmware leaves alone. That's a fact about QEMU: real
ACPI-only firmware would leave the GIC and CPUs unfound, and the kernel on one CPU.

**Exception levels on AArch64.** `el_setup` in `boot.S`, on every CPU, gets from whatever level it
starts at to EL1 with a known `SCTLR_EL1`. From EL2 it sets EL1 running as if nothing were above
it, GICv3 system registers included. PSCI's conduit comes from the tree's `/psci` node.
`virtualization=on` starts at EL2 and uses SMC. `secure=on` starts at EL3, where boot puts the
GICv2's interrupts into group 1, and QEMU then has no PSCI at all: one CPU, and no way to
power off. All of this has only run on QEMU.

## USB

`drivers/usb/xhci` drives an xHCI controller (`qemu-xhci` or `nec-usb-xhci`), found on PCI on both
ports, with `boards/common/src/usb.rs` wiring its interrupt and joining its devices to the
machine's own. At boot it enumerates each connected root port, and configures what it knows:

- **Keyboards**, on the boot protocol, whose reports it turns into Linux key codes.
- **Mice and tablets**, by their HID report descriptors. A tablet has no boot protocol.
- **Mass storage**, Bulk-Only with SCSI, as a `BlockIo`. A stick's filesystems go at `/usb<n>`.
  It's never the root and never formatted, so a blank one stays blank.

Descriptors, reports and status wrappers are parsed in code tested on the host, on malformed
input. Limits: no hubs, no hot plug, one LUN, 2 TiB. `FEOS_BOOT_TEST_ONLY=usb` types into a
Terminal through `usb-kbd`, drives `usb-tablet` through QMP (the monitor's `mouse_move` is
relative only), and reads and writes a FAT image that's then `fsck`ed and read on the host.

## Networking

See [`networking.md`](networking.md).

## The kernel heap and video path

Most of feos is allocation-free, but some of it uses `alloc` collections: the media decoders
(H.264 reference pictures, VP8, GIF's canvas, the audio codecs), the writable FAT32, exFAT and
native mounts, the network stack, xHCI, the mixer's rings and the display's later modes.
`kernel/src/allocator.rs` provides one arena with a
sorted free list, a header on each block and a lock that masks interrupts. Usage is
counters, so asking for it costs nothing, and `realloc` grows into a free neighbour when
there is one. Each block records the task that allocated it, for accounting. That can't
be used to reclaim a killed task's heap (KRN-7): a block allocated by one task can
be owned by something shared, such as a file in the VFS or a pipe's reader, and freeing
it would be a use-after-free. What a killed task leaves behind is reported instead.
`Allocator` implements `GlobalAlloc`; the player reads a media file of up to 16 MiB into a
`Vec` (a larger one is streamed: see below), allocates the RGBA frame and decoder scratch, and
releases them when the clip changes.

The video path is:

1. The `player` app scans `Platform::filesystem` for video and sound extensions. Its decode task reads
   the selected node into a bounded buffer, or for a larger file, opens it as a `Source`.
2. `feos-video` detects the container and reports dimensions, codec, frame
   count and frame rate. `decode_frame` writes one RGBA8888 frame into the
   caller buffer.
3. H.264 containers call into `feos-h264`, whose reference pictures use the kernel heap, and
   VP8 into `feos_video::vp8`, which allocates when a key frame sets the size. What a damaged
   H.264 stream doesn't deliver is concealed from the latest reference picture. The decoding is
   `libs/h264/src/ext/`, generic over the sample type, `u8` or `u16`, so that a
   progressive 8-bit stream pays nothing for the rest. A frame store holds a frame's samples
   whichever way it was coded, and a field is that with a doubled stride. Macroblock state is
   kept by position in the frame, so that rows alternate between fields, or between the two
   macroblocks of an MBAFF pair. Reference marking is per field. High 10 and 4:2:2 become 8-bit
   4:2:0 on output, and fields are woven: nothing deinterlaces them. 4:4:4, FMO and SP slices
   are refused.
4. The app scales the RGBA frame with nearest-neighbour sampling into its
   canvas, letterboxes according to the selected aspect mode and paints the
   VLC-style chrome around it. On the desktop that canvas is the window's
   content; run from the shell, it's an off-screen canvas copied to the
   framebuffer in one pass.

### Files too large to read

`feos-source` has one trait, `Source`: bytes at an offset. `apps/commands`' `FileSource` is one
over the filesystem, read at most 256 KiB at a time so that nothing else on the disk waits
long. `VideoDecoder::from_source` keeps the container's metadata and sample table, and reads a
sample as it decodes it, so memory doesn't grow with the file. `SourceStream` does the same for
sound files, a packet at a time. Each format seeks in its own way: PCM directly, FLAC and Ogg
by bisection on what their frames and pages say, and MP3 and ADTS by proportion, or an Xing
table, so they land exactly only at a constant bit rate. All of this runs on the media tasks,
so the desktop keeps drawing while the disk works: the boot test sees frames of about 300 ms at
worst, under TCG. H.264's key frames are checked as they're reached rather than when opening,
as that would read every sample. A fragmented MP4 keeps its sample tables all over the file, so
opening one reads every `moof` into memory, with a map of where each came from. A program stream
has no table at all, so opening one reads the whole file once, to note where its video's packets
and pictures are, and a picture comes together from its packets when it's decoded. Sound in both
is an ordinary list of packets. Raw H.264 and MJPEG files are read once on opening to find their
frames, and H.264's parameter sets are kept in memory, as a seek has to go through them. GIF
isn't streamed.

## The audio path

Audio crosses the same layers as graphics:

1. `feos-audio` renders and mixes 16-bit PCM in a caller-owned buffer. It is
   hardware-independent and host-testable.
2. `feos-driver-pci` reads configuration space, by ECAM on AArch64 and the legacy
   ports on x86_64. The AArch64 kernel boots without firmware, so it also sizes and
   assigns BARs; on x86_64 it keeps what SeaBIOS assigned, and assigns one only
   where nothing did. It enables memory decoding plus bus mastering.
3. `feos-driver-intel-hda` resets the controller, sets up the CORB/RIRB command
   rings, discovers the codec's DAC and output pin, and streams through a cyclic
   ring on an output stream descriptor. Its rings, BDL and PCM buffers are static,
   so no allocator is involved.
4. The board owns the controller instance and assigns the BAR address; the
   kernel exposes the initialised `&dyn Audio` in `Platform`.
5. `feos-mixer` owns the device for everything else. A client opens a `Stream`
   in its own format, and what it writes is converted to 48 kHz stereo and held in
   that stream's ring. A task, `mixer`, started with the first stream, sums the
   rings into the device about every 5 ms, and keeps its queue near 100 ms. Master
   volume and mute live here; the tray's speaker sets them. A stream whose owner
   has been killed is closed by the task. `Capture` goes the other way: one recorder at a
   time, in the device's format, from the codec's input widget on the controller's first
   input stream. `rec` writes what it hears to a WAV. Only `hda-duplex` and `hda-micro` have
   an input, so QEMU's usual `hda-output` reports there's none.
6. AAC is in `feos-audio-codec`, with HE-AAC on top of its core: `sbr.rs` rebuilds the top of the
   spectrum and `ps.rs` makes two channels of one. Their filter banks and everything per sample
   are integers, as a guest under TCG pays heavily for floats, and only what's done once per band
   of a frame is floating point. A stream's first frame can show SBR its config didn't mention, as
   ADTS always has to. Then `PacketDecoder`'s rate and channels change, as they do for MP3, and the
   worker reopens its stream.
7. `tone`, `play`, the Music window, the Media Player and FEOS-DOS all go through
   a `Stream`. `apps/commands/src/audio_worker.rs` decodes files and video tracks
   on a task of its own, and its position, from the frames the mixer reports as
   played, is the Media Player's clock when there is sound. Between half and double speed the
   sound follows, kept at its pitch by `feos_mixer::wsola`. Outside that, it's silent.
8. A media window holds no lock across a frame, because a failure with a lock held stops the
   machine. So when a window's task dies, killed or crashed, the desktop closes the window,
   which tells its decode and audio tasks to go, and they close their streams. The media boot
   test listens to the capture to be sure the sound stops.

Every board identity-maps RAM, so DMA addresses are the buffers' virtual addresses.
`feos_hal::dma::DmaBuf<T>` wraps a value shared with a device and has a `phys()` for the
address to give it, for the day that isn't so. `virtio-blk` uses it; other drivers
still use their statics' addresses directly.

## The filesystem path

The filesystem is a library service, not a device driver:

1. `feos-fs` defines the read-only `FileSystem` trait and `WritableFileSystem` on top
   of it, with backends: `StaticFs` over a compiled-in `StaticNode` table, FAT32,
   exFAT and the [native filesystem](#the-native-filesystem) read and write, FAT12/16
   read-only, and `RamFs` in memory. A `BufferCache` sits between each disk and its
   filesystem, and MBR and GPT partitions are read. All of it is host-tested.
2. `boards/common/fs_image_build.rs`, run by both boards' `build.rs`, walks
   `boards/qemu-virt/fs/` at build time and emits a `StaticNode` table with
   `include_bytes!` for file contents, adding `user/out/<arch>` as `/bin`. Optional
   `boards/qemu-virt/disk*.img` files are embedded too.
3. At boot, `feos_fs::vfs::assemble` (through `boards/common`'s `mount_filesystem`)
   builds the one shared tree, `feos_fs::Vfs`, from what the board found. The first
   disk with a filesystem is the root, later ones are `/disk<n>`, native ones
   `/native<n>` and USB sticks `/usb<n>`. A blank disk is formatted (FAT32, or exFAT when too small for FAT32) and seeded
   from the embedded image. With no usable disk, the root is a `RamFs` seeded from
   the image, so changes last until power-off. Whatever the root, `assemble` then
   mounts the kernel's own (`libs/fs/src/vfs/devfs.rs`): `/tmp`, a `RamFs` that keeps
   owners and modes, `0o1777`, whose sticky bit `VfsTree::check_remove` enforces for
   programs' unlinks; `/dev` (`DevFs`, a fixed table of `null`, `zero`, `full`,
   `console`, `tty` and the links `stdin`, `stdout`, `stderr` and `fd` into
   `/proc/self/fd`) with `/dev/shm`, another sticky RAM mount, on top; and `/proc`
   (`ProcFs`), which shows the *calling* program through the `ProcessView` the kernel
   installs (`kernel/src/user/devfs.rs`): `self`, its own `<pid>` with `exe` and `fd/`,
   nothing for a kernel app. Changes inside `/dev` and `/proc` are
   `FsError::NotPermitted` (`EPERM`). The view runs under the tree's lock and takes the
   process's file table, so nothing may call the tree holding that table. A descriptor's
   link leads to its `FD_PATH`; one that leads nowhere (a pipe's `pipe:[n]`, or
   `(too long)` for a path longer than a link may be) is opened or looked at as the
   descriptor itself, sharing its description, and so is opened one that leads to the
   console, whose node only root may open by name. `ProcFs` tells the tree "a link, and
   to where" in one look (`WritableFileSystem::synthesised_link`), so a descriptor
   changing under the walk never makes one a file. It also lays out what
   the image can't carry: `/root`, `/home/user`, `/usr/lib`, `/usr/include`, and
   `/usr/bin` as a link to `/bin`; `/etc`'s files are in the image, the CA bundle taken
   from the build host by `fs_image_build.rs`.
   A background task, `fsflush`, syncs about once a second. It starts at boot, and a
   change restarts it only once the scheduler says its PID has gone, never because a
   slow pass kept it quiet: otherwise each such pass left another flusher, and its
   stack's frames, behind.
4. The kernel stores the tree in `Platform::filesystem`, a `&dyn FileSystem`, so apps
   never learn how it's stored; `FileSystem::tree` reaches the writable interface.
   The shell, every Terminal and Explorer see the same files.
5. The `files` app renders an XP-style explorer inside a desktop window.
   `apps/commands/src/files/explorer.rs` owns the menu bar, toolbar, address
   history, folder tree, search, five view modes (Details, Icons, List, Tiles,
   Thumbnails), preview pane, dialogs and pointer/keyboard input. `App::windowed` lets the desktop drive it through
   `App::open_window`/`App::frame_window` with a `WindowContext`: the desktop
   supplies a content-sized `Canvas` sub-view, frame input and its platform
   services. The desktop's serial-key loop is in `feos-keys`; routed `Event`s give
   the explorer and the desktop identical arrow, modified-arrow, function-key,
   Page Up/Down and Escape decoding.

Explorer's search walks on a task of its own (`search`, at a low priority), reading files 64 KiB at a
time when it's looking inside them, and the window takes in what's been found each frame. So the VFS
lock is only held for a chunk, and the desktop goes on drawing: `FEOS_BOOT_TEST_ONLY=search` reads a
192 MiB file with a longest frame under 100 ms.

**Crashes.** FAT32 and exFAT mark themselves dirty before changing anything, and `check` runs at mount
on a dirty volume. A torn exFAT entry set is repaired there. A set is only rewritten when it's created,
when it holds no data, so one that isn't whole is cleared; and when its stream entry and primary
change, which leaves its name matching its hash, so it's kept and its checksum put right. On a clean
volume nothing explains a torn set but damage, and the volume stays read-only rather than guess.
`libs/fs/tests/crash.rs` cuts at every write, with writes since the last flush also landing at random.

**Renaming over a file** (`WritableFileSystem::rename_over`) is one step, crash or no crash: the
name holds the old file or the new one. The native format does it in one transaction. FAT and
exFAT mark the moving entry as leaving (a spare bit of FAT's `NTRes` byte, of exFAT's first
reserved byte), commit, have the replaced entry take on the moving file's facts in place (its
name stays), commit, then remove the moving entry and free the replaced file's clusters. A
crash after the second step leaves two names for one file's clusters, and the check drops the
marked one; before it, the mark is cleared. A directory over an empty directory, and a file
kept as an orphan, are two steps there (the orphans go at the next mount anyway).

**Times** are nanoseconds since the epoch (`feos_fs::Timestamp`), with a status change time
(`NodeTimes::changed`). The RAM mounts and the native format keep them whole (the native
format's version 2 inodes add the nanoseconds and an access time; a version 1 volume reads as
whole seconds, and is marked version 2 when first mounted to write), exFAT to 10 ms, FAT to two
seconds, and each reports what it kept. Writes and truncations set the modification and change
times, making, removing or renaming an entry sets its directory's, and a change of mode,
owner, link count, name or times sets the change time; nothing sets the access time but
`UTIMENSAT` (`noatime`). FAT and exFAT keep no change time, and `Stat2` shows the
modification time there. The clock is the board's RTC in whole seconds until the kernel has
its own, when it gives the tree `time::file_time`.

Scanners keep names raw. Only `/` and NUL are replaced, as they'd mean something else in a path.

**Identities and the file hook.** Each mount has a number (`Stat::dev`) no later mount reuses,
and each file an inode number: the native format's own, or the node's ID plus one on RAM, FAT
and exFAT mounts, whose IDs survive renames. `VfsTree::identify`, `read_file` and
`stat_file` (and `write_back`, the page cache's) work by `FileId`, wherever the file has moved; `hold` keeps a file's number from
going to another file even once it's removed (each filesystem's `keep_id`), and keeps its
mount mounted. The page cache's entry for a file is what holds it, and a removed file's hold
goes only with the entry's last reference: a description or mapping that outlives a
kernel app's removal of the name (the shell's `rm`, which makes no orphan) then finds
nothing by the identity, never a file made since with the same number. Every write, truncation and removal of a file's bytes goes through the
`FileHook` the kernel installs: asked first, which is where `ETXTBSY` comes from for kernel
apps (the shell's `cp` over a running program says "Text file busy"), and told after.

### The native filesystem

`feos_fs::native` is FEOS's own on-disk format, for what FAT can't do: inodes with mode, owner, group,
link count and times, hard links, symbolic links, and a journal in place of a check after a crash. The
format and the commit protocol are at the top of `libs/fs/src/native.rs`. In brief, metadata goes through
a redo journal as whole blocks and only ever between whole operations, file data is flushed ahead of
what refers to it, and a block freed in the open transaction isn't reused until that commits. Overwriting
existing file data isn't journalled, so a crash during one can leave a mix, sector by sector, and what wasn't
synced can be lost. It's never half an operation.

It's **never the root**. Existing systems boot from FAT or the embedded image, and a disk with the
signature mounts at `/native<n>`, whichever order the disks come in. A blank disk is still formatted as FAT.
`feos_fs::native::format` is the formatter, used by host tests. There's no guest command for it, since the VFS
has no raw devices: a test's disk comes from `scripts/mkfs-feos.py`, which a host test checks writes
identical bytes. `fsck [-r] PATH` checks the filesystem holding a path, from the guest.

A `NodeId` names a directory entry, not an inode, because a `FileSystem` node has one parent and one name:
so a hard link is two nodes on one inode. `posix`, `set_mode`, `set_owner`, `hard_link` and `fsck` on
`WritableFileSystem` default to "this filesystem can't". `Stat` always has a mode and owner, made up for
FAT, exFAT and RAM mounts from what they do know, with `Stat::posix` false to say so. `chmod`, `chown` and `ln`
without `-s` fail there, and `stat` says the values are made up.

**Users.** A task has a uid and gid in the scheduler, inherited by what it starts. Everything starts as root, and
a user program can give that up for good with `setuid`: `/bin/runas UID GID PROGRAM` is how to start one as
someone else. `VfsTree::check_access` is the check, walking directories one at a time so that a closed
directory hides what's in it. `kernel/src/user/mod.rs` applies it in `open`, `stat`, `mkdir`, `unlink`, `link`,
`chmod` and `chown` and when loading a program, and a new file belongs to its creator. Root may do anything
but run a file nobody has execute permission on. There are no names, groups beyond one per task, setuid
programs or sticky directories. The bits are kept and not acted on.

`libs/fs/tests/native.rs` crashes at every write that reaches the device, in order and with what's unflushed
landing at random, and sabotaging the barriers or the free-space rule makes it fail.
`FEOS_BOOT_TEST_ONLY=native` runs a real virtio disk through kills during writes until one has landed in a commit
and been replayed, with `fsck` and the data checked after each, and `/bin/permtest` from user mode.

## Testing strategy

Bare-metal code cannot be tested on the host when it executes privileged
instructions or touches MMIO. The split makes most logic host-testable:

- `libs/gfx`, `libs/gui`, `libs/audio`, `libs/sync`, `libs/fs`, `libs/keys`,
  and future pure libraries run `cargo test` on the host.
- Drivers are intentionally thin register accessors; they can be exercised in
  QEMU.
- The kernel is tested by booting. `scripts/boot-test.py` boots a kernel headless and
  drives the serial shell, through phases `FEOS_BOOT_TEST_ONLY` can select: `storage`,
  `milestone`, `partition`, `format`, `install`, `persistence`, `gpu`, `transport`, `smp`, `irq`, `ram`, `fault`, `sched`, `gicv3`, `net`, `usb`, `efi`, `el`, `input`, `user`, `hosted`, `jobs`, `names`, `settings`, `terminal`, `search`, `resolution`, `fonts`, `native`. `FEOS_BOOT_TEST_SMP`, `FEOS_BOOT_TEST_CPU` and `FEOS_BOOT_TEST_MEM` set a guest's CPUs, CPU model and RAM where a phase doesn't choose (4, `cortex-a53` or `max`, 512M). The `efi`
  phase needs the loader built, and everything in it runs on OVMF or AAVMF from the image. What can only be seen from inside the
  kernel, such as which CPU a task ran on or what's in an XMM register across a
  preemption, is in `kernel/src/ktest.rs`, the `ktest` shell command. Some of it stops
  the machine on purpose. `scripts/media-boot-test.py` measures real audio output, and real
  input, by giving QEMU's ALSA backend ALSA's `file` plugin to read a tone from. With an image
  from `scripts/make-media-disk.sh`, it plays large files from a virtio disk and seeks.
- `make run` still gives a scheduler smoke test by hand: `spawn tick 8 200` several
  times, `ps` to see them, `kill <pid>` to remove one.

`make check-arm` and `make check-x64` type-check every crate for each port, and `make clippy`
runs clippy over both with warnings as errors; the crates that parse untrusted input add panic
lints in their own `[lints.clippy]`. `make fuzz` runs the fuzz-style tests again, optimised with
overflow checks. `.github/workflows/ci.yml` runs the checks, tests, clippy, fuzz and boot tests.
