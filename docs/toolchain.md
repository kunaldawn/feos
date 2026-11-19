# The feos toolchain port

How relibc, the Rust target and std, and LLVM/clang/lld are ported to feos, until rustc,
cargo and clang run on feos and build feos (`docs/selfhost-plan.md` phases 2 to 4, tracked
in `docs/selfhost-todo.md`; the kernel interface they use is `docs/feos-abi-v2.md`). This
file says how to build each piece offline, what is tested on which port, and what waits
on which kernel stage. `ports/README.md` has the source layout and vendoring,
`ports/PATCHES.md` every patched upstream file, `ports/KERNEL-ISSUES.md` the kernel
problems found on the way.

The port goes in track stages, each building on the last:

| Stage | What | State |
|---|---|---|
| T0 | Toolchain build image, offline sources for relibc and cbindgen, the feos test harness | done |
| T1 | relibc's `platform/feos`, static: target specs, crt0, `libc.a`, headers; C programs run on feos | done (14 cases on each port; 9 pending on kernel stages, below) |

## Rules

- Everything builds and tests with `--network none`, from `ports/src` (pinned
  submodules) and `ports/vendor` + `ports/vendor-git` (vendored crates). The network is
  used only to build the toolchain image and to fetch a new source (`ports/README.md`).
- x86_64 and AArch64 in lockstep: every piece is built for both and tested on both ports.
- Upstream sources are patched in their submodule's working tree, small and
  upstreamable, each file listed in `ports/PATCHES.md`.
- Test kernels are built from a committed snapshot (`git archive`), never from the
  working tree, which the kernel work may be in the middle of editing.
- A test that needs a kernel call not there yet is marked pending with the stage it
  waits on, never faked.

## The build image

`localhost/feos-toolchain` is `localhost/feos` (the kernel build image: rust 1.99.1,
gcc, make, python3, QEMU) plus clang, lld and llvm (Debian's, generation 0's host tools
only), g++, the AArch64 cross gcc/g++, cmake, ninja, autoconf/automake/autopoint/libtool,
pkg-config, gettext, texinfo, bison, flex, full python3, git, qemu-user, expect, and
rust's std for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. Its
entrypoint is a plain shell.

```sh
docker build -t feos .           # localhost/feos, if not built yet
ports/toolchain-image.sh         # localhost/feos-toolchain, from ports/toolchain.Dockerfile
```

The image's build context is an empty directory, so the tree's state never invalidates
it. cbindgen isn't in it: it's built from source like everything else.

### Running builds

`ports/test/container.sh NAME COMMAND...` starts `COMMAND` detached in a container named
`feos-ports-NAME`: the image above, `--network none`, `--cpus 6 --memory 9g`,
`/dev/kvm`, the tree at `/src`, `target/ports-base` also at `/base`, and the
`feos-ports-target` volume at `/build`, with the output in `/build/NAME.log`. It refuses
to start while the kernel workflow's `feos-gate` container runs (a boot under load
times out) unless `PORTS_FORCE=1`, which is fine for a build that boots nothing.

```sh
ports/test/container.sh relibc ports/build-relibc.sh x86_64-unknown-linux-gnu
timeout 570 docker wait feos-ports-relibc      # repeat until it prints the exit status
ports/test/container.sh --log relibc
docker rm feos-ports-relibc
```

The volume keeps build output between runs, so nothing already built and unchanged is
rebuilt:

| Path in `/build` | What |
|---|---|
| `bin/cbindgen` | cbindgen, from `ports/build-cbindgen.sh` |
| `cbindgen-target/` | its cargo output |
| `relibc/<target>/` | relibc's installed sysroot: `include/`, `lib/{libc.a,libc.so,ld*.so*,crt0.o,crti.o,crtn.o,libm.a,...}` |
| `relibc-target/` | relibc's cargo output, every target |
| `relibc-test/<target>/`, `relibc-suite/<target>/` | the host-side relibc tests |
| `relibc/<arch>-unknown-feos/` | relibc's feos sysroot, from `ports/build-relibc-feos.sh` (static: no `libc.so`) |
| `t1/<arch>/` | stage T1's C programs, from `ports/test/build-t1.sh` |
| `abi-check-target/` | the host ABI test's cargo output |
| `ports-base-target/` | the test kernels' cargo output (`ports/test/build-kernels.sh`) |
| `t0-host/` | `stage-t0-host.sh`'s from-nothing build (all of the above, fresh) |
| `cargo-home/` | `ports/vendor.sh`'s download cache (the one networked step) |
| `NAME.log` | each container's output |

