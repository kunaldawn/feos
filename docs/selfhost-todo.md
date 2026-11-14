# Self-hosting todo

Tracks `docs/selfhost-plan.md`. Each item is ticked only when it's implemented **and**
its tests pass on both x86_64 and AArch64; the line says how it was tested. Items that
need hardware this work hasn't had are marked *(hardware)*.

Conventions for every stage: new system calls take new numbers in `libs/abi` and validate
every argument; user memory only through `libs/vm`; a test program or probe in `user/` for
each call, plus hostile cases in `malicious`; `make check-arm check-x64 clippy` clean;
`make test` for touched libs; boot phases run in the `localhost/feos` container.

## Phase 0: decisions and reference

- [x] Triples, approach, libc (relibc), LLVM, lockstep, gitoxide: `docs/selfhost-plan.md`
- [x] `docs/feos-abi-v2.md`: every call phase 1 adds, numbered, with relibc's needs mapped (calls 31–108; every `Pal`/`PalSignal`/`PalSocket`/`PalEpoll`/`PalPtrace` function of relibc `50c054e2` mapped; reviewed for completeness, kernel safety and ABI hygiene, with the findings applied; 28 implementation stages 1a.2–1f.1 with their tests and feature bits)
- [x] All sources in the repo: git submodules under `ports/src/` pinned to exact commits (rust 1.99.1 with its llvm-project, cargo and backtrace; relibc `50c054e2` with openlibm and dlmalloc-rs; libc 0.2.189; gitoxide v0.61.0; cpython v3.13.18; cmake v4.5.0; ninja v1.13.2; make 4.4.1), shallow, fetched once by `scripts/ports-init.sh` (`ports/README.md`)
- [ ] Forks under github.com/kunaldawn for the patched ones (rust, llvm-project, relibc, libc) and `git submodule set-url` to them; rust's fork marks its unused submodules `update = none` so plain `git clone --recursive` works too *(needs the forks created; pushing needs the owner's OK)*
- [x] Every crates.io dependency vendored under `ports/vendor/` (rust's workspace, cargo, `library/`, stdarch, bootstrap, cranelift, gitoxide: 1239 packages, 190 platform-unreachable ones stubbed by `ports/prune-vendor.py`, 864 MB), with source replacement and `net.offline` in `ports/.cargo/config.toml` and the root `.cargo/config.toml`
- [x] Offline check of the working tree: both kernels, rust's bootstrap and `gix` (`max-pure`) build in `localhost/feos` with `--network none`
- [x] Offline check from a fresh `git clone` + `scripts/ports-init.sh` (of `8fcd4866`, with the fixes below): all 13 submodules at their pins; then with `--network none` and an empty target volume, both kernels, rust's bootstrap, `gix` and the `user` boot phase on both ports. It found: Savannah refuses fetch-by-hash (make now fetches by `feos-tag`), and root `.gitignore`'s `target/` had dropped 16 vendored files of `cc`
- [ ] Repeat the fresh-clone check once the forks exist and the submodules point at them
- [ ] Reference build of rust 1.99.1 + LLVM from source on Linux: memory, disk, time, tool list *(hardware: tight on 14 GB)*

## Phase 1: kernel interface

One line per stage of `docs/feos-abi-v2.md` section 12, in implementation order; that
section has each stage's scope and its exact tests. The bit is the stage's feature bit.

### 1a Memory
- [x] 1a.1 Memory on first touch, `mprotect`, `PROT_NONE`, 4 GiB per-process limit, machine-wide promises (bit 0; `make test-vm`; boot `user`, `fault`, `ram`, `sched` on both ports)
- [ ] 1a.2 User work in kernel mode: faults on the kernel stack, cooperative kill, stops at the user boundary, teardown in task context, boot-test CPU/memory knobs (bit 1)
- [ ] 1a.3 Frame counts, VMAs, anonymous `MMAP2`, `MADVISE`, `MSYNC`, page-table charging, shared-anonymous objects, `ENOSYS` for reserved numbers (bit 2)
- [ ] 1a.4 Page cache, stable node ids, file mappings (private copy-on-write, shared read-only), `PROT_EXEC` on files, `ETXTBSY` (bit 3)
- [ ] 1a.5 Shared writable file mappings with write-back (bit 4)
- [ ] 1a.6 512 GiB window, the v2 layout with a separate argument area and 8 MiB stack, `ABI_VERSION` 2 (bit 5)

### 1b Threads
- [ ] 1b.1 `Process` and `Mm` shared and refcounted, pins, user futex keys, TLB shootdown, per-`Mm` ASIDs (bit 6)
- [ ] 1b.2 Threads: `THREAD_CREATE`, `THREAD_EXIT`, `GETTID`, `SET_TLS`, tgid, FP state, user-mode CPU controls (bit 7)
- [ ] 1b.3 Time and deadlines: `FUTEX_WAIT2`, `CLOCK_GETTIME`, `CLOCK_GETRES`, `NANOSLEEP` (bit 8)
- [ ] 1b.4 1024 task slots: scheduler bitmaps, futex buckets, kernel stacks, slots kept for kernel apps (bit 9)
- [ ] 1b.5 What scales with tasks: process snapshots, pipe pool, 1024 sockets, ASIDs under load (bit 10)

### 1c Files, processes, signals
- [ ] 1c.1 OFDs, the growable FD table, `DUP3`, `FCNTL`, `PIPE2`, `CLOSE_RANGE`, whole file transfers (bit 11)
- [ ] 1c.2 `OPENAT`, `FSTATAT`, `GETDENTS`, `MKDIRAT`, `UNLINKAT` with orphans, `FTRUNCATE`, `FSYNC`, `PREAD`/`PWRITE`, `FCHDIR`, `GETCWD`, `UMASK`, `FD_PATH`, `CURRENT_EXE` (bit 12)
- [ ] 1c.3 `/dev`, `/proc/self` and the image layout (`/tmp`, `/etc`, `$HOME`, CA certificates) (bit 13)
- [ ] 1c.4 `RENAMEAT`, `LINKAT`, `SYMLINKAT`, `READLINKAT`, `FCHMODAT`, `FCHOWNAT`, `UTIMENSAT`, `FACCESSAT`, `FSTATVFS`, `SYNC`; nanosecond times and update rules (bit 14)
- [ ] 1c.5 Locks: `FLOCK` and `fcntl` record locks (bit 15)
- [ ] 1c.6 `SPAWN2`, environments, process groups, `SYSINFO`, `GETRUSAGE`, the System V initial stack; `feos-sh` passes its environment (bit 16)
- [ ] 1c.7 `EXEC` and `#!` scripts (bit 17)
- [ ] 1c.8 `WAIT4`, exit records, `KILL` with default actions, stop/continue reporting, shell Ctrl-C (bit 18)
- [ ] 1c.9 Signal handlers: `SIGACTION`, frames, `SIGRETURN`, faults as signals, `SIGPIPE`, `SIGCHLD` (bit 19)
- [ ] 1c.10 Masks, `THREAD_KILL`, `SIGALTSTACK`, `SIGPENDING`, `SIGSUSPEND`, `EINTR`/`SA_RESTART`, `SETITIMER` (bit 20)
- [ ] 1c.11 `POLL`, `IOCTL`, the terminal line discipline, `/dev/tty` (bit 21)
- [ ] 1c.12 `/bin/sh` as a user program (bit 22)

### 1d fork
- [ ] 1d.1 `FORK` with copy-on-write and strict charging (bit 23)

### 1e Time, randomness, network
- [ ] 1e.1 CSPRNG, CPU and DTB sources, `GETRANDOM`, `CLOCK_SETTIME`, `/dev/random` (bit 24)
- [ ] 1e.2 virtio-rng and `EFI_RNG_PROTOCOL` (bit 25)
- [ ] 1e.3 Sockets: TCP and UDP, loopback, non-blocking, `POLL` (bit 26)
- [ ] 1e.4 `SOCKETPAIR` and `RESOLVE` (bit 27)

### 1f Program loading
- [ ] 1f.1 `ET_DYN`, PIE, `PT_INTERP`, the full aux vector (bit 28)

## Phase 2: relibc
- [ ] `platform/feos` in a relibc fork
- [ ] `ld_so` for feos: aux vector, TLS, `dlopen`, `dl_iterate_phdr`
- [ ] Sysroot layout and a C hello world on feos
- [ ] relibc tests and a libc-test subset as a boot phase

## Phase 3: Rust target
- [ ] `x86_64-unknown-feos`, `aarch64-unknown-feos` target specs
- [ ] `libc` crate `feos` module (glibc's `POSIX_SPAWN_*` bit values, as relibc's feos headers)
- [ ] feos arms in the crates with per-OS code listed in `docs/feos-abi-v2.md` 10.2 (rustix, getrandom, errno, socket2, mio's `poll` selector, stacker, jobserver without `pre_exec`, filetime, memmap2, home)
- [ ] std on `unix` for feos; unwinding through libunwind
- [ ] Cross-built std hello, threads, fs tests on feos
- [ ] std test suite through `remote-test-server`
- [ ] `x86_64-unknown-feos-kernel` built-in target replacing `x86_64-feos.json`

## Phase 4: LLVM
- [ ] `Triple::FEOS`, Clang driver toolchain
- [ ] compiler-rt builtins, libunwind, libc++abi, libc++ on relibc
- [ ] LLVM `Support` on feos
- [ ] clang, lld, LLVM cross-built as feos programs; C and C++ hellos run

## Phase 5: tools
- [ ] CPython, CMake, Ninja, GNU make (without `HAVE_MKFIFO`), gitoxide (rustls, `reqwest::blocking`), archives; builds on a RAM or native mount (FAT's 2 s mtimes)

## Phase 6: build system
- [ ] `ports/` recipes, `feos-build` (toolchain, os, image, world)

## Phase 7: storage and resources
- [ ] Native filesystem as root, POSIX semantics at scale *(hardware for scale tests)*
- [ ] More CPUs, big guests *(hardware: 16–32 GB guest RAM)*

## Phase 8: generation 0
- [ ] Cross-built world; milestones 1–5 on feos *(hardware)*

## Phase 9: generations 1 and 2
- [ ] Native rebuild and fixpoint *(hardware)*

## Phase 10: the loop from the network
- [ ] `gix clone`, `feos-build world`, install, reboot *(hardware)*
