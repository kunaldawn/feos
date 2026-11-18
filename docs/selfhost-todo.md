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
- [x] 1a.2 User work in kernel mode: faults on the kernel stack, cooperative kill, stops at the user boundary, teardown in task context, boot-test CPU/memory knobs (bit 1; `make test-sched test-app`; `ktest ukill`, `ktest teardown`; boot `user`, `fault`, `sched`, `jobs`, `ram` on both ports at smp=1 and smp=4, and `user`, `fault`, `sched`, `ram` on AArch64 with `FEOS_BOOT_TEST_CPU=max`)
- [x] 1a.3 Frame counts, VMAs, anonymous `MMAP2`, `MADVISE`, `MSYNC`, page-table charging, shared-anonymous objects, `ENOSYS` for reserved numbers (bit 2; `make test-frames test-vm test-abi`; `mtest`, `ktest drive`; boot `mmap` at smp=4 and smp=1, and `user`, `fault`, `sched`, `ram`, `jobs`, on both ports; and `mmap`, `user`, `fault`, `sched`, `ram` on AArch64 with `FEOS_BOOT_TEST_CPU=max`)
- [x] 1a.4 Page cache, stable node ids, file mappings (private copy-on-write, shared read-only), `PROT_EXEC` on files, `ETXTBSY` (bit 3; `make test-pagecache test-vm test-fs test-abi`; `mtest` file checks on RAM, FAT, native and disk roots, `mtest --cache-pressure`; boot `mmap` at smp=4 and smp=1, `user`, `native`, `persistence`, `ram` (cache pressure at 192 MiB), `terminal`, `fault`, `sched` on both ports; and `user`, `mmap`, `fault`, `sched`, `ram` on AArch64 with `FEOS_BOOT_TEST_CPU=max`; verified with `make check-arm check-x64 clippy test`)
- [x] 1a.5 Shared writable file mappings with write-back (bit 4; `make test-pagecache test-vm test-fs`; `mtest` shared checks on RAM, FAT, native and disk roots, `mtest --persist`, `ktest dirty-storm`; boot `mmap` at smp=4 and smp=1, `user`, `native`, `persistence` (64 MiB storm on FAT, synced mapped writes surviving a pulled plug on FAT and native), `fault`, `sched`, `ram` on both ports; verified with `make check-arm check-x64 clippy test`)
- [x] 1a.6 512 GiB window, the v2 layout with a separate argument area and 8 MiB stack, `ABI_VERSION` 2 (bit 5; `make test-abi` (`the_layout_fits_together`, `abi_version_is_2`), `make test-vm` (`the_v2_layout_starts_a_program_as_the_kernel_does`, `the_whole_window_is_usable_far_above_the_old_end`); `mtest` `mmap-above-64g`, `mmap2-noreserve-64g`, `stack-8m`, `stack-below-args`, `stack-overflow-faults`; new `malicious` probes (`mmap2-fixed-at-user-end`, `-below-base`, `-interp-area`, `-in-stack-guard`, `ecam-read`, `stack-guard-read`); AArch64's ECAM moved below the window and a boot check of the kernel's map; boot `mmap`, `user`, `ram` (now with an 8 GiB guest), `fault`, `sched` on both ports, and `mmap`, `user` again with `FEOS_BOOT_TEST_MEM=8G`; verified with `make check-arm check-x64 clippy test`)

