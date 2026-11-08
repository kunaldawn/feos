# FEOS-DOS vs rust-dos: feature gap

`rust-dos` is the reference for what a complete DOS box can do. FEOS-DOS is
built for a `no_std` kernel on AArch64 and x86_64, so some features cannot be copied
directly; this table records what exists, what is partial, and what is still a
project of its own.

Status: **yes** = usable, **partial** = a useful subset works, **no** = not
implemented.

## CPU

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| 8086/80186 real mode | yes | as a 386: `REP` resumes, interrupts go through the IVT, `BOUND` and `INS`/`OUTS` included |
| x87 FPU | yes | 80-bit arithmetic, rounding, exception flags and software transcendentals; unmasked exceptions only set flags |
| 286 protected mode | yes | 16-bit segments, gates and TSSs |
| 386/486/Pentium protected mode, paging, v86 | partial | 386 protected mode, rings, paging, task switches and v86, some 486 instructions. No Pentium |
| DOS/4GW, DOS/4G, DPMI host | yes | DOS/4GW, DOS/4G and `DPMI32VM` on the hardware emulation, and a built-in DPMI 0.9 host for the rest. See the game table in `dosbox.md` |
| MMX, SSE, 3DNow!, dynamic recompiler | no | beyond the 386/486 target; it's an interpreter |
| CPU speed control, deterministic mode | yes | `CYCLES n`; guest time follows retired instruction units |

## DOS and BIOS

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Built-in DOS, `INT 21h` | yes | MCB arena, PSP and JFT over a shared SFT, `EXEC` including overlays, TSRs, FCBs. No networking or disk-level calls |
| `COMMAND.COM` prompt, batch files | partial | built-ins, `IF`/`GOTO`/`FOR`/`CALL`/`SHIFT`/`SET`/`PATH`, `<`/`>`/`>>`, `ERRORLEVEL`. No pipes, and no guest `COMMAND.COM` apart from `/C` |
| XMS, EMS, upper memory, `LOADHIGH` | partial | XMS 3.0, EMS 4.0, VCPI under `EMM386 ON` and the HMA. No UMBs or `LOADHIGH` |
| DPMI host | partial | 0.9 and `AX=0504h`, at level 3 with paging off, with `INT 21h` translated for the common calls. `DPMI ON` fails Warcraft II's sound and `32RTM`, so `AUTO` passes over extenders that bring their own |
| Keyboard layouts | partial | US scan codes; no `KEYB` layouts |
| Mouse driver (`INT 33h`) | yes | virtual-screen coordinates, hide counter, `AX=0Ch` handlers through `IRQ 12`. The cursor's drawn by the host, without shape changes |
| `INT 10h` text/CGA/EGA/VGA | yes | all standard modes, text services in graphics modes, palette and font calls |
| VESA VBE 2.0 / banked modes | partial | `100h` and `101h` banked, logical width, display start, palette and the protected-mode interface. No linear frame buffer or other depths |
| PnP BIOS, APM, PCI, IDE | no | no hardware bus emulation |
| Boot disk images (`BOOT`) | no | no disk-image boot path |
| Printer, serial ports, modem | no | `INT 17h`/`INT 14h` report no device |
| CD-ROM / ISO / CUE-BIN / CHD | no | not implemented |

## Video

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Text, CGA, EGA, VGA mode 13h | yes | register-level core with latches, write modes and panning. Text is 8 dots wide |
| Mode X / 320x240 planar | yes | chain-4 off and CRTC-derived sizes |
| VESA VBE 2.0 up to 1024x768 | partial | 640x480x256 at most |
| Hercules, Tandy, PCjr | no | no |
| Tseng ET4000, S3 Trio/ViRGE, ATI Rage | no | no accelerator emulation |
| 3dfx Voodoo/Voodoo2/Voodoo3, PowerVR, Vérité | no | no 3D accelerator emulation |
| CRT shaders, monochrome monitors | no | the desktop scales the text screen |
| Screenshot | yes | `SCREEN` writes `C:\SCREEN.BMP` |

## Sound

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| PC speaker | yes | PIT channel 2 square wave, bit-banged port `61h`, BEL |
| AdLib / OPL2 / OPL3 | yes | Nuked-OPL3, timers on guest time so detection works, timestamped writes |
| Sound Blaster 16/Pro/2.0 DSP, DMA | yes | An SB16 by default, or a Pro or 2.0 with `SBTYPE`: stereo, 16-bit DMA on channel 5, the mixers. Recording is silent |
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
| FAT12/FAT16/FAT32 images | no | DOS sees only the RAM drive; `INT 13h` reports no disk |
| ISO, CUE/BIN, CHD | no | no CD-ROM |
| Disk images, VHD, 86F | no | no floppy/IDE controller |
| Disk speed/noise emulation | no | no |

## Display, capture and UI

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| Settings window | no | desktop app has no settings page |
| Game profiles / GOG import | no | no |
| Save states | partial | `SAVESTATE`/`LOADSTATE` at the prompt: CPU, FPU, first megabyte, video, mouse; validated on load. A running program isn't resumed, and extended memory and files aren't saved |
| Rewind | no | no |
| Screenshots | yes | `SCREEN` |
| WAV/AVI/GIF recording | no | no |
| Cheats, achievements, RetroAchievements | no | no |
| VR / 3D scene | no | no |
| WebAssembly / libretro frontends | no | this is a kernel app |

## Networking and peripherals

| rust-dos feature | FEOS-DOS | Notes |
|---|---|---|
| IPX LAN, relay, room browser | no | the guest has no network, though FEOS has one |
| NE2000 behind a router | no | no NIC emulation |
| Serial mouse / null modem / modem | no | no UART emulation |
| Joysticks | partial | game port emulated, with no host joystick to feed it |
| Printer to PDF/PNG | no | no printer |

## Recommended order for “complete”

The real-mode hardware, the DOS process model, 386 protected mode, DPMI, VCPI and the Sound
Blaster family are done, in that order. All 13 library titles run. What remains, most useful
first:

1. **Run a real `EMM386.EXE`** or similar on the CPU's virtual-8086 mode, which only has CPU
   tests so far, and find why `DPMI ON` differs for Warcraft II and `32RTM`.
2. **A joystick for the game port**: the port is emulated, but the front end has nothing to
   feed it.
3. **Paging under the DPMI host**, for clients that want `AX=0507h` and page protection.
4. **VESA linear frame buffers and more modes.**
5. **CD-ROM/ISO**, then **NE2000/IPX and serial**.
6. **Settings window, profiles, recording and cheats.**
