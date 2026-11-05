# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`feos` is a bare-metal `no_std` Rust OS: a monolithic kernel for QEMU's AArch64 `virt` machine and x86_64 `q35` machine, with a preemptive scheduler, an XP-style desktop, a Bash-compatible shell and an original DOS VM (`feos-dosbox`). Almost everything runs as EL1/ring-0 kernel threads in one identity-mapped address space. The exception is user programs: separate ELF binaries in `/bin` at EL0/ring 3, each with its own page tables (`kernel/src/user.rs`, built from `user/`).

## Commands

`.cargo/config.toml` defaults the build target to `aarch64-unknown-none`, so host tests **must** pass `--target x86_64-unknown-linux-gnu`. The x86_64 kernel target is the custom hard-float `arch/x86_64/x86_64-feos.json`, built through `scripts/cargo-x64.sh` (`core` from rust-src, unstable flags under `RUSTC_BOOTSTRAP=1`), which every x86_64 build must go through. See the script for why.

```sh
make build-arm            # also `make build`; release kernel at target/aarch64-unknown-none/release/feos
make build-x64            # release kernel at target/x86_64-feos/release/feos
make check-arm            # also `make check`; type-check the workspace for AArch64
make check-x64            # type-check the workspace for x86_64
make test                 # every host test crate in the Makefile
make test-gui             # one crate; targets are named test-<lib>

# One crate, or one test in it
cargo test -p feos-gui --target x86_64-unknown-linux-gnu
cargo test -p feos-gui --target x86_64-unknown-linux-gnu <test_name>

make run-arm              # also `make run`; VNC on 127.0.0.1:5900, serial on stdio
make run-x64
make docker-run-arm       # QEMU window via X11; also docker-run-x64, docker-run-vnc-{arm,x64}
make user-arm             # user programs into user/out/aarch64, which the kernel build puts in /bin; also user-x64
make bundles              # repack apps/desktop/bundle-src/* into apps/desktop/apps/*.fapp
```

- `cargo check --workspace` fails without the Makefile's `--exclude` lists, because each arch-specific crate contains target-specific assembly. Use the `check-*` targets, and add any new arch-specific crate to the other target's exclude list.
- `make test` lists its crates explicitly. A new test crate needs a `test-<name>` target, added to the `test` prerequisites too. `feos-apps` has tests that `make test` does not run: use `cargo test -p feos-apps --target x86_64-unknown-linux-gnu`.
- Kernel code (`kernel/`, `arch/`, `boards/`, most drivers) has no host tests; verify it by booting. `scripts/boot-test.py arm|x64 <kernel>` boots headless and drives the shell, with `FEOS_BOOT_TEST_ONLY` selecting a phase (`smp`, `irq`, `ram`, `fault`, `sched`, `gicv3` and the storage ones). The `ktest` command (`kernel/src/ktest.rs`) is what those phases run in the guest. By hand: `spawn tick 8 200` a few times in the shell, then `ps` and `kill <pid>`.
- Display and resource variables differ by entry point. `make run-arm` reads `QEMU_DISPLAY`, `QEMU_SMP` and `QEMU_MEM`. `make run-x64` and `cargo run` go through `scripts/qemu-x64.sh` / `scripts/qemu.sh`, which read `FEOS_DISPLAY`, `FEOS_SMP` and `FEOS_MEM`.
- No rustfmt or clippy config exists, and there is no CI.

## Architecture

Dependencies point one way, and nothing points back up:

```
kernel -> boards (and boards/common) -> drivers -> libs/hal
kernel -> arch/<arch> -> libs/hal
kernel -> apps -> libs/* (via libs/app's Platform; never hardware)
```

Rules that keep the layers separate (details in `docs/architecture.md`):

- **Drivers** never print and never hard-code addresses; the board passes the base address to the constructor.
- **Libs** never depend on a board, driver or the kernel. They use `#![cfg_attr(not(test), no_std)]` so tests run on the host.
- **Apps** never touch MMIO or arch registers. They get services through `Context::platform` (`feos_app::Platform`: clock, power, cpu, audio, pointer, keyboard, processes, memory, apps, filesystem).
- **The kernel** holds no device logic: boot wiring in `main.rs`, the scheduler's mechanism in `sched.rs`, secondary CPU startup in `smp.rs`, RAM in `memory.rs`, heap in `allocator.rs`, the failure policy in `fatal.rs`, self-tests in `ktest.rs`.
- Inversions avoid upward dependencies: arch crates call handlers the kernel registers (IRQ, exceptions), and libs resolve apps by name through the `AppRegistry` trait in `Platform::apps`.

