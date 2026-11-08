# vNext todo

Generated from `vnext.md`. `[x]` means fixed and verified, `[~]` means partly done (see note), `[!]` means blocked or won't fix (see note).

## Decisions

- Licensing: relicense the DOS crate and the linked kernel binary as GPL-2.0-or-later
- Game data: keep tracked; each title's own licence still needs review
- Scope: everything, including Later
- Commits: none; leave in the working tree
- Names: neutral (FEOS Media Player, Task Manager, File Explorer)
- Foreground path: keep, fix routing

## Milestones


### M0: Legal and safety net

- [x] Decide the licensing direction and fix it (DOS-6). Add root licence files.
- [~] Review the shipped game data and third-party branding (DOS-7, MED-30). (partial: MED-30 done; DOS-7's per-title licences recorded from their bundled docs, the review is the owner's)
- [x] Pin the toolchain, Docker image and `rust-version` (BLD-5).
- [x] Add CI running `check-arm`, `check-x64`, `test`, `feos-apps` tests and both release builds.
- [x] Add a scripted QEMU boot test on both architectures. Needs KRN-9 so x86 can exit.
- [x] Turn the reproductions from this audit into regression tests, marked `#[ignore]` until fixed.
- [x] Fix `boards/*/build.rs` so a no-op build doesn't relink (DRV-8).

### M1: Stop input from crashing the system

