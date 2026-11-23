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
| T2 | The sysroot (built, and installed in the feos image), relibc's own tests and a libc-test subset as a feos test suite (`ports/test/libc-suite.sh`) | relibc 224 and libc-test 193 (x64) / 137 (arm) tests pass on feos, 0 fail; the rest pending on kernel stages or explained (below) |
| T3 | The libc crate's feos module (checked against relibc's headers), std for both feos targets with unwinding, Rust programs on feos (`ports/test/stage-t3.sh`) | done: `libc-check` 2616 (x86_64) / 2596 (AArch64) checks pass, 35 allowed differences, 0 failures; `specs/t3-rust.toml` 15 of 15 on each port (base 1c.11) |
| T4 | rustc with `x86_64-unknown-feos` and `aarch64-unknown-feos` built in, LLVM with clang and lld from source, a stage 1 toolchain with std for both feos targets and cargo, offline (`ports/build-rust.sh`, `ports/test/stage-t4.sh`) | done: host checks 10 of 10 (the built-in specs equal T1's JSON); `specs/t3-rust.toml` (rebuilt by the new rustc) and `specs/t4-rust.toml` (a cargo project with vendored serde, serde_json, memchr, itoa) 17 of 17 on each port (base 1c.11; the verification run, with the rustc outputs rebuilt from scratch, on base 1c.12) |
| T5 | `Triple::Feos` and a clang driver for feos, compiler-rt, libunwind, libc++abi and libc++ for both feos targets on relibc, C and C++ (and C++ with Rust) on feos (`ports/build-clang.sh`, `ports/build-runtimes.sh`, `ports/test/stage-t5.sh`) | built and tested, awaiting verification: the driver's lit test passes; `specs/t5-toolchain.toml` 10 of 10 on each port (base 1c.12) |

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
| `sysroot/<arch>-unknown-feos/` | the feos sysroot, from `ports/sysroot.sh` (below) |
| `libc-suite/<arch>/` | the libc suite's payloads: `bin/` (`testrun`, `sysroot-check`), `relibc-tests/`, `libc-test/`, `selftest/`; the binaries of every test, pending ones included, in `relibc-build/` and `libc-test-build/` |
| `libc-suite/linux-<arch>/`, `libc-suite/testrun-host` | the suite's Linux reference (`ports/test/libc-suite-linux.sh`) |
| `libc-suite/boot-{arm,x64}.log` | each port's last suite run, which `ports/test/suite-report.py` reads |
| `libunwind/<arch>-unknown-feos/` | LLVM's libunwind, static, from `ports/build-libunwind.sh` (T3) |
| `rust-src-sysroot/` | the rustc copy whose `lib/rustlib/src/rust` is `ports/src/rust`, for `-Zbuild-std` (T3) |
| `std-target/` | std's cargo output, every feos target (T3) |
| `rust-sysroot/lib/rustlib/<arch>-unknown-feos/lib/` | std for feos with the C sysroot's libraries, what `rustc --sysroot` takes (T3) |
| `libc-check/<arch>/` | the libc crate's layout check: rustdoc JSON, the probe, the generated C and C++ (T3) |
| `t3/<arch>/` | stage T3's Rust programs, from `ports/test/rust/build.sh` |
| `rust-build/` | rust's bootstrap output (T4): LLVM's build and install (`x86_64-unknown-linux-gnu/llvm`, with clang), lld, stage 0 and stage 1 |
| `rust-feos/` | the feos Rust toolchain, from `ports/build-rust.sh` (T4) |
| `llvm-feos/` | the feos C/C++ toolchain, from `ports/build-clang.sh` and `ports/build-runtimes.sh` (T5) |
| `runtimes/<arch>-unknown-feos/`, `runtimes-build/` | LLVM's runtimes for feos, installed and their build trees (T5) |
| `sysroot-libc/<arch>-unknown-feos/` | the sysroot with relibc only, what the runtimes build against (T5) |
| `t5/<arch>/` | stage T5's programs (`ports/test/t5/build.sh`) |
| `clang-lit/` | the lit site configuration and test output of `ports/test/clang-lit/run.py` (T5) |
| `t4/` | stage T4's host-side output: the spec comparisons, `t3/<arch>/` (T3's programs rebuilt), `<arch>/t4-cargo`, `cargo-target/` |
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

## The sysroot (T2)

`ports/sysroot.sh x86_64|aarch64|all` lays relibc's feos build out as the sysroot of
`docs/selfhost-plan.md` phase 2, in `/build/sysroot/<arch>-unknown-feos`. It builds
nothing; it runs after `ports/build-relibc-feos.sh`:

| Path | What |
|---|---|
| `usr/include/` | relibc's headers (cbindgen's and its own, openlibm's) |
| `usr/lib/crt0.o`, `crt1.o` | relibc's start; `crt1.o` is a copy, for drivers that name that one |
| `usr/lib/crti.o`, `crtn.o` | the `.init`/`.fini` prologue and epilogue |
| `usr/lib/libc.a` | relibc with openlibm |
| `usr/lib/libm.a` | openlibm |
| `usr/lib/libcompiler_builtins.a` | compiler-rt's builtins for C code (Rust's build of them) |
| `usr/lib/lib{pthread,dl,rt,crypt,util,resolv,xnet}.a` | empty archives, as relibc's own install and musl's: `libc.a` has all of it, and build systems still pass `-lpthread` |
| `usr/share/feos-sysroot/MANIFEST` | size and FNV-1a hash of every file above |

With T5's runtimes built, `sysroot.sh` adds them: `usr/include/c++/v1` (libc++'s
headers), `usr/lib/libc++.a`, `libc++abi.a`, `libc++experimental.a`, `libunwind.a`, and
compiler-rt in `usr/lib/clang/23/lib/<triple>/` (1900 files per architecture in all).
`sysroot.sh ARCH --libc-only` makes the relibc-only tree in `/build/sysroot-libc`.