## cbindgen

relibc generates its C headers with cbindgen. It's `ports/src/cbindgen`, pinned at
v0.29.4, its crates in `ports/vendor`.

```sh
ports/build-cbindgen.sh          # in the image; /build/bin/cbindgen
```

## relibc

`ports/src/relibc` at `50c054e2`, with openlibm and dlmalloc-rs as its own submodules,
its crates.io dependencies in `ports/vendor` and its four git dependencies in
`ports/vendor-git` (see `ports/README.md` for why those are apart).

```sh
ports/build-relibc.sh x86_64-unknown-linux-gnu      # or aarch64-unknown-linux-gnu
```

runs relibc's own `make all` and `make install` for the target, with
`CARGO_TARGET_DIR=/build/relibc-target` so nothing is written into the source tree,
cbindgen from `/build/bin`, and the image's rust 1.99.1 instead of the nightly relibc's
`rust-toolchain.toml` names (`RUSTUP_TOOLCHAIN=1.99.1`, `RUSTC_BOOTSTRAP=1` for its
`-Zbuild-std=core,alloc,compiler_builtins`, whose std crates come from `ports/vendor`, as
for the x86_64 kernel). It installs into `/build/relibc/<target>`.

That is relibc's Linux platform, which the host-side tests use. relibc for feos is
`build-relibc-feos.sh` (next sections).

## The target specs

`ports/targets/x86_64-unknown-feos.json` and `aarch64-unknown-feos.json` are the Rust
targets as JSON until they're built into rustc (stage T4), which is to take them as
they are:

| | x86_64 | AArch64 |
|---|---|---|
| `os`, `target-family`, `env` | `feos`, `unix`, none | the same |
| `llvm-target` | `x86_64-unknown-feos` (an OS LLVM doesn't know yet: generic ELF) | `aarch64-unknown-feos` |
| CPU | `x86-64` with `fxsr`, `sse`, `sse2`, and `avx`, `avx2`, `avx512f`, `xsave` off: the kernel keeps `CR4.OSXSAVE` clear and saves FP state with `fxsave` | `+v8a,+neon,+fp-armv8`, no outline atomics (which need `getauxval` before anything runs) |
| relocation model | `pic` (see deviations), linked non-PIE | `static` |
| linking | static only: `dynamic-linking` false, `crt-static-default` | the same |
| linker | `rust-lld`, `gnu-lld` flavour: `-static --image-base=0x1000400000 -z separate-loadable-segments -z max-page-size=4096 -z noexecstack` (the image area of `user/link.ld` and section 6; every `PT_LOAD` on pages of its own, as `libs/elf` requires), relro off | the same, plus `--fix-cortex-a53-843419` |
| crt objects | `crt0.o`, `crti.o` before, `crtn.o` after (`static-nopic-exe`), then `-lc -lm -lcompiler_builtins` | the same |
| TLS | `has-thread-local`; FS (`FS_BASE`), set by relibc through `SET_TLS` | `TPIDR_EL0`, set through `SET_TLS` |
| panics | `unwind` (relibc itself builds `panic=abort`) | the same |

`rustc -Zunstable-options --print target-spec-json --target ports/targets/<t>.json`
accepts both; cargo needs `-Zjson-target-spec` for them. PIE, `dynamic-linking` and a
`pre-link-objects` set for dynamic programs arrive with kernel stage 1f.1.

## relibc for feos

relibc's `src/platform/feos` is the `Pal` (and `PalSignal`, `PalSocket`, `PalEpoll`,
`PalPtrace`) for feos, each function as section 9 of `docs/feos-abi-v2.md` maps it. Its
pieces:

