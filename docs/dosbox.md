# FEOS-DOS

![The FEOS-DOS prompt](dosbox.png)

`feos-dosbox` is FEOS's own DOS box: an original, `no_std` x86 virtual machine
and DOS prompt. It is not a port of DOSBox or of `rust-dos`; the CPU, VGA text
adapter, BIOS and DOS kernel are written for this workspace so they fit the
kernel's `no_std` + `alloc` constraints and the project's MIT/Apache licence.

## Running it

From the desktop, open **FEOS-DOS** in the Start menu. From the serial shell:

```text
dosbox
```

The desktop opens a 720x500 window; the 640x400 text screen is centred and
scaled by an integer factor. `EXIT` at the prompt (or closing the window) returns
to the desktop. `Ctrl-\` leaves the full-screen serial session.

The prompt starts on `C:`. The FEOS filesystem is copied into `C:\FEOS`, and
`C:` is a RAM drive: files you create there last until the DOS window closes.
`README.TXT`, `AUTOEXEC.BAT`, `HELLO.COM` and `BOX.COM` are created at boot.
`HELLO.COM` and `BOX.COM` are hand-assembled 8086 images, so `DEMO` proves the
CPU, DOS and VGA paths end to end.

## What is emulated

| Part | Details |
|------|---------|
| CPU | 8086/80186 integer instruction set, segmentation, flags, `REP` strings, interrupts, `PUSHA`/`ENTER`/immediate shifts. Reports itself as an 8086. |
| FPU | x87 register stack, control/status words, loads/stores, FADD/FSUB/FMUL/FDIV and POP forms, comparisons, FCOMI/FUCOMI, FILD/FIST/FISTP, FLD/FST/FSTP, BCD, FLDCW/FNSTCW, FNSAVE/FRSTOR, FCHS/FABS/FTST/FSQRT/FRNDINT/FSCALE. Transcendental instructions are accepted as no-ops. |
| Memory | 1 MiB conventional RAM, IVT at `0000:0000`, BIOS data area at `0040:0000`, colour text buffer at `B800:0000`. |
| Video | VGA mode 3 (80x25 text), mode 7 mono, 40-column modes; **VGA mode 13h** (320x200, 256 colours); **CGA modes 4/5/6**; **EGA/VGA planar modes 0Dh/0Eh/10h/12h** with map-mask, read-map, set/reset, bit-mask and logical-op registers; DAC palette through 3C8h/3C9h; CRTC cursor and start-address registers; CP437 glyphs including box drawing and blocks. |
| Keyboard | 16-entry type-ahead queue, US scan codes, `INT 16h` read/peek/shift-state/push-back. |
| Timer | 18.2 Hz BIOS tick at `0040:006C`, `INT 1Ch` delivery when `IF` is set. |
| DOS | `INT 21h` console, handles, DTA, `FIND FIRST/NEXT`, file create/open/read/write/seek/close/delete/rename, directories, date/time, memory allocation, PSP, vectors. |
| Drives | One writable RAM drive, `C:`. `MOUNT <dir>` imports a directory from the board's FEOS filesystem on demand, so a large game image does not have to fit in the kernel heap. |
| Sound | The PC speaker's PIT channel 2 divisor (`42h`/`43h`) and gate (`61h`) are tracked; BEL plays a 50 ms square-wave beep. **OPL2/OPL3 FM synthesis** is ported from Nuked-OPL3 through `388h`/`389h` and `38Ah`/`38Bh`, and mixed as 48 kHz stereo into `feos_hal::Audio`. Sound Blaster DSP/DMA, GUS, MT-32/SC-55 and MIDI are not implemented. |
| Programs | `.COM` and MZ `.EXE` loading, PSP, command tail, relocation fixups, exit codes. |

The machine intentionally answers "not present" to the hardware it does not
emulate: `INT 13h` disks, `INT 14h` serial, `INT 17h` printer, `INT 33h` mouse
and `INT 67h` EMS. That is better than hanging or returning garbage.

## Commands for the game library

The repository ships a 64 MiB exFAT image with one ZIP per shareware, demo or
freeware DOS game at `data/games/dos.img`. Keeping one ZIP per title avoids
thousands of loose files in `data/games/dos`. It was built with the FEOS exFAT
writer:

```text
truncate -s 64M data/games/dos.img
mkfs.exfat -n FEOSDOS -c 32K data/games/dos.img
cargo run -p feos-fs --example seed_games --target x86_64-unknown-linux-gnu -- \
    data/games/dos.img data/games/dos
