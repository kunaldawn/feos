# feos

A tiny bare-metal **Rust** kernel skeleton for the **aarch64** QEMU `virt`
machine, with a **ramfb framebuffer** and a reusable `no_std` **2D graphics
library**. It boots at EL1, installs exception vectors, talks to the PL011
serial port, draws a splash screen, and drops into a small interactive shell.
Everything can be built and run inside Docker, so the only host requirement is
Docker.

![feos boot splash](docs/splash.png)

![feos graphics test card](docs/demo.png)

## Features

- `#![no_std]`, `#![no_main]` kernel with zero external dependencies
- Boot assembly: stack setup, BSS zeroing, FP/SIMD enable, secondary CPU parking
- PL011 UART driver (polled) with `print!` / `println!`
- AArch64 exception vector table with an ESR/ELR/FAR diagnostic dump
- QEMU `ramfb` framebuffer: 1024x768 linear **XRGB8888** in guest RAM
- `feos-gfx`: a `no_std` 2D library with pixels, rectangles, Bresenham lines,
  circles, gradients, blitting, and an 8x8 bitmap font
- Boot splash and an interactive graphics test card
- Generic timer (`CNTPCT_EL0`) for uptime and busy-wait sleeps
- PSCI over `HVC` for clean `poweroff` / `reboot`
- Interactive serial shell with line editing (backspace, Ctrl-C)
- Host unit tests for the graphics library
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
| `help`        | List commands                                        |
| `echo <text>` | Print text back                                      |
| `info`        | Exception level, MIDR, timer, memory map, display    |
| `gfx`         | Draw the graphics test card                          |
| `splash`      | Redraw the boot splash                               |
| `uptime`      | Milliseconds since the counter started               |
| `sleep <ms>`  | Busy-wait for the given number of milliseconds       |
| `clear`       | Clear the terminal (ANSI escape)                     |
| `poweroff`    | PSCI SYSTEM_OFF; QEMU exits                          |
| `reboot`      | PSCI SYSTEM_RESET                                    |

## The graphics library

`feos-gfx` (in `gfx/`) is a standalone `no_std` crate with no dependencies. It
draws into any `&mut [u32]` pixel buffer in XRGB8888 format:

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
cargo test -p feos-gfx --target x86_64-unknown-linux-gnu
```

## Layout

```
.
├── Cargo.toml              # kernel package + workspace (member: gfx)
├── rust-toolchain.toml     # stable + aarch64-unknown-none target
├── .cargo/config.toml      # default target, rust-lld flags, cargo runner
├── linker.ld               # 0x40080000 load address, BSS, 64 KiB stack
├── Dockerfile              # rust:1-slim-bookworm + qemu-system-arm
├── Makefile
├── scripts/
│   ├── qemu.sh             # shared QEMU launch command (ramfb + display)
│   └── docker-run.sh       # container entry point: build, then boot
├── docs/                   # screenshots
├── gfx/                    # feos-gfx, the reusable 2D graphics library
│   └── src/
│       ├── lib.rs
│       ├── canvas.rs       # shapes, text, blitting, clipping
│       ├── color.rs        # XRGB8888 colours
│       ├── geometry.rs     # Point, Size, Rect
│       └── font.rs         # public-domain 8x8 bitmap font
└── src/
    ├── main.rs             # kmain, panic handler, halt
    ├── macros.rs           # print! / println!
    ├── console.rs          # locked console over PL011
    ├── shell.rs            # serial shell
    ├── graphics/
    │   ├── mod.rs          # init() -> Canvas
    │   ├── ramfb.rs        # ramfb config + scanout buffer
    │   └── demo.rs         # splash screen and test card
    ├── drivers/
    │   ├── pl011.rs        # ARM PL011 UART driver
    │   └── fw_cfg.rs       # QEMU fw_cfg MMIO/DMA driver
    └── arch/
        ├── boot.S          # _start: CPU setup, BSS, jump to Rust
        ├── vectors.S       # 16-entry EL1 exception vector table
        ├── exceptions.rs   # vector install + fault diagnostics
        ├── psci.rs         # poweroff / reboot
        └── timer.rs        # generic timer helpers
```

## How the boot works

1. QEMU loads the ELF at `0x4008_0000` and starts at `_start` in EL1 with the
   MMU, caches, and interrupts off. `x0` holds the device tree pointer (the
   ELF loader passes 0 here, so the DTB is not used).
2. `src/arch/boot.S` keeps only the boot CPU running, sets `sp` to
   `__stack_top`, enables FP/SIMD via `CPACR_EL1.FPEN`, zeroes `.bss`, and
   calls `kmain(dtb)`.
3. `kmain` initializes the UART and `VBAR_EL1`, prints the banner, and brings
   up graphics.
4. The framebuffer driver looks up the writable fw_cfg item `etc/ramfb`,
   fills in a 28-byte big-endian `RAMFBCfg` (address, `XR24` fourcc, width,
   height, stride), and submits it through fw_cfg's DMA interface. The 3 MiB
   scanout buffer is a page-aligned `static` in `.bss`.
5. `ramfb` scans out of guest RAM directly, so drawing is simply writing
   pixels; there is no flush or virtqueue. `feos-gfx` renders the splash into
   the canvas, then the shell can redraw the test card with `gfx`.
6. Faults land in the vector table, which reports `ESR_EL1`, `ELR_EL1`,
   `FAR_EL1`, `SPSR_EL1`, and the stack pointer, then halts.
7. `poweroff` issues PSCI `SYSTEM_OFF` over `HVC`; QEMU exits.

## Notes and limitations

- The MMU and caches are still off; the framebuffer is accessed through the
  identity mapping.
- Resolution is fixed at 1024x768 and the buffer lives in `.bss` (3 MiB).
- No double buffering, alpha blending, or hardware acceleration; the font is
  ASCII-only (non-ASCII falls back to `?`).
- No heap, no GIC/timer interrupts, single core. The vector table is ready for
  interrupts when a GIC driver is added.
- Debian's `qemu-system-arm` prints
  `Could not open option rom 'vgabios-ramfb.bin'` when `ramfb` starts. That ROM
  is x86 firmware and is unused on aarch64; the warning is harmless.

Natural extensions: identity-map the kernel with caches enabled, add a GICv3 +
`CNTP` timer tick and an animation loop, implement a bump allocator, add a
`virtio-gpu` driver for larger modes, or parse the device tree.

## License

MIT OR Apache-2.0.

The 8x8 bitmap font data in `gfx/src/font.rs` is the public-domain
`font8x8_basic` table by Daniel Hepper, based on Marcel Sondaar's public-domain
IBM VGA font.