- `abi.rs`, the one place the feos numbers live: every call number from the design
  (0 to 108, including calls no kernel has yet), the error numbers, the feos-only
  constants (`SPAWN_*` at glibc's values, spawn actions, aux types, `IOCTL` requests,
  wait options) and every structure (`PathAt`, `Stat2`, `Dirent`, `StatVfs`, `Flock`,
  `PollFd`, `Termios`, `WinSize`, `SpawnRequest`, `SpawnAction`, `ExecRequest`,
  `Rusage`, `SysInfo`, `SigAction`, `SigStack`, `SockAddrIn`, `UserStr`), with section
  4's sizes and offsets asserted at build time. No dependencies, so the host test
  includes it as it is.
- `checks.rs`: build-time assertions that relibc's own header values (`errno.h`, `O_*`,
  `AT_*`, `F_*`, `S_IF*`, `SEEK_*`, `PROT_*`, `MAP_*`, `CLOCK_*`, `W*`) are feos's, which
  are Linux's, and that the structures relibc passes through unconverted (`timespec`,
  `rusage`, `statvfs`, `struct flock`, `dirent`, `winsize`, `termios`) have feos's layout.
  So errors need no mapping: a negated result in `-4095..0` is relibc's `errno`.
- `syscall.rs`: `int $0xf0` (`rax`; `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`) and `svc #0`
  (`x8`; `x0`..`x5`), without `preserves_flags` (section 1 doesn't promise the flags).
- `mod.rs`: the `Pal`. Paths go as pointer and length; 32-bit arguments are sign
  extended; `fstatat` converts `Stat2` to relibc's `stat`, `fstatvfs` `StatVfs`;
  `chdir` is `OPENAT(O_PATH|O_DIRECTORY)` + `FCHDIR`; `getcwd` asks for one byte less and
  adds the NUL; `getrlimit`, `uname` and part of `sysconf` come from `SYSINFO`;
  `readv`/`writev` are libc (one `WRITE` for up to 64 KiB, so a small `writev` to a pipe
  stays atomic); `posix_spawn` builds a `SpawnRequest` with the file actions as
  `SpawnAction`s and the attribute flags as they are, filling `pgroup`, `sigmask` and
  `sigdefault` only when their flag is set (`SPAWN2` refuses them otherwise, and POSIX
  ignores an attribute whose flag isn't set); `waitpid` is `WAIT4`; `verify` checks
  `SYSINFO`'s ABI version (2) and feature bits 0 to 16 (through 1c.6).
- threads: `rlct_clone` is `THREAD_CREATE` at `__relibc_feos_thread_start`, a
  trampoline that pops relibc's start words and calls the shim; `exit_thread` is
  `THREAD_EXIT(stack, size)`, which frees the stack after leaving it (not for a stack
  the caller gave: relibc's new `CALLER_STACK` flag); the TCB is Linux's layout,
  activated with `SET_TLS`; the futexes are `FUTEX_WAIT2` (absolute `CLOCK_MONOTONIC`
  deadline, as relibc passes) and `FUTEX_WAKE`.
- start: relibc's own `_start` and `relibc_start` read the System V stack unchanged;
  the aux vector is kept for `getauxval` (`AT_FEOS_*` included).
- `signal.rs`, `socket.rs`, `epoll.rs`, `ptrace.rs`: the signal calls (`KILL`,
  `THREAD_KILL`, `SIGACTION` with `SA_RESTORER` and `__restore_rt`, `SIGPROCMASK`,
  `SIGALTSTACK`, `SIGPENDING`, `SIGSUSPEND`, `SETITIMER`), the socket calls
  (`sendmsg`/`recvmsg` gathered over `SENDTO`/`RECVFROM`; `recvmsg` never reports
  `MSG_TRUNC`, since `RECVFROM` returns at most the buffer's length and can't say a
  datagram was longer), and `ENOSYS` for epoll,
  ptrace, `sigtimedwait`, POSIX timers, `mremap`, `mkfifo`.

Outside the platform (each file in `ports/PATCHES.md`): feos takes the Linux headers'
values and files (`cfg(any(linux, feos))`); its own `signal.h` part (feos's
`ucontext_t`, `__restore_rt` calling `SIGRETURN`); `getauxval`, with the `AT_*` names in
`<sys/auxv.h>`; glibc's `POSIX_SPAWN_*` bit values, the `_np` file actions
(`addchdir_np`, `addfchdir_np`, `addclosefrom_np`, `addtcsetpgrp_np`) and glibc's
`posix_spawnp` search (past `EACCES`, `/bin:/usr/bin` with no `PATH`); a `poll`-based `select`; the `ioctl` translation; the TLS
start; `sysconf` from `SYSINFO`; and two fixes that apply to Linux too (`FD_CLOEXEC` is
1; caller-given thread stacks aren't unmapped).

### Building it

```sh
ports/build-cbindgen.sh                     # once
ports/build-relibc-feos.sh all              # or x86_64 / aarch64
```

in the toolchain image, offline (`ports/test/container.sh NAME ...` as above). It runs
relibc's own Makefile with `TARGET=<arch>-unknown-feos`, cargo's `--target` the JSON
spec, `-Zbuild-std=core,alloc,compiler_builtins -Zjson-target-spec` under
`RUSTC_BOOTSTRAP=1` with the image's rust 1.99.1, and relibc's C parts (its `src/c`,
openlibm) through the C driver below. It installs `/build/relibc/<arch>-unknown-feos`:
`include/` (cbindgen's headers and relibc's own), `lib/crt0.o` (also as `crt1.o`),
`crti.o`, `crtn.o`, `libc.a` (relibc with openlibm), `libm.a` (openlibm),
`libcompiler_builtins.a` (the `compiler_builtins` rlib of the same build: relibc renames
its own copy's symbols inside `libc.a`, so C code needs one) and empty `libdl.a`,
`libpthread.a`, `librt.a`. No `libc.so` or `ld.so` yet (stage 1f.1). A change to the spec
or the driver rebuilds that target from scratch (relibc's Makefile can't see either, and
its `cargo rustc --emit` rules emit nothing when cargo finds a crate fresh).

### The C driver

`ports/toolchain/bin/x86_64-unknown-feos-cc` and `aarch64-unknown-feos-cc` (one script,
`feos-cc`) are generation 0's C compiler for feos: the host's clang 14 for
`<arch>-unknown-none-elf`, `-nostdinc` with the sysroot's headers, `-D__feos__
-D__unix__ -D__ELF__` (what a feos clang will predefine), `-fPIC` on x86_64 and
`-fno-pic` on AArch64. Linking calls `ld.lld` itself, with the target specs' flags and
crt objects (clang would link a triple it doesn't know through the host's gcc). For a
program: `x86_64-unknown-feos-cc -O2 hello.c -o hello`. `FEOS_SYSROOT` picks the
sysroot.

## The feos test harness

`ports/test/` runs programs on feos: it builds kernels from a snapshot of a commit with
extra files in their image, boots each port headless and runs shell commands, checking
their output and exit status.

| Script | Does | Where |
|---|---|---|
| `base.sh [REF]` | `target/ports-base` = `git archive REF` (default `HEAD`; e.g. `refs/feos-checkpoints/1c.6`), the commit in `.ports-base-ref` | host or container |
| `inject.sh ARCH SRC DEST` | puts `SRC` at `DEST` in the image of `ARCH` (`aarch64`, `x86_64` or `all`) | host or container |
| `build-kernels.sh [arm\|x64]` | the base's `make build-arm build-x64`, with the overlays laid on; kernels in `target/ports-base/kernels/{arm,x64}`, the commit each was built from in `kernels/{arm,x64}.ref` | container |
| `boot.py PORT KERNEL SPEC...` | boots and runs the specs' cases | container |
| `stage-t0.sh [REF]` | stage T0's feos-side test (below) | container |
| `stage-t1.sh [REF]` | stage T1's: cbindgen and relibc for feos (each a no-op when built and unchanged), the C programs, both kernels with them injected, `specs/t1-relibc.toml` on each port (REF default `refs/feos-checkpoints/1c.6`) | container |
| `build-t1.sh [ARCH...]` | compiles `c/t1/*.c` against the feos sysroot and checks each binary's layout | container |
| `abi-check.sh [REF]` | the host test of relibc's feos constants against a base's `feos-abi` (`abi-check/`) | container |
| `fresh.sh` | sourced by the scripts that build: rebuilds a base of another commit than the last build (below) | container |
| `test-fresh.sh [A B]` | the harness's test of that: builds at A, B, B, A and checks each kernel's commit | container |
| `container.sh` | the container conventions above | host |

`inject.sh` puts `/bin/NAME` into the base's `user/out/<arch>/`, where the boards' build
scripts take each architecture's `/bin` from (`boards/common/fs_image_build.rs`). Any
other path goes into `target/ports-base/.overlay/<arch>/`, which `build-kernels.sh` copies
over `boards/qemu-virt/fs/` just for that architecture's build (`fs/` is shared by both
boards) and takes off again; it refuses to replace a file the base has. For a
`/bin` program built by the base itself, run the base's `make user-arm user-x64` first,
as `stage-t0.sh` does.

`build-kernels.sh` builds at `/base`, the container's mount of `target/ports-base`, not at
`/src/target/ports-base`: cargo merges every `.cargo/config.toml` from the working
directory up, so a base inside the tree would get the tree's rustflags on top of its own
(the linker script twice, which fails to link).

The build output stays in the volume's `ports-base-target/` between runs, and cargo
decides what is fresh by source mtimes, while `git archive | tar -x` gives each file its
commit's time. So a base of a commit older than the last build (an earlier checkpoint,
or HEAD again after one) would look built already and reuse the other commit's kernel and
user programs. `fresh.sh` keeps the commit last built per piece (`arm`, `x64`,
`user-arm`, `user-x64`) in `ports-base-target/.ports-base-built-PIECE`; before building,
when any piece being built was last built at another commit than the base's
`.ports-base-ref`, it touches every file of the base (once per base), so everything of
the base's is rebuilt, and it records the commit only after that piece built. A base of
the commit last built is left alone and reuses the build. `inject.sh` copies without
`cp -p` for the same reason: an injected file older than the last build would be missed.
`test-fresh.sh` (`PORTS_FORCE=1 ports/test/container.sh fresh ports/test/test-fresh.sh`,
boots nothing) checks this: 1c.6 and HEAD in turn give different kernels, each
`kernels/x64.ref` names its commit, and an unchanged rebuild compiles 2 crates (a new
base always reruns the image build script, so the kernel relinks) instead of 67.

`boot.py` imports the base's `scripts/boot-test.py` (its `Guest`, `qemu_command` and
`boot_to_shell`; nothing in it is changed), so `FEOS_BOOT_TEST_SMP`, `_CPU` and `_MEM` work
as there. A spec is TOML:

```toml
[[case]]
name = "hello-copy"              # unique within the spec
run = "/bin/ports-hello one"     # a line for the feos shell
expect = ["argv[1]=one"]         # substrings of its output, in this order
reject = ["FAILED"]              # substrings its output must not have
status = 0                       # its exit status; omit to not check
timeout = 60                     # seconds
arch = "x64"                     # only on this port; default both
pending = "1c.7"                 # not run: waits on this kernel stage
why = "needs EXEC"
```

Each port's cases run in one boot, in order. `boot.py` sends `RUN; echo
ports-case-N-$((0))-rc-$?`, takes the output up to that marker as the case's (the `$((0))`
keeps the echoed command line from matching), and checks it. A failure skips the rest of
that boot. Pending cases are listed as `PENDING`, with their stage.

## Tests and results

### T0

Host side, `ports/test/stage-t0-host.sh`, from an empty output directory and an empty
cargo home (so only `ports/vendor` and `ports/vendor-git` supply crates), `--network
none`:

| Test | x86_64 | AArch64 |
|---|---|---|
| cbindgen builds from `ports/src/cbindgen` | pass (host tool) | (host tool) |
| relibc builds (`make all install`, Linux platform) | pass | pass, with the `poll` patch (`ports/PATCHES.md`) |
| `test/c/hello.c` (stdio, heap, qsort, libm, environment, files, a thread), static | pass | pass under qemu-aarch64, threads not checked |
| the same, dynamic, through relibc's own `ld.so` and `libc.so` | pass | pass under qemu-aarch64, threads not checked |
| relibc's own test suite (`ports/src/relibc/tests`, 332 programs incl. the `expect` one) | pass, with one known failure: `netdb/getaddrinfo` resolves a name through DNS (`ports/test/relibc-known-failures.txt`) | not run: its runner spawns the test programs itself, which qemu-user can't follow without binfmt_misc |

feos side, `ports/test/stage-t0.sh` on a base of `HEAD` (`e3fe81ba`): a copy of the base's
own `/bin/hello` injected as `/bin/ports-hello`, and a different marker file per port at
`/usr/share/ports-test/marker.txt` through the overlay, both kernels built, each port
booted (`specs/t0-harness.toml`):

| Case | arm | x64 |
|---|---|---|
| `injected-program-runs` (argv, output) | pass | pass |
| `injected-program-exit-status` (exit 7 seen) | pass | pass |
| `injected-program-is-the-copy` (`cmp` with `/bin/hello`) | pass | pass |
| `overlay-file-arm` / `overlay-file-x64` (each port has its own file) | pass | pass |
| `failing-status-is-seen` (127 for a missing program) | pass | pass |

Then `specs/harness-selfcheck.toml` on x64 checks the harness reports what it should: a
pass, a deliberate failure (`FAIL`, exit 1), the case after it `SKIPPED` and a `PENDING`
one never run. It does.

Nothing in T0 waits on a kernel stage.

### T1

Host side:

| Test | Result |
|---|---|
| relibc builds for `x86_64-unknown-feos` and `aarch64-unknown-feos` with `--network none` (`ports/build-relibc-feos.sh all`) | pass, both |
| `ports/test/abi-check.sh` (host test, `ports/test/abi-check/`): relibc's `abi.rs` against the base's `feos-abi` (every structure's size and offsets, the constants both have, 62 call numbers and every error number in the base's `libs/abi` text) and against section 3's table (all 78 numbers 31 to 108) | 5/5 pass on base 1c.6; also with `FEOS_ABI_LIB` at the working tree's `libs/abi` (67 call numbers, through 1c.8) |
| relibc's build-time `checks.rs` (header values and pass-through layouts) | pass on both targets |
| the Linux platform still builds and passes T0's host tests after the feos patches (`build-relibc.sh` + `test/relibc-linux.sh`, x86_64 with relibc's 332-program suite; AArch64 hello) | pass |
| `ports/test/build-t1.sh`: the ten C programs build for both, each checked a static `ET_EXEC` at 0x1000400000 with no `PT_INTERP`/`PT_DYNAMIC`, no writable and executable segment, no two `PT_LOAD`s on one page, the entry in an executable segment | pass, both |

feos side, `ports/test/stage-t1.sh` on a base of `refs/feos-checkpoints/1c.6`
(`f36e522b`): the programs of `ports/test/c/t1` injected as `/bin/t1-*`, both kernels
built, `specs/t1-relibc.toml` on each port:

| Case | What it checks | arm | x64 |
|---|---|---|---|
| `hello` | stdio (`printf` formats, `fputs`, `write`), `argc`/`argv` through the System V stack | pass | pass |
| `stderr` | `fprintf(stderr)` reaches the console | pass | pass |
| `exit-status` | `exit(42)` is the shell's `$?` | pass | pass |
| `atexit` | `atexit` handlers run at `exit` | pass | pass |
| `malloc-stress` | 160k mixed `malloc`/`free` of up to 64 KiB checked by pattern, `realloc` up to 4 MiB and down, `calloc` zeroing, a 64 MiB block touched per page, `posix_memalign` | pass | pass |
| `files` | `mkdir`, `open(O_CREAT\|O_EXCL)`, `write`, `pwrite`, `lseek`, `read`, `pread`, `fstat`/`stat`, `fopen`/`fgets`/`fprintf`, `rename`, `access`, `ftruncate`, `opendir`/`readdir` with `d_type`, `chdir`/`getcwd`, `unlink`, `rmdir` (`ENOTEMPTY`, `ENOENT`, `EEXIST`, `EBADF` too) under `/tmp` | pass | pass |
| `time` | `nanosleep(200 ms)` sleeps at least that on `CLOCK_MONOTONIC`, `usleep`, `clock_getres`, `CLOCK_REALTIME`, `gettimeofday`, `time`, `gmtime_r`/`strftime`, the CPU-time clocks, `EINVAL` for an unknown clock | pass | pass |
| `pthreads` | 8 threads × 20000 mutex-protected increments, join return values, `__thread` per thread, a condition-variable producer/consumer (sum 500500), a detached thread with a 256 KiB stack | pass | pass |
| `env-export`, `env-assignment`, `env-unset` | `export VAR=...` and `VAR=... cmd` in feos-sh arrive in `environ`/`getenv`; an unset one doesn't; `setenv`/`unsetenv`/`putenv` | pass | pass |
| `posix-spawn-waitpid` | `posix_spawn` of `t1-hello` and `t1-env` with arguments and a chosen environment (and an empty one), exit status 42 through `waitpid`, stdout into a pipe through `adddup2`/`addclose`, into a file through `addopen` (its path copied when added), `posix_spawnp` searching `PATH`, past a missing directory and an `EACCES` candidate (a directory in its place), `EACCES` when that's all there is, `ENOENT` for nothing, `/bin:/usr/bin` with `PATH` unset; `addclosefrom_np` (fd 7 closed from 3, kept from 8, `EBADF` for -1); `addtcsetpgrp_np` reaching `SPAWN2` (`EINVAL` until 1c.11); a sigmask, sigdefault and pgroup set without their flags ignored, and with them the caller's group joined or a new one led; `ENOENT` for a missing program as the call's error | pass | pass |
| `platform-info` | `uname` (`feos`, the machine), `sysconf` CPUs/pages/page size/open max from `SYSINFO`, `getauxval` (`AT_PAGESZ`, `AT_PHDR`, `AT_RANDOM`, `AT_FEOS_ABI`, `AT_FEOS_EXE`, all by the names `<sys/auxv.h>` defines), ids, process group and session, `getrlimit` (8 MiB stack), `getrusage`, `readlink("/proc/self/exe")` | pass | pass |
| `later-calls-give-enosys` | `kill`, `waitpid(WNOHANG)`, `sigaction`, `sigprocmask`, `tcgetattr`, `fork`, `getrandom`, `socket` reach the kernel at their numbers and return its `ENOSYS` (each skipped once `AT_FEOS_FEATURES` has its stage's bit) | pass | pass |
| `exec` | `execv` | pending 1c.7 | pending 1c.7 |
| `waitpid-wnohang` | `waitpid(-1, WNOHANG)` | pending 1c.8 | pending 1c.8 |
| `signals` | `sigaction` + `raise` | pending 1c.10 | pending 1c.10 |
| `isatty` | `isatty(0)` from the shell | pending 1c.11 | pending 1c.11 |
| `fork` | `fork` + `waitpid` | pending 1d.1 | pending 1d.1 |
| `getrandom` | `getrandom` | pending 1e.1 | pending 1e.1 |
| `socket` | `socket(AF_INET, SOCK_STREAM)` | pending 1e.3 | pending 1e.3 |
| `spawn-sigmask` | `POSIX_SPAWN_SETSIGMASK`'s mask is the child's (read with `sigprocmask`) | pending 1c.10 | pending 1c.10 |
| `spawn-tcsetpgrp` | `addtcsetpgrp_np` on `/dev/tty` gives a child in its own group the terminal | pending 1c.11 | pending 1c.11 |

14 passed and 9 pending on each port. The pending cases run the same programs
(`t1-pending MODE`), each checking what the call should do once its stage is in.

Pending in T1, by kernel stage:

| Stage | What relibc has wired, waiting |
|---|---|
| 1c.7 `EXEC` | `execve`, `fexecve` (`ExecRequest`, `SPAWN_EMPTY_PATH`) |
| 1c.8 `WAIT4`, `KILL` | `waitpid` in full (`WNOHANG`, any child, groups); until then `waitpid` of one pid with no `WNOHANG` falls back to v1 `WAIT`, encoding its code as an exit status (v1 can't tell `exit(130)` from a `SIGINT`); `kill`, `killpg`, `sigqueue` |
| 1c.9, 1c.10 signals | `sigaction` (with `__restore_rt`, feos's `ucontext_t`), `sigprocmask`, `sigaltstack`, `sigpending`, `sigsuspend`, `raise` and `pthread_kill` (`THREAD_KILL`), `setitimer`/`alarm` |
| 1c.11 `POLL`, `IOCTL` | `poll`, `ppoll`, `select`/`pselect` (over `POLL`), `isatty`, `tcgetattr`/`tcsetattr`, window size, `FIONREAD`; stdio's line buffering of a terminal |
| 1d.1 `FORK` | `fork` (refreshes the TCB's tid in the child) |
| 1e.1 `GETRANDOM`, `CLOCK_SETTIME` | `getrandom`, `getentropy`, `clock_settime` |
| 1e.3, 1e.4 sockets | the `PalSocket` calls; `RESOLVE` for `getaddrinfo` is not wired yet (relibc's own DNS over UDP is the path until then) |
| 1f.1 dynamic programs | `libc.so`, `ld.so`, PIE targets |

## Deviations from the plan

- **`ports/vendor-git`.** The plan put new crates into `ports/vendor` with one more
  `--sync`. relibc's git `object 0.36.7` clashes with crates.io's `object 0.36.7`, which
  `cargo vendor` refuses (and one directory source couldn't hold both). relibc is
  vendored on its own and merged: crates.io packages into `ports/vendor`, git ones into
  `ports/vendor-git`, with source replacements in `ports/.cargo/config.toml`
  (`ports/vendor.sh`, `ports/vendor-merge.py`).
- **The base builds at `/base`.** The plan built the base's kernels in
  `target/ports-base`. They're built in the same directory, mounted at `/base` in the
  container, because cargo would otherwise also apply the tree's `.cargo/config.toml`
  (see the harness above). `base.sh` empties the directory instead of removing it, so
  the mount survives.
- **relibc on AArch64 Linux needed a patch** (no `poll(2)` there). Only the host-side
  tests use the Linux platform; the patch is upstreamable.
- **qemu-user and threads.** relibc's Linux `pthread_create` clones without
  `CLONE_SYSVSEM`, which qemu-user's clone emulation requires for a thread, so the
  AArch64 host-side test skips its thread check (real AArch64 Linux would run it). feos
  tests threads on feos itself, in later stages.
- **x86_64 code is position independent (T1).** The plan said a static relocation
  model. feos links programs at 0x1000400000 (64 GiB), beyond the small code model's
  32-bit absolute addresses, so static x86_64 code doesn't link there (`R_X86_64_32`
  out of range). The x86_64 spec uses `relocation-model: pic` (RIP-relative code,
  what the base's own x86_64 user programs use) and still links a static, non-PIE
  `ET_EXEC`; lld resolves the GOT at link time. AArch64 keeps `static` (ADRP reaches
  ±4 GiB from the code). The C driver matches (`-fPIC` / `-fno-pic`).
- **The thread trampoline takes its words by argument (T1).** The design has relibc's
  feos `rlct_clone` pass `arg` = 0 and the words at the new stack pointer. `THREAD_CREATE`
  wants a 16-byte aligned stack top, and on x86_64 relibc's seven start words leave it 8
  bytes off. `rlct_clone` passes the words' address as `arg` and the aligned address
  below them as the stack; the trampoline loads the stack pointer from `arg` first.
  Nothing in the kernel changes (`ports/KERNEL-ISSUES.md`).
- **`waitpid` before `WAIT4` (T1).** Stage 1c.6's kernel has no `WAIT4`, but its v1
  `WAIT` waits for a user parent's child by pid. relibc's feos `waitpid` calls `WAIT4`,
  and only on `ENOSYS`, for one pid without `WNOHANG`, uses v1 `WAIT`, so posix_spawn's
  test can check exit statuses now. Everything else `WAIT4` does is pending 1c.8.
- **No layout check against the libc crate on feos (T1).** relibc's default
  `check_against_libc_crate` compares its structures with the libc crate's, which has no
  feos module until the Rust target (T3). On feos the check is off and the dependency
  isn't built; `abi.rs`, `checks.rs` and the host test check feos's layouts instead.
- **`relibc`'s `ucontext_t` on feos is feos's (T1).** relibc's Linux `ucontext_t` is
  x86_64 Linux's on every architecture; feos's follows section 8.4 per port, with
  `__restore_rt` passing the context's address to `SIGRETURN`.
