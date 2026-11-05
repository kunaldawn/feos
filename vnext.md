# feos vNext

A roadmap from a full audit of the codebase at commit `78cf9c0` (2026-11-04): about 125,000 lines across 43 crates. All line numbers refer to that commit.

## Summary

feos builds cleanly on both architectures and its 418 `make test` tests pass, but those tests rarely try bad input, and that is where it breaks. One typed shell line, one malformed media file or one DOS program can end the session or hang the desktop.

**Recommended theme for vNext: make what exists trustworthy, then make it persistent.** New features should wait until the kernel can survive a bad app. In order:

1. **Settle licensing.** The docs and `Cargo.toml` say the DOS code is original MIT/Apache, but its own file headers say LGPL and rust-dos-derived (DOS-6). This is the only item that could force a rewrite.
2. **Add a safety net.** CI, a QEMU boot test, and the reproductions from this audit kept as regression tests.
3. **Fix the crashes that are reachable from input.** Most are small patches.
4. **Fix the kernel foundations.** A race on the interrupt stack (KRN-1), an unsafe `kill` (KRN-2, KRN-3), no stack protection (KRN-4) and no way to block (KRN-11).
5. **Get long-running work off the desktop thread.** The terminal, DOS VM, video decoder and audio all stall the compositor.
6. **Add persistence.** `virtio-blk`, a shared writable filesystem and a real-time clock.
7. **Make the headline features actually work.** Shell scripting, music playback, and DOS programs that use `CALL` and `INT` together.

## Contents

