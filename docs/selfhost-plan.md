# Self-hosting plan: a native feos toolchain, and feos built on feos

## Goal

1. **A toolchain built for feos.** `rustc`, `cargo`, LLVM, `lld` and `clang` built for
   native targets `x86_64-unknown-feos` and `aarch64-unknown-feos`. They run as native
   feos programs on feos system calls, with no Linux compatibility layer and no binaries
   taken from rust-lang or any other distribution.
2. **Self-hosting.** On a running feos, download the feos source (the OS and every
   toolchain component), build all of it, install it, and boot into the result.

## Decisions made

| Question | Choice |
|---|---|
| C library | relibc, with a `platform/feos` backend |
| Codegen and linker | LLVM, lld and clang. Cranelift is later work, not part of this plan |
| Architectures | x86_64 and AArch64 **in lockstep**: every milestone has to pass on both before the next one starts |
| Fetching source | gitoxide (`gix`) with rustls |

## Progress

| Stage | State | Tested by |
|---|---|---|
| 1a.1 Memory on first touch, `mprotect`, `PROT_NONE`, 4 GiB per-process limit, machine-wide promises | done | `make test-vm` (8 new tests); boot phase `user` on both ports (`utest` lazy-memory and `mprotect` checks, 10 new `malicious` probes, frame-leak count) |
| 1a.2 User work in kernel mode: faults on the kernel stack, cooperative kill, teardown in task context | done | `make test-sched` (5 new tests), `make test-app` (`pipe_wait_returns_killed`); boot phases `user`, `fault`, `sched`, `jobs`, `ram` on both ports at smp=1 and smp=4, and on AArch64 with `FEOS_BOOT_TEST_CPU=max` (`utest` `fault-in-kernel-thread`, `ktest ukill`, `ktest teardown`, frame-leak count) |
| 1a.3 Frame counts, VMAs, anonymous `MMAP2`, `MADVISE`, `MSYNC` | next | |
| 1a.4–1f.1 The rest of phase 1, as staged in `docs/feos-abi-v2.md` section 12 and ticked in `docs/selfhost-todo.md` | | |

## The one unavoidable seed

A Rust compiler is written in Rust, and LLVM in C++, so the *first* feos toolchain has to
be built by a compiler that already exists. That means cross-compiling it on Linux once.
From then on, each feos toolchain is built on feos by the previous one, as on any
self-hosting OS (Redox did the same).

- **Generation 0:** cross-built on Linux from source. It's the seed, and only exists to
  build generation 1.
- **Generation 1:** built *on feos* by generation 0, from the same source.
- **Generation 2:** built on feos by generation 1. Generations 1 and 2 must be
  bit-identical. That fixpoint proves the seed left nothing behind.

Optional, for full bootstrappability: rebuild generation 0 through `mrustc` (a Rust
compiler in C++) to cut the dependency on any earlier rustc binary. Not needed for the
goal; listed under later work.

## Shape of the system

```
        rustc, cargo, rustdoc          clang, lld, cmake, ninja, python, git, make
                 |                                       |
        Rust std for target_os = "feos"  (target_family = "unix")
                 |                                       |
           libc crate: feos module          C/C++ programs: libc++, libc++abi, libunwind
                 \                                      /
                  feos libc (relibc, feos platform) + ld.so (dynamic loader)
                                     |
                      feos-abi system calls (extended), native numbers
                                     |
                                feos kernel
```

### Decision 1: a POSIX-shaped libc, with std on top of it

`std` could sit directly on `feos-abi`, as Hermit's does. But LLVM, clang, CPython, CMake
and Ninja are C/C++ and need a C library anyway, and most crates written for `cfg(unix)`
go through the `libc` crate. So:

