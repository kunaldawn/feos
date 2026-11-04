# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`feos` is a bare-metal `no_std` Rust OS: a monolithic kernel for QEMU's AArch64 `virt` machine and x86_64 `q35` machine, with a preemptive scheduler, an XP-style desktop, a Bash-compatible shell and an original DOS VM (`feos-dosbox`). Everything runs as EL1/ring-0 kernel threads in one address space, with the MMU off on AArch64.

## Commands

`.cargo/config.toml` defaults the build target to `aarch64-unknown-none`, so host tests **must** pass `--target x86_64-unknown-linux-gnu`.

```sh
make build-arm            # also `make build`; release kernel at target/aarch64-unknown-none/release/feos
make build-x64            # release kernel at target/x86_64-unknown-none/release/feos
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
make bundles              # repack apps/desktop/bundle-src/* into apps/desktop/apps/*.fapp
```

- `cargo check --workspace` fails without the Makefile's `--exclude` lists, because each arch-specific crate contains target-specific assembly. Use the `check-*` targets, and add any new arch-specific crate to the other target's exclude list.
- `make test` lists its crates explicitly. A new test crate needs a `test-<name>` target, added to the `test` prerequisites too. `feos-apps` has tests that `make test` does not run: use `cargo test -p feos-apps --target x86_64-unknown-linux-gnu`.
- Kernel code (`kernel/`, `arch/`, `boards/`, most drivers) has no host tests; verify it by booting. The scheduler smoke test is `spawn tick 8 200` a few times in the shell, then `ps` and `kill <pid>`.
- Display and resource variables differ by entry point. `make run-arm` reads `QEMU_DISPLAY`, `QEMU_SMP` and `QEMU_MEM`. `make run-x64` and `cargo run` go through `scripts/qemu-x64.sh` / `scripts/qemu.sh`, which read `FEOS_DISPLAY`, `FEOS_SMP` and `FEOS_MEM`.
- No rustfmt or clippy config exists, and there is no CI.

## Architecture

Dependencies point one way, and nothing points back up:

```
kernel -> boards -> drivers -> libs/hal
kernel -> arch/<arch> -> libs/hal
kernel -> apps -> libs/* (via libs/app's Platform; never hardware)
```

Rules that keep the layers separate (details in `docs/architecture.md`):

- **Drivers** never print and never hard-code addresses; the board passes the base address to the constructor.
- **Libs** never depend on a board, driver or the kernel. They use `#![cfg_attr(not(test), no_std)]` so tests run on the host.
- **Apps** never touch MMIO or arch registers. They get services through `Context::platform` (`feos_app::Platform`: clock, power, cpu, audio, pointer, keyboard, processes, memory, apps, filesystem).
- **The kernel** holds no device logic: boot wiring in `main.rs`, scheduler policy in `sched.rs`, secondary CPU startup in `smp.rs`, heap in `allocator.rs`.
- Inversions avoid upward dependencies: arch crates call handlers the kernel registers (IRQ, exceptions), and libs resolve apps by name through the `AppRegistry` trait in `Platform::apps`.

**Target selection is by Cargo `cfg`, not source switches.** `kernel/Cargo.toml` picks the arch and board crates from `target_arch`, and `kernel/src/main.rs` aliases them as `arch` and `board`. Both arch crates and both boards must therefore expose the same public surface that `main.rs` and `sched.rs` call. A change to one port's surface usually needs the matching change in the other.

**Scheduling.** Each tick, the arch IRQ entry saves a `TrapFrame` on the stack and calls the kernel handler. `sched::preempt` swaps the saved frame for the next `Ready` slot's frame, and the assembly returns into whichever process now owns it. There are `MAX_PROCESSES` (16) slots with slot 0 as the shell (PID 1), one spin-locked table, and a per-CPU `current` array. Take the lock only with local IRQs disabled, and don't allocate inside it. AArch64 runs up to 8 CPUs (PSCI `CPU_ON`, GICv2); x86_64 has `MAX_CPUS = 1` (PIT through IOAPIC). Processes share crate-level statics, so spawned apps must not assume exclusive access to them.

**Boot flow.** `kmain` initialises the console, vectors, display, audio and input, builds `Platform`, starts the scheduler and secondaries, then runs the desktop session (`feos_desktop_app::run_session`). When that ends, it drops into the serial shell (`feos_shell::run`).

**Apps and the desktop.** A shell command is a unit struct implementing `feos_app::App`, listed in `REGISTRY` in `apps/commands/src/lib.rs`. Desktop Start-menu entries are separate: `.fapp` bundles (ZIP/TAR with `manifest.yaml` and an icon) in `apps/desktop/apps/`, embedded by `apps/desktop/build.rs`. A manifest's `entry` names a `REGISTRY` command, so a new GUI app usually needs both. Windowed apps implement `App::windowed`, `open_window` and `frame_window`, and draw into a content-sized `Canvas` the desktop provides. Session config is `apps/desktop/desktop.yaml`, embedded at build time.

**GUI.** `feos-gui` is immediate-mode and caller-owned: apps hold all state, `Ui` allocates nothing and keeps no widget tree or z-order, so floating surfaces must be drawn late in the frame. Component reference: `libs/gui/src/components/README.md`.

**Shell.** `apps/shell` (console and registry adapter) -> `feos-sh` (parser, expansion, builtins, line editor) -> `feos-coreutils` (commands, writable overlay VFS) -> `feos-fs`.

**Filesystem image.** Both boards' `build.rs` compile `boards/qemu-virt/fs/` into a `StaticNode` table, so adding a file means rebuilding. Optional `boards/qemu-virt/disk*.img` files (gitignored) are also linked into the kernel. `make games-image` links `data/games/dos.img` this way, which needs more guest RAM than the image size.

**Heap.** Most code is allocation-free, using fixed-capacity, caller-owned buffers. `kernel/src/allocator.rs` provides a 64 MiB arena for the few `alloc` users, such as the H.264 decoder. Keep new libs allocation-free where practical.

## Documentation

- `docs/architecture.md`: layers, interrupt path, audio, video and filesystem paths.
- `docs/desktop.md`, `docs/terminal.md`, `docs/dosbox.md`, `docs/dosbox-gap.md`: subsystem references.
- `README.md`: feature and command reference. Its "Notes and limitations" section is stale (it says single core, eight process slots, no heap), so trust `docs/architecture.md` and the code over it.