**Target selection is by Cargo `cfg`, not source switches.** `kernel/Cargo.toml` picks the arch and board crates from `target_arch`, and `kernel/src/main.rs` aliases them as `arch` and `board`. The kernel calls them only through the `feos_hal::Arch` and `feos_hal::Board` traits, as the type aliases `Port` and `Machine` in `main.rs`, so a change to one port's surface fails to build until the other matches.

**User programs.** `user/` is a separate cargo workspace (its own rustflags and linker script), built before the kernel by `make build-*`; a kernel built by plain `cargo` includes whatever is in `user/out/<arch>` and works without it. `libs/abi` is the system call ABI, `libs/elf` parses programs, `libs/vm` owns a process's page tables and is how the kernel reaches user memory: it walks the tables and copies through the identity map, and never dereferences a user address. A new system call takes a new number in `feos-abi`, and validates every argument. Apps linked into the kernel (nearly all of them) are not isolated by any of this.

**Scheduling.** A trap saves a `TrapFrame` straight into the running task's frame slot (the trap stack pointer is kept just past it) and the handler runs on a per-CPU interrupt stack; switching is `Arch::resume(next_frame)`, a pointer change. Policy is in `libs/sched`: `MAX_PROCESSES` (32) slots with slot 0 as the shell (PID 1), one lock, priorities 0-15, affinity masks, a home CPU per task with stealing, and a one-shot timer per CPU armed on each reschedule. Take `SCHEDULER` with its `IrqSpinLock` and don't allocate inside it. Both ports run up to 8 CPUs: PSCI `CPU_ON` with GICv2 or GICv3 on AArch64, ACPI MADT and INIT/SIPI with LAPIC/IOAPIC on x86_64. Processes share crate-level statics, so spawned apps must not assume exclusive access to them.

**Failure.** A fault or panic in a spawned task holding no lock ends just that task (status 139). Anything else stops the machine. Task stacks have guard pages. See `docs/architecture.md`.

**Device interrupts.** `feos_irq` is the handler table. Boards route lines and message-signalled interrupts through `boards/common`'s `IrqRouter`. virtio-blk is the interrupt-driven driver; input and audio playback are polled.

**Boot flow.** `kmain` initialises the console and vectors, reads firmware's tables (`libs/firmware`: DTB, multiboot, ACPI), sets up RAM (`kernel/src/memory.rs`), the interrupt controller, display, audio and input, builds `Platform`, starts the scheduler and secondaries, then runs the desktop session (`feos_desktop_app::run_session`). When that ends, it drops into the serial shell (`feos_shell::run`).

**Apps and the desktop.** A shell command is a unit struct implementing `feos_app::App`, listed in `REGISTRY` in `apps/commands/src/lib.rs`. Desktop Start-menu entries are separate: `.fapp` bundles (ZIP/TAR with `manifest.yaml` and an icon) in `apps/desktop/apps/`, embedded by `apps/desktop/build.rs`. A manifest's `entry` names a `REGISTRY` command, so a new GUI app usually needs both. Windowed apps implement `App::windowed`, `open_window` and `frame_window`, and draw into a content-sized `Canvas` the desktop provides. Session config is `apps/desktop/desktop.yaml`, embedded at build time.

**GUI.** `feos-gui` is immediate-mode and caller-owned: apps hold all state, `Ui` allocates nothing and keeps no widget tree or z-order, so floating surfaces must be drawn late in the frame. Component reference: `libs/gui/src/components/README.md`.

**Shell.** `apps/shell` (console and registry adapter) -> `feos-sh` (parser, expansion, builtins, line editor) -> `feos-coreutils` (commands, writable overlay VFS) -> `feos-fs`.

**Filesystem image.** Both boards' `build.rs` compile `boards/qemu-virt/fs/` into a `StaticNode` table, so adding a file means rebuilding. Optional `boards/qemu-virt/disk*.img` files (gitignored) are also linked into the kernel. `make games-image` links `data/games/dos.img` this way, which needs more guest RAM than the image size.

**Heap.** Most code is allocation-free, using fixed-capacity, caller-owned buffers. `kernel/src/allocator.rs` is a heap over frames from `libs/frames`, sized from discovered RAM (at least 64 MiB when it fits), for the few `alloc` users, such as the H.264 decoder. Keep new libs allocation-free where practical.

## Documentation

- `docs/architecture.md`: layers, interrupt path, audio, video and filesystem paths.
- `docs/desktop.md`, `docs/terminal.md`, `docs/dosbox.md`, `docs/dosbox-gap.md`: subsystem references.
- `README.md`: feature and command reference. Its "Notes and limitations" section is stale (it says single core, eight process slots, no heap), so trust `docs/architecture.md` and the code over it.