- [x] Shell and coreutils: SH-1, SH-2, SH-3, SH-4 (width caps; a shell-level byte budget), SH-5 (depth limits).
- [x] YAML and filesystem: FS-1, FS-2, FS-3, FS-6.
- [x] Media: MED-1, MED-2, MED-3, MED-4 to MED-8, MED-14.
- [x] FEOS-DOS: DOS-2, DOS-3, DOS-4, DOS-5, DOS-9, DOS-10.
- [~] Add panic-focused clippy lints to parser crates (`string_slice`, `indexing_slicing`, `arithmetic_side_effects`). (partial: the 16 crates that read untrusted input (image, h264, video, source, audio-codec, opus, elf, fs, archive, compress, yaml, json, firmware, bundle, net, bootinfo) have `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable` and `string_slice` in their `[lints.clippy]`, and `make clippy` passes with them as errors. What they flagged was fixed or allowed on the one item with a comment saying why it can't happen. `indexing_slicing` and `arithmetic_side_effects` are on `feos-elf` and `feos-firmware` only, which are clean of both. Not done: those two on the other crates, which have about 20,000 sites between them (5,000 in h264, 7,500 in video); `make fuzz` runs their fuzz tests under overflow checks instead)
- [x] Add host fuzz targets with a 64 MiB failing allocator and a small-stack thread. See [Testability](#testability). (notes: `libs/sh/tests/fuzz.rs` runs syntax and per-command fuzzing under the parsers' `LimitAllocator` on `run_constrained`'s thread. The allocator can also refuse every request above a size on one thread, `with_request_limit`, so the shell's `try_reserve` budgets take their failure paths without aborting the test: at 256 KiB, `cat`, `sort`, `tr`, `grep`, `$(...)` and `seq` over 2 MiB fail the command, and the shell carries on)
- [x] Add `overflow-checks = true` to a fuzzing profile, so wraps that release builds hide become failures. (notes: `[profile.fuzz]` inherits release, with overflow checks and debug assertions on and no LTO. `make fuzz` runs every crate's `fuzz` test and h264's `malformed` under it, on a 128 KiB kernel-sized stack (`FEOS_FUZZ_STACK`), and CI runs it. It passes, at 20 times the inputs too)

### M2: Kernel foundations

- [x] Per-CPU IRQ stacks (KRN-1).
- [~] Safe kill: pending-kill flag, a lock-depth counter, and a result enum (KRN-2, KRN-3, KRN-7). (partial: flag, lock count and enum done; KRN-7's killed-task heap is reported, not reclaimed)
- [x] Stack canaries checked on each tick, a high-water mark in `ps`, and stacks moved out of `Task` (KRN-4).
- [x] A decided panic policy, with an IPI to stop other CPUs or containment per task (KRN-6). IPIs need DRV-18 fixed.
- [x] Blocking primitives: a blocked state, sleep queue, `yield_now`, a mutex (KRN-11).
- [x] Process API: exit status, `wait`, `App::run -> i32`, argv (SH-25, FS-12).
- [x] Remove `nomem` from IRQ and DMA `asm!` (KRN-8, DRV-12).
- [x] Move the allocator and scheduler policy into host-testable crates.

### M3: A responsive desktop

- [x] Streaming audio: a cyclic ring, non-blocking `Audio::write`, a mixer (DRV-1, DRV-2, DRV-3).
- [x] Run the terminal's shell on its own task, with streamed output and Ctrl-C (SH-6, SH-7).
- [x] Run the DOS VM on its own task, with a watchdog (DOS-8).
- [x] Run the video decoder on a worker with a stateful API (MED-9, MED-10).
- [x] Fix window and keyboard routing (UI-1, UI-7, UI-9, UI-14).
- [x] Capture app output into windows, so Run and Start-menu items show something (UI-5).
- [x] Add occlusion-aware hit testing (UI-8).
- [x] Performance: `opt-level = 3` for hot crates, a cached wallpaper, and then damage tracking.

### M4: Persistence

- [x] Fix the PCI header offset (DRV-6) and add a `virtio-core` with a PCI transport, so x86 gains virtio.
- [x] `virtio-blk`, MBR/GPT parsing and a buffer cache.
- [x] A shared, locked, writable VFS across the serial shell, Terminal and Explorer. Fix FS-4, FS-5, FS-13 and FS-14 first.
- [x] A real-time clock (PL031 and CMOS), and then file timestamps.
- [x] Settings that survive a reboot, and Explorer write operations. (`FEOS_BOOT_TEST_ONLY=settings`, both ports: Display Properties and the Settings app each changed through the UI, QEMU killed, restored)

### M5: Make the headline features real

- [x] **Shell:** positional parameters, then functions, then an fd table with redirection on compound commands, here-docs, `case`, and then a VT emulator and an editor.
- [x] **Media:** a mixer and `play` command, so existing audio codecs become usable; then key-frame seeking, larger frames, and A/V sync.
- [x] **FEOS-DOS:** IVT dispatch (DOS-1) first, then a CPU test suite, then the PIC and PIT, keyboard hardware and a real DOS process model. These come before protected mode.

### Later

- [x] AArch64 MMU and caches. Move this into vNext if real hardware or KVM becomes a goal, because KRN-10 may block booting there.
- [x] x86_64 SMP.
- [x] EL0, a syscall boundary and per-process address spaces. (user programs in `/bin`, boot-tested on both ports; the apps and shell stay kernel-linked at EL1/ring 0 by design, so they aren't behind it. See "The boundary ends" in docs/architecture.md)
- [x] Networking.
- [x] 386 protected mode and DPMI for DOS. Built-in DPMI 0.9 host, virtual-8086 mode and VCPI under `EMM386 ON`, each with tests and with real titles. (notes: `DPMI ON` still fails Warcraft II's sound and `32RTM`, so the default passes over extenders that bring their own; a guest `EMM386.EXE` hasn't been run)
- [~] USB, UEFI and real hardware support. (partial: USB and UEFI done under QEMU; real hardware prerequisites implemented, tested only under QEMU)

## Bugs


### Kernel

- [x] KRN-1: A CPU keeps using a task's stack after offering that task to other CPUs.
- [x] KRN-2: Killing a task can leave a lock held forever.
- [x] KRN-3: A running task can't be killed.
- [x] KRN-4: No stack guard, and recursion is reachable from input.
- [x] KRN-5: Idle waiters hold CPUs while ready tasks wait.
- [x] KRN-6: Panic and fault handling don't stop the system, and the ports differ.
- [~] KRN-7: `kill` leaks the victim's heap. (partial: heap is tagged and reported, not reclaimed. Reclaiming isn't safe: a block a task allocated can belong to something shared, such as a VFS file or pipe, so freeing by owner would be a use-after-free)
- [x] KRN-8: `nomem` on IRQ mask and wait `asm!` is formally wrong.
- [x] KRN-9: x86 `poweroff` doesn't power off.
- [x] KRN-10: Atomics run on Device memory with the MMU off. (dcache invalidate added and runs at each CPU's boot, but TCG models no caches, so it has only been shown not to break anything)
- [x] KRN-11: No blocking primitive exists.
- [x] KRN-12: Idle stacks are an immutable `static` written at runtime.
- [x] KRN-13: The allocator aligns offsets, not addresses.
- [x] KRN-14: Heap and console locks are held across preemption.
- [x] KRN-15: x86 runs with caches disabled (CR0.CD and NW set).
- [x] KRN-16: Spurious and non-timer IRQs run a full `preempt`.
- [x] KRN-17: Idle accounting errors on AArch64.
- [x] KRN-18: New AArch64 tasks unmask D, A and F, unlike the boot thread.
- [x] KRN-19: `uptime_ms` saturates after about 71 days at 3 GHz.
- [x] KRN-20: FPCR and FPSR aren't saved per task.
- [x] KRN-21: Tick drift.
- [x] KRN-22: No check for 1 GiB pages or long mode.
- [x] KRN-23: No RAM bound check.
- [x] KRN-24: The byte writer rewrites `\n` to `\r\n`.
- [x] KRN-25: HAL names leak AArch64 onto x86.

### Drivers

- [x] DRV-1: HDA can replay the start of the clip after every `play`.
- [x] DRV-2: DOS audio stalls the whole desktop.
- [x] DRV-3: The HDA lock is held for the whole playback.
- [x] DRV-4: PS/2 button transitions get lost.
- [x] DRV-5: The mouse wheel is dropped on the default tablet.
- [x] DRV-6: The PCI header-type offset is wrong.
- [x] DRV-7: HDA init relies on QEMU's leniency.
- [x] DRV-8: Every build relinks the kernel.
- [x] DRV-9: The virtio queue is too small for stalled pollers.
- [x] DRV-10: GIC EOI loses the SGI source CPU.
- [x] DRV-11: Spurious handling is inconsistent.
- [x] DRV-12: fw_cfg DMA has no ordering guarantee.
- [x] DRV-13: Ring barriers are correct only because the MMU is off.
- [x] DRV-14: HDA gain isn't 0 dB.
- [x] DRV-15: UART presence and errors are unhandled.
- [x] DRV-16: i8042 decoding edge cases.
- [x] DRV-17: PCI BAR details.
- [x] DRV-18: IPIs are impossible today.

### Build

- [x] BLD-1: Medium · `make docker-shell` boots the kernel instead of opening a shell.
- [x] BLD-2: Medium · README display overrides do nothing.
- [x] BLD-3: Medium · Makefile variables are ignored or broken.
- [x] BLD-4: Medium · The x86_64 host triple is hard-coded.
- [x] BLD-5: Medium · Toolchain and image are unpinned.
- [x] BLD-6: Medium · Docker publishes unauthenticated VNC on all interfaces.
- [x] BLD-7: Low · Games image tooling is fragile.
- [x] BLD-8: Low · `make bundles` fails in Docker.
- [x] BLD-9: Low · Rootful Docker probably can't use GTK.

### Filesystem and formats

- [x] FS-1: YAML block scalars slice at a byte index.
- [x] FS-2: Mounting can exhaust the heap.
- [x] FS-3: On-disk sizes are trusted for allocation.
- [x] FS-4: An entry set ending in the last slot of a directory's last cluster returns `Corrupt`.
- [x] FS-5: Broken chains read as success.
- [x] FS-6: No directory depth limit.
- [x] FS-7: exFAT ValidDataLength is never read.
- [x] FS-8: exFAT integrity checks are missing, contrary to the module doc.
- [x] FS-9: `Text` equality and hash include stale bytes.
- [x] FS-10: `init` doesn't check Running on start.
- [x] FS-11: `init` config loses settings silently.
- [x] FS-12: Restart budget and spawn failure logic.
- [x] FS-13: Running out of space during a new chain leaks clusters.
- [x] FS-14: Node slots are never reused.
- [x] FS-15: An apostrophe in a YAML plain scalar opens a quote.
- [x] FS-16: YAML's advertised rejections don't happen.
- [x] FS-17: Valid YAML is silently misparsed.
- [x] FS-18: Common valid YAML is rejected.
- [x] FS-19: `& & & …` recurses without counting depth.
- [x] FS-20: Rename issues.
- [x] FS-21: exFAT boot and range validation.
- [x] FS-22: FAT32 spec gaps.
- [x] FS-23: TAR size handling can desync the reader.
- [x] FS-24: The YAML emitter corrupts values.
- [x] FS-25: Bundle fields over 256 bytes are dropped, not truncated as documented.
- [x] FS-26: JSON error offset is wrong.
- [x] FS-27: Packing script issues.

### Shell

- [x] SH-1: Signed division overflow panics.
- [x] SH-2: `grep -w` slices on non-char boundaries.
- [x] SH-3: `nl -w` above 65535 panics in `core::fmt`.
- [x] SH-4: Heap exhaustion from shell input.
- [x] SH-5: Unbounded recursion with no guard.
- [x] SH-6: Console stdin never reaches EOF.
- [x] SH-7: No cancellation, and the Terminal runs on the desktop thread.
- [x] SH-8: Assignments get word-split and globbed.
- [x] SH-9: The status of `$(...)` is lost.
- [x] SH-10: `$n` inside arithmetic evaluates to 0, and junk is ignored.
- [x] SH-11: `( ... )` never parses.
- [x] SH-12: `exit` in `sh -c` or `sh script` kills the outer shell.
- [x] SH-13: `wc` prints `-` as a filename on stdin.
- [x] SH-14: `/dev/null` and `/tmp` don't exist at boot.
- [x] SH-15: `basename` and `dirname` are advertised but not implemented.
- [x] SH-16: Quoted and escaped glob characters still glob.
- [x] SH-17: Interactive multi-line entry is broken.
- [x] SH-18: Positional parameters don't work.
- [x] SH-19: `${...}` defaults aren't expanded, and unsupported forms silently give an empty string.
- [x] SH-20: `$(...)` runs in the parent shell's state.
- [x] SH-21: Control-flow state leaks.
- [x] SH-22: `tail -n +N` and `head -n -N` are silently wrong.
- [x] SH-23: `sort` keys are wrong, and missing files report success.
- [x] SH-24: POSIX divergences.
- [x] SH-25: External command status is always 0.
- [x] SH-26: Aliases run only the first simple command.
- [x] SH-27: Terminal window display and keyboard.
- [x] SH-28: Editor redraw breaks on long lines and multi-line prompts.
- [x] SH-29: Serial paste loses characters past about 32 per frame.
- [x] SH-30: `mv dir existing_file` destroys the file.
- [x] SH-31: Glob.
- [x] SH-32: Redirection.
- [x] SH-33: Shell misc.
- [x] SH-34: Keys.
- [x] SH-35: Paths.
- [x] SH-36: Terminal window loses keys typed ahead while a command runs (notes: they went into the command's input pipe and stayed there when it didn't read them, so a pasted or quickly typed next line lost its start. It broke the `install` boot test's `printf` chunks whenever frames were slow enough for serial input to back up, which depended on the build (LTO, code layout), not on the code that changed. Unread keys now go back to the line editor when the command ends, as a terminal's input queue does)

### Desktop

- [x] UI-1: Enter in any focused app also launches the wallpaper selection.
- [x] UI-2: End Process fails for a running process.
- [x] UI-3: The supervisor records Stopped even when `kill` failed.
- [x] UI-4: `desktop list|status|menu|run|service` from a spawned process spins forever.
- [x] UI-5: Three of the 8 Start-menu apps show nothing in the GUI.
- [x] UI-6: Killing a preempted process can freeze the GUI.
- [x] UI-7: Ctrl+Esc never opens the Start menu.
- [x] UI-8: The Start menu and blank taskbar click through.
- [x] UI-9: Bare Esc and `q` close apps and discard state.
- [x] UI-10: Explorer wheel scrolling is undone in the same frame.
- [x] UI-11: Explorer shows stale search results.
- [x] UI-12: Task Manager controls that do nothing.
- [x] UI-13: Task Manager figures mislead.
- [x] UI-14: Run dialog bypasses windowed apps.
- [x] UI-15: Dragging a hosted window off-screen reflows it instead of clipping it.
- [x] UI-16: `TextBuffer` keeps a stale anchor.
- [x] UI-17: Windowed apps as autostart services do nothing.
- [x] UI-18: Closing a window leaks its process.
- [x] UI-19: Start-menu commands wired to stubs.
- [x] UI-20: Input queue overflow.
- [x] UI-21: Window manager details.
- [x] UI-22: Latent `libs/gui` panics.
- [x] UI-23: Explorer details.
- [x] UI-24: Session details.

### Media

- [x] MED-1: Unbounded AMF recursion in FLV.
- [x] MED-2: Unchecked `data[header.payload]` in MP4.
- [x] MED-3: Off-by-one in `sps_from_avcc`.
- [x] MED-4: Picture allocation is sized by the SPS, up to 16384x16384.
- [x] MED-5: Unbounded RPLM and MMCO vectors.
- [x] MED-6: Parameter-set allocations are unbounded.
- [x] MED-7: `parse_annex_b` builds the whole list inside `info()`.
- [x] MED-8: MBAFF geometry assumes an even MB height.
- [x] MED-9: Every displayed frame restarts decoding from frame 0.
- [x] MED-10: Wrong frames for any MP4 with B-frames.
- [x] MED-11: `sps_from_avcc` passes the NAL header and emulation-prevention bytes to the SPS parser.
- [x] MED-12: Output stride comes from the codec, reported size from the container.
- [x] MED-13: Unsupported H.264 features aren't rejected.
- [x] MED-14: AIFF `COMM` range is guarded only by the declared size.
- [x] MED-15: POC types 1 and 2 ignore FrameNumOffset.
- [x] MED-16: Per-frame timing is dropped.
- [x] MED-17: Restoring a minimized player jumps forward.
- [x] MED-18: Unknown-size Matroska clusters extend to the segment end.
- [x] MED-19: Raw AVI conversions are wrong.
- [x] MED-20: RLE delta frames decode only the target sample.
- [x] MED-21: Any decode error stops playback permanently.
- [x] MED-22: H.264 crop handling is wrong.
- [x] MED-23: H.264 correctness gaps.
- [x] MED-24: JPEG and BMP.
- [x] MED-25: Player UI.
- [x] MED-26: GIF.
- [x] MED-27: Other containers and codecs.
- [x] MED-28: `pending.clone()` deep-copies about 1 KiB per MB for each continuation slice.
- [x] MED-29: `more_rbsp_data` rescans trailing zeros for every MB.
- [x] MED-30: The player calls itself "VLC media player".

### FEOS-DOS

- [x] DOS-1: `INT` never goes through the IVT and never pops its frame.
- [x] DOS-2: A guest can panic the kernel through mouse ranges.
- [x] DOS-3: A guest can exhaust the heap through the RAM drive.
- [x] DOS-4: Renaming a directory into itself creates a cycle.
- [x] DOS-5: The prefix loop is unbounded, so the desktop hangs.
- [x] DOS-6: Licence claims are contradicted by the code (legal advice recommended).
- [~] DOS-7: Game redistribution rests on one website's label. (kept by owner decision; `data/games/README.md` now records what each title's bundled docs say, which isn't a legal review. Duke Nukem 3D and Warcraft II restrict bundling, in those words: the owner should read that)
- [x] DOS-8: Guest work isn't counted against the budget.
- [x] DOS-9: A crafted `C:\STATE.BIN` panics the kernel.
- [x] DOS-10: `MOUNT` can exhaust the heap.
- [x] DOS-11: `INC` and `DEC` never update OF or AF.
- [x] DOS-12: Rotates clobber flags and compute some wrongly.
- [x] DOS-13: `XLAT` ignores segment overrides.
- [x] DOS-14: The memory allocator doesn't model the program block.
- [x] DOS-15: The PSP command tail is off by one.
- [x] DOS-16: FPU operations are decoded wrongly.
- [x] DOS-17: FPU rounding control is ignored.
- [x] DOS-18: The planar graphics controller is wrong.
- [x] DOS-19: BIOS and port palette access is wrong.
- [x] DOS-20: OPL is undetectable.
- [x] DOS-21: `AH=0Ch` loses every key.
- [x] DOS-22: CPU identity is inconsistent, and 186 support is incomplete.
- [x] DOS-23: PC speaker and BEL are wrong.
- [x] DOS-24: `IMUL` immediate sets flags wrongly.
- [x] DOS-25: Other FPU semantics.
- [x] DOS-26: `INT 10h` text and cursor services.
- [x] DOS-27: Renderer and mode state.
- [x] DOS-28: Mouse fidelity.
- [x] DOS-29: Keyboard paths.
- [x] DOS-30: Handle and file semantics.
- [x] DOS-31: Directory search.
- [x] DOS-32: PSP, environment and BDA.
- [x] DOS-33: Time and hardware stubs.
- [x] DOS-34: CPU details.
- [x] DOS-35: Front end and names.

## Missing features


### Kernel

- [x] Per-CPU IRQ stacks
- [x] Safe kill and a result enum
- [x] Blocking primitives: blocked state, sleep queue, `yield_now`, mutex, wait queue
- [x] IPIs in the `InterruptController` HAL
- [x] Panic stop-all, then per-task fault containment (a fault or panic in a spawned task holding no lock ends it; the boot task, interrupt handlers and lock holders still stop the machine)
- [x] Stack canary and high-water mark in `ps`
- [x] Guard pages
- [x] Process API: `wait`, exit status, accounting, priorities
- [x] Device IRQ registration
- [x] RAM discovery from DTB and multiboot, plus a frame allocator
- [x] x86 TSS and IST for #DF, NMI, #MC
- [x] AArch64 MMU and caches
- [x] x86 SMP: MADT, AP trampoline, per-CPU LAPIC timer (docs admit)
- [~] Per-CPU run queues, affinity, one-shot timers (partial: affinity and one-shot timers done, boot-tested on both ports; queues are per-CPU in policy, home CPUs with stealing and balancing, but under one shared lock, not per-CPU locks)
- [x] EL0, syscalls, per-process address spaces (user programs in `/bin`, boot-tested on both ports: every malicious probe blocked, same address holds different data, 400 spawn/exit rounds leak no frames, `-smp 4` with migration. Single-threaded, no shared memory, so a futex can't yet be woken by another process; kernel-linked apps aren't behind it)
- [~] Real hardware boot: EL2 drop, SCTLR init, PSCI conduit from DT (implemented, tested only under QEMU: `virtualization=on` from EL2 with PSCI by SMC, `secure=on` from EL3 with no PSCI; break-before-make on a split done with the MMU off. No real hardware is available, so none of this has run on any)

### Drivers and platform

- [x] Streaming audio: multi-BDL ring, position tracking, `write`/`available`, mixer
- [x] CI and a scripted QEMU boot test
- [x] IRQ framework: per-device handlers, SPI and IOAPIC routing
- [x] PCI: bridges, BAR allocator, capabilities, MSI/MSI-X, INTx
- [x] `virtio-core` with a PCI transport
- [x] `virtio-blk`
- [x] `virtio-tablet` on q35, or IntelliMouse (`virtio-tablet-pci` and `virtio-keyboard-pci` boot-tested with PS/2 off)
- [x] `virtio-gpu` 2D
- [x] RTC: PL031 and CMOS
- [x] DTB parsing (AArch64)
- [x] ACPI and multiboot memory map (x86)
- [x] `virtio-net` and a TCP/IP stack (boot-tested on both ports over slirp; no IP fragment reassembly, window scaling, SACK or IPv6; predictable sequence numbers: see `docs/networking.md`)
- [x] GICv3
- [x] xHCI, USB HID and storage (root ports only: no hubs, no hot plug; disks up to 2 TiB)
- [x] UEFI boot and firmware framebuffer (boot-tested from a FAT ESP on Debian's OVMF and AAVMF: `boot/efi` loader, GOP framebuffer, memory map, RSDP or DTB; AArch64 with ACPI reads QEMU's device tree at the start of RAM; no Secure Boot)

### Filesystem and data formats

- [~] `virtio-blk` plus a multi-block, async `BlockDevice` (partial: multi-block done, and a waiting task blocks on the device's interrupt. One request at a time is kept on purpose: every filesystem operation holds the one `Vfs` state mutex across its disk I/O, so nothing can submit a second request, and the cache already coalesces contiguous writes into one multi-block request, with metadata ordered by flushes. An async API would have no caller until the VFS lock is split)
- [x] MBR and GPT parsing
- [x] Buffer cache with write-back
- [x] Real VFS: mount/unmount, writable, handles, stat, unified with the coreutils overlay
- [x] Writable on-disk root
- [x] Crash consistency: dirty bit, ordered updates, mount-time check (a torn exFAT entry set on a dirty volume is now repaired: `libs/fs/tests/crash.rs` cuts at every write, in order and with unflushed writes landing at random, and every cut mounts writable with synced files intact. On a clean volume a torn set is damage and stays read-only)
- [x] Timestamps
- [x] Attributes, then permissions (uid/gid per task, `chmod`, `chown`, enforced at the VFS and system call boundary for user programs: `/bin/permtest` at EL0/ring 3, 45 checks on both ports. Kernel-linked apps run as root, unchecked, and `id` says so. FAT, exFAT and RAM mounts stay permissionless and report made-up modes. No user names, supplementary groups, setuid programs or sticky directories; checked at open, not on each read)
- [x] Symlinks in VFS
- [x] FAT12/16 read
- [x] Runtime `format()`
- [x] Streaming inflate and extract-to-sink
- [x] ZIP64, tar.gz helper, `safe_path()`
- [x] Runtime `.fapp` install with an integrity hash
- [x] Native filesystem with POSIX metadata (`feos_fs::native`: redo journal for metadata, ordered data, inodes, mode/uid/gid, mtime, symlinks, hard links, `fsck [-r]`. `libs/fs/tests/native.rs` crashes at every write, in order and at random. `FEOS_BOOT_TEST_ONLY=native`, both ports: real virtio disk, QEMU killed during writes until commits were caught and replayed, 5 kills and 3 replays each. Never the root; mounted at `/native<n>`. Formatting is host-side only, by `native::format` or `scripts/mkfs-feos.py`: the guest has no raw devices to format. Overwrites in place aren't journalled)
- [x] YAML multi-line forms, compact block forms (multi-line plain, single- and double-quoted scalars with folding. Flow collections spanning lines still aren't read)
- [x] JSON builder API (`JsonBuilder`, streaming and allocation-free, invalid sequences are errors; `SliceOutput` for a caller's buffer)
- [x] `init`: exit status, one-shot type, ordering, backoff (host-tested; `autostart` entries can be mappings with `type`, `after`, `args`. No shipped configuration uses them yet, so not boot-tested)

### Shell and terminal

- [x] Ctrl-C cancellation
- [x] Terminal shell on its own task, with streamed output
- [x] Positional parameters, `$@`, `shift`, script args
- [x] Functions and `local` (docs admit)
- [x] Redirection on compound commands, and an fd table
- [x] Here-docs and here-strings
- [x] `case`
- [x] Full `${}` operators
- [x] Full arithmetic, `(( ))`, `let`, `[[ ]]`
- [x] Real subshells
- [x] Backticks, `$'…'`, `$!`, `+=`, IFS
- [x] `set -e/-u/-x`, `pipefail`, `trap`, `wait`, `getopts`, `readonly`, `time` (`FEOS_BOOT_TEST_ONLY=jobs`, both ports: HUP, TERM, QUIT traps from another process and at an idle prompt, HUP on closing a Terminal. Signals are for shells only: anything else is killed by them. `time`'s CPU figure is all reported as user, with sys 0, as the kernel doesn't tell them apart)
- [x] Script execution by path and PATH lookup
- [x] `App::run -> i32` and argv
- [x] Capturing app output
- [~] `jobs`, `fg`, `bg`, `wait` (docs admit) (partial: Ctrl-Z stops a foreground process, kernel-linked app or user program, and `fg`/`bg` resume it, boot-tested on both ports. Not stoppable: coreutils and builtins, which run on the shell's own task, and pipelines or commands with redirected streams. Ctrl-Z is ignored for those)
- [x] Streaming pipelines
- [x] Utilities (S each): `which`, `od`, `fold`, `comm`, `split`, `dd`, `strings`, `realpath`, `mktemp`, `file`, `date`, `timeout`, more `grep`, `ls`, `sort`, `uniq` flags
- [x] `xargs`, `diff`, fuller `find`, `tar`, `gzip`
- [x] Regex engine with groups, alternation, intervals, POSIX classes; then `sed`
- [x] `awk`
- [x] VT100/xterm emulator: cell grid, cursor addressing, colour, wrap, alternate screen
- [x] Text editor
- [x] Pager
- [x] Multiple Terminal instances
- [x] Selection, copy, paste, Unicode width (`FEOS_BOOT_TEST_ONLY=terminal`, both ports: drag, Ctrl+Shift+C, Ctrl+Shift+V through the clipboard. Wide characters take two cells; combining marks are dropped, as a cell holds one character)

### Desktop and GUI

- [x] Console capture window (output opens in a Terminal window; external apps read stdin through `Context::read_stdin`, and user programs reading a terminal get cooked lines, Ctrl-C still working)
- [x] Alt+Tab, Alt+F4, window menu
- [x] Launch with arguments and file associations
- [x] Image preview and thumbnails
- [x] Streaming file viewer
- [x] Clipboard
- [x] Explorer write operations
- [x] Content search on a background worker (`FEOS_BOOT_TEST_ONLY=search`: 192 MiB read in 2.6 s on AArch64 with a longest frame of 82 ms, 3.1 s and 89 ms on x86_64)
- [x] Settings app, persisted (`settings` bundle: colour scheme, keyboard, wallpaper, clock; survives a QEMU kill on both ports)
- [x] Notifications and tray
- [x] Volume control
- [x] Keyboard layouts
- [~] Hosted-app isolation (partial: each window's app runs on its own task, so a fault, panic or hang in one closes that window and the desktop lives, boot-tested on both ports. It's containment, not isolation: kernel-linked apps still share memory and privilege, a fault with a lock held still stops the machine, a stuck app freezes the desktop for up to 30 s, and foreground apps stay on the boot task. A user program in a window, `wdemo`, is fully isolated)
- [x] Background ticks for minimized windows
- [x] Drag and drop
- [x] Resolution changes (`FEOS_BOOT_TEST_ONLY=resolution`, both ports: live through Settings on ramfb to 1600x1200 and virtio-gpu to 1920x1080, neither of which fits the static buffers, with screenshots; maximised windows and the pointer's range follow, a confirm-or-revert prompt, 12 rounds with no leak, kept across a QEMU kill. A firmware framebuffer can't change mode, so it offers none)
- [x] Unicode fonts (`font_extended.rs`, about 590 glyphs, 6 KiB: Latin-1, Latin Extended-A, Greek, Cyrillic, box drawing and blocks. `FEOS_BOOT_TEST_ONLY=fonts`, both ports, screenshots in Terminal and Explorer from real FAT long names. Greek and box drawing are this project's, as the console fonts' tables were wrong there; the rest rests on Debian's statement that they're public domain, see LICENSING.md. Out: CJK and everything else, as an empty box, combining marks, right-to-left, and Latin Extended-B)

### Media

- [x] Stateful decode API with key-frame index (`stss`, Cues, idx1)
- [x] Decode worker with a frame queue
- [x] Mixer, resampler, channel conversion to 48 kHz stereo
- [x] Music playback app or `play` command
- [~] Streaming from disk via a `Source` trait (partial: `feos-source`, with video from MP4, fragmented or not, Matroska/WebM, AVI, FLV, IVF and MPEG program streams, and sound from those and WAV, AIFF, AU, MP3, ADTS, FLAC and Ogg. Boot-tested with files up to 148 MiB, over twice the heap, seeking included, fragmented MP4, program streams, AVI and FLV among them. Raw H.264 and MJPEG files stream too, read once on opening to find their frames. Not streamed, and still refused above 16 MiB: GIF, whose frames are drawn from the whole file in memory. MP3 and ADTS lengths and seeks are estimates unless the rate is constant or an Xing header is there)
- [x] PTS timing: ctts, stts, edit lists, MKV and FLV timestamps
- [x] A/V sync with audio-clock master
- [~] AAC-LC decoder (partial: Main prediction, coupling channels, program config elements, 960-sample frames, LTP, HE-AAC SBR and parametric stereo with IPD and OPD done, compared with FFmpeg on real ISO conformance streams, within 3 of 16 bits, and within 2 for all of SBR and PS, 78 to 89 dB SNR. The earlier 41 dB on `al04` was a real bug in pulse data, and 60 dB at 88.2 kHz was 16-bit float rounding in Main's predictors. `al18` and `aac-sce-in-stereo` differ only in noise substitution, which is random. Refused: gain control, as no real stream was found. SBR with 960-sample frames plays its core. Implicit SBR is only looked for at 24 kHz and below. Boot-tested on both ports)
- [~] FLAC (S-M), Vorbis (M), Opus (L) (partial: FLAC and Vorbis done. Opus SILK, CELT and hybrid done, in Ogg, Matroska and MP4, up to eight channels with mapping family 1, and compared with libopus: SILK narrow and wide band identical, the rest within 1 of 16 bits. All 12 RFC 8251 vectors have no final range errors, and PCM within 4 but for vectors 6 and 12 (10) and 10 (33, 76.6 dB). Three real bugs were behind the old 242: hybrid folding in mono, no SILK reset going from CELT to hybrid, no fade from concealment. Packet loss: SILK's concealment, comfort noise and FEC are ports of libopus's now: on streams from its encoder with every 23rd packet lost, mono SILK is identical to libopus 1.5.2, stereo SILK 59 dB or better, and hybrid FEC within 1 of 16 bits. CELT's pitch-based concealment isn't ported, so CELT-only streams get noise: the 12 RFC 8251 vectors with every 23rd packet dropped are 9 to 44 dB, up from -25 to 16 dB. Only an empty packet reaches it) 256 kbit/s stereo under arm TCG went from 9% to 480% of real time when a direct-sum DFT became an FFT, and `big.opus` now plays with 0% silence there)
- [x] Larger frame budget, level-based DPB limits
- [x] Colour and display: VUI range, BT.709, SAR, rotation
- [x] Image viewer, MJPEG default Huffman tables (S), multi-scan and progressive JPEG (M)
- [x] More video codecs: MPEG-1 or MPEG-4 ASP (M), VP8 (M-L) (notes: VP8 identical to FFmpeg on all 9 fixtures, in WebM and IVF, covering inter frames, golden, hidden alternate references, partitions, segmentation and profiles 1 to 3. MPEG-4 ASP was the alternative and isn't built)
- [x] Container gaps: fMP4, `wide`-first MOV, AVI `rec` and OpenDML, MKV compression
- [x] Volume and mute (S), recording (M), SRT subtitles (S-M), playlist save (S) (notes: `rec` boot-tested with real samples, by QEMU's ALSA backend and ALSA's `file` plugin; 440 and 660 Hz came through exactly. Playlists save to M3U and load back)
- [x] H.264 error concealment (M), real interlace, High10, 4:2:2 (L) (notes: done in a new core, `libs/h264/src/ext/`, our own code written from the spec, generic over `u8` and `u16` samples: High 10, High 4:2:2 and monochrome, PAFF, MBAFF and both together, CAVLC and CABAC. Compared frame by frame with FFmpeg on FATE's `h264-conformance`: 191 of 195 streams identical in their own bit depth and format, the other four being the FMO and SP streams, which stay refused, as do 4:4:4 and lossless. The vendored paths differed from FFmpeg on 20 progressive streams this matches, and this is faster, so everything takes this core now, and the vendored paths are unused: `VENDORED_FOR_PROGRESSIVE` brings them back. 8-bit 640x360 under arm TCG went from 24 to 46 frames/s, and 720p on the host 57 to 66% faster. Fixtures in `testdata/`, including PAFF made by hand as x264 can't. `tests/fate.rs` needs FATE's streams, which aren't in the repository. Boot-confirmed in the Player on both ports; weaving only, with no deinterlacing)
- [x] SSE on x86_64 for decoders

### FEOS-DOS

- [x] 8259 PIC, 8254 PIT, INT 8 to 1Ch chain
- [x] Keyboard hardware: make/break, ports 60h/64h, IRQ1, INT 9, BDA ring
- [x] DOS process model: MCB chain, JFT and SFT, `4Bh` exec and overlay, `4Dh`, parent PSP, TSRs
- [x] Time-based CPU pacing: cycles per ms
- [x] VGA register core: latches, write modes, chain-4 off, panning
- [x] Streaming audio, OPL timers, timestamped OPL writes
- [x] BIOS data and ROM: full BDA, model byte, font pointers
- [x] `COMMAND`: `%n`, IF, GOTO, FOR, CALL, SHIFT, SET, redirection, PATH, ERRORLEVEL
- [x] FCB calls, `6Ch`, `5Ah`, `59h`, `38h`, `65h`, device names
- [x] 386 real-mode ISA: `66`/`67`, `0F xx`, FS, GS
- [x] 386 protected mode, DPMI, DOS/4GW. All 13 titles run in the host harness; see `docs/dosbox.md` for what each reaches. (notes as above)
- [x] Sound Blaster DSP and 8237 DMA (docs admit)
- [x] XMS, EMS, A20, HMA (docs admit)
- [x] Mouse callbacks, hide counter, joystick
- [x] x87 transcendentals (docs admit), true 80-bit mul and div, exception flags

## Improvements


### Testability

- [x] **Fuzz everything that parses.** Host targets for inflate, gzip, zlib, ZIP, TAR, `Bundle::manifest`, YAML, JSON, FAT32 and exFAT mount-walk-read, `feos_video::{info, decode_frame}`, `feos_image::decode`, `feos_audio_codec::decode`, and the shell's `lex`, `parse` and each command. Run them under a 64 MiB failing allocator on a small-stack thread, so out-of-memory and recursion count as failures. (notes: the shell's `lex`, `parse` and each command now run under the failing allocator on a small stack too, in `libs/sh/tests/fuzz.rs`)
- [x] **Fix the vacuous H.264 fuzz test.** `libs/h264/tests/malformed.rs:49-67` has no SPS or PPS, so every input stops early. Seed it with `mini.h264` and mutate.
- [x] **Add a differential shell suite.** Run about 200 lines of scripts on host `dash` or `bash` and on `feos-sh`, comparing stdout and status. Also run the examples in `docs/terminal.md` as tests. `exec.rs` and `editor.rs` have no direct tests.
- [x] **Add a CPU conformance suite for DOS.** Per-instruction JSON test vectors for the 8086. `Cpu` is already generic over `Bus`, and `cpu.rs:1677-1696` has a flat memory bus. Today there are 3 CPU tests and none cover flags. Add table-driven FPU tests. (done with a substitute: JSON vectors recorded from the host's own CPU by `scripts/gen-dos-cpu-vectors.py`, as no published 8086 set was available offline. They found a DAS borrow bug. They run in 32-bit mode and don't cover 8086-specific behaviour)
- [x] **Add a guest fuzz test.** Random memory, registers and INT calls must never panic, and must return within a time bound.
- [x] **Add scripted input tests for `Desktop::render_with_platform`.** Enter and Ctrl+Esc with an app focused, Start-menu click-through, Tab onto a minimized window.
- [x] **Add golden-image tests.** `taskmgr.rs` and `explorer.rs` already render to a host canvas, and `preview_tests` writes PPMs without comparing them. Include minimum and tiny canvases.
- [x] **Add property tests for `TextBuffer`.** Cursor and anchor always in bounds and on char boundaries.
- [x] **Add mock driver tests.** `ConfigSpace` is already mockable; cover the PCI scan, `push_mouse`, `decode_keyboard`, BDL and verb encoding, the ramfb block, and virtqueue handling. (notes: ramfb reaches fw_cfg through a `ConfigFiles` trait now, so `make test-ramfb` checks the 28-byte block byte for byte against a fake, and that a missing item, a wrong size or a refused write configures nothing)
- [x] **Move kernel logic to testable crates.** A `libs/heap` crate so the allocator's tests finally run, and the same for `next_runnable` and the state machine.
- [x] **Add fixtures that today's tests can't see.** CABAC, B-frames, High profile, rotated video, widths not divisible by 4, multi-sector clusters, 4 KiB sectors, cycles, truncated chains and bad BPBs.
- [x] **Assert struct layouts.** `offset_of!` for the AArch64 `TrapFrame` (`arch/aarch64/src/irq.rs:40-41` checks only size and alignment) and for x86 `xmm` and `r15`.
- [x] **Include `feos-apps` and driver crates in `make test`.**

### Robustness

- [x] Add `IrqSpinLock` to `libs/sync`, restoring IRQ state on drop, replacing the hand-paired `save_and_disable`/`restore` at `kernel/src/sched.rs:290-299,355-390,395-414,419-430,435-462,467-497`.
- [x] Add owner-CPU debug tracking to `SpinLock` to catch self-deadlock, and assert IRQs are masked when taking `SCHEDULER`. (both spin locks panic on a self-deadlock; `SCHEDULER` is an `IrqSpinLock`, which masks them itself, so there's nothing to assert)
- [x] Add a header magic to the allocator. `free` silently returns on bad input (`kernel/src/allocator.rs:247-255,266-268`), which hides corruption.
- [x] Save registers in `exception_common` (`arch/aarch64/src/vectors.S:57-64`), so faults can be debugged.
- [x] Use one error policy: fallible allocation in parsers, a `DecodeError::OutOfMemory`, and `FsError::NoSpace`.
- [x] Don't let one bad directory fail a whole mount (`libs/fs/src/fat32.rs:258`, `libs/fs/src/exfat.rs:233`). Skip it and flag the volume.
- [x] Free the chain after deleting the entry, not before (`libs/fs/src/fat32.rs:1053-1056`, `libs/fs/src/exfat.rs:1432-1433`).
- [x] Keep names raw at the scanner, and escape control characters at display. (`FEOS_BOOT_TEST_ONLY=names`. Only `/` and NUL are still replaced at the scanner, as they'd mean something else in a path. Escaped in `ls`, `tree`, `find`, tab completion, the prompt, Explorer and desktop titles and toasts when writing to a terminal or window; piped output stays raw. Error messages quoting a typed path aren't escaped)
- [x] Replace silent truncation with errors where it changes behaviour: `Manifest` fields, and `Text`'s `fmt::Write`, which reports `Ok` on truncation (`libs/text/src/lib.rs:210-213`).
- [x] Validate header counts against the file (`stsz`, `dwTotalFrames`), since they feed duration directly.
- [x] Add a `DmaBuf<T>` built on `UnsafeCell`, with `phys()` and `dma_wmb`/`dma_rmb` in the HAL, before the MMU work. (virtio-blk uses it; the other drivers still use raw statics)

### Design

- [x] **Add a `Board` trait and an arch trait.** Today the only thing that keeps the ports in step is `make check-arm` plus `make check-x64`. Replace `#[cfg(target_arch)]` at `kernel/src/smp.rs:20,46` and `kernel/src/main.rs:81,92` with `arch::smp::start_cpu`. Remove x86's dead `smp::set_handler`, `entry_address` and `cpu::wait_for_event`, and the double `exceptions::init`.
- [x] **Factor out the two boards.** The two `build.rs` files differ by only 31 diff lines. Console wiring, scanout, `filesystem()` and `init_audio` are near-identical.
- [x] **Put port I/O in one crate.** `inb`, `outb` and `outl` are duplicated across i8042, uart16550, apic, pci, fw-cfg and the x86 arch crate.
- [!] **Remove the foreground path.** See [Decisions](#decisions-for-the-project-owner). (won't do: Decisions keep the foreground path and fix routing)
- [x] **Merge duplicates in the shell.** Two line editors (`libs/sh/src/editor.rs`, `apps/commands/src/terminal.rs:403-508`) and two key decoders (`editor.rs:271-348`, `libs/keys/src/lib.rs:156-279`) have already diverged.
- [x] **Replace the parallel `COMMANDS` table and `match`** with `CommandSpec { run: fn }`, so SH-15 can't recur (`libs/coreutils/src/commands/mod.rs:46,368`). Add a shared option parser.
- [x] **Remove shadowed apps.** `echo.rs`, `clear.rs`, `uptime.rs` and `help.rs` are hidden by shell builtins.
- [x] **Key windows by bundle id,** not catalog index, before supporting live rescans (`libs/desktop/src/lib.rs:583-587`).
- [x] **Make the `gui` gallery a real windowed bundle.** It's unreachable from the desktop, and it would exercise `libs/gui` with real input.
- [x] **Separate `info()` from per-frame decode** in `libs/video`.
- [x] **DOS:** a refcounted RAM drive with a quota, a `last_error` value for `AH=59h`, stop clobbering AX where it must be preserved (`1Ah`, `25h`, `35h`, `2Fh`, `51h`, `62h`), and a versioned, validated save-state.
- [x] **Record provenance** for both vendored decoders: upstream SHA, local patches and licence notices. (notes: `libs/h264/UPSTREAM.md` and `libs/audio-codec/src/mp3/UPSTREAM.md`. Both SHAs were established by comparing against upstream's history, not recorded at import. The `rust_h264` version under z264 is still only known as 0.4.x)

### Performance

- [x] Use `[profile.release.package.*] opt-level = 3` for `feos-gfx`, `feos-gui`, `feos-desktop`, `feos-dosbox` and `feos-h264`. `opt-level = "s"` applies to everything today (`Cargo.toml:70`).
- [x] Make the scheduler table zero-initialized so it returns to `.bss`. It currently adds 2.1 MiB to the image (`kernel/src/sched.rs:140`).
- [x] Use `clock.idle_ms` in `sleep.rs:25` and `tick.rs:44`, so they stop burning a core.
- [x] Make `TaskManager::animates()` dynamic. It's always true, forcing 2 fps full-desktop repaints (`apps/commands/src/taskmgr.rs:114-120`).
- [x] Request a second frame after input only when window state changed (`apps/desktop/src/session.rs:179`).
- [x] There is no damage tracking, so every dirty frame repaints the wallpaper, icons and every app, then copies 3 MB (`libs/desktop/src/lib.rs:901-947`, `apps/desktop/src/session.rs:255-256`). Cache the wallpaper and icons first, then add dirty rects.
- [x] Pointer moves recompose the whole screen. The save-under `Cursor` in `taskmgr.rs:1415-1481` is only on the unused foreground path; lift it into the session.
- [x] Use span-based row operations in `Canvas::blit` (`libs/gfx/src/canvas.rs:403-425`), `fill_rect_alpha` and `blend_pixel` (`libs/gui/src/painter.rs:300-332`), and `draw_char` (`painter.rs:517`).
- [x] Clip first in `fill_vertical_gradient` (`canvas.rs:326-339`) and `line` (`canvas.rs:235`, `painter.rs:433`).
- [x] Cache Explorer's per-row filesystem calls (`apps/commands/src/files/explorer.rs:2589,2331-2333`).
- [x] Switch context by swapping SP instead of two 800-byte copies under the global lock (`kernel/src/sched.rs:256,278,283`).
- [x] Remove the dead x86 XMM save, or build with SSE and also save MXCSR (`arch/x86_64/src/interrupts.S:110-126,145-160`).
- [x] Keep allocator counters instead of an O(n) scan under the lock (`kernel/src/allocator.rs:347-357`), and add in-place `realloc`, because the default doubles peak use when a `Vec` grows.
- [x] Use the vector already in the x86 `TrapFrame` instead of up to 8 MMIO reads per tick (`drivers/interrupt/apic/src/lib.rs:100`).
- [x] Use a ticket lock, and `wfe`/`sev` on AArch64 (`libs/sync/src/lib.rs:50-60`). (`IrqSpinLock` is the ticket lock; `SpinLock` can't be one, because a preempted or killed waiter would hold up the queue)
- [x] Batch virtio `publish` per poll, and set `VIRTQ_AVAIL_F_NO_INTERRUPT` while polled (`drivers/input/virtio/src/lib.rs:526,768-769`). (notes: `VIRTQ_AVAIL_F_NO_INTERRUPT` is set, and the buffers a `poll` or `poll_key` call recycles are published with one notification when it returns, where each event took one: a pointer report is several. Event-index and `VIRTQ_USED_F_NO_NOTIFY` aren't negotiated or read, as before. Boot-tested on both ports, including x64's virtio-pci-only input phase and arm's Settings clicks through the virtio tablet)
- [x] Read seeks restart from the first cluster on every call, making `coreutils` 4 KiB chunking O(n²) (`libs/fs/src/fat32.rs:368-374`, `libs/fs/src/exfat.rs:370-376`). Add a per-file cursor or extent map.
- [x] Each sub-block read clears an 8 KiB scratch (`libs/fs/src/volume.rs:68,113`). Add a sector cache.
- [x] `visited.contains` is O(n²) at mount (`fat32.rs:325`, `exfat.rs:325`). Use a bitset.
- [x] `chain_length` runs per write, FSInfo per cluster, and new clusters are zeroed up to three times (`fat32.rs:458,553,557`, `exfat.rs:714`).
- [x] Inflate decodes bit by bit (`libs/compress/src/inflate.rs:57-79,160-180,299`), and CRC-32 is byte-wise (`libs/compress/src/checksum.rs:45-51`). Both matter for extracting game ZIPs under TCG.
- [x] `>>` clones the whole file on each write (`libs/coreutils/src/fs.rs:415-420`), `read_dir` scans all nodes (`:567-581`), and `read_file` always returns everything (`:372-398`). Add `read_at`.
- [x] Use a capped ring buffer for scrollback (`apps/commands/src/terminal.rs:224-227`).
- [x] Stop `env.set("?")` on every command (`libs/sh/src/exec.rs:26,40,52`).
- [x] Use a fixed `[i32; 16]` instead of `vec![0i32; tc]` per residual block (`libs/h264/src/cavlc.rs:34`), and pool `PictureState`.
- [x] Avoid three full RBSP copies per slice (`libs/h264/src/bitstream.rs:25`, `decoder.rs:211,454`, `slice.rs:109`).
- [x] Use AAN or fixed-point IDCT (`libs/image/src/jpeg/mod.rs:354-376`).
- [x] Precompute an x map instead of per-pixel `u64` division in the blit (`apps/commands/src/player.rs:3989-4007`).
- [x] Index MJPEG once instead of an O(n²) count on every decode (`libs/video/src/mjpeg.rs:63-74`).
- [x] Put `TraceEntry` behind a feature (`libs/dosbox/src/cpu.rs:401-418`); it's written on every instruction.
- [x] Use lazy flags, a decode table, and fast REP paths over plain RAM. Replace per-byte range checks with a page map (`libs/dosbox/src/machine.rs:61-80`). (note: opcode dispatch is a function table, with no cache of decoded instructions. Extended memory has wide accesses through its chunk map and fetches a code page cache; the bus keeps one range check for the video and ROM area, as a table look-up wouldn't be cheaper. Host speed rose 10 to 31% for protected-mode titles, and not measurably for real-mode ones; guest speed under TCG wasn't measured)
- [x] Use a palette LUT and per-line decode (`libs/dosbox/src/vga.rs:719-737`).
- [x] Skip OPL generation when no voices or timers are active (`libs/dosbox/src/machine.rs:446`). Timers run on guest time without samples, so voices are what decide it.

### Tooling

- [x] Keep one source of truth for QEMU flags. `Makefile:37` and the two scripts can drift.
- [x] Add `-accel` passthrough to the scripts.
- [x] `FEOS_TEST_DISKS=1` forces a full LTO rebuild on every container start, and `COPY . .` invalidates the build layer on any change. Generate disks at image build, and use BuildKit cache mounts. (notes: disks are made at image build. The apt step keeps downloaded packages in a BuildKit cache, and the build step builds in one and copies the result to `target/`, where the image has always had it, so the image's contents are as before. `COPY . .` still reruns the build step on any change, but it's incremental now: with a warm cache, a one-file change rebuilt the image in 80 s, relinking the kernels only. The cargo registry isn't cached, as `build-std`'s crates have to be in the image for offline runs)
- [x] Exclude `data/` from the Docker context when it isn't needed.
- [x] Add `debug = "line-tables-only"`, which doesn't affect the loaded image.
- [x] Make clippy warnings fail CI once the 227 current warning lines are cleared. (notes: `make clippy` runs clippy over both ports and the user programs with `-D warnings`, and `.github/workflows/ci.yml` has a job for it. The warnings were fixed, by hand or `clippy --fix`, except for eight style lints the Makefile's `CLIPPY_FLAGS` allows with the reason: index loops in DSP code, argument counts, large enum variants (boxing would allocate), complex types, `!(x > 0.0)` for NaN, `next`/`from_str` names, `Result<_, ()>` and `chunks_exact` with a constant. The workflow file isn't enabled anywhere yet)

## Documentation

- [x] CLAUDE.md: correct everything listed in `vnext.md` (notes: run variables, `disk*.img` ignore, the kill smoke test, plus what changed since: clippy, fuzz and CI, interrupt-driven drivers, boot paths, the shared VFS, heap sizing, `feos-apps` now in `make test`)
- [x] README.md: correct everything listed in `vnext.md` (done by the README rewrite below)
- [x] docs/architecture.md: correct everything listed in `vnext.md` (notes: the listed items were already right, or fixed in code comments, by the time this was checked; also corrected the display, HDA and PCI paths, the heap's users, the filesystem path (rewritten for the shared VFS and disks), the boot-test phases, and added clippy, fuzz and CI)
- [x] docs/terminal.md: correct everything listed in `vnext.md` (notes: language table, pipelines and the files section rewritten from the code; the coreutils list is generated from `COMMANDS`; the `source` example now sources a script)
- [x] docs/desktop.md: correct everything listed in `vnext.md` (notes: sample manifest, app count, Ctrl-D, input devices, Alt shortcuts, layout tree, run-time `appinstall`; UI-17's autostart sample works now, so it stays)
- [x] docs/dosbox.md and docs/dosbox-gap.md: correct everything listed in `vnext.md` (notes: the listed claims are gone or true after the rewrite; fixed the RAM note, the gap table's real-mode and network wording and its numbering, and the guest's stale `ABOUT` text. `data/games/README.md` and `GAMES.TXT` checked)
- [x] Filesystem and media doc comments: correct everything listed in `vnext.md` (notes: `feos-fs`, FAT32, exFAT and YAML headers, `feos-coreutils::fs`, `feos-bundle`'s icon names, `feos-video`'s table and README (VP8, MPEG-1, streaming, audio), `feos-audio-codec`'s Opus row, the HDA driver header, the player header, and the text files in the embedded image, `media/README.txt` included)

## Added requests

- [x] Bugs seen while taking README screenshots:
  - [x] Stale boot log: `[sched] preemptive round-robin at 100 Hz, 32 process slots` and the AArch64 banner "a tiny aarch64 kernel" (now `[sched] preemptive, priorities 0-15, 10 ms slices on a one-shot timer per CPU, 32 process slots` and `feos 0.1.0 - aarch64 kernel`; no test matched the old text)
  - [x] Component Gallery doesn't fit its window (labels cut off: "Notification", "Compact dens", "New w..."), Compact density has no visible effect, and chart y-axis labels run upside down (0 at the top) (notes: short sidebar labels, buttons sized to their text, subtitles and hints ellipsized to the room left, the page scrolls when a tab is taller than the window; Compact switches to `Theme::compact()`; `feos-gui` charts label the top grid line with the maximum; README screenshot retaken)
  - [x] Task Manager Processes tab: Image Name runs into the PID column ("File Explorer1000000"); list header and selection stay blue under the xp-olive theme (notes: the overflow was the seven-digit hosted-window PID; every cell is clipped to its column and the PID column fits seven digits; `WindowContext::theme` carries the desktop's widget theme, and title, header and selection colours follow its accent, classic XP colours for xp-blue)
  - [x] FEOS-DOS `DEMO`: BOX.COM's box draws misaligned (right border detached) (notes: two causes: BOX.COM's middle row was five cells wider than its top and bottom, and the double-line corner and tee glyphs in `libs/dosbox/src/font.rs` ended their lines in the wrong column)
  - [x] x86_64 serial shell drops characters when typed at ~100 chars/s (notes: the shell's Ctrl-C poll while a command runs drained the UART into a 256-byte type-ahead buffer and threw the rest away; it now stops at a full buffer (now 1 KiB), so QEMU holds the rest back. Same code on arm. The 16550 driver also no longer drops a good byte on an overrun flag. A 4.3 KB burst arrives whole on both ports)
  - [x] Logging off leaves open windows' apps running (`dosbox`, `dos-vm`, `mixer` in `ps`); decide and fix if unintended (notes: unintended; log off now closes every window and stops their processes. `mixer` is the audio service: it starts with the first stream and exits 3 s after the last closes, so it goes once `dos-vm` does. Session services keep running)
  - [x] `play` usage text omits Opus (now "play a WAV, AIFF, AU, MP3, FLAC, Vorbis, Opus or AAC file, or the sound of a video")
  - [x] Explorer view-mode count: source header says five, `docs/architecture.md` says four (five is right: Details, Icons, List, Tiles, Thumbnails; the doc is fixed)
- [x] Fix `make docker-run-x64`: reported not working (owner request; check `make docker-run-arm` and the `docker-run-vnc-*` targets too)
- [x] Rewrite `README.md` with more screenshots and full capabilities (do last, from the final kernel) (notes: rewritten from the code and docs; 10 new screenshots in `docs/screenshots/` from release kernels of both ports, `docs/desktop.png` and `docs/dosbox.png` refreshed, six stale README-only images removed)
- [x] Build an organised iconset in `assets/icons/` from the supplied icons
- [x] Use the new icons in the desktop bundles, with an ICO decoder
- [~] Use the `assets/icons` iconset for every icon in FEOS, replacing all icons drawn in code (owner request) (partial: every picture is from the iconset, through `feos-iconset`, including `feos_gui::Icon` where a picture matches; small UI marks and symbols the iconset has no picture for stay drawn: caption buttons, chevrons, arrows, check, plus/minus, close, scroll and combo arrows, checkboxes and radio buttons, warning triangle, success, play/pause, rating stars, the Start flag, Show Desktop, the Turn Off dialog's buttons, and Explorer's Back/Forward/Up)
- [x] Add a cursor set to `assets`, organised and well named, and use it in FEOS with a `.cur`/`.ani` decoder (owner request; source is `~/Downloads/windows-xp-1`, whose readme says "License: Demo") (note: `assets/cursors`, decoded by `feos_image::cur` and `ani`; the owner chose to include them despite the demo licence, see `LICENSING.md`)