No `libc.so`, `ld.so` or `/lib/ld64.so.1`: feos runs static programs until kernel stage
1f.1, when the plan's `libc.so` joins `usr/lib`. The tree is plain files (the feos image
has no links) with no dotfiles (the image leaves them out). The C driver takes it as
`FEOS_SYSROOT` (it finds either relibc's `include/`+`lib/` or this `usr/` layout).

Installed in the feos image: `ports/test/libc-suite.sh` puts the sysroot's `usr/` at
`/usr` in the base's image for each architecture (`inject.sh`, through the overlay on
`boards/qemu-virt/fs/`), and `/bin/sysroot-check` checks every file on feos against the
manifest (size, hash, and the `!<arch>` and ELF magic of the archives and objects). On
feos that is `/usr/include` with 189 headers and `/usr/lib` with 10 archives and 4
objects (203 files, 12 MB per architecture), ready for a native compiler (track T4 on). Into the tree's own image
(`boards/common/fs_image_build.rs`'s `ASSETS`) it goes when the kernel workflow takes it:
see deviations.

## The libc crate for feos (T3)

`ports/src/libc` (the libc crate at 0.2.189, the version rust 1.99.1's `library/Cargo.lock`
names) has a `target_os = "feos"` module in the unix family, `src/unix/feos/mod.rs`:
relibc's feos types, structures, constants and functions, the same on both
architectures but for `wchar_t`, `c_char` and the `ucontext_t`/`mcontext_t` of section
8.4. feos's numbers are Linux's (section 5), relibc's own structures stay relibc's
(`stat`, `sigset_t`, the pthread types as opaque blocks of relibc's sizes),
`POSIX_SPAWN_*` are glibc's bits (section 10), and `__feos_futex_wait`/`_wake` are
std's futex. `src/unix/mod.rs` selects the module and links `libc.a` statically;
`src/new/mod.rs` gives feos the `new::relibc` module (`ports/PATCHES.md`).

### The layout check

`ports/test/libc-check/libc-check.sh [x86_64|aarch64 ...]` checks the module against
relibc's feos headers and libraries, as the libc crate's own `ctest` would (ctest
itself needs crates from crates.io this tree doesn't vendor; this needs only rustc,
rustdoc and clang, and runs offline in the toolchain image after `ports/build-std.sh`):

1. `rustdoc --output-format json` of the crate for `<arch>-unknown-feos` lists
   everything its root exports there, cfgs resolved: 147 types and structures (144 on
   AArch64), their 279 (272) public fields, 1049 integer constants, 752 C functions, 2
   statics.
2. A generated probe crate, compiled to an object file against the crate, holds rustc's
   own numbers for each (`size_of`, `align_of`, signedness, `offset_of!` and each field's
   size, each constant's value), read out of the object.
3. A generated C file asserts each number against relibc's headers (all 103 public ones
   included together, `_GNU_SOURCE`) with `_Static_assert`: `sizeof`, `_Alignof`,
   `offsetof`, the field's `sizeof`, the constant's value at the Rust constant's type
   and that the type holds the C value. A generated C++ file checks each function's
   signature: every parameter and the return value as size and class (signed or
   unsigned integer, float, pointer, struct of its size, void), and whether it is
   variadic, through templates over the declared function's type. Both compile for
   `<arch>-unknown-none-elf` with `-fsyntax-only`; each error line is mapped back to its
   item, with C's value when it differs. Each function and static must also be defined
   in `libc.a` or `libm.a`.

`ports/test/libc-check/allow.txt` lists the 35 items allowed to differ, each with its
reason: the crate's own aliases (`Ioctl`, `sighandler_t`), signal handlers as integers,
`ioctl` declared variadic as on Linux (the same registers), unix-wide constants and
functions relibc doesn't have (`SIGIOT`, `PRIO_MAX`, `fmemopen`, `times`, ...),
`in6_addr`'s alignment. Anything else fails the check, and an allowed item that passes
again is reported. The check found, and T3 fixed: `posix_spawn_file_actions_t` 24 bytes
where relibc's is 32 (std's `Command` keeps one on its stack, which relibc's
`posix_spawn_file_actions_init` overran), the `FNM_*` values (relibc's are BSD's),
`termios`' `__c_ispeed`/`__c_ospeed`, `fpos_t` shadowed by the crate's opaque unix one,
and `STD*_FILENO` defined twice (ambiguous to every user of `libc::STDIN_FILENO`).

## std for feos (T3)

`ports/src/rust/library` (rust 1.99.1's) builds std for `target_os = "feos"` on the
unix path, each change in `ports/PATCHES.md`:

| Piece | On feos |
|---|---|
| `libc` | `ports/src/libc` through `library/Cargo.toml`'s `[patch.crates-io]` (the same version, so the lockfile and `ports/vendor` are unchanged) |
| `std::os::feos` | `raw` (`stat` and its types, as relibc's) and `fs::MetadataExt`; `std::os::unix` re-exports it as its platform |
| args, env | the unix arms: libc keeps `argc`/`argv` (relibc's `_start` reads the System V stack), `environ`; `env::consts` `unix`/`feos` |
| fs | Linux's arms where relibc has the call: `fstatat` and `dirfd` for `read_dir`, `/proc/self/fd` for paths, `fdatasync`, `flock` for `File::lock`, `pread`/`pwrite` |
| threads, locks, time | pthreads (relibc's over `THREAD_CREATE`), the futex locks over `__feos_futex_wait`/`_wake` (`FUTEX_WAIT2` with absolute `CLOCK_MONOTONIC` deadlines), `available_parallelism` from `sysconf`, `clock_gettime`, `nanosleep` |
| process | `posix_spawn` (relibc's over `SPAWN2`) with `addchdir_np` and glibc's `POSIX_SPAWN_SETSID`; `waitpid` is `WAIT4`, `kill` `KILL`; pipes from `pipe2(O_CLOEXEC)`; `Command::output` reads two pipes with `poll` |
| stack overflow | `SIGSEGV` handler on a `sigaltstack` sized from `AT_MINSIGSTKSZ`; the main thread's guard below the 8 MiB stack (the kernel's, section 6), each thread's below the stack `pthread_getattr_np` reports (relibc maps it there) |
| random | `getrandom` (`GETRANDOM`, stage 1e.1), `GRND_INSECURE` for `HashMap` keys. Before 1e.1 (`ENOSYS`): `HashMap` keys from the 16 `AT_RANDOM` bytes the kernel gives every program, mixed with the clocks and a per-call counter; `fill_bytes` from `/dev/urandom` if there is one, otherwise a panic that names 1e.1. Nothing claims randomness it doesn't have |
| unwinding | `panic_unwind` over LLVM's libunwind (`library/unwind` links `libunwind.a`), which finds the program's frames through `dl_iterate_phdr` (relibc reports a static program from `AT_PHDR`) and `PT_GNU_EH_FRAME` (`--eh-frame-hdr`) |
| CPU features | `std_detect` reads `AT_HWCAP`/`AT_HWCAP2` as on Linux |

### Building it

```sh
ports/build-std.sh all           # or x86_64 / aarch64; in the toolchain image, offline
```

builds what it needs first (each a no-op when built and unchanged): cbindgen, relibc for
feos, the sysroot, and `ports/build-libunwind.sh`, LLVM's libunwind from rust's own
`src/llvm-project/libunwind` with the C driver and bootstrap's flags (static,
`_LIBUNWIND_IS_NATIVE_ONLY`, no C++ library), in `/build/libunwind/<arch>-unknown-feos/`.
Then:

1. `/build/rust-src-sysroot`: a copy of the image's rustc 1.99.1 (the binary and its
   shared libraries, so it takes this directory as its sysroot) with the host's rustlib
   linked in and `lib/rustlib/src/rust` pointing at `ports/src/rust`, so `-Zbuild-std`
   builds the patched `library/`.
2. `cargo build --release -Zbuild-std=std,panic_unwind -Zjson-target-spec` of an empty
   crate (`ports/test/rust/std-build`) for `ports/targets/<arch>-unknown-feos.json`
   with that rustc, `RUSTC_BOOTSTRAP=1`, crates from `ports/vendor`; cargo's output in
   `/build/std-target`. Cargo never looks at `-Zbuild-std` crates' sources for changes,
   so the script rebuilds a target's std from scratch when anything in `library/`, the
   libc crate or the spec is newer than its last build.
3. `/build/rust-sysroot/lib/rustlib/<arch>-unknown-feos/lib`: std's rlibs and the feos
   sysroot's `crt*.o` and archives, with `libunwind.a`.

A program then builds with rustc alone (`ports/test/rust/build.sh` does, for the T3
tests):

```sh
rustc -Zunstable-options --edition 2021 -O --target ports/targets/x86_64-unknown-feos.json \
      --sysroot /build/rust-sysroot -L native=/build/rust-sysroot/lib/rustlib/x86_64-unknown-feos/lib \
      hello.rs                 # RUSTC_BOOTSTRAP=1 for the JSON target
```

or with cargo, `-Zbuild-std` as above, or `--sysroot` and the `-L` in `RUSTFLAGS`
(cargo needs `-Zjson-target-spec`). A static `ET_EXEC` at the image base, about 800 KB
for a hello (unstripped of symbols; debuginfo off).

## rustc with the feos targets built in (T4)

`ports/src/rust` (rust 1.99.1) has `x86_64-unknown-feos` and `aarch64-unknown-feos` as
built-in tier 3 targets, and builds as a whole toolchain offline: LLVM, clang and lld
from its own `src/llvm-project`, rustc, std for both feos targets and x86_64 Linux,
cargo and rust-src. Each changed file is in `ports/PATCHES.md`:

| Piece | What |
|---|---|
| `compiler/rustc_target` | `Os::Feos`; `spec/base/feos.rs` (unix family, `rust-lld` with the `gnu-lld` flavour, the image-base and segment flags, `crt0.o`/`crti.o` before and `crtn.o` after a static executable, `-lc -lm -lcompiler_builtins` last, static only, TLS, `panic=unwind`, inline stack probes, relro off); `targets/x86_64_unknown_feos.rs` (`x86-64` without AVX or XSAVE, `pic` code in a non-PIE executable, no PLT) and `targets/aarch64_unknown_feos.rs` (`+v8a,+neon,+fp-armv8`, static code, non-leaf frame pointers, `--fix-cortex-a53-843419`). They are T1's JSON specs as code: `ports/test/rust/t4-host.sh` compares `--print target-spec-json` field by field with what rust 1.99.1 makes of `ports/targets/*.json`. The consistency check that a `pic` target is dynamic or PIE exempts feos as it does UEFI (deviations). rustc_target's own tests (338, the feos targets' consistency checks among them) pass |
| `compiler/rustc_codegen_ssa` | feos's `lib/rustlib/<target>/lib` is on the linker's search path for native libraries, so `-lc`, `-lunwind` and the rest are found without `-L` |
| `src/bootstrap` | feos in `STAGE0_MISSING_TARGETS`; LLVM's libunwind in-tree by default, and copied into the target's library directory; relibc's `crt0.o`, `crti.o`, `crtn.o`, `libc.a`, `libm.a` and `libcompiler_builtins.a` copied there from the target C compiler (`cc -print-file-name=...`), so `rust-std` for feos is self-sufficient |
| `src/doc/rustc` | `platform-support/feos.md`, the tier 3 list entries |
| `tests/assembly-llvm/targets/targets-elf.rs` | a revision per target |
| `src/llvm-project` | `Triple::Feos` (`feos` as an OS name), ahead of T5's clang driver: `Triple.h` is in nearly every LLVM file, so adding it later would rebuild LLVM. Nothing in LLVM handles it differently from an unknown OS, so rustc's code for feos is what it was |

The C driver (`ports/toolchain/bin/<arch>-unknown-feos-cc`) learned what bootstrap asks
of a target's C compiler: `-print-file-name=NAME` answers the sysroot's `lib/NAME`, a
`--target=<arch>-unknown-feos` from cc-rs is dropped (the driver picks clang's target
itself), and `FEOS_SYSROOT_X86_64` / `FEOS_SYSROOT_AARCH64` give each architecture's
sysroot in one environment.

### Building it

```sh
ports/test/container.sh rust ports/build-rust.sh     # PORTS_FORCE=1 while feos-gate runs: it boots nothing
timeout 570 docker wait feos-ports-rust              # repeat until it prints the status
```

`ports/build-rust.sh` makes relibc and the sysroot for both feos targets (no-ops when
built), then runs rust's `x.py build --stage 1 library cargo` with
`ports/rust-bootstrap.toml` (passed as `--config`, so nothing is written into the rust
tree), and lays the result out as `/build/rust-feos`, copying the library sources in as
rust-src itself (see the deviations below). The configuration:

| Setting | Value | Why |
|---|---|---|
| `build.rustc`, `build.cargo` | the image's 1.99.1 | stage 0 without a download; the same version, so bootstrap takes it as a local rebuild |
| `build.submodules`, `build.vendor` | `false`, `false` | bootstrap must not touch git; crates come from `ports/vendor` by `ports/.cargo/config.toml`'s source replacement (bootstrap runs cargo in `ports/src/rust`), with `locked-deps` |
| `llvm.download-ci-llvm`, `rust.download-rustc` | `false` | LLVM from `src/llvm-project`, rustc from source |
| `llvm.targets` | `X86;AArch64` | the two feos architectures (and the host) |
| `llvm.clang`, `rust.lld` | `true` | clang in the LLVM build (installed in `rust-build/x86_64-unknown-linux-gnu/llvm/bin`), lld as `rust-lld`, for T5 |
| `llvm.link-jobs`, `llvm.use-linker` | `1`, `lld` | static clang and LLVM tools link one at a time within the container's 9 GB |
| `build.target` | `x86_64-unknown-linux-gnu`, `x86_64-unknown-feos`, `aarch64-unknown-feos` | std for each |
| `target.<feos>.cc`, `cxx` | the C driver | libunwind's sources and `-print-file-name` |
| `build.optimized-compiler-builtins` | `false` | compiler-builtins' Rust code only, as T3's std |
| `rust.channel` | `nightly` | `-Z` flags (`--print target-spec-json`, `-Zbuild-std` later) without `RUSTC_BOOTSTRAP` |
| `build.extended`, `build.tools` | `true`, `cargo` | cargo (rust-src is laid out by `build-rust.sh`, not built by bootstrap) |
| `rust.deny-warnings` | `false` | an hours-long build shouldn't stop at a warning in a patched crate |

`/build/rust-feos` is the stage 1 sysroot (`bin/rustc`, `bin/rustdoc`, `lib/`,
`lib/rustlib/x86_64-unknown-linux-gnu` with `rust-lld`, `lib/rustlib/<feos target>/lib`
with std and relibc's files) plus `bin/cargo` and `lib/rustlib/src/rust` from the
rust-src component. With it on `PATH`:

```sh
rustc --target x86_64-unknown-feos -O hello.rs            # a static feos executable
cargo build --release --target aarch64-unknown-feos       # offline, under ports/ (the vendor)
```

No JSON spec, `--sysroot`, `-L` or `RUSTC_BOOTSTRAP`. A second run rebuilds only what
changed (bootstrap's stamps, ninja's for LLVM).

## clang for feos and LLVM's runtimes (T5)

`ports/src/rust/src/llvm-project` (LLVM 23.1.1, rust's pin) has feos as an OS in clang
as well as in LLVM's `Triple` (T4), and its runtimes build for feos. Every changed file
is in `ports/PATCHES.md`; nothing in LLVM's code generation changed, so rustc's output
is what it was.

| Piece | What |
|---|---|
| `clang/lib/Basic` | `FeosTargetInfo<X86_64TargetInfo>` and `<AArch64leTargetInfo>`: predefines `__feos__`, `__unix__`, `__unix`, `unix` (and `__ELF__`, as for every ELF target; `_REENTRANT` with `-pthread`). No `__linux__`. The C types are the targets' defaults, the ones relibc's headers and the libc crate were checked against in T1 to T3 (`wchar_t` `int` on x86_64 and `unsigned int` on AArch64, `wint_t` `int`, `char` unsigned on AArch64, `long double` 80-bit x87 and IEEE quad) |
| `clang/lib/Driver/ToolChains/Feos.{h,cpp}` | the feos toolchain, a `ToolChain` of its own (no GCC detection). Sysroot: `--sysroot`, else `<clang's bin>/../<triple>` when it exists (a cross toolchain carries its sysroot beside `bin/`), else `/` (clang running on feos). Headers: the resource directory's, `<sysroot>/usr/include/c++/v1` for C++, `<sysroot>/usr/include`. Libraries: `<sysroot>/usr/lib`. Defaults: compiler-rt, libc++, LLVM's libunwind, `ld.lld`, asynchronous unwind tables, `-fno-math-errno`; PIC code but not PIE on x86_64 (T1's deviation: the image is above the small code model's reach), static code on AArch64. The link: `ld.lld -static --image-base=0x1000400000 -z separate-loadable-segments -z max-page-size=4096 -z noexecstack -z norelro --eh-frame-hdr` (and `--fix-cortex-a53-843419` on AArch64), `crt0.o crti.o clang_rt.crtbegin.o`, the inputs, for C++ `-lc++ -lc++abi`, then `--start-group libclang_rt.builtins.a -lunwind -lc --end-group`, `clang_rt.crtend.o crtn.o`: the same layout and crt objects as the Rust targets. `-shared`, `-pie`, `-static-pie` and `-rdynamic` are errors until 1f.1 |
| `clang/test/Driver/feos.c` | the driver's lit test (with `Inputs/basic_feos_tree` and per-target resource directories) |
| `compiler-rt` | its `crt` (crtbegin/crtend) builds for CMake's `Feos` system |
| `libcxx`, `libcxxabi` | feos in libc++'s platform switches: pthreads, the default `ctype` table, Fuchsia's locale base API (only the `uselocale` family and plain functions, which relibc has), no `catopen`, futex-based `std::atomic::wait` over relibc's `__feos_futex_*`, `_LIBCPP_ELAST`, `__cxa_thread_atexit` over relibc's `__cxa_thread_atexit_impl`; `mbstate_t mb = {}` for relibc's empty `mbstate_t` |
| `relibc` | `wcstof` and `wcstold` (libc++ uses them) |

The Rust targets (scope item 3) needed nothing: their `llvm_target` was already
`<arch>-unknown-feos` (T4), and they keep linking with `rust-lld` directly, which needs
no C driver at link time and keeps `rustc` working on a host without the feos clang.
clang is the driver for C, C++ and mixed programs (below).

### Building it

```sh
PORTS_FORCE=1 ports/test/container.sh clang ports/build-clang.sh   # minutes, after T4's build
timeout 570 docker wait feos-ports-clang
PORTS_FORCE=1 ports/test/container.sh rt ports/build-runtimes.sh all
timeout 570 docker wait feos-ports-rt
```

`ports/build-clang.sh` runs ninja in T4's LLVM build tree
(`/build/rust-build/x86_64-unknown-linux-gnu/llvm/build`, configured by rust's bootstrap
from the same `llvm-project`), so only what the driver changed is rebuilt (15 steps:
clang's Driver and Basic libraries, the clang binary; lld, in its own build, is
unchanged), and lays out `/build/llvm-feos`, a self-contained C/C++ toolchain:

| Path in `/build/llvm-feos` | What |
|---|---|
| `bin/clang-23`, `clang`, `clang++`, `clang-cpp`, `<arch>-unknown-feos-clang{,++}` | clang (the prefixed names select the target) |
| `bin/lld`, `ld.lld` | lld (T4's) |
| `bin/llvm-{ar,ranlib,nm,objcopy,strip,objdump,readobj,readelf,size,symbolizer,addr2line}` | binutils |
| `lib/clang/23/include` | clang's headers |
| `lib/clang/23/lib/<triple>/` | compiler-rt: `libclang_rt.builtins.a`, `clang_rt.crtbegin.o`, `clang_rt.crtend.o` |
| `<triple>/` | the sysroot (a copy of `/build/sysroot/<triple>`, runtimes included), clang's default |

bootstrap's own install of LLVM (`rust-build/.../llvm/bin/clang`) is left as T4 built
it; bootstrap's next run sees the changed `llvm-project` (its LLVM stamp hashes the
submodule's diff), reruns cmake and ninja (the same no-op) and reinstalls.

`ports/build-runtimes.sh x86_64|aarch64|all` builds, with that clang, from
`llvm-project/runtimes` and `ports/cmake/<triple>.cmake` (a CMake toolchain file:
`CMAKE_SYSTEM_NAME Feos`, whose `ports/cmake/Platform/Feos.cmake` is a static-only Unix,
the feos clang and binutils, the sysroot; usable for any CMake project), against
relibc alone (`ports/sysroot.sh ARCH --libc-only`, `/build/sysroot-libc/<triple>`):

1. compiler-rt: the builtins (with the generic `__atomic_*` functions) and
   crtbegin/crtend (`__dso_handle`, `__cxa_finalize` from `.fini_array`; no frame
   registry, see deviations), per-target layout.
2. libunwind, libc++abi, libc++, static, as three archives (libc++abi doesn't fold in
   libunwind, nor libc++ libc++abi, so Rust and C++ share one libunwind): threads on
   relibc's pthreads, filesystem, localization, wide characters, random device
   (`/dev/urandom`), `__cxa_thread_atexit_impl`; no shared libraries, no time zone
   database.

into `/build/runtimes/<triple>`, then reruns `ports/sysroot.sh ARCH`, which now adds
them to the sysroot (`usr/include/c++/v1`, `usr/lib/libc++.a`, `libc++abi.a`,
`libc++experimental.a`, `libunwind.a`, and compiler-rt at `usr/lib/clang/23/lib/<triple>`,
the resource directory of a clang installed as `/usr/bin/clang` on feos) and to its
manifest (1900 files per architecture), and copies compiler-rt and the sysroot into the
toolchain. It is skipped when its inputs (this script, the CMake files, clang and lld,
the libc sysroot's manifest, the runtimes' diff) are unchanged; otherwise it rebuilds
from scratch (about four minutes per architecture).

With it, offline:

```sh
/build/llvm-feos/bin/clang --target=x86_64-unknown-feos hello.c -o hello
/build/llvm-feos/bin/clang++ --target=aarch64-unknown-feos -O2 hello.cpp -o hello
cmake -DCMAKE_TOOLCHAIN_FILE=ports/cmake/x86_64-unknown-feos.cmake ...
```

A C hello is about 3.3 MB and a C++ iostream hello 4.4 MB, unstripped and without
`--gc-sections` (clang doesn't pass it; `-Wl,--gc-sections` shrinks them).

### What relibc can't back yet

| What | State | Until |
|---|---|---|
| Shared libraries: `libc++.so`, `-shared`, PIE | not built; the driver refuses the flags | kernel stage 1f.1 (dynamic programs) |
| Locales other than `C`/`POSIX` | `std::locale("")` is the `C` locale (relibc's `newlocale` doesn't read `LANG` yet); a named locale needs relibc's locale files, which no feos image has, so `std::locale("de_DE.UTF-8")` throws | locale data in the image |
| `std::messages` catalogs | none (no `catopen` in relibc) | relibc `<nl_types.h>` |
| `std::chrono` time zones | `LIBCXX_ENABLE_TIME_ZONE_DATABASE=OFF` | a tzdata on feos |
| `std::random_device` | reads `/dev/urandom` (libc++'s default for an unknown platform) | `getrandom` (1e.1) could replace it, as std's does |
| `wcstold`, `strtold` precision | through `double` (relibc's `strtold` is `strtod`) | relibc `long double` parsing |
| `mbstate_t` | relibc's is an empty struct: 0 bytes in C, 1 in C++; harmless since relibc keeps no conversion state | relibc |

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
| `libc-suite.sh [REF]` | stage T2's suite on both ports (below; REF default `HEAD`) | container |
| `build-libc-suite.sh [ARCH...]` | builds what the suite puts in the image | container |
| `build-relibc-tests.sh`, `build-libc-test.sh [ARCH...]` | the two test payloads (`relibc_tests.py`, `libc_test.py`), selected by `relibc-feos.txt` and `libc-test-feos.txt` | container |
| `libc-suite-linux.sh [ARCH...]` | the same tests against relibc's Linux platform on the host, the reference | container |
| `suite-report.py [BUILD]` | the per-test table at the end of this file | host or container |
| `fresh.sh` | sourced by the scripts that build: rebuilds a base of another commit than the last build (below) | container |
| `test-fresh.sh [A B]` | the harness's test of that: builds at A, B, B, A and checks each kernel's commit | container |
| `libc-check/libc-check.sh [ARCH...]` | the libc crate's feos module against relibc (T3, above) | container |
| `rust/build.sh [ARCH...]` | compiles `rust/t3/*.rs` with rustc against the std sysroot, checks each binary's layout (static `ET_EXEC`, `PT_GNU_EH_FRAME`) | container |
| `stage-t3.sh [REF]` | stage T3's: std, `libc-check`, the programs, both kernels with them injected, `specs/t3-rust.toml` on each port (REF default `refs/feos-checkpoints/1c.11`) | container |
| `stage-t5.sh [REF]` | stage T5's (below: clang, the runtimes, the lit test, the programs, `specs/t5-toolchain.toml` on each port; REF default `refs/feos-checkpoints/1c.12`) | container |
| `t5/build.sh [ARCH...]` | builds `t5/*.c`, `*.cpp`, `*.rs` with the feos clang and the T4 rustc | container |
| `clang-lit/run.py [LIT ARGS] TEST...` | clang's lit tests against T4's LLVM build | container |
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

A spec's `[boot]` table may ask for `native_disk = MIB` (a blank native-format disk,
made by the base's `scripts/mkfs-feos.py`, mounted at `/native0` beside the RAM root)
and `memory = "1G"` (the guest's RAM; else `FEOS_BOOT_TEST_MEM`, or 512M).

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
| `ports/test/abi-check.sh` (host test, `ports/test/abi-check/`): relibc's `abi.rs` against the base's `feos-abi` (every structure's size and offsets, the constants both have, 62 call numbers and every error number in the base's `libs/abi` text) and against section 3's table (all 78 numbers 31 to 108); and the header constants relibc gives feos programs against the design (section 5's 18 signal constants, `SA_*` to `ITIMER_*` with `SIGSTKSZ` 8192, and section 10.2's `PTHREAD_STACK_MIN` 16384) and, on a base with stage 1c.9, the kernel's `ss` | 6/6 pass on base 1c.11 (75 call numbers); 5/5 on base 1c.6 before the header test; also with `FEOS_ABI_LIB` at the working tree's `libs/abi` (67 call numbers, through 1c.8) |
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

### T2

One entry point runs the whole suite on both ports, and is meant to become the boot-test
phase `libc` once the kernel's phase 1 is in:

```sh
ports/test/container.sh libc ports/test/libc-suite.sh [REF]   # default HEAD
timeout 570 docker wait feos-ports-libc                       # repeat until it prints the status
ports/test/container.sh linux ports/test/libc-suite-linux.sh  # the Linux reference (optional)
python3 ports/test/suite-report.py BUILD                      # the per-test table below
```

`libc-suite.sh` (offline, in the toolchain image) builds cbindgen and relibc for feos
(no-ops when built), the sysroot, `/bin/testrun` and `/bin/sysroot-check`
(`ports/test/c/suite`), relibc's tests (`build-relibc-tests.sh`) and the libc-test subset
(`build-libc-test.sh`); makes a base of REF; installs the sysroot at `/usr` and the
payloads under `/usr/share/libc-suite/{selftest,relibc,libc-test}`; builds both kernels;
and runs `specs/libc-suite.toml` on each port. `PORTS_SUITE_STEPS=build` builds only
(fine while `feos-gate` runs, with `PORTS_FORCE=1`), `=boot` boots kernels built before;
`PORTS_SUITE_PORTS`/`_ARCHES` narrow it to one port.

- **The runner.** `testrun LIST` runs each test with `posix_spawn`, stdin `/dev/null`,
  stdout and stderr into files, and judges it: `expect` (relibc's tests with an
  `expected/` output: exit 0 and stdout and stderr byte for byte), `status` (exit 0;
  libc-test's convention and relibc's other tests), `pending NAME STAGE WHY` and `skip
  NAME WHY` (not run). It prints one line per test and `testrun: LIST: P passed, F
  failed, N pending, S skipped`. A test that overruns its timeout fails as `TIMEOUT` and
  is killed with `SIGKILL` (on a base before `KILL`, 1c.8, it's left running). testrun
  ignores every signal it can but `SIGCHLD` and starts each test with all of them at
  their defaults (`POSIX_SPAWN_SETSIGDEF`), so a test that signals its process group or
  every process can't end the run: relibc's `signals/kill-child` does `kill(-1, SIGALRM)`
  when `fork` fails, which ended the record case on base HEAD before this.
- **relibc's tests.** `relibc_tests.py` compiles every test relibc's
  `tests/Makefile.tests.mk` names with relibc's flags (a test that doesn't build fails the
  build: all 323 do, on both), and lays out the payload as relibc's own runner expects
  (`bins_static/`, `expected/`, `example_dir/`, input files). `ports/test/relibc-feos.txt`
  lists the tests feos doesn't run, with the kernel stage or the reason.
- **libc-test.** `libc_test.py` builds `src/common` as `libtest.a` (without libc-test's
  own runner, which needs `fork`) and every test of `functional/`, `regression/` and
  `math/` with libc-test's `config.mak.def` flags. `ports/test/libc-test-feos.txt`
  lists the ones feos doesn't run: per architecture (`NAME@aarch64`), as pending on a
  stage, `skip`, or `upstream` (a relibc or openlibm limitation: not in relibc, so not
  built, or failing the same on relibc's own Linux platform).
- **The Linux reference.** `libc-suite-linux.sh` builds the same tests against relibc's
  Linux platform and runs them on the host with the same runner (AArch64 under
  qemu-aarch64, where relibc's threads can't start, so the AArch64 column says nothing
  about thread tests). A test that fails there too is relibc's, not feos's.

Spec cases, on a base of HEAD (`c7a5bb6e`, through stage 1c.9):

| Case | What | arm | x64 |
|---|---|---|---|
| `sysroot` | `/bin/sysroot-check`: every file of `/usr/include` and `/usr/lib` against the manifest | pass | pass |
| `testrun-self` | the runner's own verdicts: a pass, a failure, an output mismatch, a pending and a skipped test, a test that signals its process group (testrun outlives it; the test starts with the signal at its default), and a timeout, killed, with the right summary and exit status | pass | pass |
| `relibc-tests` | `relibc.list`: 235 passed, **0 failed**, 65 pending, 25 skipped | pass | pass |
| `libc-test` | `libc-test.list`: x64 194 passed, **0 failed**, 13 pending, 136 skipped; arm 138 passed, **0 failed**, 13 pending, 192 skipped | pass | pass |
| `relibc-all-record` | for the record, not judged: every built relibc test, the pending and skipped too (267 of 323 pass on each) | pass | pass |
| `libc-test-all-record` | the same for libc-test (x64 197 of 311, arm 140 of 312 pass: openlibm doesn't build `nearbyintl` on x86_64) | pass | pass |

What runs, by area (relibc's directories; libc-test's `functional/` and `regression/`):
strings and wide strings (relibc `string/*`, `strings`, `wchar/*`, `wctype/*`; libc-test
`string*`, `wcsstr`, `memmem-oob*`, `wcsncpy-read-overflow`), stdio (relibc `stdio/*` but
`popen`, pending 1c.12, and `setvbuf`, 1d.1; libc-test `fdopen`, `sscanf_long`,
`printf-fmt-*`, `scanf-*`, `rewind-clear-error`, `fgetwc-buffering`), stdlib (relibc
`stdlib/*`; libc-test `qsort`, `random`, `malloc-0`, `malloc-oom`, `putenv-doublefree`,
`mkstemp-failure`), math (relibc `math`; libc-test `math/`, x64 135 and arm 79 run, the
rest below), time (relibc `time/*`, `sys_time/utimes`; libc-test `clock_gettime`, `time`,
`utime`), pthread (relibc `pthread/*`, `semaphore/lock`, `tls`; libc-test
`pthread_cond`, `pthread_tsd`, `sem_init`, `tls_align`, `tls_init`, `tls_local_exec`,
`pthread_cond-smasher`, `pthread_create-oom`, `pthread_once-deadlock`,
`pthread_rwlock-ebusy`, `pthread_exit-cancel`), and files (relibc `fcntl/*`, `dirent/*`,
`sys_stat/*`, `unistd/*` under `/tmp`, `mknod`, `stdlib/realpath`, `sys_statvfs`;
libc-test `stat`, `lseek-large`, `mkdtemp-failure`).

Every test not run, by reason:

| Reason | relibc's tests | libc-test |
|---|---|---|
| pending on a kernel stage | 65: 1c.10 34, 1c.11 6, 1c.12 3, 1d.1 13, 1e.3 4, 1e.4 3, 1f.1 2 | 13: 1c.10 3, 1c.12 2, 1d.1 3, 1e.3 1, 1f.1 4 |
| relibc or openlibm upstream: not in relibc (`search.h`, `iconv.h`, `wordexp.h`, `mntent.h`, System V IPC, named semaphores, `open_memstream`/`fmemopen`, `clearenv`, `strverscmp`, `vfork`, `resolv.h`, a replaceable `malloc` in a static `libc.a`; openlibm's `exp10`, `pow10`, `scalb`, `drem`) | - | 31 |
| relibc upstream: fails the same on relibc's Linux (openlibm's results and exception flags, `fnmatch`, `crypt`'s `$2a$`, `MB_CUR_MAX` in the C locale, `sscanf`'s `%f%c` at EOF, `sigprocmask` of internal signals, ...) | - | x64 99, arm 138 (97 on both; x64 2 more, `fma` and `round`, which pass on feos arm; arm 41 more: `powf`, which passes on feos x64, and 40 for openlibm's 128-bit long double on AArch64) |
| AArch64 only: exception flags lost to clang 14 (no strict floating point on AArch64; the values are right) | - | 20 |
| x86_64 only: QEMU TCG's x87 `fprem1` gives no quotient bits, which openlibm's `remquo` reads (right on hardware) | - | 3 |
| feos as designed (`docs/feos-abi-v2.md`): no epoll, POSIX timers, PTYs, named FIFOs, `brk`, named `AF_UNIX` sockets or `SCM_RIGHTS`; `setrlimit`/`setpriority` accepted with no effect; no hard links or modes on the RAM root; `FSTATVFS` counts no files there; `mmap` below the user window | 18 | 3 |
| test assumptions that don't hold (gcc's argument order, `/dev/console` unopenable, `malloc_usable_size`'s exact value, `futimens`'s bad flags, C++ needing libc++) | 7 | - |

Bugs the suite found and fixed (`ports/PATCHES.md`, each a relibc bug on every platform
or every clang build, so upstreamable): AArch64 TLS with a `PT_TLS` aligned past 16
(`functional/tls_align`), `sem_wait` failing with `EAGAIN` when a post raced the wait
(libc-test's semaphore and pthread tests), `<math.h>` hiding openlibm's BSD and
X/Open functions from `_GNU_SOURCE` programs (`j0`, `lgamma_r`, `signgam`),
`<stdatomic.h>` not compiling under clang, and openlibm's amd64 `fenv` functions using
bits outside `FE_ALL_EXCEPT` (`math/fenv`: `fetestexcept(-1)` returned 32640, and
`feraiseexcept(1234567|FE_ALL_EXCEPT)` loaded the x87 status word's ES bit, so the next
`fwait` raised #MF under QEMU TCG, which doesn't recompute ES as hardware does: status
139 on feos x86_64, while the same binary on the host's hardware went on to exit 1).
On base HEAD (1c.9's signals) two more: AArch64's `jmp_buf` was 22 words, all `setjmp`'s,
while `sigsetjmp` keeps its return address, mask and `x19` in words 22 to 24, past the
caller's buffer, and `__sigsetjmp_tail` kept the mask in x86_64's word 9, which on
AArch64 is the saved `x28` (`signals/sigsegv`: a second fault after `siglongjmp`); and
`alarm()` was a POSIX timer, which feos doesn't have, so it did nothing: on feos it is
`setitimer(ITIMER_REAL)`, as the ABI maps it (`unistd/alarm`, `alarm_fn` now pass, still
pending on 1c.10 with the rest of the signal masks). The per-test timeouts are 120 s for
relibc's tests and 60 s for libc-test's.

Pending in T2, by kernel stage (the tests are built and in the image's build dir; each
moves into the run list when its stage's checkpoint is the base):

| Stage | relibc's tests | libc-test |
|---|---|---|
| 1c.10 masks, `THREAD_KILL`, `SIGALTSTACK`, `SIGSUSPEND`, `SETITIMER` | the rest of `signals/` (`sigprocmask-*`, `sigset-*`, `sigrelse-*`, `sigpause-*`, `pthread_kill*`, `signal-handler*`), `sigaction`, `sigaltstack`, `signal`, `alarm`, `alarm_fn`, `stackoverflow` | `functional/setjmp`, `regression/sigaltstack`, `regression/raise-race` |
| 1c.11 `POLL`, `IOCTL` | `poll/pollnval`, `select`, `pselect`, `isatty`, `unistd/getpass` | - |
| 1c.12 `/bin/sh` | `stdio/popen`, `stdlib/system`, `unistd/exec` (`execv("/bin/sh")`; `EXEC` itself is 1c.7) | `functional/popen`, `regression/execle-env` (the same) |
| 1d.1 `FORK` | `unistd/fork`, `waitpid`, `kill-waitpid`, `poll/poll`, ... | `functional/fcntl`, `regression/daemon-failure`, `regression/fflush-exit` |
| 1e.3, 1e.4 sockets, `RESOLVE` | `sys_socket/*` over `socketpair` and TCP, `netdb/getaddrinfo` | `functional/socket` |
| 1f.1 dynamic programs | `dlfcn`, `dlopen_scopes` | `functional/dlopen`, `tls_align_dlopen`, `tls_init_dlopen`, `regression/tls_get_new-dtv` |

### T3

One entry point, offline in the toolchain image:

```sh
ports/test/container.sh t3 ports/test/stage-t3.sh [REF]     # default refs/feos-checkpoints/1c.11
timeout 570 docker wait feos-ports-t3                       # repeat until it prints the status
```

`stage-t3.sh` runs `ports/build-std.sh all` (relibc, the sysroot, libunwind and std, each
a no-op when built and unchanged), `ports/test/libc-check/libc-check.sh` for both
architectures, `ports/test/rust/build.sh` (the programs), makes a base of REF, injects
the programs as `/bin/t3-*`, builds both kernels and runs `specs/t3-rust.toml` on each
port, with a native disk at `/native0` and 1 GiB of RAM.

Host side, `libc-check`: 2651 checks on x86_64 (147 sizes, 81 alignments, 62
signednesses, 279 field offsets and 279 field sizes, 1049 constants, 752 function
signatures, 2 statics) and 2631 on AArch64; 2616 and 2596 pass, the 35 differences
`allow.txt` explains are the rest, 0 fail.

The programs (`ports/test/rust/t3/*.rs`, ten per architecture, each built by
`rustc -O` with panic=unwind and checked a static `ET_EXEC` at 0x1000400000 with a
`PT_GNU_EH_FRAME` and no W+X segment), on a fresh `git archive` of
`refs/feos-checkpoints/1c.11` (`5138883c`):

| Case | What | arm | x64 |
|---|---|---|---|
| `hello` | `println!`, arguments with a space | pass | pass |
| `exit-status` | `process::exit(42)` seen by the shell | pass | pass |
| `args-env` | `env::args`/`args_os` from relibc's start, `VAR=value cmd` from the shell, `set_var`/`remove_var`, `current_exe` (`/proc/self/exe`), `set_current_dir`, `temp_dir`, `env::consts` | pass | pass |
| `fs` | create, write, append, seek, `set_len`, `sync_all`/`sync_data`, `create_new`, `NotFound`; metadata (len, ino, nlink, mode, uid, file type, times, `os::feos::fs::MetadataExt`); `File::set_times`/`set_modified`; `read_dir` with file types, `DirectoryNotEmpty`; rename (over a file), `copy`, `remove_file`; symlinks (`read_link`, `symlink_metadata`, `canonicalize`); `File::lock`/`try_lock_shared`; permissions; `remove_dir_all` (under `/tmp`, the RAM root) | pass | pass |
| `fs-links` | hard links (same ino, nlink 2 then 1, a link to a symlink), `set_permissions` 600, read-only 400, 755 (on `/native0`) | pass | pass |
| `threads` | 8 threads on one `Mutex`, `Condvar` with `wait_timeout`, `mpsc` with 4 producers and `sync_channel(0)`, `thread_local!` with destructors, `Barrier`, `RwLock`, `Once`, `LazyLock`, `Builder` names and stack sizes (`stack_size(1)` gives a 16384-byte stack, `PTHREAD_STACK_MIN`, by `pthread_getattr_np`; `sysconf(_SC_THREAD_STACK_MIN)` 16384), `scope`, `park`/`unpark`, 200 short threads | pass | pass |
| `time` | `Instant` monotonic, `sleep` 200 ms and 1.5 ms, `SystemTime` since the epoch, `checked_add`/`duration_since` | pass | pass |
| `heap` | 200,000 small blocks freed out of order, a `Vec` grown to 64 MiB by realloc and shrunk, a 128 MiB block touched per page, `HashMap`/`BTreeMap` of 100,000, the seed under `RandomState` (std adds 1 to a per-thread cached key per map, so one thread's maps always differ and prove nothing): `getauxval(AT_RANDOM)` present and not all zero, different in two child programs and from the parent's, and `RandomState` keys different in 8 fresh threads and in the 2 child programs (before 1e.1 `hashmap_random_keys` is the `AT_RANDOM` fallback: no `GETRANDOM`), 4 threads allocating at once | pass | pass |
| `panic-unwind` | `catch_unwind` through 21 frames running 21 destructors, panic hooks, `resume_unwind` with a payload, a panicking thread's `join`, nested panics, index and `expect` panics | pass | pass |
| `panic-report` | the default hook's message on stderr | pass | pass |
| `panic-exit` | an uncaught panic: the message, exit status 101 | pass | pass |
| `stdin-pipe` | `io::stdin` (`read_line`, `read_to_string`) from a shell pipe | pass | pass |
| `process` | `Command` (`posix_spawn` over `SPAWN2`): stdout piped, arguments, exit codes, stderr piped apart, `env`/`env_clear`, `current_dir`, a child's stdin piped (1000 lines), `try_wait`, `kill` (signal 9 in the status), a panicking child (101), `NotFound`, `PATH` search | pass | pass |
| `process-output` | `Command::output`, stdout and stderr piped at once (`poll`, 1c.11) | pass | pass |
| `stack-overflow` | main thread and a 128 KiB thread overflow their stacks: std's handler reports `thread '...' (tid) has overflowed its stack` and the child dies of `SIGABRT` | pass | pass |

On `refs/feos-checkpoints/1c.10` (`f0f1d692`) the other 14 cases pass too
(`process-output` was then marked pending: `poll` is `ENOSYS` there, and
`Command::output` fails with it). The T2 suite, rerun on HEAD
with T3's relibc changes (`uid_t`, `errno`, `abort`, `fchmodat`, `dl_iterate_phdr`,
`pthread_getattr_np`), is unchanged: relibc 235 passed, libc-test 194 (x64) / 138 (arm)
passed, 0 failed.

The stage gate (one `--network none` run with T3's outputs removed first: libunwind,
both `rust-src-sysroot` and `rust-sysroot`, `std-target`, `libc-check`, `t3`) rebuilt
libunwind and std for both feos targets, ran `libc-check` (2616 / 2596 passed, 35
allowed, 0 failed), built the twenty programs, and ran `ports/test/stage-t3.sh` on a
fresh `git archive` of `refs/feos-checkpoints/1c.11` (`5138883c`, a descendant of HEAD
`c7a5bb6e`): 15 passed, 0 failed, 0 pending on each port.

Pending in T3, by kernel stage (nothing in the spec is pending on 1c.11):

| Stage | What |
|---|---|
| 1d.1 `FORK` | `Command`s std can't express as `posix_spawn`: `pre_exec`, `uid`/`gid`/`groups`, `chroot`, a `PATH` set in the child's environment for a bare program name; they fail with `ENOSYS` until then |
| 1e.1 `GETRANDOM` | `getrandom`/`fill_bytes` (and so `rand`-style users of std's randomness): until then `HashMap` keys come from `AT_RANDOM` and `fill_bytes` needs `/dev/urandom` (else it panics, naming 1e.1) |
| 1e.3 sockets | `std::net` (TCP, UDP), compiled in and untested |
| 1f.1 dynamic programs | `-C prefer-dynamic`, `dylib` crates, `libstd.so` |

### T4

The toolchain, then the tests, offline in the toolchain image:

```sh
PORTS_FORCE=1 ports/test/container.sh rust ports/build-rust.sh   # hours the first time
timeout 570 docker wait feos-ports-rust                          # repeat until it prints the status
ports/test/container.sh t4 ports/test/stage-t4.sh [REF]          # default refs/feos-checkpoints/1c.11
```

`stage-t4.sh` runs `ports/build-rust.sh` (a no-op beyond bootstrap's checks and the
toolchain copy when built; `PORTS_T4_NO_BUILD=1` skips it), `ports/test/rust/t4-host.sh`,
makes a base of REF, injects T3's ten programs as rebuilt by this rustc (`/bin/t3-*`,
the names T3's spec and programs use) and the cargo project (`/bin/t4-cargo`), builds
both kernels and runs `specs/t3-rust.toml` and `specs/t4-rust.toml` on each port, with
T3's native disk and 1 GiB of RAM.

The first build (`--network none`, 6 CPUs, 9 GB): LLVM with clang (4509 ninja steps)
and lld, rustc stage 1, std for `x86_64-unknown-linux-gnu`, `x86_64-unknown-feos` and
`aarch64-unknown-feos` (with LLVM's libunwind built by bootstrap for each feos target),
and cargo (`all-static`) took about five hours of wall time, much of it with the
container paused while the kernel workflow's boot gates ran. `rustc -vV`:
`rustc 1.99.1-nightly (58cf8f9c3 2026-10-13)`, `LLVM version: 23.1.1`; `cargo
1.99.1-nightly`.

Host side, `t4-host.sh` (no `RUSTC_BOOTSTRAP`, JSON spec, `--sysroot` or `-L`):

| Check | x86_64-unknown-feos | aarch64-unknown-feos |
|---|---|---|
| in `rustc --print target-list` | pass | pass |
| `--print target-spec-json` equals rust 1.99.1's reading of `ports/targets/<target>.json` (every field; the two print byte for byte the same) | pass | pass |
| `--print cfg`: `target_os="feos"`, `target_family="unix"`, `unix`, `target_env=""`, `crt-static` | pass | pass |
| T3's ten programs built with `rustc --target <target>`, each a static `ET_EXEC` at the image base with `PT_GNU_EH_FRAME` (`ports/test/rust/check-elf.sh`) | pass | pass |
| `cargo build --release --locked --offline --target <target>` of `ports/test/rust/t4-cargo` (serde with its derive, serde_json, memchr, itoa from `ports/vendor`; proc macros and build scripts built for the host alongside), checked the same way | pass (987 KB) | pass (1023 KB) |

and the same project for `x86_64-unknown-linux-gnu`, run on the host: pass.

On feos, a fresh `git archive` of `refs/feos-checkpoints/1c.11` (`5138883c`):

| Case | What | arm | x64 |
|---|---|---|---|
| `t3-rust/*` (15 cases) | T3's whole spec (above) with its programs built by this rustc and the built-in targets | 15 pass | 15 pass |
| `cargo-vendored` | the cargo project: a serde derive round trip through serde_json, memchr's SIMD searches over 1 MiB, itoa, arguments, threads using the dependencies | pass | pass |
| `cargo-arch-arm` | `env::consts` say `feos aarch64` | pass | (arm only) |
| `cargo-arch-x64` | `env::consts` say `feos x86_64` | (x64 only) | pass |

17 passed, 0 failed, 0 pending on each port. `ports/test/stage-t3.sh` rerun after T4's
changes to shared pieces (the C driver, `library/Cargo.lock`, std's `AT_RANDOM` read now
through `ptr::with_exposed_provenance`, which bootstrap's lints require): `libc-check`
2616 / 2596 passed, 0 failed, and 15 of 15 on each port, as before. Nothing in T4 is pending on a kernel stage
beyond T3's own pending list (above); no kernel issues were found.

### T5

The toolchain pieces, then the tests, offline in the toolchain image:

```sh
ports/test/container.sh t5 ports/test/stage-t5.sh [REF]     # default refs/feos-checkpoints/1c.12
timeout 570 docker wait feos-ports-t5                       # repeat until it prints the status
```

`stage-t5.sh` runs `ports/build-clang.sh`, `ports/build-relibc-feos.sh all` and
`ports/build-runtimes.sh all` (no-ops when built; `PORTS_T5_NO_BUILD=1` skips them), the
driver's lit test (`clang/test/Driver/feos.c` through `ports/test/clang-lit/run.py`,
which writes the lit site configuration T4's test-less LLVM build lacks and runs lit
from `llvm/utils/lit`), `ports/test/t5/build.sh` (the programs, each checked by
`check-elf.sh`), makes a base of REF, injects the programs as `/bin/t5-*`, builds both
kernels and runs `specs/t5-toolchain.toml` on each port (`PORTS_T5_NO_BOOT=1` stops before
the boots, so a build can overlap the kernel workflow's gates).

Host side:

| Check | Result |
|---|---|
| `clang/test/Driver/feos.c` (both arches: cc1 triple, relocation model, unwind tables, header search for C and C++, `-nostdinc`, `-nostdlibinc`, the link job and its crt objects and libraries, C++'s `-lc++ -lc++abi`, `-nostdlib`, `-nostartfiles`, `-nodefaultlibs`, `-shared` refused, the default `<bin>/../<triple>` and native `/` sysroots, the predefined macros and no `__linux__`) | pass |
| the neighbouring driver tests, rerun against the changed driver: `Driver/serenity.cpp`, `fuchsia.c`, `managarm.cpp`, `baremetal.cpp`, `linux-ld.c`, `Preprocessor/init.c` | 6 pass |
| the programs below for both arches, each a static `ET_EXEC` at the image base with `PT_GNU_EH_FRAME`; `t5-rust-panic` and `t5-mixed` take `_Unwind_RaiseException` from the sysroot's (T5's) `libunwind.a` (lld's `--trace-symbol`) | pass |

On feos, a fresh `git archive` of `refs/feos-checkpoints/1c.12` (`dbc40ff9`), 1 GiB guests:

| Case | Built with | What | arm | x64 |
|---|---|---|---|---|
| `sysroot` | `clang -O2` (libc-suite's `sysroot-check.c`) | the sysroot with the runtimes, installed at `/usr` in the image, against its manifest: 1900 files per port | pass | pass |
| `c-hello` | `clang --target=<arch>-unknown-feos c-hello.c` (no other flags) | stdio, arguments, `strdup`; checks `__feos__`, `__unix__`, `__ELF__`, no `__linux__` | pass | pass |
| `c-hello-arch-{arm,x64}` | | the architecture macros | pass | pass |
| `cxx-hello` | `clang++ -O2` | iostreams, `std::string`, `std::map`, `std::sort`, `iomanip`, `istringstream`, a static object's constructor and destructor (crtbegin's `__dso_handle`, `__cxa_atexit`) | pass | pass |
| `cxx-hello-stderr` | | `std::cerr` reaches fd 2 | pass | pass |
| `cxx-exceptions` | `clang++ -O2` + a C file | a throw unwinding 21 frames with destructors, through a C frame (a callback), rethrow, `std::exception_ptr`, a throw and catch on a `std::thread`, a thrown `std::string` | pass | pass |
| `cxx-threads` | `clang++ -O2 -std=c++20 -pthread` | 8 `std::thread`s on a `std::mutex` (80000 increments), `thread_local` destructors (8, `__cxa_thread_atexit`), `std::condition_variable`, `std::atomic::wait`/`notify_one` (the futex path), `sleep_for` against `steady_clock`, `std::async`, `hardware_concurrency` | pass | pass |
| `cxx-filesystem` | `clang++ -O2 -std=c++17` | `std::filesystem` in `/tmp`: `create_directories`, `directory_iterator`, `recursive_directory_iterator`, `relative`, `file_size`, `copy_file`, `rename`, `exists`, `is_directory`, `absolute`, `remove_all` (8 entries), `filesystem_error` with `ENOENT` | pass | pass |
| `rust-panic` | the T4 `rustc --target <arch>-unknown-feos -O`, with the sysroot's `usr/lib` first on the native search path | a panic through 16 frames with drops, `catch_unwind`, on a thread, a panicking thread's `join` error, through T5's libunwind | pass | pass |
| `mixed-cxx-rust` | a Rust `staticlib` (T4 rustc) linked into a C++ program by `clang++` | calls both ways, a C++ exception through a Rust frame (`extern "C-unwind"`, its guard dropped) caught in C++, a Rust panic through a C++ frame (its destructor run) caught by `catch_unwind`, Rust threads and iostreams in one program, one libunwind | pass | pass |

10 passed, 0 failed, 0 pending on each port (the last run: `stage-t5.sh` with every build
step run, each finding its output up to date). No kernel issues were found; nothing in T5
waits on a kernel stage beyond the "can't back yet" table above (dynamic linking, 1f.1;
`getrandom` for `std::random_device`, 1e.1, optional).

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
- **The sysroot is installed in the test image, not yet the tree's (T2).** The plan has
  it in the feos image for native builds. The tree's image is assembled by
  `boards/common/fs_image_build.rs`, which the kernel workflow owns, and the sysroot
  (12 MB per architecture) would grow every kernel. So `libc-suite.sh` installs it
  at `/usr` in the base's image, where `sysroot-check` verifies it, and the tree's image
  gets it when a native compiler can use it (T4): a `/usr` entry built by
  `ports/sysroot.sh` in `ASSETS`, to be added by that workflow.
- **libc-test from a mirror (T2).** The submodule is musl's libc-test from repo.or.cz,
  the mirror musl's site names, which fetches by hash; no `feos-tag` is needed.
- **Not libc-test's own runner (T2).** libc-test's `src/common/runtest.c` forks, sets
  limits and kills on timeout, none of which the base kernel has. `testrun` runs both
  suites with `posix_spawn`, which feos has, and judges by exit status as libc-test
  does (and by expected output for relibc's tests, as relibc's runner does).
- **AArch64 math exception flags (T2).** Debian's clang 14 can't compile openlibm with
  strict floating point on AArch64, so 20 libc-test math tests that check exception
  flags are skipped there (their values are right; on x86_64 the build is strict and
  they pass). The skip ends when relibc is built with the clang T4 builds
  (`/build/rust-build/x86_64-unknown-linux-gnu/llvm/bin/clang`, LLVM 23), once the C
  driver moves to it with T5's feos driver.
- **Base HEAD (T2).** The suite was written against `refs/feos-checkpoints/1c.6` and
  now runs on HEAD (`c7a5bb6e`, through 1c.9): the `kill`/`killpg` tests (1c.8) and the
  1c.9 ones (`signals/kill-self`, `kill-group`, `killpg-self`, `signal-handle_return`,
  `sigsegv`; libc-test's `regression/sigreturn`) moved into the run lists; `unistd/exec`
  and `execle-env` wait on 1c.12's `/bin/sh`, not on `EXEC`. Most 1c.10 tests already
  pass on HEAD's kernel (the record table below) but stay pending until 1c.10 is a
  verified stage.
- **`-L` for the target's own libraries (T3).** rustc 1.99.1 adds
  `lib/rustlib/<target>/lib` to the linker's search path for native libraries only on
  Linux, Fuchsia, Fortanix and AIX targets (`walk_native_lib_search_dirs` in
  `rustc_session`), so `-lc`, `-lunwind` and the rest aren't found there for a JSON
  feos target. Programs pass `-L native=<sysroot>/lib/rustlib/<target>/lib` until
  track T4 builds the target into rustc, which adds `feos` to that list. (Done in T4:
  the built-in targets need no `-L`.)
- **No `ctest` (T3).** The libc crate's layouts are checked by `ports/test/libc-check`
  (rustdoc JSON, a compiled probe, generated C and C++ static assertions) rather than
  by `ctest`, whose crates.io dependencies (`syn` and the rest of a C parser's) aren't
  in `ports/vendor`; it checks the same things, function signatures included.
- **`Command` spawns only through `posix_spawn` (T3).** std falls back to `fork` and
  `exec` for what `posix_spawn` can't do (`pre_exec` closures, `uid`/`gid`/`groups`,
  `chroot`, a `PATH` in the child's own environment that differs from the parent's).
  feos has no `FORK` until stage 1d.1, so those `Command`s fail with `ENOSYS` until then;
  everything the T3 tests use goes through `posix_spawn`.
- **`poll` before 1c.11 (T3).** std's start-up check of fds 0 to 2 uses `poll`; on a
  kernel without `POLL` it stops at `ENOSYS`, as std already does for Unikraft. The T3
  tests run on 1c.11 (`refs/feos-checkpoints/1c.11`, newer than HEAD), where
  `Command::output`, which polls its two pipes, works; on 1c.10 every other case passes.
- **`chmod` takes the permission bits (T3).** feos's `FCHMODAT` refuses mode bits above
  `0o7777` (`EINVAL`), where Linux ignores them; std's `fs::copy` passes a whole
  `st_mode` to `fchmod`. relibc masks the mode, as Linux's kernel does
  (`ports/KERNEL-ISSUES.md`).
- **`abort` raises `SIGABRT` (T3).** relibc's `abort` trapped at once, so an aborted
  program died of `SIGILL` (x86_64) or `SIGTRAP` (AArch64); with signals in the kernel
  (1c.9), it raises `SIGABRT` first, as POSIX says, and std's stack overflow report
  ends in `SIGABRT` (signal 6) as on Linux.
- **1 GiB guests for T3 (T3).** `t3-heap`'s 128 MiB block doesn't fit in the boot tests'
  default 512 MiB guest (half of which is the kernel heap) beside its other
  allocations; `specs/t3-rust.toml` boots with 1 GiB (`[boot] memory`, new in
  `boot.py`, as is `[boot] native_disk`, the native mount at `/native0` the hard link
  case needs: the RAM root has no hard links, by design).
- **rust-src is copied, not `x.py dist rust-src` (T4).** The plan had `build.extended`
  with cargo and rust-src. Bootstrap's rust-src component runs `cargo vendor` of the
  library's dependencies in its own staging directory, outside `ports/` (so without
  `ports/vendor`'s source replacement, and offline that fails), and the staged library
  can't resolve `library/Cargo.toml`'s `[patch]` of libc (`../../libc`). `build-rust.sh`
  builds cargo through bootstrap (`tools = ["cargo"]`) and lays rust-src out itself:
  `library/`, `src/llvm-project/libunwind` and the libc crate under `lib/rustlib/src`,
  with the library's crates.io dependencies from `ports/vendor` as for any build under
  `ports/`.
- **Stage 1, not a stage 2 dist (T4).** The toolchain is bootstrap's stage 1 sysroot
  plus cargo (`/build/rust-feos`), which the plan allowed; `x.py dist` would rebuild
  rustc once more for nothing a cross toolchain needs.
- **`x86_64` feos is exempt from rustc's `pic` consistency check (T4).** rustc's target
  tests require a `pic` target to support dynamic linking or PIE; x86_64 feos is
  neither until 1f.1, and needs `pic` code for the reasons in T1's deviation. The check
  exempts feos as it exempts UEFI (`ports/PATCHES.md`).
- **`Triple::Feos` in LLVM now, not in T5 (T4).** The plan put LLVM's triple with the
  clang driver (phase 4). It is in this build already, unused, because `Triple.h` is
  included almost everywhere in LLVM and clang: added in T5 it would cost a full LLVM
  rebuild, added here it costs nothing, and T5's driver changes then rebuild only
  clang's driver.
- **`library/Cargo.lock` records the libc `[patch]` (T4).** T3 said the lockfile was
  unchanged by the patch; `-Zbuild-std` didn't mind, but bootstrap builds the library
  `--locked` and refused it. The lockfile now names libc without a registry source
  (two lines, `ports/PATCHES.md`).
- **cargo is `all-static` (T4).** The image has no OpenSSL headers, so cargo is built
  with bootstrap's `cargo-native-static` (vendored OpenSSL, curl, libgit2, libssh2 from
  `ports/vendor`): no system library dependencies, as cargo on feos will need.
- **bootstrap and `ports/.cargo/config.toml`'s `build.target` (T4).** That setting
  (host builds by default under `ports/`) also applies to `x.py`'s own build of
  bootstrap, which then lands in `bootstrap/<host>/debug`; `build-rust.sh` links
  `bootstrap/debug/bootstrap` to it. Every cargo run bootstrap itself makes passes
  `--target`, so nothing else is affected.
- **The Rust targets keep `rust-lld` (T5).** The plan offered clang as the Rust targets'
  linker. rustc's specs already name the feos triple as `llvm_target` (T4), and linking
  with `rust-lld` directly needs no C toolchain at link time, so they're unchanged;
  clang links mixed C++/Rust programs (`t5-mixed`), and a Rust program takes T5's
  libunwind with `-L native=<sysroot>/usr/lib` (`t5-rust-panic`).
- **No Rust changes in T5.** The rust checkout's `AGENTS.md` asks agents not to change
  that repository without a named human reviewer, among other rules; T5 needed no
  change there (the item above), so `ports/src/rust` outside `src/llvm-project` is as
  T4 left it. `src/llvm-project` is LLVM's own repository, with no such file.
- **A toolchain directory of its own (T5).** The plan didn't say where the feos clang
  lives. It's `/build/llvm-feos`, a copy of T4's clang and lld with the feos sysroots
  beside `bin/` (clang's default sysroot for the feos triples), not bootstrap's LLVM
  install, which bootstrap owns and rewrites.
- **Static by default means static only (T5).** feos loads no dynamic programs until
  1f.1, so the driver always links `-static` and refuses `-shared`, `-pie`,
  `-static-pie` and `-rdynamic` rather than producing files feos can't run; no
  dynamic loader path is set yet.
- **crtbegin without the frame registry (T5).** compiler-rt's crtbegin calls
  `__register_frame_info` if it is linked (a weak reference). A weak undefined call
  resolves to address 0, which an x86_64 `R_X86_64_PLT32` from 64 GiB can't reach, and
  libunwind finds frames through `PT_GNU_EH_FRAME` anyway, so the runtimes build with
  `COMPILER_RT_CRT_USE_EH_FRAME_REGISTRY=OFF`.
- **libc++, libc++abi and libunwind are three archives (T5).** The driver links
  `-lc++ -lc++abi` and `-lunwind`; nothing is merged into `libc++.a`, so a Rust static
  library and C++ in one program use the one `libunwind.a` (Rust's std names
  `-lunwind`, unbundled).
- **`wint_t` stays `int` (T5).** Linux's is `unsigned int`. feos's headers take
  `__WINT_TYPE__` from the compiler, and T1 to T4 compiled them for `<arch>-unknown-none-elf`,
  whose `wint_t` is `int`; the libc crate's feos module says `c_int`. The feos target keeps
  the default so nothing already built and checked changes.
- **The C driver stays on Debian's clang (T5).** `ports/toolchain/bin/feos-cc` still
  builds relibc and the T1 to T4 programs with the image's clang 14 (so T2's AArch64
  math exception-flag skips remain); moving relibc to the feos clang rebuilds the C
  library under every earlier stage's tests, a step of its own.

## T2: every test

`ports/test/suite-report.py`'s output for the last run (base HEAD `c7a5bb6e`, both
ports, and the Linux reference). "feos arm"/"feos x64" are each test's result in the
record cases, which run every built test, the pending and skipped ones too; "On feos" is
what the selection lists do with it. The pending tests that pass are 1c.10's, whose
kernel code is partly in HEAD but not yet a verified stage.


### relibc's tests

| Test | feos arm | feos x64 | Linux x86_64 | Linux AArch64 (qemu-user) | On feos |
|---|---|---|---|---|---|
| `alloca` | pass | pass | pass | pass | run, passes |
| `args` | pass | pass | pass | pass | run, passes |
| `arpa_inet/inet_addr` | pass | pass | pass | pass | run, passes |
| `arpainet` | pass | pass | pass | pass | run, passes |
| `assert` | pass | pass | pass | pass | run, passes |
| `constructor` | pass | pass | pass | pass | run, passes |
| `crypt/blowfish` | pass | pass | pass | pass | run, passes |
| `crypt/md5` | pass | pass | pass | pass | run, passes |
| `crypt/pbkdf2` | pass | pass | pass | pass | run, passes |
| `crypt/scrypt` | pass | pass | pass | pass | run, passes |
| `crypt/sha256` | pass | pass | pass | pass | run, passes |
| `crypt/sha512` | pass | pass | pass | pass | run, passes |
| `ctype` | pass | pass | pass | pass | run, passes |
| `destructor` | pass | pass | pass | pass | run, passes |
| `dirent/fdopendir` | pass | pass | pass | fail (exit status 1) | run, passes |
| `dirent/main` | pass | pass | pass | fail (exit status 1) | run, passes |
| `dirent/posix_getdents` | pass | pass | pass | fail (exit status 1) | run, passes |
| `dirent/scandir` | pass | pass | pass | fail (exit status 1) | run, passes |
| `dlfcn` | fail (exit status 1) | fail (exit status 1) | - | - | pending 1f.1: dlopen |
| `dlopen_scopes` | fail (exit status 1) | fail (exit status 1) | - | - | pending 1f.1: dlopen |
| `endian` | pass | pass | pass | pass | run, passes |
| `err` | pass | pass | pass | pass | run, passes |
| `errno` | pass | pass | pass | pass | run, passes |
| `error` | pass | pass | pass | pass | run, passes |
| `fcntl/create` | pass | pass | pass | pass | run, passes |
| `fcntl/fcntl` | pass | pass | pass | pass | run, passes |
| `fcntl/open` | fail (stderr differs) | fail (stderr differs) | pass | fail (exit status 1) | pending 1d.1: fork, exec |
| `fcntl/openat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `fcntl/posix_fallocate` | fail (exit status 1) | fail (exit status 1) | pass | fail (exit status 1) | skip: socket(AF_UNIX), which feos refuses (section 3.7) |
| `features` | pass | pass | pass | pass | run, passes |
| `fmtmsg` | fail (exit status 255) | fail (exit status 255) | pass | pass | skip: expects /dev/console to be unopenable (MM_NOCON); feos's /dev/console takes the message |
| `fnmatch` | pass | pass | pass | pass | run, passes |
| `futimens` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: test bug: passes mode bits as open flags (O_WRONLY\|O_RDWR), EINVAL on feos; relibc lists it failing on Linux too |
| `glob` | pass | pass | pass | fail (stdout differs) | run, passes |
| `grp/getgrgid_r` | pass | pass | pass | pass | run, passes |
| `grp/getgrnam_r` | pass | pass | pass | pass | run, passes |
| `grp/getgrouplist` | pass | pass | pass | pass | run, passes |
| `grp/getgroups` | pass | pass | pass | pass | run, passes |
| `grp/gr_iter` | pass | pass | pass | pass | run, passes |
| `includes` | pass | pass | pass | pass | run, passes |
| `iso646` | pass | pass | pass | pass | run, passes |
| `kill-waitpid` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, kill |
| `libgen` | pass | pass | pass | pass | run, passes |
| `limits` | pass | pass | pass | pass | run, passes |
| `link/dl-iterate-phdr` | pass | pass | pass | pass | run, passes |
| `locale/duplocale` | pass | pass | pass | pass | run, passes |
| `locale/newlocale` | pass | pass | pass | pass | run, passes |
| `locale/setlocale` | pass | pass | pass | pass | run, passes |
| `malloc/usable_size` | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | skip: prints malloc_usable_size, which depends on what the start-up code allocated before main (56 on feos, 40 on Linux; both >= 27) |
| `math` | pass | pass | pass | pass | run, passes |
| `mkfifo` | fail (exit status 1) | fail (exit status 1) | pass | fail (exit status 1) | skip: no named FIFOs in feos-abi v2 (section 9) |
| `mknod` | pass | pass | pass | fail (exit status 1) | run, passes |
| `mknodat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `net/if` | pass | pass | pass | pass | run, passes |
| `netdb/getaddrinfo` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | pending 1e.4: a resolver (RESOLVE) and the network |
| `netdb/getaddrinfo_null` | pass | pass | pass | pass | run, passes |
| `poll/poll` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, poll |
| `poll/pollnval` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.11: poll |
| `psignal` | pass | pass | pass | pass | run, passes |
| `pthread/barrier` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/cleanup` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/customstack` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/exit` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/extjoin` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/init` | pass | pass | pass | fail (killed by signal 5) | run, passes |
| `pthread/main` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/mutex_recursive` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/once` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/rwlock_randtest` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/rwlock_trylock` | pass | pass | pass | pass | run, passes |
| `pthread/timedwait` | pass | pass | pass | pass | run, passes |
| `pthread/timeout` | pass | pass | pass | fail (exit status 1) | run, passes |
| `pthread/tls` | pass | pass | pass | fail (killed by signal 5) | run, passes |
| `pty/forkpty` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no PTYs in phase 1 (section 3.3, IOCTL) |
| `pwd` | pass | pass | pass | pass | run, passes |
| `regex` | pass | pass | pass | pass | run, passes |
| `sa_restart` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, SA_RESTART |
| `semaphore/lock` | pass | pass | pass | pass | run, passes |
| `setjmp` | pass | pass | pass | pass | run, passes |
| `sigaction` | fail (killed by signal 5) | fail (killed by signal 4) | pass | pass | pending 1c.10: sigaction, raise (THREAD_KILL), sigprocmask |
| `sigaltstack` | pass | pass | pass | pass | pending 1c.10: sigaltstack |
| `sigchld` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, SIGCHLD |
| `signal` | pass | pass | pass | pass | pending 1c.10: signal, raise (THREAD_KILL) |
| `signals/kill-child` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, kill |
| `signals/kill-group` | pass | pass | pass | pass | run, passes |
| `signals/kill-invalid` | pass | pass | pass | pass | run, passes |
| `signals/kill-permission` | pass | pass | pass | pass | run, passes |
| `signals/kill-self` | pass | pass | pass | pass | run, passes |
| `signals/kill0-self` | pass | pass | pass | pass | run, passes |
| `signals/killpg-child` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork, killpg |
| `signals/killpg-esrch` | pass | pass | pass | pass | run, passes |
| `signals/killpg-invalid` | pass | pass | pass | pass | run, passes |
| `signals/killpg-self` | pass | pass | pass | pass | run, passes |
| `signals/killpg0-self` | pass | pass | pass | pass | run, passes |
| `signals/pthread_kill-child` | pass | pass | pass | fail (exit status 1) | pending 1c.10: THREAD_KILL |
| `signals/pthread_kill-invalid` | pass | pass | pass | pass | pending 1c.10: THREAD_KILL |
| `signals/pthread_kill-self` | hangs | hangs | pass | fail (exit status 1) | pending 1c.10: THREAD_KILL |
| `signals/pthread_kill0-self` | pass | pass | pass | pass | pending 1c.10: THREAD_KILL |
| `signals/raise-compliance` | pass | pass | pass | pass | run, passes |
| `signals/sigaddset-add` | pass | pass | pass | pass | run, passes |
| `signals/sigdelset-delete` | pass | pass | pass | pass | run, passes |
| `signals/sigismember-invalid` | pass | pass | pass | pass | run, passes |
| `signals/sigismember-valid` | pass | pass | pass | pass | run, passes |
| `signals/signal-h` | pass | pass | pass | pass | run, passes |
| `signals/signal-h-2` | pass | pass | pass | pass | run, passes |
| `signals/signal-handle_return` | pass | pass | pass | pass | run, passes |
| `signals/signal-handler` | pass | pass | pass | pass | pending 1c.10: signal, raise |
| `signals/signal-handler2` | pass | pass | pass | pass | pending 1c.10: signal, raise |
| `signals/signal-ignore` | pass | pass | pass | pass | pending 1c.10: signal, raise |
| `signals/signal-invalid` | pass | pass | pass | pass | run, passes |
| `signals/signal-uncatchable` | pass | pass | pass | pass | run, passes |
| `signals/sigpause-invalid` | pass | pass | pass | pass | run, passes |
| `signals/sigpause-revert` | pass | pass | pass | fail (exit status 1) | pending 1c.10: sighold, sigpause |
| `signals/sigpause-suspend` | pass | pass | pass | pass | pending 1c.10: sigpause (SIGSUSPEND) |
| `signals/sigprocmask-10` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-11` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-3` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-4` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-5` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-6` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-7` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-8` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-9` | pass | pass | pass | pass | pending 1c.10: sigprocmask |
| `signals/sigprocmask-blocksingle` | pass | pass | pass | fail (exit status 1) | pending 1c.10: sigprocmask |
| `signals/sigrelse-1` | pass | pass | pass | pass | pending 1c.10: sigrelse |
| `signals/sigrelse-2` | pass | pass | pass | pass | pending 1c.10: sigrelse |
| `signals/sigrelse-3` | pass | pass | pass | pass | run, passes |
| `signals/sigsegv` | pass | pass | pass | pass | run, passes |
| `signals/sigset-1` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-10` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-2` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-3` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-4` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-5` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/sigset-9` | pass | pass | pass | pass | pending 1c.10: sigset |
| `signals/stackoverflow` | pass | pass | fail (killed by signal 11) | fail (exit status 1) | pending 1c.10: sigaltstack, faults as signals |
| `sigqueue` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: sigprocmask, sigqueue, fork |
| `sigsetjmp` | pass | pass | pass | pass | run, passes |
| `stdio/all` | pass | pass | pass | pass | run, passes |
| `stdio/buffer` | pass | pass | pass | pass | run, passes |
| `stdio/ctermid` | pass | pass | pass | pass | run, passes |
| `stdio/dprintf` | pass | pass | pass | pass | run, passes |
| `stdio/fgets` | pass | pass | pass | pass | run, passes |
| `stdio/fputs` | pass | pass | pass | pass | run, passes |
| `stdio/fread` | pass | pass | pass | pass | run, passes |
| `stdio/freopen` | pass | pass | pass | pass | run, passes |
| `stdio/fscanf` | pass | pass | pass | pass | run, passes |
| `stdio/fscanf_offby1` | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | skip: expected output assumes gcc's right-to-left argument evaluation (ftell and fgetc in one printf) |
| `stdio/fseek` | pass | pass | pass | pass | run, passes |
| `stdio/fwrite` | pass | pass | pass | pass | run, passes |
| `stdio/getc_unget` | pass | pass | pass | pass | run, passes |
| `stdio/getline` | pass | pass | pass | pass | run, passes |
| `stdio/mutex` | pass | pass | pass | pass | run, passes |
| `stdio/popen` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.12: /bin/sh |
| `stdio/printf` | pass | pass | pass | pass | run, passes |
| `stdio/printf_neg_pad` | pass | pass | pass | pass | run, passes |
| `stdio/printf_space_pad` | pass | pass | pass | pass | run, passes |
| `stdio/putc_unlocked` | pass | pass | pass | pass | run, passes |
| `stdio/rename` | pass | pass | pass | pass | run, passes |
| `stdio/renameat` | pass | pass | pass | pass | run, passes |
| `stdio/scanf` | pass | pass | pass | pass | run, passes |
| `stdio/setvbuf` | fail (stdout differs) | fail (stdout differs) | pass | pass | pending 1d.1: fork |
| `stdio/sprintf` | pass | pass | pass | pass | run, passes |
| `stdio/tempnam` | pass | pass | pass | pass | run, passes |
| `stdio/tmpnam` | pass | pass | pass | pass | run, passes |
| `stdio/ungetc_ftell` | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | fail (stdout differs) | skip: expected output assumes gcc's right-to-left argument evaluation (getc and ftell in one printf); clang goes left to right |
| `stdio/ungetc_multiple` | pass | pass | pass | pass | run, passes |
| `stdlib/a64l` | pass | pass | pass | pass | run, passes |
| `stdlib/alloc` | pass | pass | pass | pass | run, passes |
| `stdlib/atof` | pass | pass | pass | pass | run, passes |
| `stdlib/atoi` | pass | pass | pass | pass | run, passes |
| `stdlib/bsearch` | pass | pass | pass | pass | run, passes |
| `stdlib/div` | pass | pass | pass | pass | run, passes |
| `stdlib/env` | pass | pass | pass | pass | run, passes |
| `stdlib/getsubopt` | pass | pass | pass | pass | run, passes |
| `stdlib/mkostemps` | pass | pass | pass | pass | run, passes |
| `stdlib/mktemp` | pass | pass | pass | pass | run, passes |
| `stdlib/ptsname` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no PTYs in phase 1 (posix_openpt) |
| `stdlib/qsort` | pass | pass | pass | pass | run, passes |
| `stdlib/rand` | pass | pass | pass | pass | run, passes |
| `stdlib/rand48` | pass | pass | pass | pass | run, passes |
| `stdlib/random` | pass | pass | pass | pass | run, passes |
| `stdlib/realpath` | pass | pass | pass | pass | run, passes |
| `stdlib/strtod` | pass | pass | pass | pass | run, passes |
| `stdlib/strtol` | pass | pass | pass | pass | run, passes |
| `stdlib/strtoul` | pass | pass | pass | pass | run, passes |
| `stdlib/system` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.12: /bin/sh |
| `string/mem` | pass | pass | pass | pass | run, passes |
| `string/memcpy` | pass | pass | pass | pass | run, passes |
| `string/memmem` | pass | pass | pass | pass | run, passes |
| `string/stpcpy` | pass | pass | pass | pass | run, passes |
| `string/stpncpy` | pass | pass | pass | pass | run, passes |
| `string/strcat` | pass | pass | pass | pass | run, passes |
| `string/strchr` | pass | pass | pass | pass | run, passes |
| `string/strchrnul` | pass | pass | pass | pass | run, passes |
| `string/strcpy` | pass | pass | pass | pass | run, passes |
| `string/strcspn` | pass | pass | pass | pass | run, passes |
| `string/strlen` | pass | pass | pass | pass | run, passes |
| `string/strncmp` | pass | pass | pass | pass | run, passes |
| `string/strpbrk` | pass | pass | pass | pass | run, passes |
| `string/strrchr` | pass | pass | pass | pass | run, passes |
| `string/strsep` | pass | pass | pass | pass | run, passes |
| `string/strsignal` | pass | pass | pass | pass | run, passes |
| `string/strspn` | pass | pass | pass | pass | run, passes |
| `string/strstr` | pass | pass | pass | pass | run, passes |
| `string/strtok` | pass | pass | pass | pass | run, passes |
| `string/strtok_r` | pass | pass | pass | pass | run, passes |
| `strings` | pass | pass | pass | pass | run, passes |
| `sys_epoll/epoll` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no epoll in feos-abi v2 (poll instead, section 10.1) |
| `sys_epoll/epoll_del` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no epoll in feos-abi v2 (section 10.1) |
| `sys_epoll/epollet` | pass | pass | pass | pass | run, passes |
| `sys_ioctl/tiocsctty` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no PTYs in phase 1 (posix_openpt) |
| `sys_mman/fmap` | pass | pass | pass | pass | run, passes |
| `sys_mman/mmap` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: maps at 8 GiB with MAP_FIXED_NOREPLACE, below feos's user window (64 GiB up, section 6) |
| `sys_mman/posix_madvise` | pass | pass | pass | pass | run, passes |
| `sys_resource/constants` | pass | pass | pass | pass | run, passes |
| `sys_resource/getrusage` | pass | pass | pass | pass | run, passes |
| `sys_select/pselect` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.11: pselect over POLL, sigaction |
| `sys_select/select` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.11: select over POLL |
| `sys_select/select_timed` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.11: pselect over POLL |
| `sys_shm/shm` | pass | pass | pass | pass | run, passes |
| `sys_socket/getpeername` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.3: sockets |
| `sys_socket/getsockopt` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.3: sockets |
| `sys_socket/msg_nosignal` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.4: socketpair, sigaction |
| `sys_socket/recv` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.3: sockets |
| `sys_socket/recvfrom` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.3: sockets |
| `sys_socket/unixnonblock` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: named AF_UNIX sockets: feos has AF_UNIX only through socketpair (section 3.7) |
| `sys_socket/unixpeername` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: named AF_UNIX sockets (section 3.7) |
| `sys_socket/unixrecv` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: named AF_UNIX sockets (section 3.7) |
| `sys_socket/unixrecvfrom` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: named AF_UNIX sockets (section 3.7) |
| `sys_socket/unixscmrights` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: SCM_RIGHTS: feos's sendmsg carries no control messages (section 3.7) |
| `sys_socket/unixsocketpair` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.4: socketpair, fork |
| `sys_stat/chmod` | pass | pass | pass | fail (exit status 1) | run, passes |
| `sys_stat/fstatat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `sys_stat/lstat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `sys_stat/stat` | pass | pass | pass | fail (killed by signal 5) | run, passes |
| `sys_stat/umask` | pass | pass | pass | fail (exit status 1) | run, passes |
| `sys_stat/utimensat` | pass | pass | pass | pass | run, passes |
| `sys_statvfs/statvfs` | pass | pass | pass | pass | run, passes |
| `sys_syslog/syslog` | pass | pass | pass | pass | run, passes |
| `sys_time/utimes` | pass | pass | pass | pass | run, passes |
| `sys_uio/basic` | pass | pass | pass | pass | run, passes |
| `sys_utsname/uname` | pass | pass | pass | pass | run, passes |
| `time/asctime` | pass | pass | pass | pass | run, passes |
| `time/constants` | pass | pass | pass | pass | run, passes |
| `time/gettimeofday` | pass | pass | pass | pass | run, passes |
| `time/gmtime` | pass | pass | pass | pass | run, passes |
| `time/localtime` | pass | pass | pass | pass | run, passes |
| `time/localtime_r` | pass | pass | pass | pass | run, passes |
| `time/macros` | pass | pass | pass | pass | run, passes |
| `time/mktime` | pass | pass | pass | pass | run, passes |
| `time/strftime` | pass | pass | pass | pass | run, passes |
| `time/strptime` | pass | pass | pass | pass | run, passes |
| `time/time` | pass | pass | pass | pass | run, passes |
| `time/timegm` | pass | pass | pass | pass | run, passes |
| `time/timer` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: POSIX timers aren't in feos-abi v2 (section 2) |
| `time/tzset` | pass | pass | pass | pass | run, passes |
| `tls` | pass | pass | pass | pass | run, passes |
| `unistd/access` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: wants example_dir's files 0644: the image's files are 0755 and the RAM root keeps no modes (chmod: EOPNOTSUPP) |
| `unistd/alarm` | pass | pass | pass | pass | pending 1c.10: sigaction, SETITIMER |
| `unistd/alarm_fn` | pass | pass | pass | pass | pending 1c.10: sigaction, SETITIMER |
| `unistd/brk` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: no brk in feos-abi v2: libc's sbrk gives ENOMEM (section 9) |
| `unistd/chdir` | pass | pass | pass | pass | run, passes |
| `unistd/confstr` | pass | pass | pass | pass | run, passes |
| `unistd/constants` | pass | pass | pass | pass | run, passes |
| `unistd/dup` | pass | pass | pass | pass | run, passes |
| `unistd/exec` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.12: /bin/sh as a program for execv (EXEC itself is in, 1c.7) |
| `unistd/fchdir` | pass | pass | pass | fail (exit status 1) | run, passes |
| `unistd/fork` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `unistd/fsync` | pass | pass | pass | pass | run, passes |
| `unistd/ftruncate` | pass | pass | pass | pass | run, passes |
| `unistd/getcwd` | pass | pass | pass | pass | run, passes |
| `unistd/gethostname` | pass | pass | pass | pass | run, passes |
| `unistd/getid` | pass | pass | pass | pass | run, passes |
| `unistd/getopt` | pass | pass | pass | pass | run, passes |
| `unistd/getopt_long` | pass | pass | pass | pass | run, passes |
| `unistd/getpagesize` | pass | pass | pass | pass | run, passes |
| `unistd/getpass` | fail (killed by signal 11) | fail (killed by signal 11) | - | - | pending 1c.11: a terminal for getpass, and expect(1) on feos |
| `unistd/isatty` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.11: isatty (IOCTL) |
| `unistd/link` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: hard links on the RAM root: EOPNOTSUPP by design (LINKAT, section 3.5); native mounts have them |
| `unistd/nice` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: nice values aren't kept yet: setpriority is accepted with no effect (section 9) |
| `unistd/pathconf` | pass | pass | pass | pass | run, passes |
| `unistd/pipe` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `unistd/readlinkat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `unistd/rmdir` | pass | pass | pass | pass | run, passes |
| `unistd/setid` | pass | pass | pass | pass | run, passes |
| `unistd/sleep` | pass | pass | pass | pass | run, passes |
| `unistd/swab` | pass | pass | pass | pass | run, passes |
| `unistd/symlinkat` | pass | pass | pass | pass | run, passes |
| `unistd/sysconf` | pass | pass | pass | pass | run, passes |
| `unistd/write` | pass | pass | pass | pass | run, passes |
| `waitpid` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `waitpid_multiple` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `wchar/fgetwc` | pass | pass | pass | pass | run, passes |
| `wchar/fwide` | pass | pass | pass | pass | run, passes |
| `wchar/mbrtowc` | pass | pass | pass | pass | run, passes |
| `wchar/mbsrtowcs` | pass | pass | pass | pass | run, passes |
| `wchar/printf-on-wchars` | pass | pass | pass | pass | run, passes |
| `wchar/putwchar` | pass | pass | pass | pass | run, passes |
| `wchar/ungetwc` | pass | pass | pass | pass | run, passes |
| `wchar/wcpcpy` | pass | pass | pass | pass | run, passes |
| `wchar/wcpncpy` | pass | pass | pass | pass | run, passes |
| `wchar/wcrtomb` | pass | pass | pass | pass | run, passes |
| `wchar/wcscasecmp` | pass | pass | pass | pass | run, passes |
| `wchar/wcschr` | pass | pass | pass | pass | run, passes |
| `wchar/wcscspn` | pass | pass | pass | pass | run, passes |
| `wchar/wcsdup` | pass | pass | pass | pass | run, passes |
| `wchar/wcsncasecmp` | pass | pass | pass | pass | run, passes |
| `wchar/wcsnlen` | pass | pass | pass | pass | run, passes |
| `wchar/wcsnrtombs` | pass | pass | pass | pass | run, passes |
| `wchar/wcsrchr` | pass | pass | pass | pass | run, passes |
| `wchar/wcsrtombs` | pass | pass | pass | pass | run, passes |
| `wchar/wcsstr` | pass | pass | pass | pass | run, passes |
| `wchar/wcstod` | pass | pass | pass | pass | run, passes |
| `wchar/wcstoimax` | pass | pass | pass | pass | run, passes |
| `wchar/wcstok` | pass | pass | pass | pass | run, passes |
| `wchar/wcstol` | pass | pass | pass | pass | run, passes |
| `wchar/wcstoumax` | pass | pass | pass | pass | run, passes |
| `wchar/wcswidth` | pass | pass | pass | pass | run, passes |
| `wchar/wprintf` | pass | pass | pass | pass | run, passes |
| `wchar/wscanf` | pass | pass | pass | pass | run, passes |
| `wctype/towlower` | pass | pass | pass | pass | run, passes |
| `wctype/towupper` | pass | pass | pass | pass | run, passes |

34 pending 1c.10, 6 pending 1c.11, 3 pending 1c.12, 13 pending 1d.1, 4 pending 1e.3, 3 pending 1e.4, 2 pending 1f.1, 235 run, passes, 23 skip

arm: relibc.list: 235 passed, 0 failed, 65 pending, 25 skipped

x64: relibc.list: 235 passed, 0 failed, 65 pending, 25 skipped

### libc-test

| Test | feos arm | feos x64 | Linux x86_64 | Linux AArch64 (qemu-user) | On feos |
|---|---|---|---|---|---|
| `functional/argv` | pass | pass | pass | pass | run, passes |
| `functional/basename` | pass | pass | pass | pass | run, passes |
| `functional/clocale_mbfuncs` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: MB_CUR_MAX = 4, expected 1 |
| `functional/clock_gettime` | pass | pass | pass | pass | run, passes |
| `functional/crypt` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: crypt("", "$2a$08$01234567890123456789") failed: got "$2a$8$s5SCFHAdETNa1a5mwtrhd8JKLg8... |
| `functional/dirname` | pass | pass | pass | pass | run, passes |
| `functional/dlopen` | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | pending 1f.1: dlopen |
| `functional/fcntl` | fail (exit status 1) | pass | pass | pass | pending 1d.1: fork (and the test reads an unset status when fork fails, so it may pass by chance) |
| `functional/fdopen` | pass | pass | pass | pass | run, passes |
| `functional/fnmatch` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: fnmatch("a\*.c", "a*.c", FNM_NOESCAPE) failed, got 0 want 1 |
| `functional/fscanf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: fgetc(f) failed ('l' != 'r') (Success)) |
| `functional/fwscanf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: fwscanf(f, L" %n%*d%n", &x, &y) failed (-1 != 0) |
| `functional/inet_pton` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: inet_pton(AF_INET, "1.2.03.4", addr) returned 1, want 0 |
| `functional/ipc_shm` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: shmget(IPC_CREAT\|IPC_EXCL) should have failed with EEXIST, got Success |
| `functional/mbc` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: mbrtowc(&wc, "\x80", 1, &st2) failed (failed to resume) got -1 want 1 |
| `functional/popen` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.12: /bin/sh |
| `functional/pthread_cancel` | fail (killed by signal 33) | fail (killed by signal 33) | hangs | hangs | relibc upstream: fails on relibc's Linux too: hangs |
| `functional/pthread_cancel-points` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (killed by signal 11) | relibc upstream: fails on relibc's Linux too: seqno == 1 failed (blocking sem_wait, seqno) |
| `functional/pthread_cond` | pass | pass | pass | hangs | run, passes |
| `functional/pthread_mutex` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | hangs | relibc upstream: fails on relibc's Linux too: hangs |
| `functional/pthread_mutex_pi` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | hangs | relibc upstream: fails on relibc's Linux too: relibc's ERRORCHECK relock returns EAGAIN, not EDEADLK; the NORMAL case's deadlocked thread, which the test leaves blocked on a mutex in a returned frame, is woken by relibc's unlock when the next case reuses the address and clobbers its result (relock 0, a second unlock EPERM), and strerror's one static buffer mixes up the two threads' messages ("Success" on feos) |
| `functional/pthread_robust` | hangs | hangs | hangs | fail (killed by signal 11) | relibc upstream: fails on relibc's Linux too: hangs |
| `functional/pthread_tsd` | pass | pass | pass | fail (exit status 1) | run, passes |
| `functional/qsort` | pass | pass | pass | pass | run, passes |
| `functional/random` | pass | pass | pass | pass | run, passes |
| `functional/sem_init` | pass | pass | pass | hangs | run, passes |
| `functional/setjmp` | pass | pass | pass | pass | pending 1c.10: sigprocmask (siglongjmp restores the mask) |
| `functional/snprintf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: bad integer conversion: got " 12 ", want "+12 " |
| `functional/socket` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1e.3: sockets |
| `functional/spawn` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: posix_spawnp(&pid, "echo", &fa, 0, (char *[]){"echo","hello",0}, 0) failed, got 38 want 0 |
| `functional/sscanf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: sscanf("hello, world\n", "%8c%8c", a, b) failed (2 fields, expected 1) |
| `functional/sscanf_long` | pass | pass | pass | pass | run, passes |
| `functional/stat` | pass | pass | pass | fail (exit status 1) | run, passes |
| `functional/strftime` | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | relibc upstream: fails on relibc's Linux too: "%c": expected "Sun Jan 3 13:23:45 2016", got nothing |
| `functional/string` | pass | pass | pass | pass | run, passes |
| `functional/string_memcpy` | pass | pass | pass | pass | run, passes |
| `functional/string_memmem` | pass | pass | pass | pass | run, passes |
| `functional/string_memset` | pass | pass | pass | pass | run, passes |
| `functional/string_strchr` | pass | pass | pass | pass | run, passes |
| `functional/string_strcspn` | pass | pass | pass | pass | run, passes |
| `functional/string_strstr` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: strstr("","") returned 0, wanted str+0 |
| `functional/strptime` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: "%R:%S": failed to parse "20:57:8" |
| `functional/strtod` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `functional/strtod_long` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `functional/strtod_simple` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `functional/strtof` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `functional/strtol` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: strtol(s="0xz", &c, 16) failed (wrong final position 0 != 1) |
| `functional/strtold` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `functional/swprintf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: swprintf(b, 2, L"%lc", 0xc0) failed (-1 != 1) |
| `functional/tgmath` | pass | pass | pass | pass | run, passes |
| `functional/time` | pass | pass | pass | pass | run, passes |
| `functional/tls_align` | pass | pass | pass | pass | run, passes |
| `functional/tls_align_dlopen` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | pending 1f.1: dlopen |
| `functional/tls_init` | pass | pass | pass | fail (killed by signal 11) | run, passes |
| `functional/tls_init_dlopen` | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | pending 1f.1: dlopen |
| `functional/tls_local_exec` | pass | pass | pass | fail (exit status 1) | run, passes |
| `functional/udiv` | pass | pass | pass | pass | run, passes |
| `functional/ungetc` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: fscanf(f, "%[h]", a) failed (got 1 fields, expected 0) |
| `functional/utime` | pass | pass | pass | pass | run, passes |
| `functional/wcsstr` | pass | pass | pass | pass | run, passes |
| `functional/wcstol` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: wcstol(s=L"9223372036854775808", &c, 0) failed (uncaught overflow -1 != 922337203685477... |
| `math/acos` | pass | pass | pass | pass | run, passes |
| `math/acosf` | pass | pass | pass | pass | run, passes |
| `math/acosh` | pass | pass | pass | pass | run, passes |
| `math/acoshf` | pass | pass | pass | pass | run, passes |
| `math/acoshl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/acosl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/asin` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/asinf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/asinh` | pass | pass | pass | pass | run, passes |
| `math/asinhf` | pass | pass | pass | pass | run, passes |
| `math/asinhl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/asinl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/atan` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atan2` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atan2f` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atan2l` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atanf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atanh` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atanhf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atanhl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/atanl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/cbrt` | pass | pass | pass | pass | run, passes |
| `math/cbrtf` | pass | pass | pass | pass | run, passes |
| `math/cbrtl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/ceil` | pass | pass | pass | pass | run, passes |
| `math/ceilf` | pass | pass | pass | pass | run, passes |
| `math/ceill` | pass | pass | pass | pass | run, passes |
| `math/copysign` | pass | pass | pass | pass | run, passes |
| `math/copysignf` | pass | pass | pass | pass | run, passes |
| `math/copysignl` | pass | pass | pass | pass | run, passes |
| `math/cos` | pass | pass | pass | pass | run, passes |
| `math/cosf` | pass | pass | pass | pass | run, passes |
| `math/cosh` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/coshf` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/coshl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/cosl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/erf` | pass | pass | pass | pass | run, passes |
| `math/erfc` | pass | pass | pass | pass | run, passes |
| `math/erfcf` | pass | pass | pass | pass | run, passes |
| `math/erfcl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/erff` | pass | pass | pass | pass | run, passes |
| `math/erfl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/exp` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/exp2` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/exp2f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/exp2l` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/expf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/expl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/expm1` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/expm1f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/expm1l` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT\|OVERFLOW |
| `math/fabs` | pass | pass | pass | pass | run, passes |
| `math/fabsf` | pass | pass | pass | pass | run, passes |
| `math/fabsl` | pass | pass | pass | pass | run, passes |
| `math/fdim` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/fdimf` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/fdiml` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/fenv` | pass | pass | pass | pass | run, passes |
| `math/floor` | pass | pass | pass | pass | run, passes |
| `math/floorf` | pass | pass | pass | pass | run, passes |
| `math/floorl` | pass | pass | pass | pass | run, passes |
| `math/fma` | pass | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: x64 only: fails the same on relibc's Linux x86_64: got INEXACT (passes on feos arm; fails on qemu-user's Linux AArch64 too) |
| `math/fmaf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/fmal` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/fmax` | pass | pass | pass | pass | run, passes |
| `math/fmaxf` | pass | pass | pass | pass | run, passes |
| `math/fmaxl` | pass | pass | pass | pass | run, passes |
| `math/fmin` | pass | pass | pass | pass | run, passes |
| `math/fminf` | pass | pass | pass | pass | run, passes |
| `math/fminl` | pass | pass | pass | pass | run, passes |
| `math/fmod` | pass | pass | pass | pass | run, passes |
| `math/fmodf` | pass | pass | pass | pass | run, passes |
| `math/fmodl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/fpclassify` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/frexp` | pass | pass | pass | pass | run, passes |
| `math/frexpf` | pass | pass | pass | pass | run, passes |
| `math/frexpl` | pass | pass | pass | pass | run, passes |
| `math/hypot` | pass | pass | pass | pass | run, passes |
| `math/hypotf` | pass | pass | pass | pass | run, passes |
| `math/hypotl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/ilogb` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/ilogbf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/ilogbl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/isless` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/j0` | pass | pass | pass | pass | run, passes |
| `math/j0f` | pass | pass | pass | pass | run, passes |
| `math/j1` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/j1f` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/jn` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/jnf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/ldexp` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/ldexpf` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/ldexpl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/lgamma` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/lgamma_r` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/lgammaf` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/lgammaf_r` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/lgammal` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/lgammal_r` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/llrint` | pass | pass | pass | pass | run, passes |
| `math/llrintf` | pass | pass | pass | pass | run, passes |
| `math/llrintl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/llround` | pass | pass | pass | pass | run, passes |
| `math/llroundf` | pass | pass | pass | pass | run, passes |
| `math/llroundl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/log` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/log10` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/log10f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/log10l` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/log1p` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/log1pf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/log1pl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/log2` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/log2f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/log2l` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/logb` | pass | pass | pass | pass | run, passes |
| `math/logbf` | pass | pass | pass | pass | run, passes |
| `math/logbl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/logf` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/logl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/lrint` | pass | pass | pass | pass | run, passes |
| `math/lrintf` | pass | pass | pass | pass | run, passes |
| `math/lrintl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/lround` | pass | pass | pass | pass | run, passes |
| `math/lroundf` | pass | pass | pass | pass | run, passes |
| `math/lroundl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/modf` | pass | pass | pass | pass | run, passes |
| `math/modff` | pass | pass | pass | pass | run, passes |
| `math/modfl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/nearbyint` | pass | pass | pass | pass | run, passes |
| `math/nearbyintf` | pass | pass | pass | pass | run, passes |
| `math/nearbyintl` | fail (exit status 1) | - | - | fail (exit status 1) | relibc upstream: openlibm declares nearbyintl but doesn't build it |
| `math/nextafter` | pass | pass | pass | pass | run, passes |
| `math/nextafterf` | pass | pass | pass | pass | run, passes |
| `math/nextafterl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/nexttoward` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/nexttowardf` | pass | pass | pass | pass | run, passes |
| `math/nexttowardl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/pow` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got 0 |
| `math/powf` | fail (exit status 1) | pass | fail (exit status 1) | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64: got 0 (passes on feos x64; fails on relibc's Linux x86_64 too) |
| `math/powl` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/remainder` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/remainderf` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/remainderl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/remquo` | pass | fail (exit status 1) | pass | pass | skip: x64 only: QEMU TCG's x87 fprem1 gives no quotient bits |
| `math/remquof` | pass | fail (exit status 1) | pass | pass | skip: x64 only: QEMU TCG's x87 fprem1 gives no quotient bits |
| `math/remquol` | fail (exit status 1) | fail (exit status 1) | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64; x64 only: QEMU TCG's x87 fprem1 gives no quotient bits |
| `math/rint` | pass | pass | pass | pass | run, passes |
| `math/rintf` | pass | pass | pass | pass | run, passes |
| `math/rintl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/round` | pass | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: x64 only: fails the same on relibc's Linux x86_64: got INVALID (passes on feos arm; fails on qemu-user's Linux AArch64 too) |
| `math/roundf` | pass | pass | pass | pass | run, passes |
| `math/roundl` | pass | pass | pass | pass | run, passes |
| `math/scalbln` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/scalblnf` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/scalblnl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/scalbn` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/scalbnf` | pass | pass | pass | fail (exit status 1) | run, passes |
| `math/scalbnl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sin` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/sincos` | pass | pass | pass | pass | run, passes |
| `math/sincosf` | pass | pass | pass | pass | run, passes |
| `math/sincosl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sinf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/sinh` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/sinhf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/sinhl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sinl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sqrt` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sqrtf` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/sqrtl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/tan` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/tanf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/tanh` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/tanhf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got INEXACT |
| `math/tanhl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/tanl` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/tgamma` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: got DIVBYZERO |
| `math/tgammaf` | pass | pass | pass | pass | run, passes |
| `math/tgammal` | fail (exit status 1) | pass | pass | fail (exit status 1) | relibc upstream: arm only: fails the same on relibc's Linux AArch64 |
| `math/trunc` | pass | pass | pass | pass | run, passes |
| `math/truncf` | pass | pass | pass | pass | run, passes |
| `math/truncl` | pass | pass | pass | pass | run, passes |
| `math/y0` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/y0f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/y1` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/y1f` | fail (exit status 1) | pass | pass | pass | skip: arm only: exception flags lost to clang 14 on AArch64 (no strict FP) |
| `math/yn` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `math/ynf` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too |
| `regression/daemon-failure` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `regression/execle-env` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1c.12: /bin/sh as a program for execle (EXEC itself is in, 1c.7) |
| `regression/fflush-exit` | fail (exit status 1) | fail (exit status 1) | pass | pass | pending 1d.1: fork |
| `regression/fgetwc-buffering` | pass | pass | pass | pass | run, passes |
| `regression/fpclassify-invalid-ld80` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: fpclassify(zero with msb set) failed: got FP_ZERO want FP_NORMAL |
| `regression/ftello-unflushed-append` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: ftello is broken before flush: got 3, want 7 |
| `regression/getpwnam_r-crash` | pass | pass | pass | pass | run, passes |
| `regression/getpwnam_r-errno` | pass | pass | pass | pass | run, passes |
| `regression/inet_ntop-v4mapped` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: inet_ntop failed |
| `regression/inet_pton-empty-last-field` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: inet_pton(1:2:3:4:5:6:7::) returned 30b0, wanted 0001 |
| `regression/iswspace-null` | pass | pass | pass | pass | run, passes |
| `regression/lrand48-signextend` | pass | pass | pass | pass | run, passes |
| `regression/lseek-large` | pass | pass | pass | pass | run, passes |
| `regression/malloc-0` | pass | pass | pass | pass | run, passes |
| `regression/malloc-brk-fail` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (killed by signal 9) | relibc upstream: fails on relibc's Linux too: malloc(10000) succeeded after memory is filled |
| `regression/malloc-oom` | pass | pass | pass | fail (killed by signal 9) | run, passes |
| `regression/mbsrtowcs-overflow` | pass | pass | pass | pass | run, passes |
| `regression/memmem-oob` | pass | pass | pass | pass | run, passes |
| `regression/memmem-oob-read` | pass | pass | pass | pass | run, passes |
| `regression/mkdtemp-failure` | pass | pass | pass | pass | run, passes |
| `regression/mkstemp-failure` | pass | pass | pass | pass | run, passes |
| `regression/printf-1e9-oob` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: snprintf("%g",999999999.000000) want 1e+09 got 10e+08 |
| `regression/printf-fmt-g-round` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: snprintf("%.12g", 100000000002500.000000) want 1.00000000002e+14 got 1.00000000003e+14 |
| `regression/printf-fmt-g-zeros` | pass | pass | pass | pass | run, passes |
| `regression/printf-fmt-n` | pass | pass | pass | pass | run, passes |
| `regression/pthread-robust-detach` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (killed by signal 7) | relibc upstream: fails on relibc's Linux too: pthread_mutex_timedlock(&mtx, &ts) failed: (pshared==0) got 110 "Owner died" want 130 "... |
| `regression/pthread_atfork-errno-clobber` | pass | pass | pass | pass | run, passes |
| `regression/pthread_cancel-sem_wait` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (killed by signal 11) | relibc upstream: fails on relibc's Linux too: seqno == 1 failed (uncontended sem_wait) |
| `regression/pthread_cond-smasher` | pass | pass | pass | fail (exit status 1) | run, passes |
| `regression/pthread_cond_wait-cancel_ignored` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | hangs | relibc upstream: fails on relibc's Linux too: pthread_cond_wait did not act on cancellation |
| `regression/pthread_condattr_setclock` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: pthread_condattr_setclock CLOCK_PROCESS_CPUTIME_ID should fail with EINVAL, got Success |
| `regression/pthread_create-oom` | pass | pass | pass | fail (killed by signal 9) | run, passes |
| `regression/pthread_exit-cancel` | pass | pass | pass | fail (exit status 1) | run, passes |
| `regression/pthread_exit-dtor` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: atexit handler was not run after last thread exited (exited=1, signal=0, status=0, want... |
| `regression/pthread_once-deadlock` | pass | pass | pass | hangs | run, passes |
| `regression/pthread_rwlock-ebusy` | pass | pass | pass | fail (exit status 1) | run, passes |
| `regression/putenv-doublefree` | pass | pass | pass | pass | run, passes |
| `regression/raise-race` | hangs | hangs | pass | fail (exit status 1) | pending 1c.10: raise (THREAD_KILL); hangs until then |
| `regression/regex-backref-0` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: regexec(/a\0/ ~ "a0") returned 1 (No match), wanted 0 |
| `regression/regex-backref-oob` | pass | pass | pass | pass | run, passes |
| `regression/regex-bracket-icase` | pass | pass | pass | pass | run, passes |
| `regression/regex-ere-backref` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: regexec(/(a)\1/ ~ "aa") returned 0 (No error), wanted REG_NOMATCH |
| `regression/regex-escaped-high-byte` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: regcomp(\\\xfc) returned 0 (No error) wanted REG_BADPAT |
| `regression/regex-negated-range` | pass | pass | pass | pass | run, passes |
| `regression/regexec-backref` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: regexec(&re, "aababa", 2, rm, 0) == 0 failed |
| `regression/regexec-nosub` | pass | pass | pass | pass | run, passes |
| `regression/rewind-clear-error` | pass | pass | pass | pass | run, passes |
| `regression/rlimit-open-files` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: setrlimit is accepted with no effect (section 9: no limit is enforced yet) |
| `regression/scanf-bytes-consumed` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: sscanf consumed 2 bytes but reported -2052325435 |
| `regression/scanf-match-literal-eof` | pass | pass | pass | pass | run, passes |
| `regression/scanf-nullbyte-char` | pass | pass | pass | pass | run, passes |
| `regression/setenv-oom` | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 9) | relibc upstream: fails on relibc's Linux too |
| `regression/setvbuf-unget` | hangs | hangs | hangs | hangs | relibc upstream: fails on relibc's Linux too: hangs |
| `regression/sigaltstack` | pass | pass | pass | pass | pending 1c.10: sigaltstack |
| `regression/sigprocmask-internal` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: sigaddset(&s, 32) set implementation internal rt signal |
| `regression/sigreturn` | pass | pass | pass | pass | run, passes |
| `regression/sscanf-eof` | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | fail (exit status 1) | relibc upstream: fails on relibc's Linux too: sscanf("0", "%f%c",..) returned 2, wanted 1 |
| `regression/statvfs` | fail (exit status 1) | fail (exit status 1) | pass | pass | skip: the RAM root counts no files: FSTATVFS's files, ffree, favail are 0, as Linux says for FAT |
| `regression/tls_get_new-dtv` | fail (killed by signal 11) | fail (killed by signal 11) | fail (killed by signal 11) | hangs | pending 1f.1: dlopen, $ORIGIN rpath |
| `regression/uselocale-0` | pass | pass | pass | pass | run, passes |
| `regression/wcsncpy-read-overflow` | pass | pass | pass | pass | run, passes |
| `regression/wcsstr-false-negative` | pass | pass | pass | pass | run, passes |

3 pending 1c.10, 2 pending 1c.12, 3 pending 1d.1, 1 pending 1e.3, 4 pending 1f.1, 141 relibc upstream, 134 run, passes, 24 skip

arm: libc-test.list: 138 passed, 0 failed, 13 pending, 192 skipped

x64: libc-test.list: 194 passed, 0 failed, 13 pending, 136 skipped