fsck.exfat -n data/games/dos.img
```

To expose it to the QEMU board, link it as the board's optional disk image and
rebuild:

```text
make run-games       # links the image, builds, and boots QEMU with 512 MiB
```

The board mounts it read-only at `/`. The image is linked into the kernel, so
`run-games` raises QEMU's RAM to 512 MiB; a plain `make run` keeps the default
128 MiB and is only for builds without the game image. In FEOS-DOS:

```text
GAMES                list the image's GAMES.TXT (or its top-level entries)
BEEP                 play a PC speaker beep
MOUNT WOLF3D         extract wolfenstein-3d.zip into C:\WOLF3D
CD WOLF3D
WOLF3D.EXE
SCREEN               write the current screen to C:\SCREEN.BMP
SAVESTATE            save C:\STATE.BIN
LOADSTATE            restore C:\STATE.BIN
```

`MOUNT` extracts a ZIP on demand because the kernel's heap is much smaller than
the game image; extract one title at a time. The image contains:

| ZIP | Short alias | Title | Licence | Needs |
|-----|-------------|-------|---------|-------|
| `doom.zip` | `DOOM` | DOOM | Shareware | DOS/4GW protected mode |
| `duke-nukem-3d.zip` | `DUKE3D` | Duke Nukem 3D | Shareware | DOS/4GW protected mode |
| `wolfenstein-3d.zip` | `WOLF3D` | Wolfenstein 3D | Shareware | 16-bit, VGA mode 13h |
| `quake.zip` | `QUAKE` | Quake | Shareware | DJGPP/go32 extender |
| `super-street-fighter-ii-turbo.zip` | `SSF2T` | Super Street Fighter II Turbo | Demo | 32RTM extender |
| `tank-wars.zip` | `TANK` | Tank Wars | Shareware | 16-bit, VGA mode 13h |
| `warcraft-2-tides-of-darkness.zip` | `WAR2` | Warcraft II | Demo | DOS/4GW protected mode |
| `champ-kong.zip` | `CKONG` | Champ Kong | Shareware | 16-bit |
| `galacta-the-battle-for-saturn.zip` | `GALACTA` | Galacta | Shareware | 16-bit, CGA/EGA |
| `mario-bros-vga.zip` | `MARIO` | Mario Bros VGA | Shareware | 16-bit, VGA |
| `simcity-2000.zip` | `SC2000` | SimCity 2000 | Demo | DOS/4G protected mode |
| `snood.zip` | `SNOOD` | Snood | Shareware | CWSDPMI protected mode |
| `digger-remastered.zip` | `DIGGER` | Digger Remastered | Freeware | 16-bit, CGA |

**Compatibility status:** the image mounts, `GAMES` lists it, `MOUNT` imports a
title and `DIR` shows its files. **None of the 13 titles has been validated as
playable end to end yet.** The graphics hardware and CPU paths now exist, and
the LZEXE decompressor runs after fixing two real CPU bugs (memory operands
were resolving a segment *register index* instead of its value, and `LOOP`
tested `CX` before decrementing). Wolfenstein 3D now decompresses and reaches
its self-relocation stub, but that stub still exits before the game starts; the
loader/relocation work is the next concrete task. The DOS/4GW, DOS/4G, DJGPP
and CWSDPMI titles are blocked on protected mode/DOS extenders and cannot run
until that milestone lands. Treat the table above as the target list, not a
claim that every title is playable today.

## Feature gap against rust-dos

[`docs/dosbox-gap.md`](dosbox-gap.md) maps every major rust-dos feature to its
FEOS-DOS status and names the next concrete step. The short version: the
real-mode machine is useful, but 386 protected mode/DOS extenders, SB
DSP/DMA, VESA/Mode X, CD-ROM and networking are still separate projects.

## Scope and next milestones

The first release is a **real-mode DOS machine with text, CGA, EGA and VGA
mode 13h graphics**. It does not yet implement:

- Mode X and banked VESA modes (mode 13h and planar EGA/VGA are present);
- the transcendental x87 instructions (F2XM1, FYL2X, FSIN/FCOS, FPTAN/FPATAN);
- 386 protected mode, paging, virtual-8086 mode or DOS extenders (DOS/4GW);
- Sound Blaster DSP/DMA, GUS, MT-32/SC-55 and MIDI (OPL2/OPL3 FM and the PC speaker BEL beep are implemented);
- networking (IPX/NE2000), serial mice or CD-ROM;
- video/GIF recording (`SCREEN` writes a BMP screenshot and `SAVESTATE` /
  `LOADSTATE` save the CPU, FPU, memory, video and mouse state to
  `C:\STATE.BIN`; recording is not implemented).

The crate is deliberately layered so those can be added without touching the
shell:

```text
libs/dosbox/
  cpu.rs       8086 interpreter and Bus trait
  memory.rs    1 MiB address space
  vga.rs       text, CGA, EGA planar and VGA mode 13h adapter/renderer
  font.rs      CP437 glyphs
  keyboard.rs  BIOS keyboard queue and scan-code table
  bios.rs      INT 10h/11h/12h/13h/14h/15h/16h/17h/1Ah/33h/67h
  dos.rs       INT 21h kernel, handles, DTA, allocator
  ramdisk.rs   writable C: tree and FEOS import
  shell.rs     COMMAND.COM-style prompt and batch files
  machine.rs   bus, ports, program loader, frame loop
apps/dosbox/   desktop and serial-shell front end
```

A graphics adapter would implement the same `Bus` port/memory surface and add
a second renderer; an OPL/SB device would answer the mixer ports and push PCM
to `feos_hal::Audio`. The CPU already exposes `Bus::in8`/`out8`, so neither
requires changing the interpreter.

## Licensing

`feos-dosbox` is original code, licensed with the rest of FEOS under
`MIT OR Apache-2.0`. `rust-dos` is GPL-2.0-or-later, so its source cannot be
copied into this workspace without changing FEOS's licence; this crate borrows
only public DOS/PC interface knowledge, not its implementation.
