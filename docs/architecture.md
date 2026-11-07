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
- `feos-driver-pl011`, `feos-driver-uart16550`, `feos-driver-fw-cfg`,
  `feos-driver-ramfb`
- `feos-driver-pci`, `feos-driver-intel-hda`, `feos-driver-gicv2`,
  `feos-driver-gicv3`, `feos-driver-apic`, `feos-driver-i8042`,
  `feos-driver-virtio-net`, `feos-driver-xhci`
- `feos-efi`, the UEFI loader, in `boot/efi`
- `feos-abi`, `feos-elf`, `feos-vm`, `feos-hal`, `feos-firmware`, `feos-frames`, `feos-irq`, `feos-sync`, `feos-console`, `feos-gfx`, `feos-gui`,
  `feos-net`, `feos-bootinfo`,
  `feos-audio`, `feos-image`, `feos-audio-codec`, `feos-video`, `feos-h264`,
  `feos-compress`, `feos-archive`, `feos-fs`, `feos-keys`, `feos-app`
- `feos-apps`, `feos-shell`
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

- Thirty-two slots in a static `.bss` table. Slot 0 is the shell (PID 1) and keeps the
  linker-provided kernel stack; the others have 256 KiB stacks and start with an
  otherwise zero context.
- States are `Free`, `Spawning`, `Ready`, `Running` and `Blocked`. A running task
  blocks to sleep or to wait on a word (a futex, which `Mutex` and the disk use).
- **Per-CPU queues, in policy.** Each task has a home CPU, and a CPU prefers its own
  tasks. It takes another's when it has nothing of its own at that priority or the
  other's queue is longer, and a new task goes to the quietest CPU. They're fields in
  one table, under one lock (a ticket lock: see below), so there are no per-CPU locks;
  with this few tasks, the scan is short and there's no queue to fall out of step.
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
- The process service (`Processes` in `libs/app`) is the only interface apps use.

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

**Guard pages.** The 16 KiB below each task stack and the 256 KiB below the boot stack
are taken out of the page tables at boot, so an overflow faults at the time, with a
report that says so. Interrupt, idle and early secondary stacks don't have them.
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
| `libs/vm` | A process's page tables, and every access to its memory |
| `arch/*` | EL0 entry and return, page table formats, TLB handling, CPU protections |
| `kernel/src/sched.rs` | The second frame, and switching tables |
| `kernel/src/user.rs` | Loading, the system calls, what a process owns |
| `kernel/src/surface.rs`, `user_window.rs` | Pixels a program draws, and the window that shows them |
| `user/` | The runtime and the programs: its own cargo workspace |

**Address spaces.** Physical RAM stays identity mapped for the kernel, which didn't move.
Each process has a table of its own, and what's in it depends on the port: on AArch64, a top
table holding the kernel's entries and on x86_64 a PML4 and a page directory pointer table,
under which it shares the kernel's directories. From there down, both are the same
3-level tree for the window `0x10_0000_0000..0x14_0000_0000`, which nothing of the kernel's
is in. The kernel's mappings stay in every table, for the supervisor only: `AP[1]` clear and
`UXN` set on AArch64, and no user bit in the upper-level entry on x86_64. A thread with no
process runs on the kernel's own tables.

| | AArch64 | x86_64 |
|---|---|---|
| Tables loaded by | `TTBR0_EL1` | `CR3` |
| TLB | ASID per slot, global kernel mappings, `tlbi aside1is` as a slot's space goes | No PCIDs: a flush with each change of tables |
| Kernel kept off user pages | PAN, where the CPU has it (not on the Cortex-A53 QEMU runs by default) | SMEP and SMAP, where the CPU has them |
| No execute | `UXN` and `PXN` | `NX` |
| Enter | `eret` | `iretq` |
| System call | `svc` | `int $0xf0`, the only gate with DPL 3 |
| Program's own state cleared | `TPIDR_EL0`, as tables change | `swapgs` guards the kernel's GS base; AC and DF on entry |

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
call means.

**Pointers.** The kernel never dereferences a user address. `feos-vm` walks the process's
tables for each one, checks the whole range before anything happens, and uses the frame
through the kernel's mapping. So a wild pointer is an `EFAULT`, not a kernel fault, and
there's no fixup table to get wrong. This depends on a process having one thread, so that
its tables can't change during a call. A second thread would need something more.

