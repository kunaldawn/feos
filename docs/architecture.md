# Architecture

`feos` is a monolithic kernel built from small crates. The goal is that adding
a board, a driver, a library, or an app is a local change: create one crate,
wire one dependency edge, and register it at the layer above. There are no
`mod` declarations in a central `main.rs` that need editing for every new
device.

The kernel is also a small multiprocess system: on AArch64 every online CPU
runs a GICv2 timer tick, while the x86_64 port currently ticks the boot CPU
through the PIT and IOAPIC. Either way a fixed-size round-robin scheduler in
`kernel/src/sched.rs` switches between kernel threads that share the address
space.

## Layers

| Layer | Location | Depends on | Responsibility |
|-------|----------|------------|----------------|
| Architecture | `arch/<arch>` | `libs/hal` | Boot entry, exception vectors, recoverable IRQ entry, CPU registers, firmware/power calls, timers, HAL service impls |
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

- `feos-arch-aarch64`, `feos-arch-x86_64`
- `feos-board-qemu-virt`, `feos-board-qemu-x86_64`
- `feos-driver-pl011`, `feos-driver-uart16550`, `feos-driver-fw-cfg`,
  `feos-driver-ramfb`
- `feos-driver-pci`, `feos-driver-intel-hda`, `feos-driver-gicv2`,
  `feos-driver-apic`, `feos-driver-i8042`
- `feos-hal`, `feos-sync`, `feos-console`, `feos-gfx`, `feos-gui`,
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

The kernel's `main.rs` should not need any other changes; it only calls board
functions such as `init_console`, `init_display`, `init_audio`,
`init_interrupts`, `init_cpu_interrupts`, `interrupts`, `cpu_mpidr`,
`MAX_CPUS`, `framebuffer_addr`, `kernel_range`, and `bss_range`.

## The x86_64 port

`arch/x86_64` mirrors the AArch64 surface. QEMU loads a flat multiboot image at
1 MiB; `boot.S` keeps the boot CPU, zeroes `.bss`, builds four identity-mapped
1 GiB pages covering the first 4 GiB, enables PAE/long mode and jumps to
`kmain(multiboot_info)`. `interrupts.S` owns an IDT and saves a `TrapFrame`
(GPRs, XMM0..XMM15, vector, error code, RIP/CS/RFLAGS/RSP) before calling the
kernel handler. The return path can therefore replace the frame and switch
processes exactly like the AArch64 `eret` path.

`boards/qemu-x86_64` uses COM1 (`uart16550`), the LAPIC/IOAPIC (`feos-driver-apic`),
legacy PCI config ports (`PciPort`), I/O-port fw_cfg and ramfb, Intel HDA and
the i8042 PS/2 controller. The scheduler tick is PIT channel 0 routed through
IOAPIC GSI 2 to vector 34; the monotonic clock is the TSC, calibrated once
against PIT channel 0. ACPI PM1a control and the PCI reset register implement
`poweroff`/`reboot`. This port currently runs the scheduler on the boot CPU
(`MAX_CPUS = 1`); the architecture interface leaves room for an ACPI MADT +
INIT/SIPI SMP implementation later.

## Interrupts and scheduling

The interrupt path crosses four layers; secondary CPU startup adds a fifth:

1. `feos-driver-gicv2` owns the distributor and CPU interface. The board passes
   the `virt` addresses (`0x0800_0000`, `0x0801_0000`), initializes the
   distributor once, and enables the timer PPI with `enable(TIMER_IRQ)`. QEMU's
   default machine has no GIC security extensions, so the driver configures
   Group 0 and uses `GICC_IAR`/`GICC_EOIR`; the secure Group 1 aliases would be
   needed with `secure=on`. The CPU interface and the PPI configuration
   registers are banked per core, so every core calls `init_cpu()` and enables
   its own timer line.
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
5. Secondary CPUs join through PSCI. `kernel/src/smp.rs` calls `CPU_ON` for
   each index the board reports, passing `arch/aarch64`'s secondary entry point
   and the logical CPU index as the context. The entry stub gives the core a
   private 128 KiB stack, writes the index into `TPIDR_EL1` (read by
   `cpu::index()`), and installs the shared vector table. The kernel handler
   then configures that core's GIC interface and starts its timer.

The process model is deliberately minimal:

