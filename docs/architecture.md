# Architecture

`feos` is a monolithic kernel built from small crates. The goal is that adding
a board, a driver, a library, or an app is a local change: create one crate,
wire one dependency edge, and register it at the layer above. There are no
`mod` declarations in a central `main.rs` that need editing for every new
device.

## Layers

| Layer | Location | Depends on | Responsibility |
|-------|----------|------------|----------------|
| Architecture | `arch/<arch>` | `libs/hal` | Boot entry, exception vectors, CPU registers, PSCI, timers, HAL service impls |
| HAL | `libs/hal` | nothing | Device and system-service traits shared by drivers, architectures and apps |
| Drivers | `drivers/<class>/<name>` | `libs/hal`, other drivers | One device per crate; MMIO base comes from the constructor |
| Libraries | `libs/<name>` | other `libs/*` only | Reusable, hardware-independent code |
| Board | `boards/<board>` | drivers, libraries | Machine memory map, device instances, console/display wiring |
| Apps | `apps/*` | `libs/*` only | Shell commands and their UI; never hardware access |
| Kernel | `kernel` | board, arch, libraries, apps | Boot sequence, panic handler, platform services, shell launch |

The dependency graph is acyclic and points downwards. Diagnostics are the one
place where the kernel and architecture crates meet: `feos-arch-aarch64`
captures a `FaultInfo` and calls a handler registered by the kernel, so the
architecture crate does not need to know about the console.

## Package naming

Every workspace crate uses the `feos-` prefix. Crates that fill a role use
`feos-<role>-<thing>`:

- `feos-arch-aarch64`
- `feos-board-qemu-virt`
- `feos-driver-pl011`, `feos-driver-fw-cfg`, `feos-driver-ramfb`
- `feos-hal`, `feos-sync`, `feos-console`, `feos-gfx`, `feos-app`
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
   `feos-console`, `feos-gfx`, and the `Platform` services (`Clock`, `Power`,
   `Cpu`, board facts) passed through `Context`.
5. **The kernel contains no device logic.** If `kernel/src/main.rs` starts
   growing a driver, it belongs in `drivers/` or `boards/`.

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

### A board

Copy `boards/qemu-virt`, change the constants, and update:

1. `kernel/Cargo.toml` to depend on the new board crate (and the matching
   architecture crate).
2. The kernel's `use ... as board` line.
3. The linker script path in `.cargo/config.toml` (or pass it per build).

The kernel's `main.rs` should not need any other changes; it only calls board
functions such as `init_console`, `init_display`, `framebuffer_addr`,
`kernel_range`, and `bss_range`.

## Testing strategy

Bare-metal code cannot be tested on the host when it executes privileged
instructions or touches MMIO. The split makes most logic host-testable:

- `libs/gfx`, `libs/sync`, and future pure libraries run `cargo test` on the
  host.
- Drivers are intentionally thin register accessors; they can be exercised in
  QEMU.
- The kernel is tested by booting `make run` (or `make docker-run`).

`make check` type-checks every crate for the AArch64 target.
