# feos

A tiny bare-metal **Rust** operating-system skeleton for the **aarch64** QEMU
`virt` machine, organised as a multi-crate workspace. It boots at EL1, installs
exception vectors, talks to the PL011 serial port, brings up a **ramfb**
framebuffer, draws a splash screen, and drops into a small interactive shell.

Everything can be built and run inside Docker, so the only host requirement is
Docker.

![feos boot splash](docs/splash.png)

![feos graphics test card](docs/demo.png)

## Why the workspace is split up

The project is designed to grow along four independent axes:

- **boards** — every machine gets a board support package in `boards/<name>`
- **drivers** — every device gets its own crate in `drivers/<class>/<name>`
- **libraries** — reusable, hardware-independent code lives in `libs/<name>`
- **apps** — every shell command is an app in `apps/`

Dependencies point in one direction only:

```
kernel  ->  boards  ->  drivers  ->  libs/hal
   |           |           |
   |           +---------> libs (gfx, console, ...)
   |
   +------>  arch (aarch64)  ->  libs/hal
   +------>  apps  ->  libs/app  ->  libs/hal
                       apps  ->  libs/gfx, libs/console
```

Nothing points back up the stack: an architecture crate never knows about a
board, a driver never knows about the kernel, and an app never owns hardware.
That is what makes it possible to add a second board, a second display driver,
or a second shell app without touching unrelated code.

Apps also never mention a real architecture or board. The kernel hands them a
`feos_app::Platform` containing the board facts and the `Clock`, `Power` and
`Cpu` implementations from the HAL, so the same app set runs on every port.

`members` in the root `Cargo.toml` uses globs, so a new crate directory is
picked up by Cargo automatically. The only edits needed to wire it in are the
dependency edges (usually one line in a `Cargo.toml` and one line in a board or
app registry).

## Repository layout

```
.
├── Cargo.toml                  # virtual workspace manifest (no kernel code)
├── rust-toolchain.toml         # stable + aarch64-unknown-none target
├── .cargo/config.toml          # default target, rust-lld flags, cargo runner
├── Dockerfile                  # rust:1-slim-bookworm + qemu-system-arm
├── Makefile                    # build, run, test, Docker targets
├── scripts/
│   ├── qemu.sh                 # shared QEMU launch command (ramfb + display)
│   └── docker-run.sh           # container entry point: build, then boot
├── docs/                       # screenshots and architecture notes
│
├── kernel/                     # feos-kernel: the bootable binary (bin: feos)
│   └── src/main.rs             # kmain, panic handler, wiring only
│
├── arch/
│   └── aarch64/                # feos-arch-aarch64
│       └── src/
│           ├── boot.S          # _start: CPU setup, BSS, jump to kmain
│           ├── vectors.S       # 16-entry EL1 exception vector table
│           ├── exceptions.rs   # vector install, FaultInfo, handler hook
│           ├── cpu.rs          # halt, current EL, MIDR
│           ├── psci.rs         # poweroff / reboot
│           └── timer.rs        # generic timer helpers
│
├── boards/
│   └── qemu-virt/              # feos-board-qemu-virt
│       ├── linker.ld           # 0x40080000 load address, BSS, 64 KiB stack
│       └── src/lib.rs          # MMIO map, UART/framebuffer wiring, symbols
│
├── drivers/
│   ├── serial/pl011/           # feos-driver-pl011
│   ├── firmware/fw-cfg/        # feos-driver-fw-cfg
│   └── display/ramfb/          # feos-driver-ramfb
│
├── libs/
│   ├── hal/                    # feos-hal: device + system-service traits
│   ├── sync/                   # feos-sync: SpinLock and friends
│   ├── console/                # feos-console: registered console + macros
│   ├── gfx/                    # feos-gfx: dependency-free 2D graphics
│   └── app/                    # feos-app: App trait, Context, Platform
│
└── apps/
    ├── commands/               # feos-apps: built-in apps + registry
    │   └── src/
    │       ├── help.rs  echo.rs  info.rs  uptime.rs  sleep.rs
    │       ├── clear.rs  gfx.rs  splash.rs  power.rs
    │       ├── ui.rs           # splash screen and test card renderer
    │       └── lib.rs          # REGISTRY and find()
    └── shell/                  # feos-shell: line editor + dispatcher
        └── src/lib.rs
```

## Adding things

**A driver.** Create `drivers/<class>/<name>/` with a `Cargo.toml`, implement
the relevant trait from `feos-hal` (or add a new trait there), and let the board
instantiate it. Drivers take their MMIO base address as a constructor
argument; they never hard-code board addresses.

**A library.** Create `libs/<name>/`, keep it `no_std` and dependency-light,
and add it to `[workspace.dependencies]` if several crates need it.

**An app.** Add a module under `apps/commands/src/` whose unit struct
implements `feos_app::App`, then add it to `REGISTRY` in
`apps/commands/src/lib.rs`. Apps get `args`, the optional `Canvas`, and the
`Platform` services through `Context`; they do not depend on a board or
architecture crate.