- **libc: relibc** (Redox's C library, written in Rust) with a new `platform/feos`
  backend. It has a platform layer meant for porting, its own dynamic loader (`ld_so`),
  pthreads, and it already runs LLVM, rustc, CPython and GCC on Redox. A musl port (a
  `syscall_arch.h` translating Linux numbers into feos calls) is the fallback, if relibc's
  gaps turn out larger than they look.
- **std:** `target_os = "feos"`, `target_family = "unix"`, `target_env = ""`, using
  std's `unix` module through the `libc` crate, the same way the `redox` target does. Most
  of std then works with little feos-specific code.
- **The kernel stays the kernel.** relibc's feos platform turns POSIX calls into
  `feos-abi` calls. The kernel doesn't learn Linux numbers or Linux structs.

### Decision 2: the full LLVM toolchain, not a Rust-only one

A toolchain without C++ would be rustc with the Cranelift backend and a Rust linker
(`wild`). It's attractive, but rustc without LLVM is a little-used configuration,
Cranelift has no LTO and weaker codegen, its `asm!` support needs an external assembler,
`wild` lacks the linker-script support the kernel needs, and C crates would still need a
C compiler. **Use LLVM**, and keep Cranelift as an optional second backend for faster
debug builds later.

### Decision 3: dynamic linking

Proc-macros are `.so` files rustc loads with `dlopen`, and rustc itself is
`librustc_driver.so`. So the feos host target has `dynamic_linking: true`, and the kernel
loads programs with a PT_INTERP through relibc's `ld.so`. Ordinary programs can still be
static.

## Phase 0: design decisions on paper, and a Linux reference build

1. Fix the triples, the target-spec contents (section 3) and the `feos-abi` extensions
   (section 1), and write them into this document before any code.
2. On Linux, build the reference: the exact rust-lang/rust version matching
   `rust-toolchain.toml` and its bundled LLVM, from source, with `x.py dist`. Record
   peak memory per job, disk used (expect 30–60 GB for a full LLVM + rustc build tree),
   wall time, and the tool set the build invokes (`strace -f -e trace=execve`): `cmake`,
   `ninja`, `python3`, `c++`, `ar`, `tar`, `xz`, `git` (which `gix` replaces on feos).
   Do this on both an x86_64 and an arm64 host. That list is the set of tools
   feos has to port in phase 5.
3. Make all repositories local mirrors: forks of `rust-lang/rust` (with its `src/llvm-project`
   submodule), `libc`, `relibc`, CPython, CMake, Ninja, GNU make, zlib, xz, and a crate
   vendor directory for everything those builds fetch from crates.io (rustc's own
   `Cargo.lock` is large). From now on, no build step touches the network.

## Phase 1: the kernel interface a POSIX system needs

New `feos-abi` numbers, all with the existing rule that every argument is validated and
user memory only goes through `libs/vm`. Grouped, with what needs them:

**Memory** (everything)
- Demand paging: `mmap` reserves, a fault maps. rustc, LLVM and relibc's allocator
  reserve far more than they touch.
- `mmap` of files (`MAP_PRIVATE` copy-on-write, `MAP_SHARED` read-only to start),
  `MAP_FIXED`, address hints, `PROT_EXEC` for file mappings (the loader), `mprotect`,
  `madvise`. Keep W^X: no page writable and executable at once. Nothing here JITs.
- A shared page cache for file pages, so `librustc_driver.so` and `libLLVM.so`
  (~150 MB each) are mapped once, not copied per process.
- Address space: a layout for big programs (tens of GiB virtual). `USER_MEMORY_LIMIT`
  (64 MiB) becomes a per-process limit, several GiB by default. User frames come from
  `libs/frames`, not the kernel heap (capped at 1 GiB).

**Threads** (pthreads, `std::thread`, LLVM's thread pool, rustc's parallel codegen)
- `thread_create(entry, stack, tls, arg)`, `thread_exit`, `gettid`, and a
  clear-and-wake word on exit (what `pthread_join` waits on).
- Per-task TLS register: `FS_BASE` on x86_64 and `TPIDR_EL0` on AArch64, saved and
  restored. Neither is switched today.
- `futex` gains timeouts against the monotonic clock, and wait-for-any-of-bitset if
  relibc needs it.
- `Process` becomes shared by its threads: one address space, one FD table, one cwd.
  `kernel/src/user.rs` relies today on a process being single threaded (see
  `with_user_buffer`). This is the most invasive change, and needs its own review and a
  `malicious`-style test.
- `MAX_PROCESSES` (32 tasks in total) goes to 1024 or more. Check `libs/sched`'s lock
  hold times as it grows.
- Full FP/SIMD state per task. x86_64 saves FXSAVE today: either keep AVX off in `XCR0`
  (CPUID then hides it, and LLVM won't use it) or move to XSAVE.

**Files and directories** (everything)
- `openat`-style calls with a directory FD, `getdents`, `fstat`/`fstatat` with
  nanosecond times, `rename` (atomic over an existing name), `symlink`/`readlink`,
  `ftruncate`, `fsync`, `pread`/`pwrite`, `utimens`, `access`, `fchmod`, `flock` (cargo's
  locks), `getcwd`/`chdir`, `dup`/`dup2`, `fcntl` (close-on-exec, non-blocking),
  `isatty`/window size for the console.
- `MAX_FDS` 16 goes to 1024 or more, with a growable table.
- `/proc/self/exe`, or a `current_exe` call: rustc finds its sysroot from it.

**Processes** (cargo, make, ninja, cmake, the shell)
- `spawn` gains an environment, a working directory and a list of FD actions (dup, close),
  the shape of `posix_spawn`. relibc's `posix_spawn` maps straight onto it.
- `exec` (replace the image), `exit` statuses with signal-style codes, `wait` for any
  child, non-blocking wait.
- **`fork` with copy-on-write.** Several C tools call it (CMake through libuv, GNU make,
  CPython's `subprocess` fallback). `libs/vm` owns page tables, so COW is feasible. Do it
  after `spawn`, and patch whatever is easy to `posix_spawn` instead.
- Minimal signals: `kill`, `SIGCHLD`, `SIGINT` from the console, `SIGPIPE`, handlers and
  masks, and turning a CPU fault into `SIGSEGV`/`SIGBUS` delivered to a handler (rustc's
  stack-overflow report, and Ninja and Python both expect `SIGINT`). Fault without a
  handler stays exit 139.
- `poll` across pipes, the console and sockets (cargo's jobserver, Ninja, make's
  jobserver).

**Time and randomness** (TLS, file times, build tools)
- Wall clock from the RTC (PL031 and CMOS drivers already exist), monotonic clock,
  sleep with a deadline.
- `getrandom` from a kernel CSPRNG seeded from virtio-rng, `RDRAND`/`RDSEED`, or `RNDR`.
  The xorshift in `libs/net` isn't good enough for keys.

**Network** (downloading the source)
- Sockets over `libs/net`: TCP and UDP, blocking and non-blocking, `connect` with
  in-progress, `bind`/`listen`/`accept`, `shutdown`, options, and `poll` readiness through
  the word `netd` already moves. IPv4 only, as the stack is.
- Name resolution: either a `resolve` call over `libs/net`'s DNS client (simplest, and
  relibc's `getaddrinfo` calls it), or `/etc/resolv.conf` written from DHCP and
  resolution in libc.

**Program loading**
- `libs/elf` accepts `ET_DYN` and PT_INTERP (both rejected today as `NotStatic`), and
  loads PIE with a load bias. The kernel maps the interpreter and passes an aux vector
  (`AT_PHDR`, `AT_ENTRY`, `AT_BASE`, `AT_PAGESZ`, `AT_RANDOM`, `AT_HWCAP`) and `envp`.

Each group gets a test program in `user/`, a `ktest` case and a boot-test phase.
`libs/abi` stays host-testable and allocation-free.

## Phase 2: relibc for feos

1. `platform/feos` in a relibc fork: each `Pal` function as a `feos-abi` call. relibc's
   Linux backend is the template: anything that's a thin syscall wrapper there is one
   here.
2. `ld_so` for feos: the aux vector, TLS setup through the per-task register, `dlopen`
   and `dl_iterate_phdr` (the unwinder needs it).
3. A sysroot layout: `/usr/include`, `/usr/lib/{crt0.o,crti.o,crtn.o,libc.a,libc.so}`,
   `/lib/ld64.so.1`.
4. Tests: relibc's own tests, then a subset of libc-test, cross-compiled and run on feos
   as a boot-test phase (`libc`).

relibc is built with a Rust compiler, so generation 0 relibc is cross-built on Linux with
the phase 3 Rust target.

## Phase 3: the Rust target

In the rust-lang/rust fork (keeping patches small and upstreamable as a tier 3 target):

1. **Target specs** `x86_64-unknown-feos` and `aarch64-unknown-feos` in
   `compiler/rustc_target`: `os = "feos"`, `families = ["unix"]`, ELF,
   `dynamic_linking`, `position_independent_executables`, `has_thread_local`,
   `crt_static_respected`, panic strategy unwind, `linker_flavor` `gnu-lld-cc` through
   `clang` (or `gnu-lld` straight to `rust-lld`, the simpler start), the x86_64 baseline
   CPU, and `llvm_target` set to the matching LLVM triple from phase 4.
2. **`libc` crate**: a `feos` module (types, constants, functions), modelled on `redox`.
   Upstream to rust-lang/libc as well.
3. **std**: `library/std/src/sys` picks the `unix` implementation for `feos`, with
   `cfg(target_os = "feos")` where relibc differs (as `redox` has today). Threads,
   `fs`, `process`, `net`, `env`, `time`, thread-locals, `available_parallelism`,
   stack-overflow guard pages.
4. **Unwinding**: LLVM's `libunwind` (built in phase 4), linked as std's unwinder, with
   `.eh_frame_hdr` found through `dl_iterate_phdr`. rustc needs unwinding: it catches
   panics from proc-macros and for ICE reports.
5. **Cross-build on Linux**: `x.py build library --target x86_64-unknown-feos,
   aarch64-unknown-feos`, then a std hello world, `std::thread` and `std::fs` tests
   run on feos. Then std's own test suite (`x.py test library/std --target ...`) with a
   remote test client (`remote-test-server` running on feos, over the network or a
   virtio-serial port). That's how Rust tests other OS targets, and it's the quickest
   way to find std bugs.
6. **Bare-metal targets in the same toolchain**: the feos host toolchain must also be
   able to build the kernel, so `dist` includes `aarch64-unknown-none` std (core,
   alloc) and `rust-src` for `-Zbuild-std` on x86_64 (`x86_64-feos.json`). Better still:
   add `x86_64-unknown-feos-kernel` (the contents of `arch/x86_64/x86_64-feos.json`) as a
   built-in target, so the kernel no longer needs `-Zbuild-std`, `RUSTC_BOOTSTRAP` or
   `scripts/cargo-x64.sh`.

## Phase 4: LLVM, Clang and lld for feos

In the `llvm-project` fork the Rust fork uses as its submodule:

1. `llvm::Triple`: an `FEOS` OS value. `x86_64-unknown-feos`, `aarch64-unknown-feos`.
2. A Clang driver toolchain (`clang/lib/Driver/ToolChains/FEOS.cpp`, modelled on
   Fuchsia's or the OpenBSD one): sysroot, include paths, crt objects, `ld.lld` as the
   linker, the dynamic loader path, PIE by default, `-lc`, compiler-rt builtins.
3. Runtimes on relibc: compiler-rt builtins, libunwind, libc++abi, libc++. libc++ needs
   a locale and thread support layer on relibc; begin with `_LIBCPP_HAS_NO_LOCALIZATION`
   off only if relibc's locale support covers it, otherwise the minimal "C" locale.
4. LLVM's `Support` library: `lib/Support/Unix` works on a POSIX libc, with `#if
   defined(__feos__)` where it reaches for Linux-only things (`/proc`, `sched_getaffinity`,
   `mallinfo`, signals for crash recovery).
5. Cross-build on Linux with a CMake toolchain file: `clang`, `lld`, `llvm-ar`,
   `llvm-objcopy`, `libLLVM.so` for feos. Test with `hello.c`, `hello.cpp` (iostream,
   threads, exceptions) and LLVM's `check-llvm` subset through `lit` once Python runs
   (phase 5).

## Phase 5: the other build tools

Everything the phase 0 trace showed, cross-built in generation 0 and rebuilt on feos in
generation 1:

| Tool | Why | Notes |
|---|---|---|
| CPython 3 | `x.py` (bootstrap), LLVM's CMake, `lit` | C, on relibc; needs `fork` or `posix_spawn`, `select`/`poll`, sockets for `pip` (not needed) |
| CMake | building LLVM | C++, uses libuv: patch libuv's process code to `posix_spawn`, or rely on `fork` |
| Ninja | building LLVM | small C++, `posix_spawn` |
| GNU make | feos's `Makefile`, some ports | C, `fork` or `posix_spawn` |
| gitoxide (`gix`) | fetching the source | Rust, with rustls over feos sockets. `gix clone`, `fetch`, checkout of a pinned revision, submodules (rust's `src/llvm-project`). No C git, curl or OpenSSL |
| tar, xz, gzip | source tarballs, rust's `dist` | Reuse `libs/archive` and `libs/compress` as coreutils, or port xz-utils |
| zlib, zstd | optional LLVM compression | can be disabled |
| feos-sh | `x`, configure scripts, `scripts/*.sh` | gaps found here get fixed in `feos-sh` |

TLS for downloading: `rustls` with a CA bundle shipped in the image. Its crypto provider
(`ring` or `aws-lc-rs`) has C and assembly built through `cc`, which works once clang
runs on feos.

## Phase 6: one build for everything

The whole system, OS and toolchains, built by one command from one tree.

1. **A ports tree** in the repository: `ports/<name>/` with a recipe (source location,
   pinned revision and hash, patches, configure and build steps) for relibc, llvm-project,
   rust, cpython, cmake, ninja, make, gitoxide, and the small libraries. Sources
   come from local mirrors or a feos source server, by hash, never "latest".
2. **`feos-build`** (a Rust `xtask`-style program in the tree) replacing the Makefile's
   build logic, so building needs neither GNU make nor Python on the critical path:
   - `feos-build toolchain`: relibc, LLVM, rust, tools, in dependency order, each into a
     staging sysroot.
   - `feos-build os`: `user/`, then the kernels for both ports with the toolchain just
     built (or the installed one).
   - `feos-build image`: the root filesystem image (today's `boards/common/fs_image_build.rs`
     inputs plus the toolchain under `/usr`) and the disk images.
   - `feos-build world`: all of it.
   - It runs the same on Linux (generation 0, cross) and on feos (generation 1+, native).
3. **Installed layout on feos**: `/usr/bin` (rustc, cargo, clang, ld.lld, cmake, ninja,
   python3, gix, make), `/usr/lib` (relibc, `librustc_driver.so`, `libLLVM.so`, libc++),
   `/usr/lib/rustlib`, `/usr/include`, `/usr/src` for sources.

## Phase 7: storage and resources

Building LLVM and rustc needs a real disk and a lot of memory, so this is its own phase.

- **Filesystem.** The root must be a writable POSIX filesystem with 64–100 GB: owners,
  modes, symlinks, hard links, nanosecond mtimes, atomic rename, large files, millions of
  small files (rust's build tree), and enough speed. Today the native journalled
  format can't be the root, and FAT/exFAT can't do POSIX. Make the native format
  root-capable and scale it, with a write-back page cache and host tests in `libs/fs`
  for each property.
- **Memory.** Linking `libLLVM.so` and `librustc_driver.so` takes several GiB per link
  job; plan for 16–32 GiB of guest RAM and cap link parallelism
  (`LLVM_PARALLEL_LINK_JOBS`).
- **CPUs.** Both ports stop at 8. Raise `MAX_CPUS` where it's cheap, since a full build is
  CPU-bound.
- **Time.** Under KVM (`-accel kvm -cpu host`) a full world build is hours, comparable to
  Linux if the kernel's I/O and scheduling hold up. Under TCG it's days.
- **Lockstep means two KVM hosts.** Keeping both ports level, with native builds on
  each, needs an x86_64 Linux machine *and* an arm64 Linux machine with KVM (an Ampere
  server, or Linux on Apple silicon), since AArch64 under TCG makes every native
  milestone take days. Both ports' boards need to run under KVM: on AArch64 that means
  `-machine virt,gic-version=host` (GICv3 is already supported) and checking nothing
  depends on TCG-only behaviour.
- **What lockstep doesn't require.** Cross builds are cheap: x86_64 feos can build the
  AArch64 world and the reverse, since LLVM targets both. Add a boot-test phase for
  that: each native feos builds the other port's image, which boots on the host.

## Phase 8: generation 0, cross-built on Linux

`feos-build world --cross` on Linux:

1. Rust stage 1 compiler on Linux with the feos targets (phase 3).
2. relibc for feos (phase 2).
3. Clang and runtimes for feos, then LLVM, lld and clang *as feos programs* (phase 4).
4. rustc, cargo, rustdoc, rust-lld *as feos programs*: `x.py dist --host x86_64-unknown-feos
   --target x86_64-unknown-feos,aarch64-unknown-feos,aarch64-unknown-none,...`.
5. The phase 5 tools.
6. feos itself, and an image with all of the above installed.

Milestones on feos, each a boot-test phase driving the shell:

1. `clang hello.c`, `clang++ hello.cpp`, run the results.
2. `rustc hello.rs`, then a proc-macro crate and a crate that uses it.
3. `cargo build --offline` of a vendored workspace with `build.rs` and proc-macros
   (`serde` with derive).
4. `cargo build --release` in `user/`; compare with the cross-built output.
5. Both kernels, from `/usr/src/feos`; boot the result on the host with
   `scripts/boot-test.py`.

## Phase 9: generation 1 and 2, built on feos

On feos, from generation 0:

1. Rebuild the tools, relibc and LLVM natively.
2. `x.py dist` natively, with stage 0 being the installed feos rustc and cargo (set in
   `bootstrap.toml`: `build.rustc`, `build.cargo`, `llvm.download-ci-llvm = false`,
   vendored sources, offline).
3. `feos-build world` with generation 1 installed. That's generation 2.
4. **Fixpoint check**: generation 1 and generation 2 artifacts compared byte for byte
   (needs reproducible builds: `--remap-path-prefix`, fixed `SOURCE_DATE_EPOCH`, no
   timestamps or build paths, deterministic archive ordering). Any difference is a bug to
   find before claiming self-hosting.

## Phase 10: the full loop, from the network

On a fresh feos with only the installed toolchain:

```sh
gix clone https://<feos source host>/feos.git /usr/src/feos
cd /usr/src/feos
feos-build fetch       # every port's source, by hash
feos-build world       # toolchain + OS + image
feos-build install     # write to a second disk, or the boot partition
reboot                 # into the system it just built
```

Needs: sockets and DNS (phase 1), a CSPRNG and wall clock for TLS, `gix`, a
source server or mirror reachable over IPv4, and installing to the boot path the
UEFI loader (`boot/efi`) reads. Boot-test phase `world`: run the loop against a local
mirror through slirp, entirely offline, then boot the image it produced.

## Order of work and size

| # | Work | Depends on | Size |
|---|---|---|---|
| 0 | Decisions, Linux reference build, mirrors and vendoring | none | M |
| 1a | Demand paging, file mmap, mprotect, page cache, big address spaces | none | L |
| 1b | Threads, shared `Process`, TLS registers, FP state, more tasks and FDs | none | L |
| 1c | Files and dirs, spawn with env/cwd/FDs, exec, wait, poll, signals | none | L |
| 1d | `fork` with copy-on-write | 1a | M |
| 1e | Clock, CSPRNG, sockets, resolve | 1c | M |
| 1f | Dynamic ELF: PT_INTERP, PIE, aux vector | 1a | M |
| 2 | relibc `platform/feos` and `ld_so` | 1a–1c, 1f | L |
| 3 | Rust target, `libc` module, std, remote test runner | 2 | L |
| 4 | LLVM triple, Clang toolchain, runtimes, LLVM on relibc | 2 | L |
| 5 | CPython, CMake, Ninja, make, gix, archives | 2, 4 | L |
| 6 | Ports tree, `feos-build` | none (start early) | M |
| 7 | Root-capable native fs at scale, memory, CPUs | none | L |
| 8 | Generation 0 cross build, milestones on feos | 3–7 | M (mostly debugging) |
| 9 | Generations 1 and 2 on feos, reproducibility fixpoint | 8 | L |
| 10 | Fetch, build, install, reboot loop | 9, 1e | M |

Every row is done only when it passes on both x86_64 and AArch64. Phases 1a–1c, 6 and 7
can run in parallel. 3 and 4 can run in parallel once 2 can run a
C hello world.

## Risks

- **Scale.** This is the largest project feos could take on. Redox, the closest
  precedent, took years to get rustc and LLVM running natively. The parts here that are
  feos-specific (kernel interface, relibc platform, std and Clang target) are the
  smaller share. Most of the work is debugging big programs on a new kernel.
- **Upstream drift.** Forks of rust, llvm-project, relibc and libc must move with each
  `rust-toolchain.toml` bump. Keep patches small and upstream the target (tier 3 in Rust,
  the triple and driver in LLVM, the module in `libc`) to make that cheap.
- **Isolation.** Threads, shared processes, `fork` and signals all touch the code that
  keeps user processes apart. Each needs a hostile test program in `user/`.
- **Filesystem.** A build that writes millions of files will find every bug the
  filesystem has. Expect phase 7 to take longer than its size suggests.
- **Reproducibility.** The fixpoint in phase 9 needs reproducible builds for every port,
  and some upstreams don't quite get there. Track the differences explicitly.

## Later

- Cranelift as a second codegen backend, for fast debug builds on feos (not in this plan).
- `mrustc` to rebuild generation 0 without trusting an earlier rustc binary.
- Moving the native user programs in `user/` (`#![no_std]` on `feos-abi` today) to the
  std target.
- IPv6 in `libs/net`, for source hosts that need it.
