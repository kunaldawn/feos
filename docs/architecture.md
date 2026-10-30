# Architecture

`feos` is a monolithic kernel built from small crates. The goal is that adding
a board, a driver, a library, or an app is a local change: create one crate,
wire one dependency edge, and register it at the layer above. There are no
`mod` declarations in a central `main.rs` that need editing for every new
device.

The kernel is also a small multiprocess system: a GICv2 timer tick preempts
apps, and a fixed-size round-robin scheduler in `kernel/src/sched.rs` switches
between kernel threads that share the address space.

## Layers

| Layer | Location | Depends on | Responsibility |
|-------|----------|------------|----------------|
| Architecture | `arch/<arch>` | `libs/hal` | Boot entry, exception vectors, recoverable IRQ entry, CPU registers, PSCI, timers, HAL service impls |
| HAL | `libs/hal` | nothing | Device and system-service traits shared by drivers, architectures and apps |
| Drivers | `drivers/<class>/<name>` | `libs/hal`, other drivers | One device per crate; MMIO base comes from the constructor |
| Libraries | `libs/<name>` | other `libs/*` only | Reusable, hardware-independent code |
| Board | `boards/<board>` | drivers, libraries | Machine memory map, device instances, console/display/audio/IRQ wiring |
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

- `feos-arch-aarch64`
- `feos-board-qemu-virt`
- `feos-driver-pl011`, `feos-driver-fw-cfg`, `feos-driver-ramfb`
- `feos-driver-pci`, `feos-driver-intel-hda`, `feos-driver-gicv2`
- `feos-hal`, `feos-sync`, `feos-console`, `feos-gfx`, `feos-gui`,
  `feos-audio`, `feos-app`
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
   `feos-console`, `feos-gfx`, `feos-gui`, and the `Platform` services
   (`Clock`, `Power`, `Cpu`, `Audio`, `Processes`, board facts) passed through
   `Context`.
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
   architecture crate).
2. The kernel's `use ... as board` line.
3. The linker script path in `.cargo/config.toml` (or pass it per build).

The kernel's `main.rs` should not need any other changes; it only calls board
functions such as `init_console`, `init_display`, `init_audio`,
`init_interrupts`, `interrupts`, `framebuffer_addr`, `kernel_range`, and
`bss_range`.

## Interrupts and scheduling

The interrupt path crosses four layers:

1. `feos-driver-gicv2` owns the distributor and CPU interface. The board passes
   the `virt` addresses (`0x0800_0000`, `0x0801_0000`), calls `init()`, and
   enables the timer PPI with `enable(TIMER_IRQ)`. QEMU's default machine has
   no GIC security extensions, so the driver configures Group 0 and uses
   `GICC_IAR`/`GICC_EOIR`; the secure Group 1 aliases would be needed with
   `secure=on`.
2. The board exposes the initialized controller as `&dyn InterruptController`
   from the HAL. The kernel's IRQ handler acknowledges, services, and ends each
   interrupt without naming a concrete driver.
3. `arch/aarch64` owns the entry mechanics. IRQs taken from EL1 with `SP_EL1`
   land in slot 5 of `vectors.S`, which saves an 800-byte `TrapFrame` (x0-x30,
   q0-q31, SP, ELR, SPSR), calls `irq_dispatch`, restores the frame it finds on
   return, and executes `eret`. A handler may therefore replace the frame.
4. `kernel/src/sched.rs` installs that handler. On each tick it reprograms
   `CNTP_TVAL_EL0`, saves the interrupted process's frame, picks the next
   `Ready` slot round-robin, and writes that slot's saved frame over the entry
   frame. The assembly returns to whichever process now owns the frame.

The process model is deliberately minimal:

- Eight slots in a static `.bss` table. Slot 0 is the shell (PID 1) and keeps
  the linker-provided kernel stack; slots 1..7 are background processes with
  16 KiB stacks and start with an otherwise zero context: `ELR` is the app
  entry, `SP` the stack top, `SPSR` EL1h with IRQs unmasked, `x0` the slot
  index, and `LR` a task-exit trampoline.
- States are `Free`, `Ready`, `Running`, and `Exited`. A returning app marks
  itself `Exited` and waits in `wfi`; the next tick frees the slot. `kill()`
  frees a non-shell, non-current slot immediately.
- Scheduling is preemptive and has no priorities: every ready process gets one
  tick. `sleep` and other delays spin, but they are still preempted.
- Critical sections disable IRQs with `irq::save_and_disable`/`restore` and do
  not allocate, so they are safe against a tick arriving at any instruction.
- The process service (`Processes` in `libs/app`) is the only interface apps
  use. `spawn` copies the argument string into the process table, and
  `app_entry` copies it again onto the process stack before running the app so
  a reused slot cannot invalidate the `&str`.

Because the MMU is off and every process runs at EL1, these are kernel threads,
not isolated user processes. There is no per-process page table, no syscall
boundary, and no recovery from a fault inside a background app; adding EL0 and
an address space would turn the same scheduler into a real process switcher.

## The GUI path

`feos-gui` is a pure library above `feos-gfx`; it never talks to a device:

1. A board driver supplies a `feos_gfx::Canvas` over the framebuffer. On the
   QEMU board this is the ramfb XRGB8888 buffer.
2. An app owns its persistent state — `InteractionState`, `TextBuffer`,
   `WindowState`, booleans, sliders — and passes it to widgets each frame.
3. `Ui` clips drawing through a `Painter`, computes deterministic row/column/
   grid layout, and turns pointer and keyboard `Event`s into hover, press,
   click, drag, scroll and focus changes.
4. The app decides what to do with each `Response`; `Ui` never owns a widget
   tree, allocates, or calls `unsafe`.

The current board has no mouse or keyboard driver, so the `gui` app renders a
static dashboard frame. Adding an input driver only requires feeding `Event`s
into `Input` and clearing them per frame; the widget layer is already complete.

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
5. `tone` synthesizes a chord with `feos-audio` and calls `Audio::play`; the
   driver polls the stream's completion bit and returns when the block has been
   consumed. The scheduler may preempt this polling loop, but the HDA stream
   continues and playback finishes when `tone` runs again.

The MMU is off, so DMA addresses are the static buffers' virtual addresses.
When paging is added, the driver must translate them before writing the BDL and
stream registers.

## Testing strategy

Bare-metal code cannot be tested on the host when it executes privileged
instructions or touches MMIO. The split makes most logic host-testable:

- `libs/gfx`, `libs/gui`, `libs/audio`, `libs/sync`, and future pure libraries
  run `cargo test` on the host.
- Drivers are intentionally thin register accessors; they can be exercised in
  QEMU.
- The kernel is tested by booting `make run` (or `make docker-run`). The
  scheduler smoke test is to run `spawn tick 8 200` twice, confirm with `ps`
  that both PIDs report `ready`/`running`, and watch their output interleave
  while the shell stays responsive. `kill <pid>` should remove one, and an
  exited process should disappear from the next `ps`.

`make check` type-checks every crate for the AArch64 target.