**A board.** Copy `boards/qemu-virt/`, adjust the constants and devices, point
the kernel at it in `kernel/Cargo.toml`, and select the linker script from
`.cargo/config.toml` (or a per-board build flag). The kernel source does not
change.

**A new architecture.** Add `arch/<name>/` with the same public surface as
`arch/aarch64`, plus a board that uses it.

## Features

- `#![no_std]`, `#![no_main]` kernel with zero external dependencies
- Layered Cargo workspace: kernel, arch, board, drivers, libraries, apps
- Boot assembly: stack setup, BSS zeroing, FP/SIMD enable, secondary CPU parking
- PL011 UART driver behind a `Serial` HAL trait
- `feos-hal` device trait (`Serial`) and system-service traits (`Clock`,
  `Power`, `Cpu`) that keep apps board- and architecture-independent
- Registered console with `print!` / `println!` and panic-safe output
- AArch64 exception vector table with an ESR/ELR/FAR diagnostic dump
- QEMU `ramfb` framebuffer: 1024x768 linear **XRGB8888** in guest RAM
- `feos-gfx`: a `no_std` 2D library with pixels, rectangles, Bresenham lines,
  circles, gradients, blitting, and an 8x8 bitmap font
- Boot splash and an interactive graphics test card
- Generic timer (`CNTPCT_EL0`) for uptime and busy-wait sleeps
- PSCI over `HVC` for clean `poweroff` / `reboot`
- Interactive serial shell with line editing (backspace, Ctrl-C)
- App registry with per-command crates/modules and aliases
- Host unit tests for the graphics and synchronization libraries
- Docker image containing Rust, QEMU with GTK (window) and VNC display support

## Quick start (Docker)

```sh
make docker-build
make docker-run
```

`make docker-run` opens a **real QEMU window** on your desktop while the serial
shell stays in your terminal. It grants QEMU temporary access to your X11 /
XWayland display (`xhost +si:localuser:<you>`), binds it in with
`-v /tmp/.X11-unix`, and revokes the access again when QEMU exits.

Requirements for the window: an X11 or Wayland (XWayland) session and a
`qemu-system-gui` build inside the image (already installed by the
`Dockerfile`). On Wayland the window appears through XWayland.

Then use the `gfx` command in the serial shell to redraw the test card. Use the
`poweroff` command (or `Ctrl-A` then `X`) to leave QEMU.

### VNC instead of a window

For headless machines, SSH sessions, or a remote desktop, use:

```sh
make docker-run-vnc
```

This publishes QEMU's display over **VNC on port 5900**; connect from another
terminal:

```sh
vncviewer localhost:5900
# or GNOME Connections / Remmina / TigerVNC: vnc://localhost:5900
```

Or run the image directly with an explicit backend:

```sh
docker build -t feos .
docker run --rm -it -p 5900:5900 -e FEOS_DISPLAY=vnc=0.0.0.0:0 feos
```

`FEOS_DISPLAY` selects the QEMU backend: `gtk` (window through the host X
server), `vnc=...` (VNC server), or `none` (serial only). The GTK backend needs
the X socket mount and `DISPLAY`; the Makefile handles both for you.

`make docker-shell` opens a shell with the source mounted and a named volume
for `target/`, so build artifacts are not written as root on the host:

```sh
make docker-shell
# inside:
cargo build --release
./scripts/qemu.sh
```

## Native build and run

Requirements: Rust (rustup) and `qemu-system-aarch64`.

```sh
rustup target add aarch64-unknown-none   # also declared in rust-toolchain.toml
cargo build --release
make run                                 # VNC on localhost:5900
```

`cargo build` builds the kernel because the workspace sets
`default-members = ["kernel"]`. Use `cargo build --workspace` to build every
crate.

On Debian/Ubuntu, QEMU comes from `qemu-system-arm`:

```sh
sudo apt install qemu-system-arm
```

Useful overrides:

| Command                              | Result                                    |
|--------------------------------------|-------------------------------------------|
| `make run`                           | VNC display on `127.0.0.1:5900`           |
| `FEOS_DISPLAY=gtk make run`          | Native GTK window (if QEMU has GTK)       |
| `FEOS_DISPLAY=none make run`         | Headless: serial console only             |
| `make run QEMU_DISPLAY=vnc=0.0.0.0:0`| VNC reachable from other machines         |

The scripts pass `-net none` because the kernel does not use networking and
some distributions' QEMU packages omit the `efi-virtio.rom` file that the
default NIC would load.

## Shell commands