### 1b Threads
- [x] 1b.1 `Process` and `Mm` shared and refcounted, pins, user futex keys, TLB shootdown, per-`Mm` ASIDs (bit 6; `make test-vm` (`unmap_with_pinned_page_defers_free`, `translate_and_pin_is_atomic`, `futex_words_are_named_by_what_maps_them`, `feos_vm::asid`), `make test-sched` (`user_key_never_matches_kernel_key`, `user_key_by_space_and_address`); `ktest tlb`, `ktest asid-rollover`, `ktest futex-namespace` in the `smp` phase; boot `smp` (smp=4 and 1), `user`, `fault`, `sched`, `ram`, `mmap` on both ports; verification run: `make check-arm check-x64 clippy test` clean, and every boot phase above again on both ports in one build)
- [x] 1b.2 Threads: `THREAD_CREATE`, `THREAD_EXIT`, `GETTID`, `SET_TLS`, tgid, FP state, user-mode CPU controls (bit 7; `make test-sched` (`tasks_share_tgid`, `kill_tgid_flags_every_thread`, `find_pid_survives_leader_exit`, `exit_record_once_at_last_thread`, `heap_charged_by_slot`, `wake_key_unaffected_by_threads`, `user_reservations_keep_slots_for_the_kernel`), `make test-abi`; `/bin/ttest`, `malicious` thread probes, `ktest thread-fault`; new boot phase `threads` (smp=4 and 1), with `user` and `fault`, on both ports; verification run: `make check-arm check-x64 clippy test` clean, and boot `threads` (smp=4 and 1), `user`, `fault`, `sched`, `ram` again on both ports in one build)
- [x] 1b.3 Time and deadlines: `FUTEX_WAIT2`, `CLOCK_GETTIME`, `CLOCK_GETRES`, `NANOSLEEP`, the wall clock from the RTC at its second's edge, CPU-time clocks (bit 8; `make test-sched` (`deadline_wakes_at_deadline`, `wake_before_deadline`), `make test-abi` (`Timespec` layout, `EINTR`, `EOPNOTSUPP`, clock ids, `int_arg`), `make test-hal` (tick/nanosecond conversions), `make test-rtc`; `/bin/rtest`, `malicious` time probes and `toctou-nanosleep`; new boot phase `time` (smp=4 and 1, against the host's UTC time) on both ports; verification run: `make check-arm check-x64 clippy test` clean, and boot `time`, `user`, `fault`, `sched`, `ram` on both ports in one build, `threads` in an earlier one)
- [x] 1b.4 1024 task slots: scheduler bitmaps, futex buckets, kernel stacks, slots kept for kernel apps (bit 9; `make test-sched` (`bitmap_pick_matches_scan`, `exit_bitmap_matches_scan`, `futex_buckets_match_scan`, `table_of_1024`, `current_beyond_255`, `kernel_reserve_kept`, `per_uid_caps`); `ttest` `64-threads`, `500-threads`; `malicious` `thread-bomb` at 960 tasks, `uid-thread-cap` and `setuid-spawn-race`; `ktest kstack`, `ktest kstack-kill`; boot `smp` (`spawn 200 spin`), `threads`, `user`, `sched`, `fault`, `ram` on both ports; verified with `make check-arm check-x64 clippy test`; verification run: `make check-arm check-x64 clippy test` clean, boot `threads` hung in `ktest kstack-kill` on both ports (a kill waiting on `NoKill` hardly ever landed; now taken as it's dropped), then `threads`, `smp` (smp=4 and 1), `user`, `sched`, `fault`, `ram` on both ports on the fixed tree; still owed: the AArch64 `fault`, `ram`, `sched`, `threads` and `smp` runs with `FEOS_BOOT_TEST_CPU=max`, only `user` has had one; the stack-charge item from 1a.6 moved to 1b.5)
- [x] 1b.5 What scales with tasks: process snapshots, pipe pool, 1024 sockets, ASIDs under load, the main stack's charge and the per-uid process-cap probe (bit 10; `make test-app` (`pipe_pool_grows`, `holders_beyond_32`, `process_snapshot_of_1024`), `make test-net` (`sockets_beyond_64`), `make test-init` and `make test-desktop` (a full table), `make test-vm` (`asids_for_1024_live_spaces`); `malicious` `pipe-exhaustion` (`ENFILE` at 64) and `uid-process-cap` (255 children beside itself, then `EBUSY` from v1 `SPAWN`); `ktest asid-load` (300 programs on 8-bit ASIDs), `ktest asid-rollover` at 1024 spaces; `taskmgr --list`; the main stack and argument area charged as touched; boot `user`, `sched`, `net`, `terminal` (`spawn 500 spin`, listed by `ps` and `taskmgr`), `smp` on both ports; verified with `make check-arm check-x64 clippy test`; verification run: `make check-arm check-x64 clippy test` clean (159 host suites, 2350 tests), then boot `user` (smp=4 and 1), `sched`, `net`, `terminal`, `smp` (smp=4 and 1), `fault`, `ram` on both ports in one container run; still owed: the AArch64 phases with `FEOS_BOOT_TEST_CPU=max` beyond `user`)

### 1c Files, processes, signals
- [x] 1c.1 OFDs, the growable FD table, `DUP3`, `FCNTL`, `PIPE2`, `CLOSE_RANGE`, whole file transfers (bit 11; `make test-fdtable` (`lowest_free`, `grows_to_max`, `dup3_replaces`, `cloexec_sweep`, `close_range`, `close_range_cloexec`), `make test-app` (`pipe_buf_writes_atomic`, `nonblocking_read_eagain`, `large_write_completes`); new `ftest` (with `append-two-processes`, `path-relative-4096`), new `malicious` FD, `fcntl`, `pipe2`, `close-range` and path probes, `fd-exhaustion`, `fd-heap-budget`; new boot phase `posix-fs` (RAM root, then a FAT root beside a native disk, `fsck` after), `user`, `native` (`permtest`) on both ports; verification run: `make check-arm check-x64 clippy test` clean (161 host suites, 2364 tests), then boot `posix-fs`, `user`, `native`, `fault`, `sched` (smp=4 and 1), `ram` on both ports in one container run)
- [x] 1c.2 `OPENAT`, `FSTATAT`, `GETDENTS`, `MKDIRAT`, `UNLINKAT` with orphans, `FTRUNCATE`, `FSYNC`, `PREAD`/`PWRITE`, `FCHDIR`, `GETCWD`, `UMASK`, `FD_PATH`, `CURRENT_EXE` (bit 12; `make test-abi` (`stat2_layout`, `dirent_layout`, `path_at_layout`), `make test-fs` (`orphans_cleaned_at_mount`, `orphans_hidden_from_listing`, `trailing_slash_rules`, `dotdot_is_physical`, `forty_links_is_eloop`, `cookies_survive_resnapshot`); the stage's `ftest` checks and `malicious` probes (1b's `mmap-dir` now wants the unlinked file's own bytes, as the description holds it); boot phases `posix-fs`, `native`, `persistence` on both ports; verification run: `make check-arm check-x64 clippy test` clean (161 host suites, 2377 tests), then boot `posix-fs`, `native`, `persistence`, `user`, `fault`, `sched` (smp=4 and 1), `ram` on both ports)
- [x] 1c.3 `/dev`, `/proc/self` and the image layout (`/tmp`, `/etc`, `$HOME`, CA certificates) (bit 13; `make test-fs` (`devfs_nodes_and_modes`, `devfs_refuses_changes`, `procfs_lists_fds`, `procfs_hides_other_pids`, `sticky_tmp_refuses_foreign_unlink`, `procfs_links_stay_links`); `ftest` with `dev-console-as-user`, `image-layout`, `etc-files-present`; `malicious` /dev and /proc probes; `fsflush` restarts only once its PID has gone; boot `posix-fs`, `user`, `persistence`, `fault`, `sched`, `ram` on both ports)
- [x] 1c.4 `RENAMEAT`, `LINKAT`, `SYMLINKAT`, `READLINKAT`, `FCHMODAT`, `FCHOWNAT`, `UTIMENSAT`, `FACCESSAT`, `FSTATVFS`, `SYNC`; nanosecond times and update rules (bit 14; `make test-fs` (`rename_replaces_file_atomically` on each filesystem and at every crash cut, `rename_dir_over_empty_dir`, `rename_noreplace`, `rename_over_file_in_use_orphans_it`, `ram_times_are_nanoseconds`, `native_times_are_nanoseconds`, `native_old_image_reads_zero_nanoseconds`, `exfat_times_10ms`, `time_update_rules`, `program_calls_judge_what_the_walk_finds`, `changes_by_identity_ask_of_the_file_changed`); the stage's `ftest` checks (`--coarse` on FAT, `--hard-links` on native), `permtest` `*at` calls as user 1000, `malicious` rename, link, owner, time and `toctou-renameat` probes; native format version 2; boot `posix-fs`, `native`, `persistence` (rename and nanosecond times across a power-off) on both ports; verification run: `make check-arm check-x64 clippy test` clean (161 host suites, 2396 tests), then boot `posix-fs`, `native`, `persistence`, `user`, `fault`, `sched` (smp=4 and 1), `ram` on both ports in one container run)
- [x] 1c.5 Locks: `FLOCK` and `fcntl` record locks (bit 15; `make test-locks` (new crate: `range_split_merge`, `read_locks_share`, `write_conflicts`, `unlock_middle_splits`, `close_any_fd_drops_posix_locks`, `flock_follows_ofd`, `per_owner_cap`, `range_of_whence_and_lengths`, `matches_byte_model`, `ranges_at_the_largest_offset_fit_a_flock`); the stage's `ftest` checks (`flock-exclusive-blocks`, `flock-nb-eagain`, `flock-shared-shares`, `flock-released-at-exit`, `fcntl-setlk-conflict`, `fcntl-getlk-reports-pid`, `fcntl-locks-dropped-on-close`, `flock-struct-padding-ignored`, `sqlite-style`), `malicious` `flock-*`/`setlk-*`/`getlk-*` probes, `lock-exhaustion`, `toctou-setlk`; `ktest setlkw-killed`, and `posix-fs` checks no locks are left after each `ftest` and `malicious` run; boot `posix-fs` on both ports; verification run: `make check-arm check-x64 clippy test` clean (163 host suites, 2407 tests), then boot `posix-fs`, `user`, `fault`, `sched` (smp=4 and 1), `ram` on both ports in one container run)
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
