# feos

A tiny bare-metal **Rust** operating-system skeleton for the **aarch64** QEMU
`virt` machine, organised as a multi-crate workspace. It boots at EL1, installs
exception vectors, talks to the PL011 serial port, brings up a **ramfb**
framebuffer, draws a splash screen, starts a preemptive round-robin scheduler,
and opens an interactive **desktop session** where applications can be launched
with the keyboard. Logging off (or `Ctrl-\` on the serial console) drops into a
**Bash-compatible terminal** with a writable overlay filesystem, pipelines,
redirection, job-style background launches and a built-in POSIX coreutils
library.

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
   |                    apps  ->  libs/gfx, libs/gui, libs/console
   +------>  libs/hal
```

Nothing points back up the stack: an architecture crate never knows about a
board, a driver never knows about the kernel, and an app never owns hardware.
That is what makes it possible to add a second board, a second display driver,
or a second shell app without touching unrelated code. The kernel's scheduler
is the one piece of policy that lives above the layers: it consumes the
architecture IRQ entry and the `Processes` service defined in `libs/app`.

Apps also never mention a real architecture or board. The kernel hands them a
`feos_app::Platform` containing the board facts and the `Clock`, `Power`, `Cpu`
and `Audio` implementations from the HAL, so the same app set runs on every
port.

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
│   ├── qemu.sh                 # shared QEMU launch (ramfb + Intel HDA + display)
│   ├── pack-app.py             # pack a directory into a single-file .fapp bundle
│   └── docker-run.sh           # container entry point: build, then boot
├── docs/                       # screenshots and architecture notes
│
├── kernel/                     # feos-kernel: the bootable binary (bin: feos)
│   └── src/
│       ├── main.rs             # kmain, IRQ handler, panic handler, wiring
│       ├── allocator.rs        # 16 MiB static first-fit heap for H.264
│       └── sched.rs            # process table and round-robin scheduler
│
├── arch/
│   └── aarch64/                # feos-arch-aarch64
│       └── src/
│           ├── boot.S          # _start: CPU setup, BSS, jump to kmain
│           ├── vectors.S       # exception table + recoverable IRQ entry
│           ├── exceptions.rs   # vector install, FaultInfo, handler hook
│           ├── irq.rs          # TrapFrame, IRQ dispatch, mask helpers
│           ├── cpu.rs          # halt, current EL, MIDR
│           ├── psci.rs         # poweroff / reboot
│           └── timer.rs        # generic timer helpers
│
├── boards/
│   └── qemu-virt/              # feos-board-qemu-virt
│       ├── build.rs            # generates the embedded filesystem image
│       ├── fs/                 # files and directories compiled into the kernel
│       │   └── media/          # sample GIF, MJPEG, AVI, MP4, MKV, FLV clips
│       ├── linker.ld           # 0x40080000 load address, BSS, 256 KiB stack
│       └── src/lib.rs          # MMIO map, UART/framebuffer/audio/pointer wiring, symbols
│
├── drivers/
│   ├── serial/pl011/           # feos-driver-pl011
│   ├── firmware/fw-cfg/        # feos-driver-fw-cfg
│   ├── display/ramfb/          # feos-driver-ramfb
│   ├── bus/pci/                # feos-driver-pci: PCIe ECAM + BAR assignment
│   ├── audio/intel-hda/        # feos-driver-intel-hda: Intel HDA playback
│   ├── input/virtio/           # feos-driver-virtio-input: tablet/mouse/keyboard
│   └── interrupt/gicv2/        # feos-driver-gicv2: GICv2 distributor + CPU interface
│
├── libs/
│   ├── hal/                    # feos-hal: device + system-service traits
│   ├── sync/                   # feos-sync: SpinLock and friends
│   ├── console/                # feos-console: registered console + macros
│   ├── coreutils/              # feos-coreutils: POSIX utilities + overlay VFS
│   ├── sh/                     # feos-sh: Bash-compatible shell engine
│   ├── gfx/                    # feos-gfx: dependency-free 2D graphics
│   ├── gui/                    # feos-gui: immediate-mode widgets, layout, charts
│   ├── desktop/                # feos-desktop: YAML desktop, XP taskbar, WM
│   ├── init/                   # feos-init: service supervisor and restart policy
│   ├── bundle/                 # feos-bundle: single-file .fapp applications
│   ├── fs/                     # feos-fs: read-only filesystem abstraction
│   ├── keys/                   # feos-keys: serial + evdev keyboard decoding
│   ├── text/                   # feos-text: fixed-capacity UTF-8 strings
│   ├── audio/                  # feos-audio: PCM buffers, mixing, synthesis
│   ├── json/                   # feos-json: allocation-free JSON parser/writer
│   ├── yaml/                   # feos-yaml: practical-subset YAML parser/emitter
│   ├── image/                  # feos-image: PNG, JPEG, BMP, PPM and QOI codecs
│   ├── audio-codec/            # feos-audio-codec: WAVE, AIFF, AU, MP3, Ogg
│   ├── h264/                   # feos-h264: vendored H.264/AVC decoder
│   ├── video/                  # feos-video: containers, GIF, MJPEG, raw video
│   ├── compress/               # feos-compress: DEFLATE, zlib and gzip
│   ├── archive/                # feos-archive: ZIP, TAR and gzip archives
│   └── app/                    # feos-app: App trait, Context, Platform
│
└── apps/
    ├── desktop/                # feos-desktop-app: `desktop` session + app folder
    │   ├── desktop.yaml        # embedded desktop configuration
    │   ├── apps/               # .fapp bundles scanned by build.rs
    │   └── bundle-src/         # unpacked sources for `make bundles`
    ├── commands/               # feos-apps: built-in apps + registry
    │   └── src/
    │       ├── help.rs  echo.rs  info.rs  uptime.rs  sleep.rs
    │       ├── coreutils.rs    # `coreutils` command/help for the terminal
    │       ├── clear.rs  gfx.rs  splash.rs  tone.rs  power.rs
    │       ├── gui.rs          # feos-gui dashboard demo
    │       ├── files.rs        # file explorer app + ls/cat/tree/stat
    │       ├── files/
    │       │   └── explorer.rs # XP-style explorer UI, input and rendering
    │       ├── player.rs       # VLC-style video player, playlist and dialogs
    │       ├── taskmgr.rs      # XP-style task manager, graphs and process control
    │       ├── spawn.rs  ps.rs  kill.rs  tick.rs
    │       ├── ui.rs           # splash screen and test card renderer
    │       └── lib.rs          # REGISTRY and find()
    └── shell/                  # feos-shell: terminal adapter + app host
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
architecture crate. To put an app in the desktop Start menu instead, drop a
single-file `.fapp` bundle into `apps/desktop/apps/` (or a source folder in
`apps/desktop/bundle-src/` plus `make bundles`); the desktop discovers it
automatically.

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
- PCIe ECAM enumeration and BAR assignment, enough to bring up firmware-less
  PCI devices
- Intel HDA audio driver: CORB/RIRB command rings, codec discovery, and polled
  DMA playback through one output stream
- `feos-hal` device traits (`Serial`, `Audio`) and system-service traits
  (`Clock`, `Power`, `Cpu`) that keep apps board- and architecture-independent
- Registered console with `print!` / `println!` and panic-safe output
- AArch64 exception vector table with an ESR/ELR/FAR diagnostic dump, plus a
  recoverable IRQ entry that saves and restores a full 800-byte `TrapFrame`
- GICv2 driver behind an `InterruptController` HAL trait, with Group 0 and the
  timer PPI wired up by the board
- Preemptive round-robin scheduler: sixteen process slots, 16 KiB stacks, and a
  100 Hz generic-timer tick; the shell is PID 1
- `spawn`, `ps`, `kill`, and `tick` commands for running and inspecting
  background processes
- **Bash-compatible terminal** (`feos-sh`): quoting and escapes, parameter and
  command substitution, integer arithmetic, globbing, `&&`/`||`/`;`/`&`
  lists, multi-stage pipelines, `<`/`>`/`>>`/`2>`/`2>&1` redirection, variables,
  aliases, `if`/`elif`/`else`, `for`, `while`, `until`, `break`, `continue`,
  `source`, `eval`, `read`, `history`, and tab completion
- Interactive line editor: cursor movement, Home/End, history, Ctrl-A/E/U/K/W,
  Ctrl-L, Ctrl-C, Ctrl-D, reverse history search (Ctrl-R), multi-line
  continuation for open quotes and substitutions
- Terminal zoom: Ctrl-`=`/Ctrl-`+` and Ctrl-`-` scale the desktop Terminal
  window from 1x to 4x, with Ctrl-`0` resetting it
- **POSIX coreutils** (`feos-coreutils`): `ls`, `cat`, `echo`, `printf`, `pwd`,
  `mkdir`, `rmdir`, `rm`, `cp`, `mv`, `touch`, `head`, `tail`, `wc`, `grep`,
  `sort`, `uniq`, `cut`, `tr`, `seq`, `basename`, `dirname`, `nl`, `rev`,
  `tac`, `paste`, `tee`, `cmp`, `find`, `stat`, `tree`, `du`, `df`, `env`,
  `printenv`, `uname`, `whoami`, `id`, `hostname`, `nproc`, `arch`, `uptime`,
  `free`, `hexdump`/`xxd`, `base64`, `sha256sum`, `test`, `expr`, `true`,
  `false`, `yes`
- **Writable overlay VFS**: the compiled-in read-only image is extended with
  an in-RAM copy-on-write layer, so `mkdir`, `cp`, `mv`, `rm`, `touch` and
  shell redirection work without a block device; writes never touch the base
  image
- `feos-console::write_bytes` for raw binary terminal output (`cat` of a
  binary file, `hexdump`, and friends)
- QEMU `ramfb` framebuffer: 1024x768 linear **XRGB8888** in guest RAM
- `feos-gfx`: a `no_std` 2D library with pixels, rectangles, Bresenham lines,
  circles, gradients, blitting, and an 8x8 bitmap font
- `feos-gui`: a `no_std`, allocation-free immediate-mode GUI toolkit with
  widgets, a Windows/Fluent-inspired component library, linear layout,
  scrolling, text input, charts, windows and dialogs
- `feos-desktop`: a YAML-configured desktop session with XP-style taskbar and
  Start menu, wallpaper shortcuts, a window manager and theme support
- `feos-driver-virtio-input`: a virtio MMIO tablet/mouse/keyboard driver, wired
  through the `Pointer` and `Keyboard` HAL traits so the desktop tracks a real
  pointer and accepts typing from the QEMU display as well as the serial port
- `feos-fs`: a read-only, allocation-free filesystem with path lookup, parent
  links and a compiled-in `StaticFs` image
- `feos-keys`: serial-console and Linux-evdev keyboard decoding shared by the
  desktop and full-screen apps
- A **File Explorer** app with XP Luna window chrome, a menu bar, standard
  buttons, an editable address bar with history, a folder tree, recursive
  search, Details/Icons/List/Tiles views, sortable columns, multi-selection,
  a preview pane, text/binary viewer, properties, context menus, mouse and
  wheel input, and keyboard shortcuts, plus `files ls`, `files cat`,
  `files tree` and `files stat` shell forms
- A VLC-style **Media Player** with menus, transport controls, seek bar,
  playlist, fullscreen mode, aspect modes, speed/frame controls, media
  information, mouse context menus and a static kernel heap for H.264
  playback
- A Windows XP-style **Task Manager** (`taskmgr`) with Applications,
  Processes, Performance, Networking and Users tabs; live CPU/memory history
  graphs; sortable process columns; per-process CPU time, memory, threads and
  priority; Run, Select Columns, Properties, About, Help and confirmation
  dialogs; context menus; configurable update speed; and mouse/keyboard control
- Real scheduler and memory accounting: per-process CPU ticks, idle-tick
  detection through `WFI`, and a HAL `Memory` service backed by the kernel heap
  for commit charge, physical memory and kernel-memory panels
- Foreground app launching: an app can request the full framebuffer from the
  desktop and return to it with Esc
- `feos-bundle`: single-file `.fapp` applications (ZIP/TAR with manifest and
  icon), detected by content and scanned from an application folder
- `feos-init`: a fixed-capacity service supervisor with `never`/`on-failure`/
  `always` restart policies and `ps`-style status snapshots
- `feos-audio`: a `no_std` 16-bit PCM library with sample buffers, saturating
  mixing, fades, and sine/square/triangle/sawtooth/noise synthesis
- `feos-json`: a `no_std`, allocation-free RFC 8259 parser/writer with a
  caller-provided node arena
- `feos-yaml`: a practical-subset YAML parser/emitter (block and flow
  collections, quoted and block scalars, comments, document markers)
- `feos-image`: PNG, baseline JPEG, BMP, PPM and QOI decoders plus PNG/BMP/
  PPM/QOI encoders, all writing into caller-provided RGBA buffers
- `feos-audio-codec`: WAVE/AIFF/AU and MP3 decoding, PCM16 WAVE/AIFF
  encoding, and Ogg Opus header/page/TOC inspection
- `feos-h264`: a vendored, `no_std` H.264/AVC decoder (CAVLC/CABAC, P/B
  pictures, reference frames) with `alloc`-backed picture storage
- `feos-video`: GIF, AVI, MP4/MOV, Matroska/WebM, FLV, IVF, Annex-B H.264 and
  Motion JPEG demuxing, with RGBA decoding for GIF, MJPEG, raw/RLE video and
  H.264; other codecs are recognised and reported as unsupported
- `feos-compress`: allocation-free raw DEFLATE, zlib and multi-member gzip
  compression/decompression with CRC-32 and Adler-32
- `feos-archive`: ZIP, TAR (ustar/GNU/PAX) and gzip readers, extraction into
  caller buffers, and ZIP/TAR writers
- `tone` shell command: synthesizes a chord with `feos-audio` and plays it
  through the HDA codec
- Boot splash followed by the interactive desktop session, with an interactive
  graphics test card available from the shell
- Generic timer (`CNTPCT_EL0`) for uptime, busy-wait sleeps, and the scheduler
  tick (`CNTP_TVAL_EL0`)
- PSCI over `HVC` for clean `poweroff` / `reboot`
- Interactive serial shell with line editing (backspace, Ctrl-C)
- App registry with per-command crates/modules and aliases
- Host unit tests for the graphics, GUI, audio, synchronization, JSON, YAML,
  image, audio-codec, video, H.264, compression, and archive libraries
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

Then use the `gfx` command in the serial shell to redraw the test card,
`gui` to render the widget and chart dashboard, and `tone` to play an A-major
chord through the emulated Intel HDA codec. Use the `poweroff` command (or
`Ctrl-A` then `X`) to leave QEMU.

`make docker-run` wires the guest's HDA codec to a host backend. On Linux with
`/dev/snd`, the Makefile passes `--device /dev/snd` and QEMU uses ALSA, so
`tone` is audible on the default output. If there is no `/dev/snd`, QEMU
writes `target/audio/qemu.wav` at 48 kHz stereo instead; play that file on the
host to hear the same samples. See
[Hearing it](#hearing-it) for backend overrides.

### VNC instead of a window

For headless machines, SSH sessions, or a remote desktop, use:

```sh
make docker-run-vnc
```

This publishes QEMU's display over **VNC on port 5900**, using the same audio
backend rules as `make docker-run`. Connect from another terminal:

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
| `make run AUDIODEV=alsa,id=snd0`      | Live ALSA output (needs `/dev/snd`)       |
| `make run AUDIODEV=pa,id=snd0`        | Live PulseAudio/PipeWire output           |
| `make run AUDIODEV=wav,id=snd0,path=/tmp/qemu.wav` | Capture playback to WAV     |
| `make audio-demo`                    | Render the audio example WAV natively      |
| `make docker-audio`                  | Render the audio example WAV in Docker     |

The run targets add `-device virtio-tablet-device` for the pointer and
`-device virtio-keyboard-device` for typing in the QEMU window. Override
`POINTER_FLAGS` (for example with `-device virtio-mouse-device`) to use a
relative mouse, or set it to an empty value for a serial-only boot. Set
`KEYBOARD_FLAGS=` to disable the virtio keyboard and use only the serial
console.

The scripts pass `-net none` because the kernel does not use networking and
some distributions' QEMU packages omit the `efi-virtio.rom` file that the
default NIC would load.

## Shell commands

The serial console is a Bash-compatible terminal, not just a line dispatcher.
It understands quoting, variables, globs, pipelines, redirection, `&&`/`||`,
`if`/`for`/`while`, aliases and command substitution. The complete reference is
in [`docs/terminal.md`](docs/terminal.md).

```text
feos:/$ ls -l /docs
-rw-r--r-- 1 feos feos      569 Jan  1 00:00 getting-started.md
-rw-r--r-- 1 feos feos      546 Jan  1 00:00 filesystem.md

feos:/$ cat /docs/getting-started.md | grep -i build
feos:/$ mkdir -p /tmp/demo && cp /README.txt /tmp/demo/
feos:/$ for f in /tmp/demo/*; do echo "$f"; done
feos:/$ echo "the answer is $((6 * 7))" > /tmp/answer
feos:/$ cat /tmp/answer
the answer is 42
feos:/$ ps | grep shell
feos:/$ help grep
```

The utilities live in `feos-coreutils`; the language engine lives in
`feos-sh`. `apps/shell` only adapts them to the console and the kernel's app
registry.

| Command       | Description                                          |
|---------------|------------------------------------------------------|
| `help`        | List apps, shell builtins and coreutils               |
| `help <cmd>`  | Usage and description for one command                 |
| `echo <text>` | Print text back                                      |
| `printf FORMAT [args]` | Formatted output (`%s`, `%d`, `%x`, `%b`, width) |
| `ls [-la1hRFr] [path]` | List directory contents                    |
| `cat [-nbsA] [file]` | Concatenate files                            |
| `pwd`, `cd [dir]` | Print or change the working directory            |
| `mkdir`, `rmdir`, `rm`, `cp`, `mv`, `touch` | File management   |
| `head`, `tail`, `wc`, `nl`, `rev`, `tac`, `paste` | Text slicing   |
| `grep`, `sort`, `uniq`, `cut`, `tr`, `seq` | Text filtering          |
| `find`, `stat`, `tree`, `du`, `df` | Filesystem inspection            |
| `tee`, `cmp`, `sha256sum`, `base64`, `hexdump` | Data utilities     |
| `test`, `expr`, `true`, `false` | Conditionals and arithmetic      |
| `env`, `printenv`, `uname`, `whoami`, `id`, `hostname`, `nproc`, `arch`, `uptime`, `free` | System facts |
| `coreutils [cmd]` | List or describe the POSIX utilities             |
| `terminal`    | Open the Bash-compatible terminal window (`term`, `xterm`) |
| `sh [script]`, `bash` | Start a nested shell or run a script        |
| `info`        | Board, exception level, MIDR, timer, memory, display, audio |
| `gfx`         | Draw the graphics test card (`demo` is an alias)     |
| `gui`         | Draw the feos-gui dashboard and component gallery (`dashboard`) |
| `desktop`     | Run the YAML-configured desktop session (`wm`; `list`, `status`, `run <id>`, `service <action> <id>`) |
| `files`       | Open the file explorer, or use `ls`, `cat`, `tree` and `stat` (`explorer` alias) |
| `player`      | Open the VLC-style video player (`vlc`, `video`, `media`, `media-player` aliases) |
| `taskmgr`     | Open the XP-style Task Manager (`task-manager`, `taskmanager`, `tm` aliases) |
| `splash`      | Redraw the boot splash                               |
| `tone [ms]`   | Play an A-major chord for `ms` (default 1500, max 3000; `play` alias) |
| `uptime`      | Milliseconds since the counter started               |
| `sleep <ms>`  | Busy-wait for the given number of milliseconds       |
| `spawn <cmd> [args]` | Run a registered command as a background process |
| `ps`          | List processes and their state (`procs`, `processes`) |
| `kill <pid>`  | Stop a background process (the shell cannot be killed) |
| `tick [count] [ms]` | Print periodic ticks; useful with `spawn`       |
| `clear`       | Clear the terminal (ANSI escape)                     |
| `poweroff`    | PSCI SYSTEM_OFF; QEMU exits (`shutdown` alias)       |
| `reboot`      | PSCI SYSTEM_RESET (`reset` alias)                    |

## Processes and scheduling

Processes in feos are **kernel threads**, not isolated user processes: they
share the kernel address space, all run at EL1, and each has its own 16 KiB
stack and saved register frame. The process table has sixteen slots in `.bss`;
slot 0 is the shell, which is always PID 1 and is never reused.

The board initializes a GICv2 and enables the EL1 physical timer PPI (IRQ 30).
The timer fires at 100 Hz, the architecture's IRQ stub saves a full `TrapFrame`
on the interrupted stack, and `kernel/src/sched.rs` swaps that frame for the
next runnable process's frame. Returning from the exception therefore resumes a
different process. Scheduling is a plain round-robin; there are no priorities
and no blocking wait queues.

Try it from the shell:

```text
feos> spawn tick 8 200
started pid 2: tick
feos> spawn tick 8 200
started pid 3: tick
feos> ps
  PID  STATE    NAME
*   1  running  shell
    2  ready    tick
    3  ready    tick
feos> kill 2
killed pid 2
```

The ticks from PIDs 2 and 3 interleave with the prompt because both apps are
busy-waiting and the timer preempts them. `spawn` looks the command up in the
normal app registry, so any command can run in the background. Background apps
receive their own argument string and the `Platform` services (clock, power,
CPU, audio, processes), but no framebuffer: graphics apps such as `gfx` and
`splash` should stay in the foreground. A process exits when its `App::run`
returns; the scheduler frees the slot on the next timer tick so it can be
reused by a later `spawn`.

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
cargo test -p feos-gfx -p feos-gui -p feos-audio -p feos-sync \
    -p feos-json -p feos-yaml -p feos-image -p feos-audio-codec \
    -p feos-h264 -p feos-video -p feos-compress -p feos-archive \
    --target x86_64-unknown-linux-gnu
```

## The GUI library

`feos-gui` (in `libs/gui/`) builds a small desktop toolkit on top of
`feos-gfx`. It is `no_std`, has no allocator, no `unsafe`, and no dependencies
beyond `feos-gfx`. The model is **immediate mode**: an app rebuilds its
interface every frame, widgets return a `Response`, and state that must survive
between frames lives in caller-owned structs.

```rust
use feos_gfx::{Canvas, Rect};
use feos_gui::InteractionState;
use feos_gui::prelude::*;

let mut pixels = [0u32; 480 * 320];
let mut canvas = Canvas::new(&mut pixels, 480, 320, 480).unwrap();
let theme = Theme::dark();
let input = Input::new();
let mut state = InteractionState::<8>::new();
let mut volume = 0.6f32;
let mut enabled = true;

let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
ui.desktop_background(ui.canvas_rect());
let panel = Rect::new(24, 24, 280, 180);
ui.card(panel, "Settings", |ui| {
    ui.label_muted("APPEARANCE");
    ui.checkbox(&mut enabled, "Enable notifications");
    ui.slider(&mut volume, 0.0, 1.0);
    if ui.primary_button("Apply").clicked {
        // handle the click
    }
});
```

Key pieces:

- `Ui` — the frame handle. It owns a clipped `Painter`, the active theme,
  input, and interaction state. Widgets are inherent methods, so custom
  widgets can mix freely with the built-in ones.
- `InteractionState<N>` — fixed-capacity hover/active/focus/tab-order table
  and scroll offsets. It never grows; extra regions degrade gracefully.
- `LayoutStack` / `ui.column`, `ui.row`, `ui.grid` — deterministic linear
  layout. No constraint solver and no allocations.
- `Painter` — clipping, rounded rectangles, alpha blending, shadows, and
  text drawing/wrapping/ellipsis over `feos_gfx::Canvas`.
- `Input` / `Event` — pointer, wheel and keyboard events. The QEMU board feeds
  them from virtio tablet/keyboard devices and the serial console decoder.
- Widgets — labels, headings, paragraphs, buttons, checkboxes, radios, toggles,
  sliders, progress bars, badges, separators, text fields with selection and
  UTF-8 editing, panels, cards, scroll areas, tabs, windows, dialogs, lists,
  tooltips, and a desktop background.
- Components — search fields, selects, menus, segmented controls, numeric
  steppers, ratings, colour pickers, form fields, alerts, toasts, spinners,
  skeletons, empty states, loading overlays, progress rings, confirmation
  dialogs, toolbars, status bars, breadcrumbs, pagination, step indicators,
  tables, key/value lists, timelines, trees, stat cards, avatars, chips,
  accordions, split panes, drawers, popovers and a built-in icon set.
- Windows/Fluent-inspired components — `NavigationView`, `TabView`,
  `MenuBar`, `CommandBar`, `ListView`, `GridView`, `FlipView`, `PipsPager`,
  `InfoBar`, `InfoBadge`, `Persona`, `SettingsCard`, `HeroBanner`,
  `TeachingTip`, `Flyout`, context menus, `Calendar`/`DatePicker`/
  `TimePicker`, `AutoSuggestBox`, `ToggleButton`, `SplitButton`,
  `DropDownButton`, `HyperlinkButton` and radio groups. They follow WinUI
  behaviour and naming where practical while keeping the same caller-owned,
  allocation-free state model.
- Advanced inputs — password boxes, multiline text areas, two-thumb range
  sliders, rotary knobs, tag editors, hotkey recorders, cascaders, transfer
  lists, file drop zones and `@mention` fields.
- Patterns — avatar groups, presence badges, group boxes, property grids, log,
  code and diff viewers, comment/chat threads, result pages, selector bars,
  pivots, title bars, floating action buttons, back-to-top controls, bottom
  sheets, popconfirms, command palettes, swipe actions, pull-to-refresh,
  annotated scrollbars and zoom controls.
- Charts — line, area, bar and sparkline series over caller-provided slices,
  plus pie, donut, scatter, radar, heatmap, funnel, bullet, treemap and
  candlestick charts. All chart data is caller-owned and allocation-free.

The library now exposes well over 100 reusable components and widgets, and the
same list keeps growing without changing the layering rules: `Ui` methods,
caller-owned state and a clipped painter. The full component inventory lives in
[`libs/gui/src/components/README.md`](libs/gui/src/components/README.md).
- `WindowState` — draggable and resizable window geometry, also caller-owned.

Components are still inherent methods on `Ui`, and their state is caller-owned
the same way widget state is. There is no hidden widget tree or z-order stack:
popups, menus, drawers, popovers and toasts paint inline, so draw them late in
the frame when they need to float over other content.

```rust
let mut select = SelectState::new();
let mut theme_choice = 0;
let mut page = 0;
let mut tabs = TabViewState::from_titles(["Overview", "Details", "Activity"]);
let mut menu = MenuBarState::new();
let mut list = ListState::<8>::new();

ui.menu_bar(&mut menu, &[MenuBarMenu::new("File", &file_entries)]);
ui.list_view(list_rect, &items, &mut list);
ui.tab_view(&mut tabs, panel, |ui, _content| {
    ui.label("Content for the active tab");
});
ui.select(&mut select, &mut theme_choice, &["Dark", "Light"], "Theme");
ui.pagination(&mut page, 8);
```

Everything is fallible only at the edges: `Canvas::new` validates the buffer,
while widgets simply clip or skip work when there is no room.

```sh
make test-gui
# equivalent to:
cargo test -p feos-gui --target x86_64-unknown-linux-gnu
```

## The desktop environment

![The feos desktop with the XP-style Start menu](docs/desktop.png)

`feos-desktop` (in `libs/desktop/`) is the session layer above `feos-gui`:
wallpaper and shortcuts, an XP-style taskbar and Start menu, a fixed-capacity
window manager, and an init/service supervisor. It is `no_std`, allocation-free
and independent of the board. Full reference:
[`docs/desktop.md`](docs/desktop.md).

On boot the kernel draws the splash, waits briefly, and then starts the desktop
automatically. QEMU supplies a virtio tablet and, with the default run flags, a
virtio keyboard, so windows, the Start menu and terminal work with the pointer
and keyboard in the QEMU window. The serial console is still a keyboard and can
be used alongside it:

| Input | Action |
|-------|--------|
| Mouse | Move the pointer; click Start, menus, taskbar buttons and window controls |
| Keyboard | Type in the focused window (terminal, dialogs) or use arrows/Enter on the desktop |
| Double-click | Open a desktop shortcut, or maximise/restore a window from its title bar |
| Drag | Move a window by its title bar, or resize it from any edge or corner |
| Cursor | Arrow on the desktop, horizontal/vertical arrows on window edges, diagonal arrows on corners |
| Right-click | Open the desktop menu |
| Arrow keys | Move the Start-menu highlight, or the wallpaper selection when the menu is closed |
| `Enter` | Launch the highlighted application |
| `Space` | Open the Start menu from the desktop (or toggle All Programs while it is open) |
| `Ctrl+Esc` | Open the Start menu while an application has focus |
| `Tab` | Toggle All Programs, or focus the next window when the menu is closed |
| `Esc` | Close the Start menu |
| `Ctrl-\` | Leave the desktop and return to the serial shell (`Ctrl-D` also works) |

The Start menu starts closed; click **start** to open it. **Run...** accepts a
bundle id (`system-info`) or a shell command (`ps`), **Turn Off Computer**
shows the XP shutdown confirmation, and the tray clock opens Date and Time
Properties.

The same controls work in the shell with the `desktop` command:

```text
feos> desktop
[desktop] session started
[desktop] click Start, double-click desktop icons, drag windows; Ctrl-\ returns to the shell
[desktop] 7 application(s) discovered in the application folder (0 skipped)
feos> desktop list
  files                System         entry=files  Browse, search and preview files with an XP-style explorer
  terminal             System         entry=gui    GUI dashboard and component gallery
  system-info          System         entry=info   Board, CPU, memory and kernel details
  process-monitor      System         entry=taskmgr Applications, processes, graphs and process control
feos> desktop run files
[desktop] launching 'files' in the foreground
[files] explorer closed
[desktop] returned from 'files'
```

The session is configured entirely in YAML
([`apps/desktop/desktop.yaml`](apps/desktop/desktop.yaml)): theme, wallpaper
style and colours, taskbar edge/height/clock/quick-launch count, Start-menu
size and layout, desktop icons, default window sizes, application folders and
autostart services. Unknown keys and invalid values keep their defaults.

Applications are **single-file bundles** (`.fapp`): one ZIP or TAR archive with
`manifest.yaml`, an icon and any resources. The scanner detects them by archive
magic, not by suffix, so renaming a bundle does not hide it. On QEMU the
application folder is embedded at build time: `apps/desktop/build.rs` turns
every file in `apps/desktop/apps/` into an embedded `DirEntry`, so dropping a
bundle in that folder and rebuilding adds it to the Start menu. A
filesystem-backed board passes the same `&[DirEntry]` list to `Desktop::scan`
at run time.

```yaml
# manifest.yaml inside a .fapp
id: terminal
name: Terminal
entry: gui
category: System
description: GUI dashboard and component gallery
icon: icon.bmp
window:
  width: 760
  height: 520
```

`Desktop::launch` opens the window, resolves the manifest entry through the
`AppRegistry` published in `Platform::apps`, and spawns it through
`Processes`. `libs/init` supervises services with `never`, `on-failure` or
`always` restart policies and a bounded retry budget; the YAML `init.autostart`
list is registered when the session starts and ticked once per frame.

```sh
make test-desktop test-desktop-app
make bundles      # repack apps/desktop/bundle-src/* into apps/desktop/apps/*.fapp
```

## The Task Manager

![The feos Task Manager showing the Processes tab](docs/taskmgr.png)

`taskmgr` is a Windows XP-style task manager with five tabs and live data from
the kernel scheduler and heap:

- **Applications** lists running tasks with End Task, Switch To and New Task.
- **Processes** shows PID, CPU percentage, CPU time, memory, threads, start
  time, status and priority. Click a heading to sort; double-click for
  properties; right-click for the context menu; End Process or Delete ends the
  selected task.
- **Performance** draws CPU and memory history graphs on the classic black
  grid, alongside Totals, Commit Charge, Physical Memory and Kernel Memory
  panels.
- **Networking** presents the adapter view; on a board without a network driver
  it reports a disconnected adapter and a flat graph rather than pretending.
- **Users** shows the console session.

The File, Options, View and Help menus expose Run, Select Columns, update
speed (High/Normal/Low/Paused), status-bar and graph toggles, Help and About.
Dialogs use the Luna title bar and include Run, Select Columns, Process
Properties, About, Help, End Process and Log Off. The window can be dragged,
resized, maximised or closed with the mouse.

```text
feos> taskmgr
feos> spawn tick 100 200
feos> taskmgr
```

The `ps` command remains the serial-console view. The task manager is a
windowed app, so the desktop opens it from the Start menu or the desktop
context menu's **Task Manager** item; it can be moved, resized, minimized and
restored while the taskbar and other windows stay live.

CPU accounting is real: the scheduler records one tick per process and marks
ticks spent in the architecture's `WFI` idle wait separately, so an idle
desktop is quiet while a busy app actually raises the graph. Memory figures
come from a HAL `Memory` service backed by the kernel's static heap and the
loaded image ranges.

## The filesystem and File Explorer

![The feos file explorer showing the docs directory](docs/files.png)

`feos-fs` (in `libs/fs/`) is a small read-only filesystem. The QEMU board has
no block device driver yet, so its image is compiled into the kernel:
`boards/qemu-virt/build.rs` walks `boards/qemu-virt/fs/` and generates a flat
`StaticNode` table. Each node knows its name, kind, parent and children; file
data is borrowed from the image, so lookup and reads never allocate.

Drop a file into `boards/qemu-virt/fs/` and rebuild to add it to the explorer
and the shell. The initial image contains `README.txt`, `docs/`, `etc/`,
`examples/` and an empty directory.

Apps reach the filesystem through `Platform::filesystem`, so the same API works
for a future disk or RAM backend. The `files` app has a graphical mode and
text-mode forms:

```text
feos> files              # full-screen explorer (Esc returns)
feos> files ls /
feos> files cat /README.txt
feos> files tree /docs
feos> files stat /etc/motd
```

The explorer is a complete Windows XP-style browser rather than a directory
dump. It has the Luna title bar, File/Edit/View/Favorites/Tools/Help menus,
Back/Forward/Up/Search/Folders/Views standard buttons, an editable Address bar
with a history drop-down, a synchronised folder tree, File and Folder Tasks,
and a Details pane. The main view supports Details, Icons, List and Tiles
layouts, sortable columns, multi-selection and right-click context menus.
Folders can be searched recursively (with an optional subfolder toggle), and
text or binary files open in a scrollable viewer with a hex dump for binary
data. Properties, Folder Options and About are modal XP-style dialogs.

The filesystem is read-only, so file-changing commands are deliberately
disabled and explain that the image is compiled into the kernel. Everything
else works with the mouse, the virtio keyboard or the serial keyboard:

| Input | Action |
|-------|--------|
| Click / double-click | Select, open folders, or open a file viewer |
| Right-click | Context menu for an item or the current folder |
| Mouse wheel | Scroll the list, tree, preview or viewer |
| Title bar | Drag a restored window; the square button maximises/restores |
| Window edges | Drag any edge or corner to resize; the cursor changes to the matching XP resize arrow |
| Arrow keys | Move the list or tree selection |
| `Shift` + arrows | Extend the selection |
| `Ctrl` + arrow keys | Move the focus without changing the selection |
| `Enter` / `Right` | Open a folder or file |
| `Backspace` / `Left` | Go to the parent folder |
| `Alt+Left` / `Alt+Right` | Back / Forward |
| `F3` / `Ctrl+F` | Open the Search pane |
| `F5` / `Ctrl+R` | Refresh |
| `F10` | Open the File menu |
| `F1` | About File Explorer |
| `Ctrl+L` | Focus and edit the Address bar |
| `Ctrl+A` | Select all (Edit menu can invert the selection) |
| `Tab` / `Shift+Tab` | Move focus between list, tree and address bar |
| `Esc` | Close a menu/dialog, or return to the desktop |

Interactive apps implement `App::windowed`. The desktop opens a managed
window, then calls `App::frame_window` once per frame with a content canvas and
the desktop's input. The app keeps its state while the desktop stays in its
normal loop, so wallpaper icons remain visible beside the window and the
taskbar, Start menu and other windows keep working. Minimizing a window
preserves the app; restoring it from the taskbar resumes the same session.
File Explorer, Task Manager and Media Player all run this way.

## The video player

![The feos video player showing an H.264 MP4](docs/player.png)

`player` is a VLC-inspired windowed application built on `feos-video`,
`feos-fs` and the GUI toolkit. It scans `/media` recursively, opens the first
movie automatically, and keeps a session playlist with the other supported
files. The initial filesystem ships with tiny GIF, MJPEG, raw AVI, H.264 MP4,
MKV, FLV and Annex-B samples; VP9 files are listed as recognised but not
decodable so the media-information path can be inspected too.

The interface has VLC-style Media/Playback/Audio/Video/Subtitle/Tools/View/Help
menus, standard transport buttons, a seek bar with elapsed/duration labels,
playback speed, loop and shuffle modes, a playlist strip, fullscreen mode with
auto-hiding controls, and Fit/Fill/Stretch/Original aspect modes. Media
Information reports the container, codec, dimensions, frame count/rate,
duration, file size and decode support. A modal Open Media dialog lists every
video on the filesystem. Right-clicking the video or playlist opens a context
menu.

```text
feos> player                    # scan /media and start playing
feos> player /media/h264-sample.mp4
feos> player /media/animation.gif
```

| Input | Action |
|-------|--------|
| `Space` | Play or pause |
| `S` / `B` / `N` | Stop / previous / next |
| `Left`, `Right` | Seek 5 seconds (`Shift` seeks 1 second) |
| `Page Up`, `Page Down` | Seek 30 seconds |
| `Home`, `End` | Start / end |
| `,`, `.` | Previous / next frame while paused |
| `+`, `-`, `0` | Faster, slower, normal speed |
| `F` / `F11` | Toggle fullscreen |
| `P` | Toggle the playlist |
| `O` | Open the media browser |
| `I` | Media information |
| `L` / `R` | Cycle repeat mode / toggle shuffle |
| `A` | Cycle aspect mode |
| `Tab` | Switch between video and playlist focus |
| Mouse | Click video to play/pause, double-click for fullscreen |
| Wheel | Seek, or scroll the playlist |
| `Esc` | Close a dialog, leave fullscreen, or quit |

`feos-video` demuxes the container and decodes GIF, MJPEG, uncompressed/RLE AVI
and H.264 into caller-provided RGBA buffers. H.264 needs `alloc` for reference
pictures, so the kernel now has a small static first-fit heap
(`kernel/src/allocator.rs`). Everything else remains allocation-free outside
the player's file and frame buffers. The player has no audio track support yet
because the current video demuxers expose the video track only; the Audio menu
is present but explains that limitation.

## The audio library

`feos-audio` (in `libs/audio/`) is the audio counterpart of `feos-gfx`: a
standalone `no_std` crate with no dependencies. It works on caller-provided
interleaved 16-bit PCM buffers, never allocates, and uses only integer math.

```rust
use feos_audio::{AudioBuffer, AudioFormat, Oscillator, Sample, Waveform};

let format = AudioFormat::mono(48_000).unwrap();
let mut samples = [0i16; 48_000];
let mut buffer = AudioBuffer::new(&mut samples, format).unwrap();

// Render a one-second 440 Hz sine wave at half volume.
let mut tone = Oscillator::new(Waveform::Sine, 440, Sample::from_i16(16_000));
buffer.render_oscillator(&mut tone, Sample::MAX);

// Layer a quieter square wave on top.
let mut second = Oscillator::new(Waveform::Square, 660, Sample::from_i16(6_000));
buffer.mix_oscillator(&mut second, Sample::MAX);

// Add short fades and mutate the whole buffer.
buffer.fade_in(480);
buffer.fade_out(480);
buffer.apply_gain(Sample::from_i16(24_000));
```

API summary:

- `AudioBuffer::new(samples, format)` over `&mut [i16]`, validating whole frames
- `frames`, `channels`, `sample_rate`, `duration_ms`
- `clear`, `sample`, `set_sample`, `frame`, `frame_mut`
- `write_frame`, `mix_frame` with saturating addition
- `mix` another buffer of the same format
- `apply_gain`, `fade_in`, `fade_out`, `peak`, `is_silent`
- `render_oscillator`, `mix_oscillator`
- `Sample`: signed 16-bit PCM with `from_i16` / `from_i8` / `from_u8` /
  `from_f32`, `raw`, `to_f32`, `magnitude`, `gain`, and saturating `mix`
- `AudioFormat`: sample rate plus mono/stereo constructors
- `Oscillator` + `Waveform`: sine, square, triangle, sawtooth, and noise with
  changeable frequency, amplitude, and phase reset

## Data and media libraries

These libraries follow the same rules as `feos-gfx` where practical: `no_std`,
dependency-light, and caller-provided buffers. `feos-video` builds on
`feos-image` and `feos-h264`; `feos-h264` is the one media crate that uses
`alloc` and contains the vendored unsafe NEON kernels. The kernel's 16 MiB
static heap exists to serve it.

| Crate | Formats | Notes |
|-------|---------|-------|
| `feos-json` | JSON | RFC 8259 parse into a node arena, compact/pretty emit |
| `feos-yaml` | YAML subset | block/flow mappings and sequences, quoted and block scalars |
| `feos-image` | PNG, JPEG, BMP, PPM, QOI | decodes to RGBA8888; PNG/BMP/PPM/QOI encoders |
| `feos-audio-codec` | WAVE, AIFF, AU, MP3, Ogg | decodes to interleaved `i16`; WAVE/AIFF encoders |
| `feos-h264` | H.264/AVC | Baseline/Main/High 4:2:0 decode; uses `alloc` |
| `feos-video` | GIF, AVI, MP4/MOV, Matroska/WebM, FLV, IVF, Annex-B, MJPEG | decodes GIF, MJPEG, raw/RLE and H.264 to RGBA8888 |
| `feos-compress` | DEFLATE, zlib, gzip | inflate plus a small fixed-Huffman deflater; CRC-32/Adler-32 |
| `feos-archive` | ZIP, TAR, gzip | stored/deflate ZIP, ustar/GNU/PAX TAR, ZIP/TAR writers |

JSON and YAML borrow both the source text and a caller-owned node arena:

```rust
use feos_json::{JsonDocument, JsonNode};

let mut arena = [JsonNode::EMPTY; 64];
let doc = JsonDocument::parse(r#"{"name":"feos","cores":4}"#, &mut arena).unwrap();
let root = doc.root();
assert_eq!(doc.as_i64(doc.get(root, "cores").unwrap()), Some(4));
```

Images decode into an RGBA buffer, with optional scratch space for PNG's
inflated scanlines:

```rust
use feos_image::{decode, info, scratch_len};

let info = info(png_bytes)?;
let mut rgba = vec![0u8; info.rgba_len()?];
let mut scratch = vec![0u8; scratch_len(png_bytes)?];
decode(png_bytes, &mut rgba, &mut scratch)?;
```

Audio follows the same pattern with `i16` samples:

```rust
use feos_audio_codec::{decode, info};

let props = info(wav_bytes)?;
let mut pcm = vec![0i16; props.sample_count()?];
let decoded = decode(wav_bytes, &mut pcm, &mut [])?;
assert_eq!(decoded.info.sample_rate, 44_100);
```

Video decodes into the same RGBA canvas, with scratch space only for animated
GIF:

```rust
use feos_video::{decode_frame, info, scratch_len};

let props = info(video_bytes)?;
let mut rgba = vec![0u8; props.rgba_len()?];
let mut scratch = vec![0u8; scratch_len(video_bytes)?];
let frame = decode_frame(video_bytes, 0, &mut rgba, &mut scratch)?;
assert_eq!((frame.width, frame.height), (props.width, props.height));
```

Compression and archives use the same caller-buffer model:

```rust
use feos_archive::zip::ZipReader;

let mut gz = vec![0u8; feos_compress::deflate::bound(data.len()) + 64];
let len = feos_compress::gzip::compress(data, &mut gz)?;
let mut plain = vec![0u8; data.len()];
let written = feos_compress::gzip::decompress(&gz[..len], &mut plain)?;
assert_eq!(&plain[..written], data);

let mut zip = ZipReader::open(zip_bytes)?;
let mut out = vec![0u8; 4096];
while let Some(entry) = zip.next()? {
    let written = entry.extract(&mut out)?;
    let _ = (entry.name()?, &out[..written]);
}
```

Current scope limits:

- JPEG supports baseline and extended-sequential 8-bit files. Progressive
  JPEG, arithmetic coding and CMYK are rejected with `UnsupportedFeature`.
- The PNG encoder uses uncompressed zlib "stored" blocks, which keeps the
  encoder tiny but does not reduce file size.
- Ogg Opus container headers, pages and packet TOC bytes can be inspected, but
  Opus packet decoding to PCM is **not implemented yet** and returns
  `AudioError::UnsupportedFeature`.
- MP3 is decoded by a vendored, safe-Rust port of minimp3
  (`libs/audio-codec/src/mp3/`, MIT OR Apache-2.0).
- `feos-video` reconstructs pixels for GIF, MJPEG, uncompressed/RLE AVI and
  H.264. HEVC, VP8, VP9, AV1, MPEG-1/2/4, Theora, ProRes, VC-1, VP6, H.263 and
  Cinepak are recognised and their samples are locatable, but `decode_frame`
  returns `UnsupportedCodec` for them. Fragmented MP4 is not supported.
- H.264 limits follow `feos-h264`: 8-bit progressive 4:2:0 Baseline, Main and
  High profiles, with unsupported features reported as errors.
- `feos-archive` reads ZIP64, multi-disk and encrypted archives only far enough
  to reject them; extraction supports stored (`0`) and deflate (`8`) members.
  The TAR writer emits ustar headers, while the reader also handles GNU long
  names and PAX `path`/`linkpath`/`size` records.
- `feos-compress` writes one fixed-Huffman DEFLATE block with greedy LZ77
  matching. The output is standards-compliant but larger than zlib's optimal
  encoder. Decompression handles all legal dynamic-Huffman DEFLATE streams.
- The PNG decoder uses `feos-compress` for zlib, so compressed image support is
  shared with archives rather than duplicated.

Host tests:

```sh
make test-json test-yaml test-image test-audio-codec test-h264 test-video test-compress test-archive
```

## The audio driver

The kernel boots with no PCI firmware, so `feos-driver-pci` enumerates QEMU's
PCIe ECAM window, sizes BAR0 of the Intel HDA controller (`8086:2668`), assigns
it `0x1000_0000`, and enables memory decoding plus bus mastering. The board
does this in `init_audio()` and hands the resulting `&dyn Audio` to apps
through `Platform`.

`feos-driver-intel-hda` then:

- resets the controller and points CORB/RIRB at static command rings
- discovers the audio function group, output DAC and output pin at runtime
  (QEMU's `hda-output` reports DAC 2, pin 3)
- programs 48 kHz/16-bit stereo, unmutes the DAC and enables the output pin
- plays one block at a time through output stream descriptor 4, polling the
  buffer-completion bit instead of using interrupts

The rings, buffer descriptor list and PCM buffer are owned by the board's
static `AUDIO` object, so playback needs no allocator. The MMU is off, so DMA
addresses are the buffers' identity-mapped virtual addresses. A block may hold
up to 288,000 interleaved samples (three seconds); longer blocks are rejected
with `AudioError::TooLong`.

`tone` fills a static 48 kHz stereo buffer with a 440 + 554 + 659 Hz chord,
applies fades, and calls `Audio::play`. `info` prints whether the device is
ready.

> QEMU's `hda-duplex` codec also exposes a capture path and fails to create its
> ADC voice on some hosts. The launcher therefore uses `hda-output`, which is
> playback-only and matches the driver.

### Hearing it

`make docker-run` and `make docker-run-vnc` start QEMU with an Intel HDA codec
and a host audio backend. At the prompt:

```sh
make docker-build
make docker-run
# feos> tone 1500
```

The backend is chosen by `AUDIODEV`:

| `AUDIODEV` | Effect |
|------------|--------|
| `alsa,id=snd0` (default when `/dev/snd` exists) | `--device /dev/snd`; QEMU opens the default ALSA output |
| `pa,id=snd0` | mounts `PULSE_SOCKET` and sets `PULSE_SERVER` |
| `wav,id=snd0,path=target/audio/qemu.wav,...` (default otherwise) | writes a WAV under `target/audio` |

The WAV backend is pinned to `out.frequency=48000,out.channels=2,out.format=s16`
so the capture matches the codec and is not resampled. Listen to or inspect it
with:

```sh
file target/audio/qemu.wav
aplay target/audio/qemu.wav
```

If ALSA reports a busy device, stop the program holding the output or select a
different backend:

```sh
make docker-run AUDIODEV=pa,id=snd0
make docker-run AUDIODEV=wav,id=snd0,path=target/audio/qemu.wav,out.frequency=48000,out.channels=2,out.format=s16
```

The WAV path is rewritten to `/out/...` inside the container and bind-mounted
back to `target/audio`.

The library alone can also be rendered without QEMU:

```sh
make docker-audio             # render inside the container
aplay target/audio/feos-tone.wav
```

`make docker-audio` uses the same Docker image as `make docker-run`, runs the
`tone` example with `cargo run`, and streams the WAV back to
`target/audio/feos-tone.wav` without booting QEMU. It works with rootless
Podman as well, and the file is owned by your host user. Use `paplay`,
`ffplay -autoexit`, or `afplay` instead of `aplay` if that is what your host
provides.

The same demo works without Docker:

```sh
make audio-demo               # native render to target/audio/feos-tone.wav
aplay target/audio/feos-tone.wav

# or directly, choosing your own path:
cargo run -p feos-audio --example tone --target x86_64-unknown-linux-gnu -- /tmp/tone.wav
```

Change the output path with `make audio-demo AUDIO_OUT=/tmp/tone.wav`. The
example renders a three-second 48 kHz/16-bit stereo A-major chord and prints
its peak level, so a non-zero peak confirms the DSP path ran.

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
   the canvas. After a short pause the kernel starts the desktop session, which
   redraws the same canvas; logging off returns to the shell, where `gfx` can
   redraw the test card.
6. The board finds the Intel HDA controller through the PCIe ECAM window,
   assigns BAR0, enables bus mastering, and initialises the driver: reset,
   CORB/RIRB setup, codec discovery, 48 kHz stereo format. The desktop and
   shell receive the resulting `&dyn Audio` in `Platform`.
7. `kmain` installs the IRQ handler, asks the board to initialize the GICv2 and
   enable timer PPI 30, creates the shell as PID 1, and starts the 100 Hz tick.
   Unmasking IRQs is the point where the desktop and shell become preemptible.
8. Faults land in the vector table, which forwards a `FaultInfo` to the kernel's
   reporter: `ESR_EL1`, `ELR_EL1`, `FAR_EL1`, `SPSR_EL1`, and the stack
   pointer, then halts. IRQs take the separate recoverable path described above.
9. `poweroff` issues PSCI `SYSTEM_OFF` over `HVC`; QEMU exits.

## Notes and limitations

- The MMU and caches are still off; the framebuffer is accessed through the
  identity mapping.
- Resolution is fixed at 1024x768 and the buffer lives in the board's `.bss`
  (3 MiB).
- The desktop composes each frame in an off-screen `BACK` buffer and copies it
  to the scanout in one pass. Foreground apps use their own off-screen canvas,
  sized to the desktop work area so the taskbar stays visible, and present it
  with one copy as well, so pointer movement does not expose a half-drawn
  frame. There is no hardware acceleration, and the font is ASCII-only
  (non-ASCII falls back to `?`).
- Single core. All processes share one address space and run at EL1, so there
  is no memory protection or privilege boundary between them; a fault in any
  process halts the machine.
- Eight process slots including the shell, 16 KiB per spawned stack, and no
  heap. `spawn` copies at most 96 bytes of arguments into the process table.
- Round-robin has no priorities, blocking sleep, or wait/join. `sleep` and
  `tick` busy-wait and rely on preemption, and a process cannot wait for
  another to exit.
- Background processes get no framebuffer, and apps share crate-level statics:
  avoid running the same stateful app (for example `tone`, which owns a static
  PCM buffer) in two processes at once.
- Audio is playback-only and interrupt-free: one HDA output stream, fixed
  48 kHz/16-bit stereo, no hardware volume or jack detection, and a three-second
  maximum block while the driver waits for completion. The scheduler can
  preempt the polling loop, but the HDA stream keeps running and playback
  resumes when the `tone` process is next scheduled. QEMU's `hda-output`
  codec is required (`hda-duplex` has a capture path that can fail to start).
- Debian's `qemu-system-arm` prints
  `Could not open option rom 'vgabios-ramfb.bin'` when `ramfb` starts. That ROM
  is x86 firmware and is unused on aarch64; the warning is harmless.

Natural extensions: identity-map the kernel with caches enabled, add an EL0
user mode with per-process page tables and a syscall boundary, add blocking
sleep/wait queues and priorities to the scheduler, support GICv3/ITS, add
a `virtio-gpu` driver for larger modes, add
interrupt-driven HDA playback with a mixer and hardware volume control, or
parse the device tree. Each of these fits into an existing layer: `libs/` for
media and scheduler policy, `kernel/` for the heap, `drivers/` for GICv3,
virtio-gpu and audio, and `apps/` for an animation or audio app.

## License

MIT OR Apache-2.0.

The 8x8 bitmap font data in `libs/gfx/src/font.rs` is the public-domain
`font8x8_basic` table by Daniel Hepper, based on Marcel Sondaar's public-domain
IBM VGA font.