| Command       | Description                                          |
|---------------|------------------------------------------------------|
| `help`        | List registered apps and their aliases                |
| `echo <text>` | Print text back                                      |
| `info`        | Board, exception level, MIDR, timer, memory, display |
| `gfx`         | Draw the graphics test card (`demo` is an alias)     |
| `splash`      | Redraw the boot splash                               |
| `uptime`      | Milliseconds since the counter started               |
| `sleep <ms>`  | Busy-wait for the given number of milliseconds       |
| `clear`       | Clear the terminal (ANSI escape)                     |
| `poweroff`    | PSCI SYSTEM_OFF; QEMU exits (`shutdown` alias)       |
| `reboot`      | PSCI SYSTEM_RESET (`reset` alias)                    |

## The graphics library

`feos-gfx` (in `libs/gfx/`) is a standalone `no_std` crate with no
dependencies. It draws into any `&mut [u32]` pixel buffer in XRGB8888 format:

```rust
use feos_gfx::{Canvas, Color, Rect};

let mut pixels = [0u32; 64 * 32];
let mut canvas = Canvas::new(&mut pixels, 64, 32, 64).unwrap();

canvas.clear(Color::rgb(8, 11, 24));
canvas.fill_rect(Rect::new(4, 4, 20, 10), Color::RED);
canvas.fill_circle(48, 16, 10, Color::YELLOW);
canvas.line(0, 31, 63, 0, Color::CYAN);
canvas.fill_vertical_gradient(
    Rect::new(30, 18, 30, 12),
    Color::NAVY,
    Color::PURPLE,
);
canvas.draw_text(4, 22, "hi feos", Color::WHITE, 1);
```

API summary:

- `Canvas::new(pixels, width, height, stride)` with clipping on every draw
- `clear`, `put_pixel`, `pixel`
- `fill_rect`, `stroke_rect`, `hline`, `vline`, `line`
- `circle`, `fill_circle`
- `fill_vertical_gradient`
- `draw_char`, `draw_text`, `text_width`, `text_height` (8x8 bitmap font)
- `blit` for packed XRGB8888 images
- `Color` with `rgb`, `from_u32`, channel accessors, constants, and `lerp`
- `Point`, `Size`, `Rect` geometry helpers

The library is host-testable:

```sh
make test
# equivalent to:
cargo test -p feos-gfx -p feos-sync --target x86_64-unknown-linux-gnu
```

## How the boot works

1. QEMU loads the ELF at `0x4008_0000` (from
   `boards/qemu-virt/linker.ld`) and starts at `_start` in EL1 with the MMU,
   caches, and interrupts off. `x0` holds the device tree pointer (the ELF
   loader passes 0 here, so the DTB is not used).
2. `arch/aarch64/src/boot.S` keeps only the boot CPU running, sets `sp` to
   `__stack_top`, enables FP/SIMD via `CPACR_EL1.FPEN`, zeroes `.bss`, and
   calls `kmain(dtb)`.
3. `kmain` asks the board to initialize the console (PL011 plus the console
   writer/reader registration), installs `VBAR_EL1`, and prints the banner.
4. The board looks up the writable fw_cfg item `etc/ramfb`, fills in a 28-byte
   big-endian `RAMFBCfg` (address, `XR24` fourcc, width, height, stride), and
   submits it through fw_cfg's DMA interface. The 3 MiB scanout buffer is a
   page-aligned `static` in the board crate's `.bss`.
5. `ramfb` scans out of guest RAM directly, so drawing is simply writing
   pixels; there is no flush or virtqueue. `feos-gfx` renders the splash into
   the canvas, then the shell can redraw the test card with `gfx`.
6. Faults land in the vector table, which forwards a `FaultInfo` to the kernel's
   reporter: `ESR_EL1`, `ELR_EL1`, `FAR_EL1`, `SPSR_EL1`, and the stack
   pointer, then halts.
7. `poweroff` issues PSCI `SYSTEM_OFF` over `HVC`; QEMU exits.

## Notes and limitations

- The MMU and caches are still off; the framebuffer is accessed through the
  identity mapping.
- Resolution is fixed at 1024x768 and the buffer lives in the board's `.bss`
  (3 MiB).
- No double buffering, alpha blending, or hardware acceleration; the font is
  ASCII-only (non-ASCII falls back to `?`).
- No heap, no GIC/timer interrupts, single core. The vector table is ready for
  interrupts when a GIC driver is added.
- Debian's `qemu-system-arm` prints
  `Could not open option rom 'vgabios-ramfb.bin'` when `ramfb` starts. That ROM
  is x86 firmware and is unused on aarch64; the warning is harmless.

Natural extensions: identity-map the kernel with caches enabled, add a GICv3 +
`CNTP` timer tick and an animation loop, implement a bump allocator, add a
`virtio-gpu` driver for larger modes, or parse the device tree. Each of these
fits into an existing layer: `libs/` for the allocator, `drivers/` for the GIC
and virtio-gpu, and `apps/` for an animation app.

## License

MIT OR Apache-2.0.

The 8x8 bitmap font data in `libs/gfx/src/font.rs` is the public-domain
`font8x8_basic` table by Daniel Hepper, based on Marcel Sondaar's public-domain
IBM VGA font.
