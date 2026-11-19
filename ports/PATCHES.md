# Patches to upstream sources

Every file under `ports/src/` that feos changes, with why. The changes stay
uncommitted in the submodules' working trees until the forks exist
(`docs/selfhost-todo.md` phase 0); each is small and meant to go upstream, in the
style of a tier 3 target. `git -C ports/src/<name> diff` shows them.

A commit of this repo records only each submodule's pinned commit, so the changes are
also kept here, one patch per submodule, in `ports/patches/<name>.patch` (nested
submodules as `rust__src__llvm-project.patch`). `ports/patches.sh save` writes them
from the working trees, new files included; `ports/patches.sh check` fails if they are
stale; `scripts/ports-init.sh` applies them to a fresh clone (`ports/patches.sh apply`).
**Run `ports/patches.sh save` before every commit of this repo.**

| Submodule | File | Why | Upstreamable |
|---|---|---|---|
| `relibc` | `src/platform/linux/mod.rs` | `poll` on AArch64 Linux goes through `ppoll` (AArch64 has no `poll(2)` syscall; relibc's Linux platform didn't build for `aarch64-unknown-linux-gnu`). Lets the host-side relibc tests run on both architectures. | yes, as is |
| `relibc` | `src/platform/feos/` (new: `mod.rs`, `abi.rs`, `syscall.rs`, `signal.rs`, `socket.rs`, `epoll.rs`, `ptrace.rs`, `checks.rs`) | The feos platform (track T1): `Pal`, `PalSignal`, `PalSocket`, `PalEpoll`, `PalPtrace` over the feos calls as `docs/feos-abi-v2.md` section 9 maps them, the raw `int $0xf0`/`svc #0` calls, the thread trampoline, `Stat2`/`StatVfs`/`SysInfo` conversions, `ioctl` translation, `getauxval`, `sysconf` from `SYSINFO`. `abi.rs` is the one place the feos numbers and structures live; `checks.rs` asserts at build time that relibc's header values are feos's. | yes, as a new platform |
| `relibc` | `src/platform/mod.rs` | Selects `platform/feos` for `target_os = "feos"`, uses the Linux `auxv_defs` there (re-exported from `header/sys_auxv`, which now holds the module so cbindgen sees it), and keeps the aux vector for `getauxval` (`init`). | yes |
| `relibc` | `src/platform/auxv_defs.rs` | `AT_MINSIGSTKSZ` (51) for all, and the four `AT_FEOS_*` types for feos. | yes |
| `relibc` | `src/platform/types.rs`, `src/header/bits_mode-t/mod.rs`, `src/header/bits_suseconds-t/mod.rs` | feos takes Linux's `mode_t` (`unsigned int`) and `suseconds_t` (`long`). | yes |
| `relibc` | `src/header/{_paths,bits_open-flags,fcntl,grp,limits,netdb,pwd,shadow,stdio/constants,stdlib,sys_epoll,sys_mman,sys_syslog,termios,time/constants,unistd/sysconf}.rs` (the `mod.rs` or named file of each) | `cfg(target_os = "linux")` becomes `any(linux, feos)` where the Linux values or files apply: feos's constants and layouts are Linux's (section 5). | yes |
| `relibc` | `src/header/unistd/mod.rs` | The `alarm` microsecond conversion takes the 64-bit Linux path on feos too. | yes |
| `relibc` | `src/header/unistd/sysconf/linux.rs` | On feos, the values the kernel knows (`_SC_NPROCESSORS_*`, `_SC_PHYS_PAGES`, `_SC_AVPHYS_PAGES`, `_SC_ARG_MAX`, `_SC_OPEN_MAX`, `_SC_CHILD_MAX`, `_SC_MINSIGSTKSZ`, ...) come from `SYSINFO`; the rest of the Linux table is shared. | yes |
| `relibc` | `src/header/sys_ioctl/mod.rs` | `ioctl` goes to `Sys::ioctl` on feos too, which translates the Linux request numbers to feos's (section 3.3, `IOCTL`). | yes |
| `relibc` | `src/header/signal/mod.rs`, `src/header/signal/feos.rs` (new), `src/header/signal/cbindgen.toml` | feos's `signal.h` part: Linux's numbers and `SA_*`, Linux's `siginfo_t`, feos's own `ucontext_t`/`mcontext_t` (section 8.4) and `__restore_rt` (`SIGRETURN`, 89); the `si_*` macros for feos. | yes |
| `relibc` | `src/header/sys_auxv/mod.rs` | `getauxval` reads the saved aux vector on feos (a stub returning 0 elsewhere, as before). On Linux and feos it holds `platform/auxv_defs.rs` (by `#[path]`) instead of re-exporting it, so cbindgen puts the `AT_*` names, `AT_HWCAP`, `AT_MINSIGSTKSZ` and `AT_FEOS_*` included, in `<sys/auxv.h>`; before, it declared only the function. | yes: a fix for Linux too |
| `relibc` | `src/header/spawn/spawn_attr.rs` | On feos, `POSIX_SPAWN_*` are glibc's bit values (and `POSIX_SPAWN_USEVFORK`): relibc's 1-to-7 sequence can't be ORed, and `SPAWN2` takes glibc's values unchanged (section 3.4). | yes; the sequence is a bug on every platform |
| `relibc` | `src/header/sys_select/mod.rs` | A `poll`-based `select`/`pselect` for feos, which has no epoll (section 10.1). | yes |
| `relibc` | `src/header/fcntl/linux.rs` | `FD_CLOEXEC` is 1, not `O_CLOEXEC`'s 0x80000: Linux's `F_SETFD` ignores the other bits (so `fcntl(fd, F_SETFD, FD_CLOEXEC)` set nothing on Linux) and feos refuses them with `EINVAL`. | yes: a Linux bug fix |
| `relibc` | `src/ld_so/tcb.rs` | feos's TCB: Linux's layouts, activated with the `SET_TLS` call on both ports. | yes |
| `relibc` | `src/ld_so/mod.rs` | `init` on feos: the TLS register is zero at start (section 7.1) and x86_64 user mode can't read `FS_BASE`, so the TLS is taken as set only when an interpreter loaded the program (`AT_BASE`). | yes |
| `relibc` | `src/pthread/mod.rs` | feos's `OsTid` is a thread id; the main thread reports its 8 MiB stack below the argument area (section 6). A new `CALLER_STACK` flag keeps `exit_thread` from unmapping a stack the caller gave (`pthread_attr_setstack`), which feos's `THREAD_EXIT` would otherwise free. | yes (Redox has the same unmap) |
| `relibc` | `src/macros.rs`, `Cargo.toml` | No layout checks against the libc crate on feos (it has no feos module until the Rust target, track T3): the optional `libc` dependency moves to a `cfg(not(target_os = "feos"))` table. `Cargo.lock` is unchanged. | yes, until the libc crate has feos |
| `relibc` | `cbindgen.globdefs.toml` | `target_os = "feos"` is `__feos__` in the generated headers. | yes |
| `relibc` | `src/header/spawn/mod.rs` | `posix_spawnp` as glibc's: `/bin:/usr/bin` when `PATH` is unset (was `ENOENT`), each candidate tried with `posix_spawn`, `ENOENT`/`ENOTDIR` and `EACCES` continuing the search, and `EACCES` the error if one was seen (it took the first candidate `access(F_OK)` passed, so a directory or unexecutable file early in `PATH` ended the search). | yes |
| `relibc` | `src/header/spawn/file_actions.rs` | glibc's `_np` file actions: `addchdir_np` and `addfchdir_np` (aliases; Rust's std calls them) everywhere, and on feos `addclosefrom_np` and `addtcsetpgrp_np` (`Action::CloseFrom`, `Action::TcSetPgrp`, `SPAWN2` action kinds 6 and 7). `addopen` and `addchdir` copy the caller's path instead of taking ownership of it with `CString::from_raw` (undefined behaviour, and a buffer reused before the spawn changed the action), and `destroy` frees the actions instead of leaking them. | yes: the copy and the leak are bugs on every platform |