**Frames** all come from `feos-frames`, one at a time, and are all reachable from the
process's tables, so they go back whichever way it ends: exit, fault, kill, or failing
during load. `sched::collect_exits` takes the address space under the lock, once no CPU has
it loaded, and `GoneTasks::finish` frees it. A section that has a frame in hand that isn't
yet in a table holds off a kill. Files need no cleaning up, being a path and a position,
and pipes are closed by `release_pid` as for any task. A surface's frames aren't the
program's, as a window may be reading them while it dies: they're counted, and the last
user frees them.

**The boundary ends** at memory and privilege. It doesn't reach:

- **Kernel-linked apps.** Those aren't behind it at all. They run as root, and `id` says so: a shell, its coreutils and every app are uid 0 and nothing checks them. Permissions apply to user programs.
- **The filesystem, except where it has owners.** A program has a user and group, and on the [native filesystem](#the-native-filesystem) its modes are enforced at the system call boundary. FAT, exFAT and RAM mounts have no owners, so programs read and write all of those whoever they are. An open file is only checked when it's opened.
- **Resources other than memory.** A program has a 64 MiB limit, but enough programs can fill the frames (they get `ENOMEM`), or the 32 process slots (they get `EBUSY`), and a program can write to the console, to disk or keep spinning. Nothing bills for it.
- **Bugs in the kernel.** A system call handler is kernel code, and a bug in one is a kernel bug. A panic there ends the process if it held no lock, and otherwise stops the machine.
- **Side channels.** Nothing here tries, and there's no page table isolation.
- **The x86_64 NMI window.** An NMI in the few instructions around `swapgs` would run with the wrong GS base. The only NMI is the fatal path's stop.

Other limits: one thread per process, so a futex has nothing to wake but itself; no `fork`,
`exec`, signals, demand paging, shared memory or `dup`; a program reading a terminal gets the
shell's cooked lines, with no way to ask for raw keys; `mmap` addresses are only reused from the top; and
surfaces are a fixed size, with no way to hear about a resize.

Adding a program is a file in `user/src/bin/` and its name in `USER_BINS` in the `Makefile`. A
windowed one also needs a kernel-linked host app, which `user_window.rs` is, and a bundle.

## The GUI path

`feos-gui` is a pure library above `feos-gfx`; it never talks to a device:

1. A board driver supplies a `feos_gfx::Canvas` over the framebuffer. On the
   QEMU board this is the ramfb XRGB8888 buffer.
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
the menu and `Ctrl-\` returns to the shell. The `gui` app still renders a static dashboard
frame with an overview, charts, a classic component gallery and a Fluent gallery
showing the newer collection, navigation and overlay controls, and a Patterns
gallery for advanced composition and chart types. Adding a pointer driver only
requires feeding `Event`s into `Input` and clearing them per frame; the widget
layer is already complete.

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

Most of feos is allocation-free, but the vendored H.264 decoder keeps reference
pictures in `alloc` collections. `kernel/src/allocator.rs` provides one arena with a
sorted free list, a header on each block and a lock that masks interrupts. Usage is
counters, so asking for it costs nothing, and `realloc` grows into a free neighbour when
there is one. Each block records the task that allocated it, for accounting. That can't
be used to reclaim a killed task's heap (KRN-7): a block allocated by one task can
be owned by something shared, such as a file in the VFS or a pipe's reader, and freeing
it would be a use-after-free. What a killed task leaves behind is reported instead.
`Allocator` implements `GlobalAlloc`; the player reads a media file of up to 16 MiB into a
`Vec`, allocates the RGBA frame and decoder scratch, and releases them when the clip changes.
GIF, MJPEG and raw/RLE decoders do not allocate themselves.

The video path is:

1. The `player` app scans `Platform::filesystem` for video extensions. Its decode task reads
   the selected node into a bounded buffer, or for a larger file, opens it as a `Source`.
2. `feos-video` detects the container and reports dimensions, codec, frame
   count and frame rate. `decode_frame` writes one RGBA8888 frame into the
   caller buffer.
3. H.264 containers call into `feos-h264`, whose reference pictures use the kernel heap, and
   VP8 into `feos_video::vp8`, which allocates when a key frame sets the size. What a damaged
   H.264 stream doesn't deliver is concealed from the latest reference picture. Interlaced,
   High10 and 4:2:2 streams are refused.
4. The app scales the RGBA frame with nearest-neighbour sampling into an
   off-screen canvas, letterboxes according to the selected aspect mode, paints
   the VLC-style chrome around it, and copies the completed frame to the ramfb
   scanout in one pass.

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
is an ordinary list of packets. Raw H.264, MJPEG files and GIF aren't streamed.

## The audio path

Audio crosses the same layers as graphics:

1. `feos-audio` renders and mixes 16-bit PCM in a caller-owned buffer. It is
   hardware-independent and host-testable.
2. `feos-driver-pci` reads PCIe ECAM configuration space. Because the kernel
   boots without firmware, it also sizes and assigns the HDA controller's BAR
   and enables memory decoding plus bus mastering.
3. `feos-driver-intel-hda` resets the controller, sets up the CORB/RIRB command
   rings, discovers the codec's DAC and output pin, and plays one block through
   an output stream descriptor. Its rings, BDL and PCM buffer are static, so no
   allocator is involved.
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

1. `feos-fs` defines the read-only `FileSystem` trait and `StaticFs`, a backend
   over a flat `StaticNode` table. Path resolution, parent links, directory
   iteration and chunked reads are all allocation-free and host-tested.
2. `boards/qemu-virt/build.rs` walks `boards/qemu-virt/fs/` at build time and
   emits a `StaticNode` table with `include_bytes!` for file contents. The
   board exposes it as `&dyn FileSystem`.
3. The kernel stores that reference in `Platform::filesystem`, so apps never
   learn how the image is stored. A future virtio-blk or RAM backend implements
   the same trait.
4. The `files` app renders an XP-style explorer inside a desktop window.
   `apps/commands/src/files/explorer.rs` owns the menu bar, toolbar, address
   history, folder tree, search, four view modes, preview pane, dialogs and
   pointer/keyboard input. `App::windowed` lets the desktop drive it through
   `App::open_window`/`App::frame_window` with a `WindowContext`: the desktop
   supplies a content-sized `Canvas` sub-view, frame input and its platform
   services. Because the app no longer owns the screen, wallpaper icons, the
   taskbar, the Start menu and other windows stay interactive. The desktop's
   serial-key loop is in `feos-keys`; routed `Event`s give the explorer and
   the desktop identical arrow, modified-arrow, function-key, Page Up/Down and
   Escape decoding.

Because the image is compiled in, adding a file is a rebuild, not a mount. The
same structure can later be fed from an archive or a filesystem driver without
changing the explorer.

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

Scanners keep names raw. Only `/` and NUL are replaced, as they'd mean something else in a path.

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
directory hides what's in it. `kernel/src/user.rs` applies it in `open`, `stat`, `mkdir`, `unlink`, `link`,
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
  drives the serial shell, through phases `FEOS_BOOT_TEST_ONLY` can select: storage,
  `smp`, `irq`, `ram`, `fault`, `sched`, `gicv3`, `net`, `usb`, `efi`, `el`, `input`, `user`, `hosted`, `jobs`, `names`, `settings`, `terminal`, `search`, `resolution`, `fonts`, `native`. The `efi`
  phase needs the loader built, and everything in it runs on OVMF or AAVMF from the image. What can only be seen from inside the
  kernel, such as which CPU a task ran on or what's in an XMM register across a
  preemption, is in `kernel/src/ktest.rs`, the `ktest` shell command. Some of it stops
  the machine on purpose. `scripts/media-boot-test.py` measures real audio output, and real
  input, by giving QEMU's ALSA backend ALSA's `file` plugin to read a tone from. With an image
  from `scripts/make-media-disk.sh`, it plays large files from a virtio disk and seeks.
- `make run` still gives a scheduler smoke test by hand: `spawn tick 8 200` several
  times, `ps` to see them, `kill <pid>` to remove one.

`make check-arm` and `make check-x64` type-check every crate for each port.