- [How this was verified](#how-this-was-verified)
- [Baseline](#baseline)
- [Root causes](#root-causes)
- [Milestones](#milestones)
- [Decisions for the project owner](#decisions-for-the-project-owner)
- [Bugs](#bugs): [kernel](#kernel-and-architecture) · [drivers and tooling](#drivers-boards-and-tooling) · [filesystem and formats](#filesystem-and-data-formats) · [shell](#shell-coreutils-and-terminal) · [desktop](#desktop-gui-and-gui-apps) · [media](#media-stack-and-player) · [FEOS-DOS](#feos-dos)
- [Missing features](#missing-features)
- [Improvements](#improvements)
- [Documentation that is wrong](#documentation-that-is-wrong)
- [Checked and found correct](#checked-and-found-correct)
- [Limits of this audit](#limits-of-this-audit)

## How this was verified

Each bug is tagged with its strongest evidence:

| Tag | Meaning |
|---|---|
| `[boot]` | Reproduced on the real kernel, booted in QEMU 7.2 from the `feos` Docker image over the serial console |
| `[test]` | Reproduced on the host, by a test added to a throwaway copy of the repo or by running the build command |
| `[code]` | Traced through the source by reading only, and not run |
| `[check]` | Plausible from the code, but needs a runtime check before spending effort on it |

Most `[code]` findings were not independently re-checked. Treat them as well-reasoned leads, and confirm each before fixing it. Claims about QEMU, the Arm ARM or the Intel SDM come from memory unless the finding is tagged `[boot]`.

Severity:

- **Critical:** input can kill the session, corrupt memory or hang the system for good, or the issue is a legal blocker.
- **High:** a core feature is broken or gives wrong results with ordinary use.
- **Medium:** wrong behaviour with a workaround or a narrower trigger.
- **Low:** edge cases, and latent problems with no effect today.

Size: **S** is a small local patch, **M** is a contained piece of work, **L** is a project of its own.

## Baseline

| Check | Result |
|---|---|
| `make test` | 418 passed, 0 failed |
| `cargo test -p feos-apps` (not part of `make test`) | 34 passed, 0 failed |
| `cargo build --release`, AArch64 and x86_64 | Both succeed |
| `cargo clippy` (AArch64 workspace) | Exit 0, 227 warning lines, mostly style |
| Boot on QEMU, both architectures | Reaches the desktop and serial shell |
| Image size, AArch64 | text 1.07 MB, data 2.67 MB, bss 75.9 MB |
| Image size, x86_64 | text 1.22 MB, data 2.53 MB, bss 74.9 MB |

CI does not exist, and kernel, arch and most driver code has no tests. The allocator's tests never run, because `kernel/Cargo.toml` sets `test = false`.

## Root causes

Most of the bugs below come from a few causes. Fixing these is worth more than fixing the individual symptoms.

1. **No containment.** Everything runs as kernel threads in one address space. When PID 1 panics, the desktop or shell it was running is gone and only a reset recovers it. On AArch64, background tasks keep running (`[boot]`). Allocation failure panics too. A grep finds 552 `unwrap`/`expect`/`panic!`/`unreachable!` sites outside `tests/` directories (some are in in-file test modules), plus many unchecked slices.
2. **No stack protection.** Stacks have no guards. Process stacks sit directly next to the task's own saved state, and the boot stack sits directly above `.bss`. Unbounded recursion therefore corrupts memory instead of faulting, and several recursions are reachable from input.
3. **Input sizes are trusted.** Parsers allocate whatever a header asks for from one shared 64 MiB heap.
4. **`kill` is not safe.** It frees a slot without running destructors, so locks stay held and memory leaks. It also refuses to touch a task that is currently running.
5. **No way to block.** There is no sleep state, wait queue or yield. As a result, long work runs in place on the compositor thread, and `Audio::play` holds a lock while it waits.
6. **Happy-path tests.** Malformed-input tests are almost absent. The one H.264 fuzz test stops at "no PPS available" for every input, so it exercises almost nothing.
7. **Docs describe intent, not behaviour.** Many documented features don't work. See [Documentation that is wrong](#documentation-that-is-wrong).

## Milestones

Each milestone depends on the ones before it, except where noted.

### M0: Legal and safety net

Do this first, because it decides what code you can keep and it protects all later work.

- [ ] Decide the licensing direction and fix it (DOS-6). Add root licence files.
- [ ] Review the shipped game data and third-party branding (DOS-7, MED-30).
- [ ] Pin the toolchain, Docker image and `rust-version` (BLD-5).
- [ ] Add CI running `check-arm`, `check-x64`, `test`, `feos-apps` tests and both release builds.
- [ ] Add a scripted QEMU boot test on both architectures. Needs KRN-9 so x86 can exit.
- [ ] Turn the reproductions from this audit into regression tests, marked `#[ignore]` until fixed.
- [ ] Fix `boards/*/build.rs` so a no-op build doesn't relink (DRV-8).

**Done when:** a pull request can't break either architecture or either boot without CI failing.

### M1: Stop input from crashing the system

Mostly small, independent patches that can land in any order.

- [ ] Shell and coreutils: SH-1, SH-2, SH-3, SH-4 (width caps; a shell-level byte budget), SH-5 (depth limits).
- [ ] YAML and filesystem: FS-1, FS-2, FS-3, FS-6.
- [ ] Media: MED-1, MED-2, MED-3, MED-4 to MED-8, MED-14.
- [ ] FEOS-DOS: DOS-2, DOS-3, DOS-4, DOS-5, DOS-9, DOS-10.
- [ ] Add panic-focused clippy lints to parser crates (`string_slice`, `indexing_slicing`, `arithmetic_side_effects`).
- [ ] Add host fuzz targets with a 64 MiB failing allocator and a small-stack thread. See [Testability](#testability).
- [ ] Add `overflow-checks = true` to a fuzzing profile, so wraps that release builds hide become failures.

**Done when:** an agreed fuzzing run over the shell, YAML, JSON, archive, filesystem, video and DOS targets finds no panic or stack overflow.

### M2: Kernel foundations

- [ ] Per-CPU IRQ stacks (KRN-1).
- [ ] Safe kill: pending-kill flag, a lock-depth counter, and a result enum (KRN-2, KRN-3, KRN-7).
- [ ] Stack canaries checked on each tick, a high-water mark in `ps`, and stacks moved out of `Task` (KRN-4).
- [ ] A decided panic policy, with an IPI to stop other CPUs or containment per task (KRN-6). IPIs need DRV-18 fixed.
- [ ] Blocking primitives: a blocked state, sleep queue, `yield_now`, a mutex (KRN-11).
- [ ] Process API: exit status, `wait`, `App::run -> i32`, argv (SH-25, FS-12).
- [ ] Remove `nomem` from IRQ and DMA `asm!` (KRN-8, DRV-12).
- [ ] Move the allocator and scheduler policy into host-testable crates.

**Done when:** the smoke test `spawn`, `ps`, `kill` works for a spinning task on any CPU count, and killing a task can't wedge a lock.

### M3: A responsive desktop

- [ ] Streaming audio: a cyclic ring, non-blocking `Audio::write`, a mixer (DRV-1, DRV-2, DRV-3).
- [ ] Run the terminal's shell on its own task, with streamed output and Ctrl-C (SH-6, SH-7).
- [ ] Run the DOS VM on its own task, with a watchdog (DOS-8).
- [ ] Run the video decoder on a worker with a stateful API (MED-9, MED-10).
- [ ] Fix window and keyboard routing (UI-1, UI-7, UI-9, UI-14).
- [ ] Capture app output into windows, so Run and Start-menu items show something (UI-5).
- [ ] Add occlusion-aware hit testing (UI-8).
- [ ] Performance: `opt-level = 3` for hot crates, a cached wallpaper, and then damage tracking.

**Done when:** no app or guest can freeze the pointer, and closing any window works during heavy load.

### M4: Persistence

- [ ] Fix the PCI header offset (DRV-6) and add a `virtio-core` with a PCI transport, so x86 gains virtio.
- [ ] `virtio-blk`, MBR/GPT parsing and a buffer cache.
- [ ] A shared, locked, writable VFS across the serial shell, Terminal and Explorer. Fix FS-4, FS-5, FS-13 and FS-14 first.
- [ ] A real-time clock (PL031 and CMOS), and then file timestamps.
- [ ] Settings that survive a reboot, and Explorer write operations.

**Done when:** a file created in Terminal shows up in Explorer and is still there after a reboot.

### M5: Make the headline features real

These three tracks are independent of each other.

- **Shell:** positional parameters, then functions, then an fd table with redirection on compound commands, here-docs, `case`, and then a VT emulator and an editor.
- **Media:** a mixer and `play` command, so existing audio codecs become usable; then key-frame seeking, larger frames, and A/V sync.
- **FEOS-DOS:** IVT dispatch (DOS-1) first, then a CPU test suite, then the PIC and PIT, keyboard hardware and a real DOS process model. These come before protected mode.

**Done when:** a script takes arguments and defines functions, an MP3 plays, and the six real-mode titles in the game library have each been tried and their status recorded.

### Later

Each of these is an L, and none is needed for the theme above:

- AArch64 MMU and caches. Move this into vNext if real hardware or KVM becomes a goal, because KRN-10 may block booting there.
- x86_64 SMP.
- EL0, a syscall boundary and per-process address spaces.
- Networking.
- 386 protected mode and DPMI for DOS.
- USB, UEFI and real hardware support.

## Decisions for the project owner

1. **Licensing of FEOS-DOS (legal advice recommended).** Either keep the OPL and F80 code and change the licence terms, or replace it. Replacing it is the only route to a clean MIT/Apache claim, and needs a clean-room F80 and a permissively licensed OPL core. `cpu.rs`, `dos.rs` and `vga.rs` also need comparing against rust-dos.
2. **Game data in git.** 13 ZIPs and a 64 MiB image are tracked, and the basis for redistributing them is one website's label. Keep them, or switch to a download script?
3. **Third-party names.** The player calls itself "VLC media player", and the Explorer and Task Manager use Windows naming. Rename them?
4. **Target.** Is real hardware or KVM a goal? This decides whether the AArch64 MMU work moves up, and how much HDA and UART spec conformance matters.
5. **Platform parity.** Must x86_64 reach parity with AArch64 in vNext? SMP on x86 is a large item.
6. **The foreground app path.** All shipped foreground apps are also windowed, so the old path is reachable only by mistake (UI-14). Remove it?

---

# Bugs

## Kernel and architecture

### Critical

- **KRN-1 · A CPU keeps using a task's stack after offering that task to other CPUs.** `[code]` Confirmed by reading `arch/aarch64/src/vectors.S:88,136-160` and `kernel/src/sched.rs:244-286`. `irq_entry` keeps the `TrapFrame` on the interrupted task's stack. `preempt` marks that task Ready and unlocks, but the CPU still restores from that stack. If another CPU resumes the task, its first push overwrites the saved SP, ELR and SPSR. **Impact:** rare, random crashes on the default `-smp 4`, likeliest when a vCPU thread is descheduled. **Fix:** per-CPU IRQ stacks. x86 needs the same before it gets SMP.
- **KRN-2 · Killing a task can leave a lock held forever.** `[boot]` `kernel/src/sched.rs:405-409`. The slot is freed without running destructors. **Reproduced on x86_64:** `spawn tone 3000`, `kill 2`, then `tone` hangs the shell for good, and `info` hangs when it reaches the audio line. The same risk applies to the print and heap locks, where a hang would be system-wide. **Fix:** a per-task lock depth and pending-kill flag, reaping only at depth 0, or cooperative cancellation.

### High

- **KRN-3 · A running task can't be killed.** `[boot]` `kernel/src/sched.rs:405`, `apps/commands/src/kill.rs:31`. **Reproduced on AArch64:** `spawn tick 1000 10000`, `kill 2` prints `cannot kill pid 2 (missing, or it is the shell)`. While runnable tasks don't outnumber CPUs, the target is always current. Task Manager's End Process fails the same way, and the supervisor records the service as stopped anyway (UI-3). The documented smoke test only passes because repeated spawns oversubscribe the CPUs. **Fix:** a pending-kill flag honoured at the owner's next tick, and a result enum.
- **KRN-4 · No stack guard, and recursion is reachable from input.** `[boot]` `kernel/src/sched.rs:78-92`, `boards/*/linker.ld`, `libs/sh/src/lib.rs:433-438`. **Reproduced:** `echo ". /a" > /a; . /a` kills the serial shell with no panic message. The cause of death wasn't captured. Overflowing the boot stack would run into console function pointers; overflowing a process stack would run into its own saved context. **Fix:** a recursion limit in the shell (SH-5), stack canaries, and stacks moved out of `Task`. Guard pages can follow, and x86 paging already allows them.

### Medium

- **KRN-5 · Idle waiters hold CPUs while ready tasks wait.** `[code]` `arch/aarch64/src/timer.rs:57-73`, `arch/x86_64/src/timer.rs:115-130`. `idle_ms` doesn't yield. On one CPU with the desktop and a CPU-bound task, up to about half the throughput is lost and reported as idle. **Fix:** KRN-11.
- **KRN-6 · Panic and fault handling don't stop the system, and the ports differ.** `[boot]` `kernel/src/main.rs:262-263,278-282`, `arch/*/src/cpu.rs:8-12`. **Reproduced on AArch64:** after a shell panic, `tick` keeps printing while the shell is dead. A panicking AArch64 task stays in rotation, because `wfe` wakes on the tick and any locks it holds stay held. x86 uses `cli; hlt`, which stops its single CPU. A sync fault prints "system halted" while other CPUs carry on. The docs and comments wrongly say the machine halts. **Fix:** decide a policy; mask IRQs and stop other CPUs by IPI, or contain the fault per task.
- **KRN-7 · `kill` leaks the victim's heap.** `[code]` `kernel/src/sched.rs:406-407`. There is no per-process ownership, so a killed task's allocations are lost from the 64 MiB arena. **Fix:** cooperative exit.
- **KRN-8 · `nomem` on IRQ mask and wait `asm!` is formally wrong.** `[check]` `arch/aarch64/src/irq.rs:111,117,128-129,137,143`, `arch/aarch64/src/timer.rs:71`, `arch/x86_64/src/irq.rs:172,178,205`, `arch/x86_64/src/timer.rs:126`. The compiler may move memory operations across these, for example moving a lock swap outside the masked region. The current binary is correct. **Fix:** drop `nomem`, and keep `nostack`.
- **KRN-9 · x86 `poweroff` doesn't power off.** `[boot]` `arch/x86_64/src/power.rs:13,35`. **Reproduced:** QEMU keeps running until killed, whereas AArch64 exits correctly. It writes `SLP_TYP=5`, which QEMU probably doesn't treat as soft-off; the usual QEMU value is `0x2000` to port `0x604`. The command also prints "powering off via PSCI", which is an Arm mechanism. **Fix:** use the right value or read `_S5` from ACPI, and make the message generic.
- **KRN-10 · Atomics run on Device memory with the MMU off.** `[check]` `libs/sync/src/lib.rs:51,65`. Exclusives on Device-nGnRnE memory are implementation-defined. TCG doesn't model this, so it works in QEMU, but on KVM or real hardware the first lock could hang. **Fix:** enable the MMU and map RAM as Normal before the first lock.
- **KRN-11 · No blocking primitive exists.** `[code]` `sleep` and `tick` spin. Every wait burns a CPU, and long-held spinlocks are the only synchronisation, against the guidance at `libs/sync/src/lib.rs:7-10`. Listed as a bug because it causes KRN-5, DRV-3 and DOS-8. **Fix:** a blocked state, sleep queue and software reschedule.
- **KRN-12 · Idle stacks are an immutable `static` written at runtime.** `[code]` `kernel/src/sched.rs:73,221-226`, `arch/x86_64/src/irq.rs:123`. They land in `.rodata`, which is undefined behaviour that works only because nothing is write-protected. It will fault as soon as `.rodata` is mapped read-only. It also adds 128 KiB to the AArch64 image. **Fix:** `UnsafeCell` in `.bss`.
- **KRN-13 · The allocator aligns offsets, not addresses.** `[code]` `kernel/src/allocator.rs:30-31,133,227`. `Layout(64, align=64)` can return a pointer that is 0x30 mod 64. No current heap caller over-aligns, so this is latent. **Fix:** align `base() + offset`.
- **KRN-14 · Heap and console locks are held across preemption.** `[code]` `kernel/src/allocator.rs:348,363,367`, `libs/console/src/lib.rs:136,182`. This isn't a deadlock, but spinners burn up to about 15 ticks on x86 waiting for a preempted holder. **Fix:** an IRQ-saving lock for the heap and a buffered console.
- **KRN-15 · x86 runs with caches disabled (CR0.CD and NW set).** `[check]` `arch/x86_64/src/boot.S:106`. No effect under TCG. On real hardware it would be very slow and unsafe with DMA. **Fix:** clear both bits.

### Low

- **KRN-16 · Spurious and non-timer IRQs run a full `preempt`.** `[code]` `kernel/src/main.rs:239-254`, `kernel/src/sched.rs:250-251`. They count as ticks and rotate the task. **Fix:** preempt only on the timer IRQ.
- **KRN-17 · Idle accounting errors on AArch64.** `[code]` `arch/aarch64/src/timer.rs:65,68`. A migrated task sets the old CPU's hint, and a tick after one `wfi` counts fully as idle. **Fix:** mask IRQs around the hint and `wfi`, as x86 does.
- **KRN-18 · New AArch64 tasks unmask D, A and F, unlike the boot thread.** `[code]` `arch/aarch64/src/irq.rs:69,111`.
- **KRN-19 · `uptime_ms` saturates after about 71 days at 3 GHz.** `[code]` `libs/hal/src/lib.rs:57-62`. Time then freezes and deadlines stop expiring. **Fix:** 128-bit maths.
- **KRN-20 · FPCR and FPSR aren't saved per task.** `[code]` `arch/aarch64/src/irq.rs:22-37`. No writers today.
- **KRN-21 · Tick drift.** `[code]` `arch/aarch64/src/timer.rs:120`. TVAL re-arms relative to now. **Fix:** `CVAL += period`.
- **KRN-22 · No check for 1 GiB pages or long mode.** `[check]` `arch/x86_64/src/boot.S:61-69`. `-cpu qemu64` should triple-fault silently. The scripts pass `-cpu max`.
- **KRN-23 · No RAM bound check.** `[check]` `boards/*/linker.ld`. The image needs about 76 MiB, so `-m 64M` should fault during BSS clear with no output. **Fix:** linker `ASSERT`s and memory discovery.
- **KRN-24 · The byte writer rewrites `\n` to `\r\n`.** `[code]` `boards/qemu-virt/src/lib.rs:266-273`, `boards/qemu-x86_64/src/lib.rs:197-204`. Contradicts "keeps binary output intact" at `libs/console/src/lib.rs:55`.
- **KRN-25 · HAL names leak AArch64 onto x86.** `[boot]` On x86, `info` prints `exception level : EL0`, `MIDR_EL1` and "generic timer". **Fix:** arch-neutral names in `Cpu`.

## Drivers, boards and tooling

### High

- **DRV-1 · HDA can replay the start of the clip after every `play`.** `[code]` `drivers/audio/intel-hda/src/lib.rs:454-458`. DMA is cyclic, and the code spins 100 ms with RUN still set after completion. Older WAV captures in `target/audio` show the head samples repeating at the end, though they predate the current code. **Fix:** a two-entry BDL with silence in the second entry, stopping during the silence.
- **DRV-2 · DOS audio stalls the whole desktop.** `[code]` `apps/dosbox/src/lib.rs:102,164`. `play` blocks the desktop thread for the block length plus 100 ms, which gives roughly 5 FPS and gapped audio by calculation. **Fix:** streaming audio (M3).
- **DRV-3 · The HDA lock is held for the whole playback.** `[boot]` `drivers/audio/intel-hda/src/lib.rs:530-539`, `apps/commands/src/tone.rs:66`. `stop()` and `is_ready()` block until the clip ends, contradicting "stops immediately" at `libs/hal/src/lib.rs:141`, and it is what makes KRN-2 wedge. **Fix:** hold the lock only while programming registers.
- **DRV-4 · PS/2 button transitions get lost.** `[code]` `drivers/input/i8042/src/lib.rs:167,218`. `pending: Option<PointerEvent>` is overwritten by each decoded packet, so a press followed by another packet in the same drain is lost. **Impact:** dropped clicks and stuck drags on x86_64. **Fix:** a small event ring that coalesces only position reports.

### Medium

- **DRV-5 · The mouse wheel is dropped on the default tablet.** `[code]` `drivers/input/virtio/src/lib.rs:636`, with the QEMU event type to confirm at runtime. `EV_REL` is only accepted for relative pointers, and x86 PS/2 has no wheel at all, so the wheel scrolling claimed at `README.md:1023` likely works on neither default board. **Fix:** accept `REL_WHEEL` for any pointer, and add the IntelliMouse handshake.
- **DRV-6 · The PCI header-type offset is wrong.** `[code]` Confirmed by reading `drivers/bus/pci/src/lib.rs:32`. It uses `0x0c` (cache line size) where the spec says `0x0e`, so functions above 0 are never scanned. **Impact:** blocks a future q35 AHCI driver at 00:1f.2. **Fix:** use `0x0e`, and add a mock `ConfigSpace` test.
- **DRV-7 · HDA init relies on QEMU's leniency.** `[code]` `drivers/audio/intel-hda/src/lib.rs:188-206,218,120,437,307,278,71,226`. The reset sequence is inverted and leaves the controller in reset, CORB and RIRB sizes aren't programmed, the BDL has one entry where the spec requires two, and the AFG is never powered. **Impact:** would fail on real controllers. **Fix:** follow the spec sequence.
- **DRV-8 · Every build relinks the kernel.** `[test]` `boards/qemu-virt/build.rs:22-24`, `boards/qemu-x86_64/build.rs:27-29`. **Reproduced:** a no-op `cargo build` reports ``Dirty feos-board-qemu-virt: the file `boards/qemu-virt/disk.img` is missing`` and takes 14 s. `rerun-if-changed` on an absent file is always stale. Nothing tracks `linker.ld` either. **Fix:** watch the directory, register only images that exist, and add the linker script.
- **DRV-9 · The virtio queue is too small for stalled pollers.** `[check]` `drivers/input/virtio/src/lib.rs:22`. 16 descriptors hold about 5 tablet groups. A dropped release during a stall leaves a button or modifier stuck. **Fix:** 128 or more entries, or an IRQ.

### Low

- **DRV-10 · GIC EOI loses the SGI source CPU.** `[code]` `drivers/interrupt/gicv2/src/lib.rs:125,133`. Latent until the first IPI. **Fix:** return an opaque acknowledge token.
- **DRV-11 · Spurious handling is inconsistent.** `[code]` `drivers/interrupt/apic/src/lib.rs:39`, `libs/hal/src/lib.rs:219`. The APIC returns `0xff` where the HAL specifies 1023. **Fix:** an `is_spurious()` trait method.
- **DRV-12 · fw_cfg DMA has no ordering guarantee.** `[check]` `drivers/firmware/fw-cfg/src/lib.rs:197-212,238-241`. Fine in the current binary. **Fix:** drop `nomem` on the kick and add a fence.
- **DRV-13 · Ring barriers are correct only because the MMU is off.** `[code]` `drivers/input/virtio/src/lib.rs:745,766`, `drivers/audio/intel-hda/src/lib.rs:246,409,429`. Will break once the AArch64 MMU is on.
- **DRV-14 · HDA gain isn't 0 dB.** `[check]` `drivers/audio/intel-hda/src/lib.rs:83`. Older captures are about 0.71 of the expected level. **Fix:** read the amp capabilities.
- **DRV-15 · UART presence and errors are unhandled.** `[check]` `drivers/serial/uart16550/src/lib.rs:106-112`, `drivers/serial/pl011/src/lib.rs:106-112`. With no COM1, `read_byte` returns `0xff` forever, and `keys.poll` never returns.
- **DRV-16 · i8042 decoding edge cases.** `[code]` `drivers/input/i8042/src/lib.rs:320-348`. The Pause key's `E1` prefix isn't handled and emits LCtrl and NumLock. Mouse overflow bits are ignored, and a lost byte misaligns packets (`[check]`).
- **DRV-17 · PCI BAR details.** `boards/qemu-x86_64/src/lib.rs:74-76,309` overwrites SeaBIOS's BAR with a constant and no conflict check (`[check]`). `drivers/bus/pci/src/lib.rs:297-303` leaves decode enabled during sizing, and `:315` claims 64-bit BARs while `:331` rejects addresses above 4 GiB (`[code]`).
- **DRV-18 · IPIs are impossible today.** `[code]` Combines DRV-10 with the lack of a send API in `InterruptController`. Needed for KRN-6 and reschedules.

### Build and tooling

- **BLD-1 · Medium · `make docker-shell` boots the kernel instead of opening a shell.** `[code]` `Makefile:319`, `scripts/docker-run.sh:53-54`. **Fix:** add `--entrypoint bash`.
- **BLD-2 · Medium · README display overrides do nothing.** `[code]` `README.md:447-448`, `Makefile:88,92-93`. `FEOS_DISPLAY=gtk make run` has no effect, because `run-arm` reads `QEMU_DISPLAY`, and `make run-x64` overwrites `FEOS_*` from the environment. **Fix:** one variable family.
- **BLD-3 · Medium · Makefile variables are ignored or broken.** `[code]` `Makefile:4,8,10,61,64`. `TARGET`, `KERNEL` and `QEMU_X64` are unused, `QEMU_FLAGS` and related flags don't reach x64 runs, and `PROFILE=debug` produces the invalid `--debug`. **Fix:** route both architectures through the scripts, and use `--profile`.
- **BLD-4 · Medium · The x86_64 host triple is hard-coded.** `[code]` `scripts/docker-run.sh:47-50`, `Makefile:101-167,212,225`. On an arm64 host, such as Docker on Apple Silicon, `make docker-run` aborts before boot and `make test` fails. **Fix:** derive it from `rustc -vV`.
- **BLD-5 · Medium · Toolchain and image are unpinned.** `[code]` `rust-toolchain.toml:2`, `Dockerfile:3,23`. Builds aren't reproducible, and `rustup target add` may run against a different toolchain than the build uses. **Fix:** pin one version everywhere.
- **BLD-6 · Medium · Docker publishes unauthenticated VNC on all interfaces.** `[code]` `Makefile:272,298`. The comment says localhost, but `-p 5900:5900` binds everywhere. Anyone who can reach the host gets guest keyboard and display. **Fix:** `-p 127.0.0.1:5900:5900`, and drop it for GTK.
- **BLD-7 · Low · Games image tooling is fragile.** `[code]` `Makefile:174,183,188`. The symlink is sticky, so later kernels on both architectures embed 64 MiB until it is removed by hand, and `make -j` doesn't order it. An absolute `GAMES_IMAGE` dangles silently. Comments say 256 MiB and 128 MiB where the real values are 64 MiB and 512M. `.gitignore` misses `disk.img`.
- **BLD-8 · Low · `make bundles` fails in Docker.** `[code]` The image has no `python3`, and `scripts/pack-app.py` isn't executable despite its usage text.
- **BLD-9 · Low · Rootful Docker probably can't use GTK.** `[check]` `Makefile:249` grants X access to `$user`, while QEMU runs as root.

## Filesystem and data formats

All parsed input here is currently embedded at build time, so these severities assume the runtime loading planned in M4. The exceptions reachable today are FS-3 (a large embedded file, or a `MOUNT`), FS-10 and FS-12.

### Critical

- **FS-1 · YAML block scalars slice at a byte index.** `[test]` `libs/yaml/src/scalar.rs:223-224`, `libs/yaml/src/parser.rs:466-472`. **Reproduced:** `a: |\n     x\n    é\n` panics. The ASCII form silently corrupts: `a: |\n    four\n  xyz\n` decodes to `four\nz\n`. Reaches the kernel through bundle scanning and desktop config. **Fix:** end the block at a less-indented line, and strip only leading spaces.
- **FS-2 · Mounting can exhaust the heap.** `[code]` `libs/fs/src/fat32.rs:314,334-337`, `libs/fs/src/exfat.rs:273-276,306`, `libs/fs/src/tree.rs:14`. A self-looping directory chain re-adds entries until `Vec<Node>` growth passes the 64 MiB arena; `MAX_NODES` is 1M and is never reached. A legitimate volume with over about 262,000 entries hits this too. **Fix:** spec limits on directory size, cluster-bitset cycle detection, `try_reserve`, and a heap-based node budget.
- **FS-3 · On-disk sizes are trusted for allocation.** `[code]` `libs/dosbox/src/dos.rs:223,257`, `libs/coreutils/src/fs.rs:386`, fed by `libs/archive/src/zip.rs:212`, `libs/fs/src/exfat.rs:1691`, `libs/fs/src/fat32.rs:304`, `libs/fs/src/tree.rs:217`. A small ZIP declaring 4 GiB panics on `MOUNT`. So does a legitimate file larger than free heap, on `cat`. **Fix:** plausibility checks at parse time, and `try_reserve` at consumers.

### High

- **FS-4 · An entry set ending in the last slot of a directory's last cluster returns `Corrupt`.** `[test]` `libs/fs/src/fat32.rs:944-947`, `libs/fs/src/exfat.rs:1072-1078`. **Reproduced on FAT32:** with the test fixture, create four short-named files and remove the fourth; `remove` fails with `Corrupt` after the chain is freed and the entries are cleared, leaving the node alive. `rename` shares the path. On exFAT (`[code]`), `write` then fails at the checksum update after data has been written, leaving a set other systems reject. **Fix:** advance only when entries remain.
- **FS-5 · Broken chains read as success.** `[code]` `libs/fs/src/fat32.rs:236,368-374,398-401`, `libs/fs/src/exfat.rs:203,370-376,406-409`, `libs/fs/src/lib.rs:175`. `read` has no error channel, so a truncated or cyclic chain returns short or repeated data as `Ok`. **Fix:** `try_read -> Result`, and validate chain length against size.
- **FS-6 · No directory depth limit.** `[check]` `libs/fs/src/fat32.rs:325-331`, `libs/fs/src/exfat.rs:321-334`. Recursive walkers at `libs/coreutils/src/commands/files.rs:538,777,916,993` and `libs/coreutils/src/fs.rs:313,331` would overflow a stack on a few hundred nested directories. **Fix:** cap depth at mount, and make walkers depth-limited.

### Medium

- **FS-7 · exFAT ValidDataLength is never read.** `[code]` `libs/fs/src/exfat.rs:1691-1694,1476-1488`. A preallocated file reads stale cluster contents instead of zeros, which is a data leak, and `rename` then makes it permanent. **Fix:** track it and zero-fill.
- **FS-8 · exFAT integrity checks are missing, contrary to the module doc.** `[code]` `libs/fs/src/exfat.rs:3-7,1666-1714,596`. SetChecksum, NameHash and the upcase TableChecksum are never read.
- **FS-9 · `Text` equality and hash include stale bytes.** `[test]` `libs/text/src/lib.rs:26,61,95,101`. **Reproduced:** `set("abcd"); set("ab")` isn't equal to `from_str("ab")`. Affects `Manifest`, `Config` and `ServiceStatus`. **Fix:** compare over `as_str()`.
- **FS-10 · `init` doesn't check Running on start.** `[code]` `libs/init/src/lib.rs:343-357`. Starting a service twice spawns a second process and forgets the first PID. **Fix:** an `AlreadyRunning` error.
- **FS-11 · `init` config loses settings silently.** `[code]` `libs/init/src/lib.rs:171,179,192-200,285-289`, `libs/desktop/src/config.rs:293,449`. A 64-node arena against the desktop's 160 means a 65-160 node config silently resets `init:` to defaults. `restart: "always"` is ignored because quoted values return `None`. A 9th autostart entry is dropped, and long IDs are truncated. **Fix:** a shared arena, `as_str`, and errors for over-capacity input.
- **FS-12 · Restart budget and spawn failure logic.** `[code]` `libs/init/src/lib.rs:401-405,439,451,454`. `restarts` never resets, there is no backoff, and a transient `NoFreeSlot` marks a service permanently failed. **Fix:** reset on manual action and after a healthy run, add backoff, and retry transient errors.
- **FS-13 · Running out of space during a new chain leaks clusters.** `[code]` `libs/fs/src/fat32.rs:541-548`, `libs/fs/src/exfat.rs:853-860,898-901`. **Fix:** free the partial chain on error.
- **FS-14 · Node slots are never reused.** `[code]` `libs/fs/src/tree.rs:79-81,130-147`, `libs/fs/src/mount.rs:59,108`. A create and remove loop grows until the heap runs out. `node_count()` also returns the live count while IDs index all slots, so `MountTable` hides children of a volume mutated before mounting. **Fix:** a free list and a high-water mark.
- **FS-15 · An apostrophe in a YAML plain scalar opens a quote.** `[test]` `libs/yaml/src/parser.rs:761-762,801-802`. **Reproduced:** `a: don't panic # note` keeps the comment in the value. **Fix:** recognise quotes only at the start of a token.
- **FS-16 · YAML's advertised rejections don't happen.** `[code]` `libs/yaml/src/parser.rs:325-336,539-549,857-862`, `libs/yaml/src/scalar.rs:89-90`, `libs/yaml/src/lib.rs:6-7`. Anchors and tags are skipped silently, and `InvalidEscape` is never constructed. `name: "\q"` silently takes the field's default.
- **FS-17 · Valid YAML is silently misparsed.** `[code]` `libs/yaml/src/parser.rs:183-191,427`, `libs/yaml/src/scalar.rs:230-235`. `- - a` gives the string `"- a"`, indentation indicators are ignored, and more-indented folded lines are folded.
- **FS-18 · Common valid YAML is rejected.** `[test]` `libs/yaml/src/parser.rs:235-237,294,186,460-464,793-795`. **Reproduced:** `tags:\n- a\n` fails. By reading, it reports a misleading `MultipleDocuments`. `-   a: 1\n    b: 2`, a tab in a literal block and `- 'it''s: x'` also fail (`[code]`).
- **FS-19 · `& & & …` recurses without counting depth.** `[check]` `libs/yaml/src/parser.rs:325-336`. **Fix:** a loop.

### Low

- **FS-20 · Rename issues.** `[code]` `libs/fs/src/fat32.rs:1082,1092-1099`, `libs/fs/src/exfat.rs:1461,1471-1488`. A case-only rename returns `Ok` and does nothing. Rename also clears read-only, hidden and all timestamps.
- **FS-21 · exFAT boot and range validation.** `[code]` `libs/fs/src/exfat.rs:67,91-94,105,114,202`. Shift overflow at `boot[109]=0xFF`, NumberOfFats ignored in layout checks, 28-bit masks on 32-bit FAT entries, no fallback to the backup boot region, and unchecked start clusters.
- **FS-22 · FAT32 spec gaps.** `[code]` `libs/fs/src/fat32.rs:424,1241-1246,1296,1371`. BPB_ExtFlags is ignored, writes clear reserved FAT bits, the LFN sequence is unverified, and control characters survive in names, which allows terminal escape injection.
- **FS-23 · TAR size handling can desync the reader.** `[code]` `libs/archive/src/tar.rs:336-346`. `pax_size` applies to the next header regardless of type, and directories with non-zero size consume data blocks.
- **FS-24 · The YAML emitter corrupts values.** `[code]` `libs/yaml/src/emit.rs:92-94,97-109`. No in-tree callers today.
- **FS-25 · Bundle fields over 256 bytes are dropped, not truncated as documented.** `[code]` `libs/bundle/src/manifest.rs:122-125`. An oversized `entry` falls back to `id`, launching a different command.
- **FS-26 · JSON error offset is wrong.** `[code]` `libs/json/src/parser.rs:337-343`. `[nul]` reports 1 where 4 is correct.
- **FS-27 · Packing script issues.** `[code]` `scripts/pack-app.py:30,44,46`. Hidden directories get packed, as can a prior output inside the source, `compresslevel=9` is ignored with a `ZipInfo`, and `id: ../x` shapes the output path.

### Tests that need fixing

- `libs/compress/tests/compress.rs:103-105` flips ISIZE, not data, so CRC comparison has no negative test.
- `libs/fs/src/tests.rs:295-299` computes a non-upcased hash, and passes only because of FS-8.
- `libs/init/src/lib.rs:631` registers "missing" with a real app.
- FS fixtures have one sector per cluster, no upcase table and only 512-byte sectors, and no malformed-image tests exist.

## Shell, coreutils and terminal

### Critical

- **SH-1 · Signed division overflow panics.** `[boot]` `libs/sh/src/expand.rs:312,317`, `libs/coreutils/src/commands/system.rs:417,424`. **Reproduced:** `echo $(( (-9223372036854775807 - 1) / -1 ))` gives `[panic] attempt to divide with overflow`, and the shell is gone. **Fix:** `wrapping_div` and `wrapping_rem`, or `checked_*` with an error.
- **SH-2 · `grep -w` slices on non-char boundaries.** `[boot]` `libs/coreutils/src/commands/text.rs:936`. **Reproduced:** `printf '\303\251 x\n' | grep -w x` panics with `end byte index 1 is not a char boundary`. **Fix:** skip non-boundary offsets, or match on bytes. The search is also O(n²).
- **SH-3 · `nl -w` above 65535 panics in `core::fmt`.** `[test]` `libs/coreutils/src/commands/text.rs:1565,1569`. **Fix:** clamp the width.
- **SH-4 · Heap exhaustion from shell input.** `[boot]` `libs/coreutils/src/commands/text.rs:332-359,370`, `libs/sh/src/exec.rs:226-231`, `libs/coreutils/src/io.rs:132-134`, `libs/coreutils/src/fs.rs:386,427`. **Reproduced:** `printf '%99999999d' 1 | wc -c` gives `memory allocation of 33554432 bytes failed`. `x=a; while :; do x=$x$x; done`, `seq 1000000 | sort`, and `cat` on a large file should do the same (`[code]`). **Fix:** cap printf widths, add a shell-level byte budget with `try_reserve`, and add `FsError::NoSpace`.
- **SH-5 · Unbounded recursion with no guard.** `[boot]` `libs/sh/src/exec.rs:573-576,847`, `libs/sh/src/lib.rs:433-437`, `libs/sh/src/expand.rs:36,324-354`, `libs/sh/src/parser.rs:279-283,341-367`, `libs/coreutils/src/commands/system.rs:257-273`, `libs/coreutils/src/regex.rs:66`, `libs/coreutils/src/glob.rs:12-48`, tree walkers in `libs/coreutils/src/commands/files.rs`. **Reproduced:** self-sourcing kills the shell on a real boot (KRN-4), and `x='eval "$x"'; eval "$x"` overflows a 256 KiB thread on the host. **Fix:** a nesting limit of about 32 at each entry, and depth counters in the parser, arithmetic, regex and glob.

### High

- **SH-6 · Console stdin never reaches EOF.** `[boot]` `libs/sh/src/lib.rs:84-95`, `libs/coreutils/src/io.rs:20-30,33-45`. **Reproduced:** `cat` on the serial shell ignores Ctrl-D and Ctrl-C, and only a reset recovers it. `grep foo`, `sort`, `wc` and `tr a b` use the same path. **Fix:** a cooked line discipline with echo, CR to LF, Backspace, Ctrl-D as EOF and Ctrl-C as interrupt.
- **SH-7 · No cancellation, and the Terminal runs on the desktop thread.** `[code]` `apps/commands/src/terminal.rs:174,457`, `libs/sh/src/exec.rs:166-183`. `while :; do while :; do :; done; done` freezes the desktop. The inner loop stops at 1M iterations but the outer one restarts it, so this effectively never ends. **Fix:** run the shell on its own task, and poll a cancel flag.
- **SH-8 · Assignments get word-split and globbed.** `[boot]` `libs/sh/src/exec.rs:227`, `libs/sh/src/expand.rs:93-100`. **Reproduced:** `A="hello world"; B=$A; echo "[$B]"` prints `[hello]`. **Fix:** a no-split, no-glob expansion for assignments.
- **SH-9 · The status of `$(...)` is lost.** `[boot]` `libs/sh/src/expand.rs:175`, `libs/sh/src/exec.rs:237-238`. **Reproduced:** `x=$(false); echo $?` prints `0`, so `if out=$(cmd)` takes the wrong branch.
- **SH-10 · `$n` inside arithmetic evaluates to 0, and junk is ignored.** `[boot]` `libs/sh/src/expand.rs:386,255-262`. **Reproduced:** `n=5; echo "$(($n + 1))"` prints `0`, and `$((3 > 5))` prints `3`. **Fix:** expand first, return `Result`, and report unconsumed input as an error.
- **SH-11 · `( ... )` never parses.** `[boot]` `libs/sh/src/parser.rs:285-290`, `libs/sh/src/lexer.rs:227-234,315-320`. **Reproduced:** `(echo sub)` gives `syntax error: expected a command`. The parser tests for a word `"("`, but the lexer emits `Token::LParen`. Documented as supported. **Fix:** match the token, and add a `Subshell` node that restores state.
- **SH-12 · `exit` in `sh -c` or `sh script` kills the outer shell.** `[boot]` `libs/sh/src/exec.rs:517-524,845-854`. **Reproduced:** `sh -c "exit 3"; echo still` never prints `still`, and the serial shell restarts its prompt. In the Terminal window (`[code]`) this closes the window and loses its overlay.
- **SH-13 · `wc` prints `-` as a filename on stdin.** `[boot]` `libs/coreutils/src/commands/text.rs:582-590`. **Reproduced:** `echo a | wc -l` prints `1 -`. This breaks a docs example and any `test $(... | wc -l) -eq N`.
- **SH-14 · `/dev/null` and `/tmp` don't exist at boot.** `[boot]` `libs/sh/src/exec.rs:433-443`, `libs/sh/src/lib.rs:169-183`. **Reproduced:** `ls nope 2>/dev/null` prints `sh: sh: /dev/null: No such file or directory`. `if cmd >/dev/null; then` is always false, and a docs example writing to `/tmp` fails on a fresh boot. **Fix:** special-case `/dev/*` in redirection, and create `/tmp`.
- **SH-15 · `basename` and `dirname` are advertised but not implemented.** `[boot]` `libs/coreutils/src/commands/mod.rs:167-178,368-422`. **Reproduced:** `basename /a/b` gives `command not found`, yet both appear in `help` and Tab completion. **Fix:** implement them, and test that every `COMMANDS` name dispatches.
- **SH-16 · Quoted and escaped glob characters still glob.** `[test]` `libs/sh/src/expand.rs:50-73,80`. **Reproduced:** `touch fooa foob; rm foo\*` deletes both files. This is a data-loss bug. **Fix:** track quoting per character.
- **SH-17 · Interactive multi-line entry is broken.** `[code]` `libs/sh/src/editor.rs:70`, `apps/commands/src/terminal.rs:153-189`. `for i in 1 2; do` followed by Enter is an immediate syntax error, and an unterminated quote misbehaves. The window has no continuation at all. **Fix:** a shared `needs_more(full_text)` and `ParseError.incomplete`.
- **SH-18 · Positional parameters don't work.** `[test]` `libs/sh/src/expand.rs:102-110`. **Reproduced:** `echo 'echo $1' > /s; sh /s hi` prints an empty line. `$#` is hard-coded to 0, and `$0` is always `sh`. Scripts can't take input, though the docs list these as supported.

### Medium

- **SH-19 · `${...}` defaults aren't expanded, and unsupported forms silently give an empty string.** `[test]` `libs/sh/src/expand.rs:121-153`. **Reproduced:** `foo=a; echo ${X:-$foo}` prints `$foo`. By reading, `${x%y}`, `${x#y}` and `${x:1:2}` silently give an empty string, and `${X:?m}` doesn't abort.
- **SH-20 · `$(...)` runs in the parent shell's state.** `[code]` `libs/sh/src/expand.rs:156-186`. `echo $(cd /docs; pwd); pwd` prints `/docs` twice. **Fix:** snapshot and restore env, cwd and aliases.
- **SH-21 · Control-flow state leaks.** `[code]` `libs/sh/src/exec.rs:41-53,530-532,545,558`, `libs/sh/src/lib.rs:187-218,516`. `return` at the prompt stays set, so every later line runs only its first list until the next `source`. `break 5` has the same effect, and `break && echo x` prints `x`. **Fix:** clamp levels, reset flags at top-level entry, and stop lists when a flag is set.
- **SH-22 · `tail -n +N` and `head -n -N` are silently wrong.** `[test]` `libs/coreutils/src/commands/text.rs:409,422,430`. **Reproduced:** `tail -n +2` on four lines prints the last two. `head -n abc` prints the first ten (`[code]`).
- **SH-23 · `sort` keys are wrong, and missing files report success.** `[code]` `libs/coreutils/src/commands/text.rs:985-990,1014-1017,1054-1064,2055-2064`. `sort -k 2 f` compares field 2 only, `-k 2,3` ignores the 3, `-k2` and `-t,` are rejected, and `sort /nope; echo $?` prints `0`.
- **SH-24 · POSIX divergences.** `[test]` for the first four, `[code]` for the rest.
  - `echo a,b | cut -d, -f1` is rejected (`libs/coreutils/src/commands/text.rs:1186-1210,1247-1266`).
  - `tr '[:lower:]' '[:upper:]'` turns `hello` into `heuup` (`:1372-1388`).
  - `seq 3 -1 1` fails with an invalid option (`:1406,1490`).
  - `expr 2 + 3 '*' 4` gives `20`, and a test at `libs/coreutils/src/commands/mod.rs:682` enshrines it. `:` is missing and `&`/`|` are bitwise (`libs/coreutils/src/commands/system.rs:384-444`).
  - `[ -s missing ]` returns 2 and `[ abc -eq 0 ]` is true (`system.rs:283-287,330-331`).
  - `grep -E 'a|b'` treats `|` as a literal, and `[[:digit:]]` matches garbage (`text.rs:757`, `libs/coreutils/src/regex.rs:125-130,199-225`).
- **SH-25 · External command status is always 0.** `[code]` `apps/shell/src/lib.rs:101,128-129`. `kill 99 && echo ok` prints `ok`. `args.join(" ")` also loses quoting. **Fix:** `App::run -> i32`, and pass argv.
- **SH-26 · Aliases run only the first simple command.** `[code]` `libs/sh/src/exec.rs:269-277`. `alias x='cd /docs && ls'` only runs `cd`. **Fix:** expand at the token level.
- **SH-27 · Terminal window display and keyboard.** `[code]` `apps/commands/src/terminal.rs`:
  - Esc closes the window and destroys history and overlay (`:502`).
  - The banner says Ctrl-D closes, but Ctrl-D over serial logs off the whole desktop (`:146`, `libs/keys/src/lib.rs:159`).
  - Lines are clipped with no wrap, and input past the edge is invisible (`:564-575,601`).
  - All stderr prints after all stdout (`:177-178`).
  - `\r` is dropped, OSC sequences leak, tabs are always 4 spaces, and colours are discarded (`:200-219`).
  - `ESC[2J` anywhere clears text printed before it (`:192-194`).
- **SH-28 · Editor redraw breaks on long lines and multi-line prompts.** `[code]` `libs/sh/src/editor.rs:266-269`, `libs/sh/src/lib.rs:279`.
- **SH-29 · Serial paste loses characters past about 32 per frame.** `[code]` `libs/keys/src/lib.rs:76,80`, `libs/gui/src/input.rs:8,173`.
- **SH-30 · `mv dir existing_file` destroys the file.** `[test]` `libs/coreutils/src/commands/files.rs:651-662`. **Reproduced:** `mkdir /d; echo x > /f; mv /d /f; cat /f` prints nothing. **Fix:** return `NotDir` before `remove_file`.

### Low (all `[code]`)

- **SH-31 · Glob.** `echo */` includes files, so `rm -r */` deletes them too (`libs/sh/src/expand.rs:196,240-243`). `d/../*` lists `/` (`expand.rs:209`, `libs/coreutils/src/fs.rs:173-176`). `a[!b]*` wrongly matches `a` (`libs/coreutils/src/glob.rs:35-40`).
- **SH-32 · Redirection.** `cd /docs > log` writes `log` into `/docs`, a bad target doesn't stop the command, `2>&1 > f` order is ignored, and `> $EMPTY` is silently dropped (`libs/sh/src/exec.rs:287-443,864`).
- **SH-33 · Shell misc.** `foo | cat` gives status 1 instead of 127 (`exec.rs:476-482`). `echo 'echo hi' | sh` starts the interactive editor (`exec.rs:847`). `FOO=1 echo $FOO` prints `1` (`exec.rs:226-234`). `~foo` becomes `/foo` (`libs/sh/src/lexer.rs:333-340`). `?` appears in `env` (`exec.rs:26,40,52`). `builtin_names` omits `sh`, `true` and `:` (`libs/sh/src/lib.rs:493-498`).
- **SH-34 · Keys.** `ESC[11~` to `ESC[19~` map to Home (`libs/sh/src/editor.rs:319-322`). A lone Esc swallows the next key (`editor.rs:297-301`). Ctrl-R can't cycle (`editor.rs:221-252`). xterm's F1 to F4 are dropped, and releasing one of two held modifiers clears the state (`libs/keys/src/lib.rs:240-272,337-348`).
- **SH-35 · Paths.** `touch new/` and `cat file/` succeed (`libs/coreutils/src/fs.rs:147-155`). The cwd dangles after `rm -r .`.

## Desktop, GUI and GUI apps

No panic was found on shipped paths at the shipped 1024x768 resolution, though the only `Ui` users are the desktop and the one-frame `gui` app. Release builds wrap on overflow, so dev builds behave differently.

### Critical

- **UI-1 · Enter in any focused app also launches the wallpaper selection.** `[test]` `libs/desktop/src/lib.rs:1069-1073,1194-1197`. **Reproduced by test:** with a focused windowed app, `Key::Enter` produces a launch request. So typing `ls` then Enter in Terminal opens Console Help and takes focus away. **Fix:** require `!window_owns_keyboard && !desktop_overlay`.

### High

- **UI-2 · End Process fails for a running process.** `[boot]` Same cause as KRN-3. Shown at `apps/commands/src/taskmgr.rs:1126-1136`.
- **UI-3 · The supervisor records Stopped even when `kill` failed.** `[code]` `libs/init/src/lib.rs:386-397,401-405`. An unsupervised orphan keeps running, and `restart` creates a second instance. **Fix:** keep the pid until the process has gone.
- **UI-4 · `desktop list|status|menu|run|service` from a spawned process spins forever.** `[code]` `apps/desktop/src/lib.rs:85`, `apps/desktop/src/session.rs:62`. The session lock is held for the whole session. Start > Run `desktop list` burns a CPU that can't be killed (KRN-3). **Fix:** `try_lock`, and print "session busy".
- **UI-5 · Three of the 8 Start-menu apps show nothing in the GUI.** `[code]` `libs/desktop/src/lib.rs:673-685,2303-2307`, `kernel/src/sched.rs:336`, `apps/commands/src/splash.rs:28`, `apps/desktop/src/lib.rs:307-309,386-395`. System Information, Console Help and Splash Screen write only to serial. Splash prints "no framebuffer; restart QEMU with -device ramfb". Run `ps` shows no output. **Fix:** a console-capture view.
- **UI-6 · Killing a preempted process can freeze the GUI.** `[check]` Same cause as KRN-2. The timing on the print and heap locks needs a runtime check.

### Medium

- **UI-7 · Ctrl+Esc never opens the Start menu.** `[code]` `libs/desktop/src/lib.rs:486-491,1068,1236-1238`. It opens and closes in the same frame. With an app focused, there is then no keyboard route back to the desktop. **Fix:** exclude CTRL from `escape`.
- **UI-8 · The Start menu and blank taskbar click through.** `[code]` `libs/desktop/src/start_menu.rs:143-621`, `libs/desktop/src/lib.rs:906-947`, `libs/gui/src/ui.rs:435-455`. Double-clicking blank menu space can launch the icon beneath it, and one click can close a window under the panel. **Fix:** blockers now, occlusion-aware hit testing later.
- **UI-9 · Bare Esc and `q` close apps and discard state.** `[code]` `apps/commands/src/terminal.rs:502`, `apps/commands/src/taskmgr.rs:2322-2325`, `apps/commands/src/files/explorer.rs:2258-2259,2312-2313`. **Fix:** close through window chrome or Alt+F4.
- **UI-10 · Explorer wheel scrolling is undone in the same frame.** `[code]` `apps/commands/src/files/explorer.rs:1949-1967,2010-2011,3242`. **Fix:** re-pin only when the cursor moved.
- **UI-11 · Explorer shows stale search results.** `[code]` `apps/commands/src/files/explorer.rs:750-763,1131-1159,2414-2418`.
- **UI-12 · Task Manager controls that do nothing.** `[code]` `apps/commands/src/taskmgr.rs:593-595,2096-2107,2225-2230,2591-2593,2684-2686,2692`. Switch To only closes Task Manager, Log Off only closes it, Always On Top and similar options are never read, and End Process Tree is End Process.
- **UI-13 · Task Manager figures mislead.** `[code]` `kernel/src/main.rs:36-49`, `kernel/src/sched.rs:455`, `apps/commands/src/taskmgr.rs:43,198,3107-3113,3845,3887-3897,4615`. "Physical Memory" is image size, "System Cache" is free heap, Mem Usage is a constant 128 KiB, Handles is 0, hosted apps' CPU goes to PID 1, and "60 seconds" is 120 s. CPU percentage maths is correct.
- **UI-14 · Run dialog bypasses windowed apps.** `[code]` `apps/desktop/src/lib.rs:361-384`, `apps/commands/src/taskmgr.rs:1604,1158-1169`, `apps/commands/src/files/explorer.rs:3301`. Run `taskmgr` takes a blocking foreground path with no window, and the virtio keyboard goes dead. **Fix:** resolve to a bundle first, and prefer windowed.
- **UI-15 · Dragging a hosted window off-screen reflows it instead of clipping it.** `[code]` `libs/desktop/src/lib.rs:2186-2190`, `libs/desktop/src/window.rs:388-393`. The canvas shrinks to the visible part, as small as 47x39.
- **UI-16 · `TextBuffer` keeps a stale anchor.** `[test]` `libs/gui/src/text.rs:129-142,222-232`, `libs/gui/src/widgets/text_input.rs:184-195`. **Reproduced:** `abc`, Shift+Left, Shift+Right, Backspace panics at `text.rs:229`. Latent today, because no interactive app uses the widget. It becomes critical as soon as one does, for example a settings app. **Fix:** clear the anchor on an empty selection.
- **UI-17 · Windowed apps as autostart services do nothing.** `[code]` `docs/desktop.md:147-148`, `libs/desktop/src/lib.rs:727-739`, `apps/commands/src/terminal.rs:380`. The docs' own sample fails three restarts and no window opens.
- **UI-18 · Closing a window leaks its process.** `[code]` `libs/desktop/src/window.rs:292-304`, `libs/desktop/src/lib.rs:2152-2157`. Rare today, because shipped bundles are short-lived. Needs KRN-3 first.
- **UI-19 · Start-menu commands wired to stubs.** `[code]` `apps/desktop/src/lib.rs:300-316`, `libs/desktop/src/lib.rs:1155,2303-2307`. My Computer, My Documents and Search all just open `files`. Help opens Terminal, Settings cycles the theme, and Refresh does nothing.
- **UI-20 · Input queue overflow.** `[code]` Character loss at `libs/gui/src/input.rs:168-171`, `apps/desktop/src/session.rs:85-97`. A dropped `PointerUp` leaving the button stuck is `[check]`. **Fix:** budget keyboard input, and always apply pointer state.

### Low (all `[code]`, except where noted)

- **UI-21 · Window manager details.**
  - Tab can focus a minimized window, which leaves desktop keys dead (`libs/desktop/src/window.rs:274-289`, `libs/desktop/src/lib.rs:849-852`).
  - The top-right 8x9 px of the close button starts a resize (`lib.rs:2101,2406-2409`).
  - Show Desktop's second click restores windows the user had minimized (`lib.rs:439-449`).
  - Pressing a top window over a lower one raises the lower one too (`lib.rs:2042-2044,2174-2176`).
  - Drag then quick re-press counts as a double click (`lib.rs:2063-2068`).
  - Right-click on the wallpaper can open a menu during a modal dialog (`lib.rs:988-997`).
- **UI-22 · Latent `libs/gui` panics.** `annotated_scrollbar` on `clamp(18.0, h)` when `0 < h < 18` (`libs/gui/src/components/patterns.rs:1498`). `Ui::window` clamp on narrow canvases (`libs/gui/src/widgets/containers.rs:249-252`). A push past depth 16 is ignored, but `pop` still pops (`libs/gui/src/layout.rs:200-232`).
- **UI-23 · Explorer details.** View > Sort doesn't re-sort until a refresh (`apps/commands/src/files/explorer.rs:1167-1169,1217-1224,1768`). `Alt+F4` is shown but never handled (`:239`). Sort has no effect on search results (`:795-803`). 256 entry and 128 search caps truncate silently (`:28-29`). Selection bitsets stop at NodeId 1024 (`:32,2436-2454`; `[check]`). The read-only notice is wrong for mounted volumes (`:1005`). "Modified" is always `-` (`:4373`).
- **UI-24 · Session details.** `Ctrl-\` and `Ctrl-D` work only from serial (`libs/keys/src/lib.rs:166`). `desktop` restarted from the shell has no back buffer, so frames can tear (`apps/desktop/src/lib.rs:78-80`). Window-slot and spawn failures report only on serial (`libs/desktop/src/lib.rs:1201`, `apps/desktop/src/lib.rs:279-281`). All Programs shows about 8 rows with no scrolling (`libs/desktop/src/start_menu.rs:287-289`).

## Media stack and player

A hostile file in `/media` needs no click, because `PlayerState::new` auto-opens the first playlist entry (`apps/commands/src/player.rs:531,839-841`). `feos-audio-codec` isn't linked into the kernel at all, so its bugs are latent until audio playback exists.

### Critical

- **MED-1 · Unbounded AMF recursion in FLV.** `[test]` `libs/video/src/flv.rs:173,214,219`, also `:183,225`. **Reproduced:** `info()` on a script tag with repeated `00 00 03` overflows a 128 KiB stack on the host. **Fix:** a depth limit of about 16.
- **MED-2 · Unchecked `data[header.payload]` in MP4.** `[test]` `libs/video/src/mp4.rs:125`. **Reproduced:** a valid MP4 with an empty `tkhd` as the last bytes panics in `info()`. **Fix:** check the bound first.
- **MED-3 · Off-by-one in `sps_from_avcc`.** `[test]` `libs/video/src/h264.rs:129,136`. **Reproduced:** a 7-byte FLV AVC sequence header panics in `info()`. **Fix:** `config.len() < 8`.
- **MED-4 · Picture allocation is sized by the SPS, up to 16384x16384.** `[code]` `libs/h264/src/decoder.rs:611-655`. Nothing ties it to container dimensions or `MAX_FRAME_BYTES`, and about 4096x4096 exhausts the heap. **Fix:** `Decoder::with_limits`, `try_reserve_exact`, and `DecodeError::OutOfMemory`.
- **MED-5 · Unbounded RPLM and MMCO vectors.** `[code]` `libs/h264/src/slice.rs:203-224,267-299`. About 1 MiB of `0xFF` exhausts the heap. **Fix:** cap at spec limits.
- **MED-6 · Parameter-set allocations are unbounded.** `[code]` `libs/h264/src/sps.rs:218,293-296`, `libs/h264/src/decoder.rs:193,205`, `libs/h264/src/pps.rs:39`. **Fix:** reject cycle > 255, `sps_id` > 31, `pps_id` > 255.
- **MED-7 · `parse_annex_b` builds the whole list inside `info()`.** `[code]` `libs/h264/src/nal.rs:97-136,350`. A file of about 3.7 MiB can exhaust the heap on open. **Fix:** an iterator API or `MAX_NALS`.
- **MED-8 · MBAFF geometry assumes an even MB height.** `[code]` `libs/h264/src/decoder.rs:614-615,853-859`, `libs/h264/src/slice_context.rs:371`. A crafted SPS then a P slice indexes past the plane. **Fix:** derive geometry from SPS units, and reject odd MBAFF heights.

### High

- **MED-9 · Every displayed frame restarts decoding from frame 0.** `[code]` `apps/commands/src/player.rs:744,2453`, `libs/video/src/lib.rs:335`, `libs/video/src/mp4.rs:391`, `libs/video/src/h264.rs:194`, `libs/video/src/matroska.rs:427`, `libs/video/src/avi.rs:225`, `libs/video/src/flv.rs:342`, `libs/video/src/gif.rs:394`. H.264 and GIF cost is O(n²) overall, and it runs on the compositor thread, so the whole desktop freezes during decode. The drop logic makes it worse, because each jump is another full re-decode. **Fix:** a stateful API and a worker thread.
- **MED-10 · Wrong frames for any MP4 with B-frames.** `[code]` `libs/video/src/mp4.rs:338-351,400`. The fixtures use `bframes=0`, so tests can't see it. **Fix:** keep decoding until the reorder output reaches the target, as MKV, AVI and FLV do.
- **MED-11 · `sps_from_avcc` passes the NAL header and emulation-prevention bytes to the SPS parser.** `[code]` `libs/video/src/h264.rs:140-145`. The fixture parses correctly only by accident. Real FLVs get wrong dimensions. **Fix:** reuse `parse_avcc_config`.
- **MED-12 · Output stride comes from the codec, reported size from the container.** `[code]` `libs/video/src/lib.rs:359-363`, `libs/video/src/h264.rs:174-176`, `libs/video/src/mjpeg.rs:84`. Rotated phone video and anamorphic video scramble, or stop with `BufferTooSmall`. **Fix:** return decoded dimensions, and treat scaling and rotation as display metadata.
- **MED-13 · Unsupported H.264 features aren't rejected.** `[code]` `libs/h264/src/decoder.rs:476-484,876`. High10, 4:2:2, 4:4:4, monochrome and MBAFF CABAC produce silent garbage, contradicting "Unsupported H.264 features return an error". **Fix:** reject in `parse_sps` and `decode_slice`.
- **MED-14 · AIFF `COMM` range is guarded only by the declared size.** `[code]` `libs/audio-codec/src/aiff.rs:90,99`. Panics on a truncated file. Not reachable until audio is wired in, then critical. **Fix:** `need(input.len(), start, 22)`.

### Medium

- **MED-15 · POC types 1 and 2 ignore FrameNumOffset.** `[code]` `libs/h264/src/dpb.rs:362-370,400-408`. The visible effect is `[check]`: frames out of order when `frame_num` wraps without an IDR.
- **MED-16 · Per-frame timing is dropped.** `[code]` `apps/commands/src/player.rs:681-693,754`, `libs/video/src/lib.rs:231,297,308`. `frame_delay` is stored and never read, and a GIF played after a 30 fps MP4 runs at 30 fps. **Fix:** reset fps on open, and drive the clock from per-frame delay.
- **MED-17 · Restoring a minimized player jumps forward.** `[code]` `apps/commands/src/player.rs:813`, `libs/desktop/src/window.rs:498`, `libs/desktop/src/lib.rs:1999`. **Fix:** cap elapsed time per tick, or pause on hide.
- **MED-18 · Unknown-size Matroska clusters extend to the segment end.** `[code]` `libs/video/src/matroska.rs:91-93,347-349`. Only the first cluster of a live-muxed file plays, and a truncated last cluster refuses the whole file.
- **MED-19 · Raw AVI conversions are wrong.** `[code]` No row padding for widths not divisible by 4 (`libs/video/src/pixel.rs:165-170`). 8-bit palette decodes as gray (`libs/video/src/avi.rs:77`). Positive `biHeight` flips YUV (`avi.rs:94-99,278`). YV12 swaps U and V (`libs/video/src/codec.rs:182`). `V_UNCOMPRESSED` is always BGR (`codec.rs:238`).
- **MED-20 · RLE delta frames decode only the target sample.** `[code]` `libs/video/src/avi.rs:275-276`, `libs/video/src/rle.rs`. Artefacts after any seek or dropped frame.
- **MED-21 · Any decode error stops playback permanently.** `[code]` `apps/commands/src/player.rs:768-773`, `libs/video/src/h264.rs:197,258,314`, `libs/video/src/avi.rs:194,274`. A zero-length AVI null frame triggers it. **Fix:** repeat the previous frame, or skip to the next IDR.
- **MED-22 · H.264 crop handling is wrong.** `[code]` `libs/h264/src/decoder.rs:391-405,614-615`. Left and top crops show the wrong region, and crops of 16 px or more decode with the wrong width.
- **MED-23 · H.264 correctness gaps.** `[check]` The default P list is sorted by POC, not PicNum (`libs/h264/src/dpb.rs:131`). MMCO picNum has no wrap handling (`libs/h264/src/decoder.rs:341,350`). `second_chroma_qp_index_offset` is unused (`libs/h264/src/pps.rs:105`). Deblock parameters come from the first slice only (`decoder.rs:101-105`), and `idc=2` is treated as 0 (`libs/h264/src/deblock.rs:155`).

### Low (all `[code]`)

- **MED-24 · JPEG and BMP.** Grayscale with sampling other than 1x1 is garbled (`libs/image/src/jpeg/mod.rs:612-616`). `find_eoi` ignores segment lengths, so an EXIF thumbnail truncates a frame (`libs/video/src/mjpeg.rs:45-61`). Trailing partial frames over-count (`mjpeg.rs:63-74`). 32-bit bitfields ignore masks (`libs/image/src/bmp.rs:253`).
- **MED-25 · Player UI.** Playlist context "Play" toggles the current file instead of opening the clicked one (`apps/commands/src/player.rs:384`), and "Media Information" shows the current file (`:385`). Double-click pauses and goes fullscreen (`:1566-1572`). Transport keys are swallowed while the playlist has focus (`:1547,1897`). Play on a failed file ticks over black (`:857-875`).
- **MED-26 · GIF.** A missing EOI or trailer fails the file, frames larger than the canvas fail, disposal 2 paints opaque background, and `copy_from_slice` panics on an oversized scratch with disposal 3 (`libs/video/src/gif.rs:81,204,449,467,493`).
- **MED-27 · Other containers and codecs.** EBML lacing reads one size too many (`libs/video/src/matroska.rs:310-319`). A short FLV video tag rejects the file (`libs/video/src/flv.rs:26,33`). µ-law polarity is inverted (`libs/audio-codec/src/pcm.rs:50-54`). MP3 Layer I and II `decode` returns `Ok` with 0 frames while `info` errors (`libs/audio-codec/src/mp3_decode.rs`). Short zlib output is accepted as PNG pixels (`libs/image/src/png.rs:340-343`).
- **MED-28 · `pending.clone()` deep-copies about 1 KiB per MB for each continuation slice.** `libs/h264/src/decoder.rs:236-240`.
- **MED-29 · `more_rbsp_data` rescans trailing zeros for every MB.** `libs/h264/src/bitstream.rs:136-139`. Megabytes of trailing zeros become a hang on the compositor thread. **Fix:** cache `last_nz`.
- **MED-30 · The player calls itself "VLC media player".** `apps/commands/src/player.rs:2664,2666,3492,3733`. VLC is a third-party name. See [Decisions](#decisions-for-the-project-owner).

### Claims against reality

| Claim | Reality |
|---|---|
| `feos-audio-codec` decodes WAVE, AIFF, AU, MP3, Ogg | Library only. No kernel crate links it, and the player excludes audio extensions (`player.rs:4099`), so the OS can't play a WAV or MP3 |
| Ogg | Opus headers only. Vorbis isn't detected, and FLAC is absent |
| `feos-image` formats | Reachable only through MJPEG and bundle icons. No viewer exists |
| JPEG "extended-sequential, arbitrary subsampling" | Single interleaved scan only, `h*v>4` rejected, Adobe RGB read as YCbCr, no default Huffman tables, so DHT-less MJPEG fails |
| H.264 Baseline, Main, High | The only fixtures are CAVLC Baseline without B-frames, so Main and High are untested in this tree |
| "NEON kernels are the only unsafe" | `once.rs` also uses `unsafe` |
| Fullscreen | Fills only the window canvas when embedded |
| Undocumented limits | 4 MiB file, 2 MiB RGBA (720p is rejected, with a misleading "video heap" message), 64 playlist entries, 128 in the Open dialog |
| Provenance | Licence files exist, but `UPSTREAM.md` pins no commit and lists no patches. The MP3 port has SPDX headers only |

## FEOS-DOS

Paths in this section are relative to `libs/dosbox/src/` unless given in full.

### Critical

- **DOS-1 · `INT` never goes through the IVT and never pops its frame.** `[test]` `cpu.rs:438-461`, `machine.rs:153-233,817-822`. `software_interrupt` pushes FLAGS, CS and IP, calls the Rust service, and returns. **Reproduced:** a `.COM` with `CALL F … F: MOV AH,30h; INT 21h; RET` never returns from the subroutine. Each `INT` leaks 6 bytes and leaves IF=0. Hooked vectors never run. **Impact:** real DOS programs can't work. `HELLO.COM` and `BOX.COM` pass only because they have no `CALL`/`RET`. This is the likely cause of Wolfenstein 3D's early exit, which wasn't run to confirm. **Fix:** dispatch through the IVT, seeded with F000 stubs of `<callback> n; IRET`, with the callback patching CF and ZF into the stacked FLAGS.
- **DOS-2 · A guest can panic the kernel through mouse ranges.** `[test]` `mouse.rs:87-88,92-93,140-141,167-174`. **Reproduced:** `MOV AX,7; MOV CX,100; MOV DX,50; INT 33h; MOV AX,4; INT 33h` panics in `clamp`. **Fix:** swap inverted ranges, as mouse drivers do.
- **DOS-3 · A guest can exhaust the heap through the RAM drive.** `[code]` `dos.rs:763-776`, `ramdisk.rs:204-205,247-252`. Seek to 2 GiB and write one byte: `resize(0x7FFFFFFF)` exceeds the arena. **Fix:** a drive quota, `try_reserve`, and freeing data on last close.
- **DOS-4 · Renaming a directory into itself creates a cycle.** `[test]` `ramdisk.rs:135-144,209-226`, `dos.rs:912-933`. **Reproduced:** `rename(a, a, "B")` succeeds. By reading, `path()` then pushes forever until allocation fails. **Fix:** reject targets that are the node or a descendant.
- **DOS-5 · The prefix loop is unbounded, so the desktop hangs.** `[test]` `cpu.rs:388-399`. **Reproduced:** filling the segment with `2Eh` prefix bytes means `frame()` never returns. It runs on the desktop thread, so Close can't be reached. **Fix:** cap at 15 bytes, then raise INT 6.
- **DOS-6 · Licence claims are contradicted by the code (legal advice recommended).** Confirmed by reading.
  - `opl3/core.rs:1-17` and `opl3/mod.rs:1-30` carry LGPL-2.1-or-later notices and "Tony Gies (Rust port)".
  - `core.rs:273,490,495` and `mod.rs:456,659,665` carry `rust-dos:` markers.
  - `fpu.rs:3-5` says the F80 type comes from rust-dos, and `f80.rs:26` cites mwemu.
  - `docs/dosbox.md:159-164` says the crate is original, that rust-dos is GPL-2.0-or-later and "cannot be copied", and that only interface knowledge was borrowed. The crate declares `MIT OR Apache-2.0`.
  - Commit `c605128` says "we have ported dos-box". The only licence files in the repo are under `libs/h264/docs/`.
  - **Impact:** if the rust-dos reading is right, the distributed LTO kernel may be a combined GPL work, and the MIT/Apache claim fails for it. LGPL static linking also has its own obligations.
  - **Fix:** see [Decisions](#decisions-for-the-project-owner). Check mwemu's licence, and compare `cpu.rs`, `dos.rs` and `vga.rs` against rust-dos, which isn't on this machine.
- **DOS-7 · Game redistribution rests on one website's label.** Confirmed by reading `data/games/README.md:8-11`. 13 ZIPs and `dos.img` are tracked in git and can be linked into the kernel, including DOOM, Quake, Duke Nukem 3D and demos of Warcraft II and SimCity 2000. Each title's own licence inside its ZIP hasn't been reviewed. **Fix:** review each licence, or replace them with a download script.

### High

- **DOS-8 · Guest work isn't counted against the budget.** `[code]` `machine.rs:777`, `cpu.rs:1582-1659`, `apps/dosbox/src/lib.rs:97`. `L: MOV CX,FFFFh; REP MOVSW; JMP L` does about 1.09e9 word moves per 50,000-"instruction" frame, and `INT 10h` mode sets and big writes are also uncounted. **Fix:** resumable REP, service costs, and the VM on its own task.
- **DOS-9 · A crafted `C:\STATE.BIN` panics the kernel.** `[code]` `machine.rs:1050-1065,1148-1160`, `vga.rs:769,885-905`. **Fix:** version the format and validate every field.
- **DOS-10 · `MOUNT` can exhaust the heap.** `[code]` `dos.rs:189,223,257`, `ramdisk.rs:318`. Mounting several large titles in a session approaches the 64 MiB arena, and a ZIP's declared size is trusted. **Fix:** `try_reserve`, an error from `MOUNT`, and an `UNMOUNT` command.
- **DOS-11 · `INC` and `DEC` never update OF or AF.** `[test]` `cpu.rs:879-890,1504-1514,1518-1527`. **Reproduced:** `MOV AX,7FFFh; INC AX; JO x` isn't taken. **Fix:** compute OF and AF, and preserve CF.
- **DOS-12 · Rotates clobber flags and compute some wrongly.** `[code]` `cpu.rs:565-597,619-634`. `ROL`, `ROR`, `RCL` and `RCR` overwrite SF, ZF and PF. `ROR 1` always gives OF=0.
- **DOS-13 · `XLAT` ignores segment overrides.** `[code]` `cpu.rs:1264-1268`.
- **DOS-14 · The memory allocator doesn't model the program block.** `[code]` `dos.rs:30,367-418,850-870`, `machine.rs:655,738-746`. `AH=4Ah` on the PSP always fails, a failed `AH=48h` leaves BX unchanged, and `min_alloc` and `max_alloc` are ignored. **Fix:** a real MCB chain.
- **DOS-15 · The PSP command tail is off by one.** `[code]` `machine.rs:687-692`. Programs scanning from `81h` see no arguments.
- **DOS-16 · FPU operations are decoded wrongly.** `[code]` `fpu.rs:155-164,294-302,342`. `D8 /4` (FSUB) computes a divide, `FST` and `FSTP` register forms copy ST(i) to itself, and `FCOMPP` pops once.
- **DOS-17 · FPU rounding control is ignored.** `[code]` `fpu.rs:417,504,638-647`, `f80.rs:392,630`. FIST, FRNDINT and FBSTP always truncate. FRNDINT turns values of 2^63 and above, Inf and NaN into 0.
- **DOS-18 · The planar graphics controller is wrong.** `[code]` `vga.rs:537-568,804-814`. GC6 overwrites `bit_mask`, the logic op decodes the wrong bits, there are no latches, and write and read modes are ignored.
- **DOS-19 · BIOS and port palette access is wrong.** `[code]` `bios.rs:402-444`, `machine.rs:89-96,145`, `vga.rs:839-852`. `AX=1012h` reads the wrong registers, `1015h` is missing, port `3C7` is ignored, and reading `3DA` doesn't reset the attribute flip-flop.
- **DOS-20 · OPL is undetectable.** `[code]` `opl.rs:46-53`, `opl3/mod.rs:216-230`. Timers never tick, so the standard AdLib check fails and games choose no AdLib. **Fix:** tick timers from guest time.
- **DOS-21 · `AH=0Ch` loses every key.** `[code]` `dos.rs:544-563`. `Blocked` retries the whole call, and each retry clears the keyboard.

### Medium (all `[code]`, except where noted)

- **DOS-22 · CPU identity is inconsistent, and 186 support is incomplete.** `cpu.rs:891-894,1095,1240,1561`, `machine.rs:782-813`. `PUSH SP` acts like a 286, FLAGS like an 8086, shifts like a 186. `BOUND` and `INS`/`OUTS` are missing, despite claims of a full set. Undefined opcodes kill the program instead of raising INT 6.
- **DOS-23 · PC speaker and BEL are wrong.** `machine.rs:454-458,462-466`. The tone plays one octave low. BEL is an impulse train, not the documented square wave. Divisors under 25 overflow.
- **DOS-24 · `IMUL` immediate sets flags wrongly.** `cpu.rs:931,947`. `-1 * 1` sets CF and OF.
- **DOS-25 · Other FPU semantics.** FXAM misreports classes (`fpu.rs:469-481`). FCOM treats ±Inf as unordered (`f80.rs:606-613`). FSQRT gives wrong results for large and small inputs, for example √1e20 = 2.47e10 (`fpu.rs:669-678`). FNSAVE and FNSTENV overrun guest buffers (`fpu.rs:535-565`). FPREM doesn't clear C2 (`fpu.rs:491-498`).
- **DOS-26 · `INT 10h` text and cursor services.** `AH=08/09/0A` in graphics modes write to `B800h` (`bios.rs:84-112`). `AH=02` doesn't move the graphics cursor (`vga.rs:336-343,467-494`). `AH=0E` ignores BL (`vga.rs:451,482`). `AH=01 CX=2000h` doesn't hide the cursor (`vga.rs:345-351`). `AH=03` returns CH and CL swapped (`bios.rs:71`). `AH=11h` clobbers CX and DL (`bios.rs:128-137`).
- **DOS-27 · Renderer and mode state.** Mode 12h shows 400 of 480 lines (`vga.rs:718`). Graphics ignores `start_addr`, so page flipping does nothing. 40-column text fills only the left half (`vga.rs:899`). `AX=0093h` falls through to text mode (`vga.rs:238,316`). BIOS file reads into `A000h` skip planar routing (`dos.rs:1099-1101`).
- **DOS-28 · Mouse fidelity.** Every `INT 10h` resets the guest's range (`machine.rs:165`). Coordinates use the mode's pixels rather than the driver's virtual screen. `AX=0` returns BX=0 (`mouse.rs:71-81,121-125`). There is no hide counter. `AX=0Ch` handlers are stored but never called (`mouse.rs:183-186`).
- **DOS-29 · Keyboard paths.** `INT 21h` functions `01`, `06`, `07` and `08` drop extended scan codes (`dos.rs:505-535`). `INT 16h AH=05` uses the wrong registers (`bios.rs:193-196`). Space is queued twice (`libs/keys/src/lib.rs:365-367`, `apps/dosbox/src/lib.rs:173,198-199,217`). `set_modifiers` is never called, so shift state is always 0.
- **DOS-30 · Handle and file semantics.**
  - Handles aren't closed at exit (`machine.rs:825-848`).
  - `DUP` copies the file offset instead of sharing it (`dos.rs:816-823`).
  - Open modes and read-only are never enforced (`dos.rs:713-733,1109-1139`).
  - `AH=40h CX=0` doesn't truncate (`ramdisk.rs:247-255`).
  - `AH=3Ah` deletes files and the current directory (`dos.rs:667-678`).
  - `CON`, `NUL` and `PRN` aren't device names.
  - `AH=47h` returns unbounded long names into a 64-byte guest buffer (`dos.rs:844-849`).
- **DOS-31 · Directory search.** `ramdisk.rs:270-280,378-405`, `dos.rs:477-486`. `*.*` misses extension-less names and subdirectories, `????????.???` matches only full-length names, and one global find state breaks recursive walkers.
- **DOS-32 · PSP, environment and BDA.** `machine.rs:666-684`, `vga.rs:508-530`. There is no environment count word or program path, so `argv[0]` fails. The PSP has no JFT or FCBs. Several BDA fields are wrong or unset, and vectors are all zero at boot. Which programs this affects is `[check]`.
- **DOS-33 · Time and hardware stubs.** `INT 1Ah` returns binary where BCD is expected (`bios.rs:213-228`). Port `201h` reports a joystick with all buttons pressed (`machine.rs:103`). Ports `40h`-`43h` read 0 (`machine.rs:99,140`). Port `61h` bit 4 never toggles, so refresh-based delay loops hang (`machine.rs:100`). `AH=2Ah` always reports Sunday. Which programs hang is `[check]`.

### Low (all `[code]`)

- **DOS-34 · CPU details.** `ENTER` ignores nesting (`cpu.rs:1202-1209`). TF isn't implemented. There is no STI or `MOV SS` inhibition (`cpu.rs:805-808,1055-1059,1501`). `CLI; HLT` resumes on the next tick (`machine.rs:817-818`). Word access at offset `FFFFh` crosses into the next linear byte (`cpu.rs:219-226`).
- **DOS-35 · Front end and names.** `AUDIO_FAILED` is sticky until reboot (`apps/dosbox/src/lib.rs:24,155,165`). 8.3 truncation makes names collide (`ramdisk.rs:97,171-177`). Names like `*.*` can be created. `strip_drive` treats any drive letter as C:. Directory `MOUNT` silently skips files over 4 MiB (`ramdisk.rs:315`). Half-block glyphs are swapped (`font.rs:76-77,168`).

---

# Missing features

Ranked within each area. "Docs admit" means the project already lists it as absent.

## Kernel

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| Per-CPU IRQ stacks | Fixes KRN-1, required for x86 SMP | M | Per-CPU area, asm on both ports |
| Safe kill and a result enum | Fixes KRN-2, KRN-3, UI-2, UI-3 | M | Lock-depth hook in `SpinLock` |
| Blocking primitives: blocked state, sleep queue, `yield_now`, mutex, wait queue | Replaces spin delays and long spinlock holds | M-L | Software reschedule |
| IPIs in the `InterruptController` HAL | Reschedule, panic stop, kill, later TLB shootdown | S-M | DRV-10 |
| Panic stop-all, then per-task fault containment | Fixes KRN-6 | M | IPIs, a full frame in `exception_common` |
| Stack canary and high-water mark in `ps` | Detects overflow now | S | None |
| Guard pages | Hard stop on overflow | M | x86: 4 KiB tables. AArch64: MMU |
| Process API: `wait`, exit status, accounting, priorities | `priority` and `threads` are stubs; `init` needs exit status | M | Safe kill |
| Device IRQ registration | Replaces polling of UART, virtio, i8042 and HDA | M | Blocking primitives |
| RAM discovery from DTB and multiboot, plus a frame allocator | Sizes the heap, fixes KRN-23 | M | None |
| x86 TSS and IST for #DF, NMI, #MC | Avoids silent triple faults | S | GDT extension |
| AArch64 MMU and caches | Fixes KRN-10, enables W^X and guards | L | Page tables, DMA coherency |
| x86 SMP: MADT, AP trampoline, per-CPU LAPIC timer (docs admit) | Parity; the PIT can't tick each CPU | L | ACPI, per-CPU stacks, TSS |
| Per-CPU run queues, affinity, one-shot timers | Global lock contention at 8 CPUs | M-L | IPIs, blocking |
| EL0, syscalls, per-process address spaces (docs admit) | Real isolation, the true fix for root cause 1 | L | MMU |
| Real hardware boot: EL2 drop, SCTLR init, PSCI conduit from DT | QEMU EL1 entry only today | M | DTB parsing |

## Drivers and platform

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| Streaming audio: multi-BDL ring, position tracking, `write`/`available`, mixer | Fixes DRV-1 to DRV-3, enables music and DOS audio | M | HAL change |
| CI and a scripted QEMU boot test | None exists; manual monitor logs in `target/` show it's done by hand | S-M | Pinned toolchain, KRN-9 |
| IRQ framework: per-device handlers, SPI and IOAPIC routing | `on_irq` assumes every IRQ is a tick | M | DRV-10, HAL trait |
| PCI: bridges, BAR allocator, capabilities, MSI/MSI-X, INTx | Needed by any second PCI device | M-L | DRV-6 |
| `virtio-core` with a PCI transport | x86 has no virtio at all | M | PCI |
| `virtio-blk` | Persistence, replacing linked disk images | M | `virtio-core` |
| `virtio-tablet` on q35, or IntelliMouse | x86 pointer is relative with no wheel | S-M | `virtio-core`, or standalone |
| `virtio-gpu` 2D | Runtime resolution, damage flush; ramfb is fixed at 1024x768 | M | `virtio-core` |
| RTC: PL031 and CMOS | The clock shows uptime as wall time | S | None |
| DTB parsing (AArch64) | RAM, GIC, ECAM, virtio and UART are hard-coded | M | None |
| ACPI and multiboot memory map (x86) | PM ports are hard-coded | M | None |
| `virtio-net` and a TCP/IP stack | Networking | L | `virtio-core`, IRQs |
| GICv3 | More than 8 CPUs, many arm64 boards | M | DTB |
| xHCI, USB HID and storage | Real input on modern PCs | L | PCI, MSI |
| UEFI boot and firmware framebuffer | Real hardware | L | MMU, DTB/ACPI |

## Filesystem and data formats

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| `virtio-blk` plus a multi-block, async `BlockDevice` | Nothing persists today | L | Virtio, DMA memory |
| MBR and GPT parsing | `detect` fails on any partitioned disk | S | Block layer |
| Buffer cache with write-back | All FAT and directory I/O goes straight to the device | M | Block layer |
| Real VFS: mount/unmount, writable, handles, stat, unified with the coreutils overlay | `MountTable` is read-only and `FileSystem` has no errors | L | `try_read` API |
| Writable on-disk root | The write path exists, but only host tools use it | M | Block layer, cache, VFS, FS-4, FS-13, FS-14 |
| Crash consistency: dirty bit, ordered updates, mount-time check | No ordering guarantees | M | Writable root |
| Timestamps | All writes store zero | S | RTC |
| Attributes, then permissions | Attributes are dropped | S, L | Privilege boundary for permissions |
| Symlinks in VFS | TAR parses them, nothing can represent them | M | VFS |
| FAT12/16 read | Likely for DOS-era images | S-M | None |
| Runtime `format()` | `RamDisk` advertises scratch volumes | S-M | None |
| Streaming inflate and extract-to-sink | Output must fit in RAM today | M | None |
| ZIP64, tar.gz helper, `safe_path()` | Large archives; each consumer filters traversal alone | S-M | None |
| Runtime `.fapp` install with an integrity hash | Bundles are build-time only | M | VFS |
| Native filesystem with POSIX metadata | FAT can't hold permissions | L | VFS, cache |
| YAML multi-line forms, compact block forms | Common YAML fails | M | None |
| JSON builder API | The encoder only re-emits parsed documents, and the crate has no in-tree user | S | None |
| `init`: exit status, one-shot type, ordering, backoff | On-failure currently equals always | M | Exit codes and `wait` |

## Shell and terminal

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| Ctrl-C cancellation | Prerequisite for everything else here | M | Per-shell flag, checkpoints |
| Terminal shell on its own task, with streamed output | Fixes SH-7 | M-L | Kernel pipe or queue |
| Positional parameters, `$@`, `shift`, script args | Scripts can't take input today | S | None |
| Functions and `local` (docs admit) | The largest language gap | M | Positional parameters |
| Redirection on compound commands, and an fd table | `while read` loops and `>&2` | M | Layered `Io` |
| Here-docs and here-strings | Currently a syntax error | M | Stateful lexer |
| `case` | Very common | S | Reuses `glob_match` |
| Full `${}` operators | `#`, `%`, `/`, nested words | M | None |
| Full arithmetic, `(( ))`, `let`, `[[ ]]` | Today comparison isn't supported | M | None |
| Real subshells | `( )`, `$()`, pipeline isolation | M | Env and cwd snapshot |
| Backticks, `$'…'`, `$!`, `+=`, IFS | Common syntax | S each | None |
| `set -e/-u/-x`, `pipefail`, `trap`, `wait`, `getopts`, `readonly`, `time` | Accepted and ignored today | M | `trap` and `wait` need jobs |
| Script execution by path and PATH lookup | `./x.sh`, `#!` | S-M | Metadata or shebang check |
| `App::run -> i32` and argv | Touches every app | M | None |
| Capturing app output | `ps \| grep`, `$(ps)` | M-L | Per-process console sink |
| `jobs`, `fg`, `bg`, `wait` (docs admit) | Job control | L | Blocking wait, shared VFS |
| Streaming pipelines | Fixes memory blow-ups, enables `yes \| head` | L | Threads, bounded pipes |
| Utilities (S each): `which`, `od`, `fold`, `comm`, `split`, `dd`, `strings`, `realpath`, `mktemp`, `file`, `date`, `timeout`, more `grep`, `ls`, `sort`, `uniq` flags | Everyday use | S | Some need a clock or cancellation |
| `xargs`, `diff`, fuller `find`, `tar`, `gzip` | Libraries already exist | S-M | None |
| Regex engine with groups, alternation, intervals, POSIX classes; then `sed` | Fixes SH-24 grep issues | M | None |
| `awk` | Common tool | L | Regex engine |
| VT100/xterm emulator: cell grid, cursor addressing, colour, wrap, alternate screen | Prerequisite for editors and pagers | M-L | None |
| Text editor | The only way to write a file today is `echo >` | L | VT layer, raw tty, cancellation |
| Pager | `less` or `more` | M | Same as editor |
| Multiple Terminal instances | One `TERMINAL` static today | S-M | Per-window state |
| Selection, copy, paste, Unicode width | Everyday use | M | Clipboard |

## Desktop and GUI

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| Console capture window | Fixes UI-5; makes Run and Help usable | M | Per-process console sink |
| Alt+Tab, Alt+F4, window menu | No keyboard window switching once an app has focus | S | UI-1, UI-7, UI-9 |
| Launch with arguments and file associations | Double-clicking media only opens the hex viewer; My Documents and Search can use it too | S | `WindowContext.args` |
| Image preview and thumbnails | `feos-image` exists; images show as hex | S-M | Decode memory |
| Streaming file viewer | Capped at 8 KiB | S | `fs.read` with offset |
| Clipboard | "Copy Path" only writes the address bar; Terminal has no paste | M | Shared store |
| Explorer write operations | Terminal writes go to a private overlay Explorer can't see | L | Shared VFS |
| Content search on a background worker | Search is name-only and blocks per keystroke | M | Worker |
| Settings app, persisted | Theme resets at startup | M | UI-16, writable config |
| Notifications and tray | Tray icons are decorative; `ToastQueue` is unused; failures go to serial | M | Desktop toast layer |
| Volume control | Tray speaker does nothing | S | Mixer |
| Keyboard layouts | `EvdevKeys` is US only | S | None |
| Hosted-app isolation | A stalled app freezes the desktop, and Task Manager can't end it | L | Per-window thread, shared surface |
| Background ticks for minimized windows | Minimized apps get no frames | S | New `App` hook |
| Drag and drop | Files to apps | L | Clipboard-style payload, occlusion-aware hit testing |
| Resolution changes | `DISPLAY_WIDTH/HEIGHT` are constants | L | `virtio-gpu`, dynamic buffers |
| Unicode fonts | Non-ASCII falls back to `?`, and layout assumes 8 px cells | L | Font format, width API |

## Media

| Feature | Why it matters | Size | Depends on |
|---|---|---|---|
| Stateful decode API with key-frame index (`stss`, Cues, idx1) | Fixes MED-9, MED-10, MED-20; real seeking | M | None |
| Decode worker with a frame queue | Keeps the desktop responsive | M | Stateful API |
| Mixer, resampler, channel conversion to 48 kHz stereo | Codecs output the file's rate, the driver is fixed | M | Streaming audio |
| Music playback app or `play` command | Makes existing codecs useful | S-M | Mixer |
| Streaming from disk via a `Source` trait | Removes the 4 MiB file cap | L | Stateful API |
| PTS timing: ctts, stts, edit lists, MKV and FLV timestamps | Correct VFR and B-frame timing | M | Stateful API |
| A/V sync with audio-clock master | Video with sound | L | PTS, mixer, container audio demux |
| AAC-LC decoder | Default audio in MP4 and FLV | L | Needed by A/V sync |
| FLAC (S-M), Vorbis (M), Opus (L) | Common formats; Ogg demux exists | Varies | Slow on x86 without SSE |
| Larger frame budget, level-based DPB limits | 720p and 1080p | S | MED-4, a benchmark |
| Colour and display: VUI range, BT.709, SAR, rotation | Dropped at `sps.rs:347-353` | S | None |
| Image viewer, MJPEG default Huffman tables (S), multi-scan and progressive JPEG (M) | Common images | S-M | None |
| More video codecs: MPEG-1 or MPEG-4 ASP (M), VP8 (M-L) | Coverage. VP9, AV1 and HEVC are too large for now | Varies | None |
| Container gaps: fMP4, `wide`-first MOV, AVI `rec` and OpenDML, MKV compression | Real-world files | M | None |
| Volume and mute (S), recording (M), SRT subtitles (S-M), playlist save (S) | Everyday use | Varies | Mixer |
| H.264 error concealment (M), real interlace, High10, 4:2:2 (L) | Robustness and coverage | Varies | None |
| SSE on x86_64 for decoders | x86 is soft-float | M | Arch FP state save |

## FEOS-DOS

Ranked by how many programs each unlocks, assuming DOS-1 is fixed. Six of the 13 library titles are real-mode. **Change the order in `docs/dosbox-gap.md`** so that items 1-7 come before protected mode.

| # | Feature | Why it matters | Size | Depends on |
|---|---|---|---|---|
| 1 | 8259 PIC, 8254 PIT, INT 8 to 1Ch chain | Almost every game's timing and music | M | DOS-1 |
| 2 | Keyboard hardware: make/break, ports 60h/64h, IRQ1, INT 9, BDA ring | Action games read scan codes directly | M | #1 |
| 3 | DOS process model: MCB chain, JFT and SFT, `4Bh` exec and overlay, `4Dh`, parent PSP, TSRs | C runtimes, launchers, installers, overlays | M-L | DOS-1, DOS-14 |
| 4 | Time-based CPU pacing: cycles per ms | Game speed; PIT, OPL and DMA timing | S-M | Before #1 |
| 5 | VGA register core: latches, write modes, chain-4 off, panning | EGA and VGA games | M | DOS-18 |
| 6 | Streaming audio, OPL timers, timestamped OPL writes | Makes AdLib usable | S-M | M3, DOS-20 |
| 7 | BIOS data and ROM: full BDA, model byte, font pointers | Many small probes | S | DOS-1 |
| 8 | `COMMAND`: `%n`, IF, GOTO, FOR, CALL, SHIFT, SET, redirection, PATH, ERRORLEVEL | Games ship launcher batch files | S-M | #3 for full behaviour |
| 9 | FCB calls, `6Ch`, `5Ah`, `59h`, `38h`, `65h`, device names | Older programs | S-M | #3 |
| 10 | 386 real-mode ISA: `66`/`67`, `0F xx`, FS, GS | Prerequisite for protected mode | M-L | CPU test suite |
| 11 | 386 protected mode, DPMI, DOS/4GW (docs admit) | 7 of 13 titles | L | #10, stable #1-3 |
| 12 | Sound Blaster DSP and 8237 DMA (docs admit) | Digital sound | M-L | #1, #6 |
| 13 | XMS, EMS, A20, HMA (docs admit) | Larger 16-bit programs | M | #3 |
| 14 | Mouse callbacks, hide counter, joystick | Mouse and joystick games | S | DOS-1 |
| 15 | x87 transcendentals (docs admit), true 80-bit mul and div, exception flags | Floating-point programs | S-M | DOS-16, DOS-17, DOS-25 |

---

# Improvements

## Testability

- **Fuzz everything that parses.** Host targets for inflate, gzip, zlib, ZIP, TAR, `Bundle::manifest`, YAML, JSON, FAT32 and exFAT mount-walk-read, `feos_video::{info, decode_frame}`, `feos_image::decode`, `feos_audio_codec::decode`, and the shell's `lex`, `parse` and each command. Run them under a 64 MiB failing allocator on a small-stack thread, so out-of-memory and recursion count as failures.
- **Fix the vacuous H.264 fuzz test.** `libs/h264/tests/malformed.rs:49-67` has no SPS or PPS, so every input stops early. Seed it with `mini.h264` and mutate.
- **Add a differential shell suite.** Run about 200 lines of scripts on host `dash` or `bash` and on `feos-sh`, comparing stdout and status. Also run the examples in `docs/terminal.md` as tests. `exec.rs` and `editor.rs` have no direct tests.
- **Add a CPU conformance suite for DOS.** Per-instruction JSON test vectors for the 8086. `Cpu` is already generic over `Bus`, and `cpu.rs:1677-1696` has a flat memory bus. Today there are 3 CPU tests and none cover flags. Add table-driven FPU tests.
- **Add a guest fuzz test.** Random memory, registers and INT calls must never panic, and must return within a time bound.
- **Add scripted input tests for `Desktop::render_with_platform`.** Enter and Ctrl+Esc with an app focused, Start-menu click-through, Tab onto a minimized window.
- **Add golden-image tests.** `taskmgr.rs` and `explorer.rs` already render to a host canvas, and `preview_tests` writes PPMs without comparing them. Include minimum and tiny canvases.
- **Add property tests for `TextBuffer`.** Cursor and anchor always in bounds and on char boundaries.
- **Add mock driver tests.** `ConfigSpace` is already mockable; cover the PCI scan, `push_mouse`, `decode_keyboard`, BDL and verb encoding, the ramfb block, and virtqueue handling.
- **Move kernel logic to testable crates.** A `libs/heap` crate so the allocator's tests finally run, and the same for `next_runnable` and the state machine.
- **Add fixtures that today's tests can't see.** CABAC, B-frames, High profile, rotated video, widths not divisible by 4, multi-sector clusters, 4 KiB sectors, cycles, truncated chains and bad BPBs.
- **Assert struct layouts.** `offset_of!` for the AArch64 `TrapFrame` (`arch/aarch64/src/irq.rs:40-41` checks only size and alignment) and for x86 `xmm` and `r15`.
- **Include `feos-apps` and driver crates in `make test`.**

## Robustness

- Add `IrqSpinLock` to `libs/sync`, restoring IRQ state on drop, replacing the hand-paired `save_and_disable`/`restore` at `kernel/src/sched.rs:290-299,355-390,395-414,419-430,435-462,467-497`.
- Add owner-CPU debug tracking to `SpinLock` to catch self-deadlock, and assert IRQs are masked when taking `SCHEDULER`.
- Add a header magic to the allocator. `free` silently returns on bad input (`kernel/src/allocator.rs:247-255,266-268`), which hides corruption.
- Save registers in `exception_common` (`arch/aarch64/src/vectors.S:57-64`), so faults can be debugged.
- Use one error policy: fallible allocation in parsers, a `DecodeError::OutOfMemory`, and `FsError::NoSpace`.
- Don't let one bad directory fail a whole mount (`libs/fs/src/fat32.rs:258`, `libs/fs/src/exfat.rs:233`). Skip it and flag the volume.
- Free the chain after deleting the entry, not before (`libs/fs/src/fat32.rs:1053-1056`, `libs/fs/src/exfat.rs:1432-1433`).
- Keep names raw at the scanner, and escape control characters at display.
- Replace silent truncation with errors where it changes behaviour: `Manifest` fields, and `Text`'s `fmt::Write`, which reports `Ok` on truncation (`libs/text/src/lib.rs:210-213`).
- Validate header counts against the file (`stsz`, `dwTotalFrames`), since they feed duration directly.
- Add a `DmaBuf<T>` built on `UnsafeCell`, with `phys()` and `dma_wmb`/`dma_rmb` in the HAL, before the MMU work.

## Design

- **Add a `Board` trait and an arch trait.** Today the only thing that keeps the ports in step is `make check-arm` plus `make check-x64`. Replace `#[cfg(target_arch)]` at `kernel/src/smp.rs:20,46` and `kernel/src/main.rs:81,92` with `arch::smp::start_cpu`. Remove x86's dead `smp::set_handler`, `entry_address` and `cpu::wait_for_event`, and the double `exceptions::init`.
- **Factor out the two boards.** The two `build.rs` files differ by only 31 diff lines. Console wiring, scanout, `filesystem()` and `init_audio` are near-identical.
- **Put port I/O in one crate.** `inb`, `outb` and `outl` are duplicated across i8042, uart16550, apic, pci, fw-cfg and the x86 arch crate.
- **Remove the foreground path.** See [Decisions](#decisions-for-the-project-owner).
- **Merge duplicates in the shell.** Two line editors (`libs/sh/src/editor.rs`, `apps/commands/src/terminal.rs:403-508`) and two key decoders (`editor.rs:271-348`, `libs/keys/src/lib.rs:156-279`) have already diverged.
- **Replace the parallel `COMMANDS` table and `match`** with `CommandSpec { run: fn }`, so SH-15 can't recur (`libs/coreutils/src/commands/mod.rs:46,368`). Add a shared option parser.
- **Remove shadowed apps.** `echo.rs`, `clear.rs`, `uptime.rs` and `help.rs` are hidden by shell builtins.
- **Key windows by bundle id,** not catalog index, before supporting live rescans (`libs/desktop/src/lib.rs:583-587`).
- **Make the `gui` gallery a real windowed bundle.** It's unreachable from the desktop, and it would exercise `libs/gui` with real input.
- **Separate `info()` from per-frame decode** in `libs/video`.
- **DOS:** a refcounted RAM drive with a quota, a `last_error` value for `AH=59h`, stop clobbering AX where it must be preserved (`1Ah`, `25h`, `35h`, `2Fh`, `51h`, `62h`), and a versioned, validated save-state.
- **Record provenance** for both vendored decoders: upstream SHA, local patches and licence notices.

## Performance

### Cheapest wins

- Use `[profile.release.package.*] opt-level = 3` for `feos-gfx`, `feos-gui`, `feos-desktop`, `feos-dosbox` and `feos-h264`. `opt-level = "s"` applies to everything today (`Cargo.toml:70`).
- Make the scheduler table zero-initialized so it returns to `.bss`. It currently adds 2.1 MiB to the image (`kernel/src/sched.rs:140`).
- Use `clock.idle_ms` in `sleep.rs:25` and `tick.rs:44`, so they stop burning a core.
- Make `TaskManager::animates()` dynamic. It's always true, forcing 2 fps full-desktop repaints (`apps/commands/src/taskmgr.rs:114-120`).
- Request a second frame after input only when window state changed (`apps/desktop/src/session.rs:179`).

### Rendering

- There is no damage tracking, so every dirty frame repaints the wallpaper, icons and every app, then copies 3 MB (`libs/desktop/src/lib.rs:901-947`, `apps/desktop/src/session.rs:255-256`). Cache the wallpaper and icons first, then add dirty rects.
- Pointer moves recompose the whole screen. The save-under `Cursor` in `taskmgr.rs:1415-1481` is only on the unused foreground path; lift it into the session.
- Use span-based row operations in `Canvas::blit` (`libs/gfx/src/canvas.rs:403-425`), `fill_rect_alpha` and `blend_pixel` (`libs/gui/src/painter.rs:300-332`), and `draw_char` (`painter.rs:517`).
- Clip first in `fill_vertical_gradient` (`canvas.rs:326-339`) and `line` (`canvas.rs:235`, `painter.rs:433`).
- Cache Explorer's per-row filesystem calls (`apps/commands/src/files/explorer.rs:2589,2331-2333`).

### Kernel and memory

- Switch context by swapping SP instead of two 800-byte copies under the global lock (`kernel/src/sched.rs:256,278,283`).
- Remove the dead x86 XMM save, or build with SSE and also save MXCSR (`arch/x86_64/src/interrupts.S:110-126,145-160`).
- Keep allocator counters instead of an O(n) scan under the lock (`kernel/src/allocator.rs:347-357`), and add in-place `realloc`, because the default doubles peak use when a `Vec` grows.
- Use the vector already in the x86 `TrapFrame` instead of up to 8 MMIO reads per tick (`drivers/interrupt/apic/src/lib.rs:100`).
- Use a ticket lock, and `wfe`/`sev` on AArch64 (`libs/sync/src/lib.rs:50-60`).
- Batch virtio `publish` per poll, and set `VIRTQ_AVAIL_F_NO_INTERRUPT` while polled (`drivers/input/virtio/src/lib.rs:526,768-769`).

### Filesystem

- Read seeks restart from the first cluster on every call, making `coreutils` 4 KiB chunking O(n²) (`libs/fs/src/fat32.rs:368-374`, `libs/fs/src/exfat.rs:370-376`). Add a per-file cursor or extent map.
- Each sub-block read clears an 8 KiB scratch (`libs/fs/src/volume.rs:68,113`). Add a sector cache.
- `visited.contains` is O(n²) at mount (`fat32.rs:325`, `exfat.rs:325`). Use a bitset.
- `chain_length` runs per write, FSInfo per cluster, and new clusters are zeroed up to three times (`fat32.rs:458,553,557`, `exfat.rs:714`).
- Inflate decodes bit by bit (`libs/compress/src/inflate.rs:57-79,160-180,299`), and CRC-32 is byte-wise (`libs/compress/src/checksum.rs:45-51`). Both matter for extracting game ZIPs under TCG.

### Shell

- `>>` clones the whole file on each write (`libs/coreutils/src/fs.rs:415-420`), `read_dir` scans all nodes (`:567-581`), and `read_file` always returns everything (`:372-398`). Add `read_at`.
- Use a capped ring buffer for scrollback (`apps/commands/src/terminal.rs:224-227`).
- Stop `env.set("?")` on every command (`libs/sh/src/exec.rs:26,40,52`).

### Media

- Use a fixed `[i32; 16]` instead of `vec![0i32; tc]` per residual block (`libs/h264/src/cavlc.rs:34`), and pool `PictureState`.
- Avoid three full RBSP copies per slice (`libs/h264/src/bitstream.rs:25`, `decoder.rs:211,454`, `slice.rs:109`).
- Use AAN or fixed-point IDCT (`libs/image/src/jpeg/mod.rs:354-376`).
- Precompute an x map instead of per-pixel `u64` division in the blit (`apps/commands/src/player.rs:3989-4007`).
- Index MJPEG once instead of an O(n²) count on every decode (`libs/video/src/mjpeg.rs:63-74`).

### FEOS-DOS

- Put `TraceEntry` behind a feature (`libs/dosbox/src/cpu.rs:401-418`); it's written on every instruction.
- Use lazy flags, a decode table, and fast REP paths over plain RAM. Replace per-byte range checks with a page map (`libs/dosbox/src/machine.rs:61-80`).
- Use a palette LUT and per-line decode (`libs/dosbox/src/vga.rs:719-737`).
- Skip OPL generation when no voices or timers are active (`libs/dosbox/src/machine.rs:446`).

## Tooling

- Keep one source of truth for QEMU flags. `Makefile:37` and the two scripts can drift.
- Add `-accel` passthrough to the scripts.
- `FEOS_TEST_DISKS=1` forces a full LTO rebuild on every container start, and `COPY . .` invalidates the build layer on any change. Generate disks at image build, and use BuildKit cache mounts.
- Exclude `data/` from the Docker context when it isn't needed.
- Add `debug = "line-tables-only"`, which doesn't affect the loaded image.
- Make clippy warnings fail CI once the 227 current warning lines are cleared.

---

# Documentation that is wrong

Fix these as the matching code lands, or sooner where the docs are simply out of date.

## `CLAUDE.md`

- Says `make run-x64` reads `FEOS_DISPLAY`, `FEOS_SMP` and `FEOS_MEM`. It now sets them from `QEMU_DISPLAY`, `QEMU_SMP_X64` and `QEMU_MEM` (`Makefile:92-94`), overriding the environment.
- Says all optional `disk*.img` files are gitignored. `.gitignore` only covers `disk-*.img`, so `disk.img` isn't.
- Its scheduler smoke test ends with `kill <pid>`, which fails on AArch64 while runnable tasks don't outnumber CPUs (KRN-3).

## `README.md`

- "Notes and limitations" still says single core, eight process slots and no heap.
- `README.md:447-448`: `FEOS_DISPLAY=… make run` has no effect (BLD-2).
- `README.md:486`: `ps | grep shell` is rejected by `libs/sh/src/exec.rs:476`.
- `README.md:496-497`: `help` doesn't list apps, and `help <cmd>` fails.
- `README.md:522`: `uptime` is shadowed by a coreutil that prints a fake load average.
- `README.md:611`: the "equivalent" test command is stale.
- `README.md:394-404`: `docker-shell` doesn't open a shell (BLD-1).
- `README.md:1023`: wheel scrolling (DRV-5).

## `docs/architecture.md`

- `:192` and `kernel/src/sched.rs:9`: the task table is in `.data`, not `.bss`.
- `:154`, `arch/x86_64/src/lib.rs:31-32`, `arch/x86_64/src/timer.rs:3`: calibration uses the ACPI PM timer, not the PIT.
- `arch/x86_64/src/lib.rs:4`, `arch/x86_64/Cargo.toml:3`: the tick is the PIT, not the APIC timer.
- "Halts the machine" for panics and faults is wrong for AArch64 (KRN-6).
- Other comments that contradict the code: `arch/x86_64/src/boot.S:125-126` (the target has no SSE), `arch/x86_64/src/irq.rs:200-202`, `libs/hal/src/lib.rs:219`, `arch/aarch64/src/exceptions.rs:109`, `arch/aarch64/src/cpu.rs:7`, and `libs/console/src/lib.rs:55`.

## `docs/terminal.md`

| Claim | Reality |
|---|---|
| `( ... )` (`:39`) | Syntax error (SH-11) |
| `$#`, `$0`-`$9` (`:31`) | Hard-coded or empty (SH-18) |
| Redirection (`:38`) | Simple commands only. `>&2` prints "unsupported" |
| Line continuations (`:29`) | Serial only, and broken for quotes (SH-17) |
| Ctrl-R (`:41`) | Not in the window |
| Arithmetic with variables (`:33`) | `$n` gives 0 (SH-10) |
| Writes reset on reboot (`:66`) | The window's overlay also dies on window close or Esc |
| `read`, `set` (`:40`) | `read` is always EOF in the window. `set -e` is ignored |
| `basename`, `dirname` (`:78`) | Status 127 (SH-15) |
| `coreutils <cmd>`, two front ends (`:3,87`) | The window has no external commands, so `ps`, `sleep`, `kill` and `spawn` give 127 |
| `source /examples/notes.txt` (`:99`) | The file is prose, so every line is "command not found" |
| `> /tmp/fruit` (`:106`) | `/tmp` doesn't exist (SH-14) |
| `ls /docs \| wc -l` (`:102`) | Prints `N -` (SH-13) |

Undocumented limits: `while` stops at 1M iterations (`libs/sh/src/exec.rs:179`), `seq` at 1M values, history at 200.

## `docs/desktop.md`

- The sample uses `autostart: - terminal` (UI-17) and `entry: gui`, and its session log says 7 applications. There are 8.
- `:29`: Ctrl+Esc (UI-7).
- `:231`: promises an explanation for unknown entries. It shows "Ready - choose the app again".
- `libs/gui/src/lib.rs:49` and `apps/commands/src/gui.rs:3` still say the board has no mouse or keyboard driver.

## `docs/dosbox.md` and `docs/dosbox-gap.md`

- Licensing section (DOS-6).
- CPU row: "full" and "interrupts" (DOS-1, DOS-22). "IVT at 0000:0000" exists only as bytes the CPU never reads. "INT 1Ch delivery" is false.
- FPU row: FSUB and FDIV forms, FSTP, FCOMI and FNSAVE don't work as stated (DOS-16, DOS-25).
- Video row: logical-op and DAC claims (DOS-18, DOS-19).
- "Command tail" (DOS-15). BEL isn't a square wave (DOS-23).
- `dosbox.md:41` says INT 33h is absent, but it's implemented. "`C:\FEOS` is copied" is stale.
- `dosbox-gap.md` overclaims "8086/80186 yes", "Mouse yes", "AdLib yes", "batch yes" and save states. "PC speaker … no continuous music" and "FEOS can mount FAT32/exFAT read-only" look out of date.
- `libs/dosbox/src/lib.rs:24-28`, the `ABOUT` text and the `SOUND` command are stale too.

## Filesystem and media

- `libs/fs/src/fat32.rs:1` and `exfat.rs:1` say "Read-only". `exfat.rs:3-7` claims validation that doesn't exist (FS-8).
- `libs/yaml/src/lib.rs:6-7` claims rejections that don't happen (FS-16).
- See [Claims against reality](#claims-against-reality) for media.

---

# Checked and found correct

These were examined and are fine, so they don't need re-auditing.

- **Kernel:** `TrapFrame` offsets on both architectures, x86 error-code vectors, IRET and same-CPL paths, the disabled red zone, legacy PIC masking, `SpinLock` Acquire and Release ordering, allocator coalescing and splitting, PSCI function IDs and secondary boot order. No lock is taken inside `SCHEDULER`'s IRQ-off regions, so ticks can't deadlock on the console or heap.
- **Drivers:** fw_cfg endianness, virtqueue layout and index wrap, i8042 read order and bounded handshakes, GIC per-CPU banking and timer EOI order, HDA CORB and RIRB indices, and DMA buffer lifetimes.
- **Formats:** inflate output bounds and Huffman validation, gzip and zlib checksums, ZIP size and CRC checks, TAR and ZIP index guards, JSON depth limit and surrogate pairs, exFAT boot checksum, and `..` handling.
- **Shell:** overlay whiteouts, rmdir, rename of base subtrees, and the self-move guard. There is no unlocked shared global, because each `Shell` owns its overlay. No large arrays sit in recursive frames.
- **Desktop:** CPU percentage maths and the absence of a canvas race. Background processes get no display.
- **Media:** no infinite loops on truncated data, no stack-array overflow, closing the player frees everything, and `libs/audio` has no bugs.
- **FEOS-DOS:** memory and VRAM accesses are masked, `DIV`, `IDIV` and `AAM` can't panic, `load_exe` slicing is safe, and window close is sound.

# Limits of this audit

- **Coverage was uneven.** `libs/gui/src/components/*` and `apps/commands/src/gui.rs` were sampled by risky lines. In `libs/h264`, `intra_pred.rs`, `residual.rs` maths, `neighbor.rs` and most `decode_*` bodies weren't audited. MP3 IMDCT wasn't read.
- **Only some bugs were run.** 44 of the 203 numbered findings were reproduced, on real boots or on the host. The rest come from reading the code. KRN-1 is a timing race, and wasn't reproduced.
- **QEMU only.** Nothing ran on real hardware or KVM, so `[check]` findings about them are unverified. The GUI wasn't driven interactively, so UI findings other than UI-1 and UI-16 weren't observed on screen.
- **No game was run.** The status of each title in `data/games` is unknown. DOS-1 suggests real programs fail early.
- **No comparison against rust-dos.** It isn't on this machine, so the extent of any copying beyond the marked files is unknown. Nothing here is legal advice.
- **Line numbers will drift.** They are accurate at `78cf9c0`.