- Sixteen slots in a static `.bss` table. Slot 0 is the shell (PID 1) and keeps
  the linker-provided kernel stack; slots 1..15 are background processes with
  128 KiB stacks and start with an otherwise zero context: `ELR` is the app
  entry, `SP` the stack top, `SPSR` EL1h with IRQs unmasked, `x0` the slot
  index, and `LR` a task-exit trampoline.
- States are `Free`, `Ready`, `Running`, and `Exited`. A returning app marks
  itself `Exited` and waits in `wfi`; the tick on the CPU that owns the slot
  frees it. `kill()` frees a non-shell slot immediately only when no CPU is
  currently running or exiting on it.
- The scheduler has one shared, spin-locked table and a per-CPU `current`
  array. A task is `Running` on at most one core at a time, so the same process
  cannot execute twice in parallel and can migrate between cores between ticks.
  A core with no runnable task runs a small idle loop on a private stack and
  counts those ticks as idle.
- Scheduling is preemptive and has no priorities: every ready process gets one
  tick on whichever CPU picks it. `sleep` and other delays spin, but they are
  still preempted.
- Critical sections disable local IRQs with
  `irq::save_and_disable`/`restore` before taking the scheduler lock and do not
  allocate, so they are safe against a tick arriving at any instruction on the
  same core.
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

Floating surfaces (select popups, menus, popovers, drawers, dialogs and toasts)
paint immediately rather than registering an overlay. Apps therefore draw them
late in the frame; the toolkit keeps no hidden z-order. Component state is
fixed-capacity and caller-owned, matching the rest of the toolkit. Components
that need more room than their container simply clip.

The QEMU board exposes a virtio tablet and virtio keyboard, and the desktop
session also translates serial-console keys into `feos_gui::Input` events:
arrows navigate the Start menu and wallpaper, `Enter` launches, `Space` opens
the menu and `Ctrl-\` returns to the shell. The `gui` app still renders a static dashboard
frame with an overview, charts, a classic component gallery and a Fluent gallery
showing the newer collection, navigation and overlay controls, and a Patterns
gallery for advanced composition and chart types. Adding a pointer driver only
requires feeding `Event`s into `Input` and clearing them per frame; the widget
layer is already complete.

## The kernel heap and video path

Most of feos is allocation-free, but the vendored H.264 decoder keeps reference
pictures in `alloc` collections. `kernel/src/allocator.rs` provides one 64 MiB
static arena with a sorted free list, boundary tags and a spin lock for the
multi-core preemptive scheduler. The default QEMU launch boots with 512 MiB of
RAM, leaving room for the arena, process stacks, scanout buffers, and the
optional disk images. `Allocator` implements `GlobalAlloc`; the player reads a
media file into a `Vec`, allocates the RGBA frame and decoder scratch, and
releases them when the clip changes. GIF, MJPEG and raw/RLE decoders do not
allocate themselves.

The video path is:

1. The `player` app scans `Platform::filesystem` for video extensions and reads
   the selected node into a bounded buffer.
2. `feos-video` detects the container and reports dimensions, codec, frame
   count and frame rate. `decode_frame` writes one RGBA8888 frame into the
   caller buffer.
3. H.264 containers call into `feos-h264`; its reference pictures use the
   kernel heap.
4. The app scales the RGBA frame with nearest-neighbour sampling into an
   off-screen canvas, letterboxes according to the selected aspect mode, paints
   the VLC-style chrome around it, and copies the completed frame to the ramfb
   scanout in one pass.

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

## Testing strategy

Bare-metal code cannot be tested on the host when it executes privileged
instructions or touches MMIO. The split makes most logic host-testable:

- `libs/gfx`, `libs/gui`, `libs/audio`, `libs/sync`, `libs/fs`, `libs/keys`,
  and future pure libraries run `cargo test` on the host.
- Drivers are intentionally thin register accessors; they can be exercised in
  QEMU.
- The kernel is tested by booting `make run` (or `make docker-run`). The
  scheduler smoke test is to run `spawn tick 8 200` several times, confirm with
  `ps` that the PIDs report `ready`/`running`, and watch their output interleave
  while the shell stays responsive. With the default `QEMU_SMP=4`, several of
  them run simultaneously on different cores; `info` reports the online CPU
  count. `kill <pid>` should remove one, and an exited process should disappear
  from the next `ps`.

`make check` type-checks every crate for the AArch64 target.
