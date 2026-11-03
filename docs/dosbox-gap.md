# FEOS-DOS vs rust-dos: feature gap

`rust-dos` is the reference for what a complete DOS box can do. FEOS-DOS is
built for a `no_std` AArch64 kernel, so some features cannot be copied
directly; this table records what exists, what is partial, and what is still a
project of its own.

Status: **yes** = usable, **partial** = a useful subset works, **no** = not
implemented.

## CPU

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| 8086/80186 real mode | yes | full integer instruction set, segmentation, `REP`, interrupts |
| x87 FPU | partial | arithmetic/load/store/compare/BCD/control; transcendentals are no-ops |
| 286 protected mode | no | needs descriptor tables, 16-bit protected mode |
| 386/486/Pentium protected mode, paging, v86 | no | the main blocker for DOS extenders and Windows |
| DOS/4GW, DOS/4G, DPMI host | no | depends on 386 protected mode |
| MMX, SSE, 3DNow!, dynamic recompiler | no | beyond the real-mode DOS target |
| CPU speed control, deterministic mode | no | no cycle accounting beyond an instruction budget |

## DOS and BIOS

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Built-in DOS, `INT 21h` | partial | console, handles, DTA, find, files, dirs, memory, PSP |
| `COMMAND.COM` prompt, batch files | yes | DIR/CD/TYPE/COPY/DEL/REN/MD/RD/ECHO/PROMPT/RUN and batch |
| XMS, EMS, upper memory, `LOADHIGH` | no | `INT 67h` reports no EMS |
| DPMI host | no | needs protected mode |
| Keyboard layouts | partial | US scan codes; no `KEYB` layouts |
| Mouse driver (`INT 33h`) | yes | position, buttons, counters, ranges, handler fields |
| `INT 10h` text/CGA/EGA/VGA | partial | text, CGA 4/5/6, EGA/VGA planar, VGA mode 13h |
| VESA VBE 2.0 / banked modes | no | VBE calls report unsupported |
| PnP BIOS, APM, PCI, IDE | no | no hardware bus emulation |
| Boot disk images (`BOOT`) | no | no disk-image boot path |
| Printer, serial ports, modem | no | `INT 17h`/`INT 14h` report no device |
| CD-ROM / ISO / CUE-BIN / CHD | no | not implemented |

## Video

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Text, CGA, EGA, VGA mode 13h | yes | renderer and tests |
| Mode X / 320x240 planar | no | planar engine exists, mode timing is not |
| VESA VBE 2.0 up to 1024x768 | no | next graphics milestone |
| Hercules, Tandy, PCjr | no | no |
| Tseng ET4000, S3 Trio/ViRGE, ATI Rage | no | no accelerator emulation |
| 3dfx Voodoo/Voodoo2/Voodoo3, PowerVR, Vérité | no | no 3D accelerator emulation |
| CRT shaders, monochrome monitors | no | the desktop scales the text screen |
| Screenshot | yes | `SCREEN` writes `C:\SCREEN.BMP` |

## Sound

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| PC speaker | partial | PIT channel 2/gate tracked, BEL beep; no continuous music |
| AdLib / OPL2 / OPL3 | yes | Nuked-OPL3 ported to `no_std`, ports `388h`-`38Bh`, 48 kHz stereo mix |
| Sound Blaster 16/Pro/2.0 DSP, DMA | no | OPL FM works, DSP/DMA does not |
| Gravis Ultrasound | no | no |
| Roland MT-32, SC-55, General MIDI | no | no |
| Covox, Disney Sound Source, Tandy sound | no | no |
| CD audio | no | no CD-ROM |

## Drives and files

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Writable RAM drive `C:` | yes | seeded with built-ins |
| Host folder as a drive | partial | `MOUNT` imports a FEOS filesystem directory |
| ZIP game mounting | yes | `MOUNT` extracts a per-game ZIP with the FEOS ZIP reader |
| FAT12/FAT16/FAT32 images | partial | FEOS can mount FAT32/exFAT read-only; no DOS INT 13h disk |
| ISO, CUE/BIN, CHD | no | no CD-ROM |
| Disk images, VHD, 86F | no | no floppy/IDE controller |
| Disk speed/noise emulation | no | no |

## Display, capture and UI

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Settings window | no | desktop app has no settings page |
| Game profiles / GOG import | no | no |
| Save states | partial | `SAVESTATE`/`LOADSTATE` save CPU/FPU/memory/video/mouse; files stay |
| Rewind | no | no |
| Screenshots | yes | `SCREEN` |
| WAV/AVI/GIF recording | no | no |
| Cheats, achievements, RetroAchievements | no | no |
| VR / 3D scene | no | no |
| WebAssembly / libretro frontends | no | this is a kernel app |

## Networking and peripherals

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| IPX LAN, relay, room browser | no | no network stack |
| NE2000 behind a router | no | no NIC |
| Serial mouse / null modem / modem | no | no UART emulation |
| Joysticks | no | no gameport |
| Printer to PDF/PNG | no | no printer |

## What was added in this pass

- Per-game ZIPs in `data/games/dos`; the exFAT image is 64 MiB with 14 files.
- `MOUNT` extracts a ZIP (`doom.zip`, `wolfenstein-3d.zip`, ...) into `C:`.
- `INT 33h` mouse driver wired to the desktop pointer.
- OPL2/OPL3 FM synthesis (Nuked-OPL3, `no_std`) and stereo audio mixing.
- PC speaker PIT/gate state and BEL beep.
- x87 FPU common instruction subset.
- Save states via `SAVESTATE`/`LOADSTATE` to `C:\STATE.BIN`.
- Tests for ZIP mounting, mouse, OPL3, FPU and save states.

## Recommended order for “complete”

1. **386 protected mode + DPMI + DOS/4GW** — unlocks Doom, Duke Nukem 3D,
   Quake, Warcraft II, SimCity 2000, SSF2T and Snood.
2. **Sound Blaster DSP/DMA** — digital sound effects and SB music.
3. **VESA VBE 2.0 + Mode X** — higher-resolution and planar games.
4. **CD-ROM/ISO** — CD audio and disc-based games.
5. **NE2000/IPX and serial** — multiplayer and modem/serial games.
6. **Settings window, profiles, recording, cheats** — the front-end features.

The first item is by far the largest; it is a separate emulator core, not a
small port. Everything below it is incremental on top of the current machine.
