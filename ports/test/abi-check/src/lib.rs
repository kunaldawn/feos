//! Checks relibc's feos platform constants against the feos kernel's own.
//!
//! relibc keeps every feos call number, structure and feos-only constant in
//! `src/platform/feos/abi.rs`, included here as it is. The tests compare it with:
//! - `feos-abi` (the base's `libs/abi`, a dev-dependency): every structure's size and
//!   offsets, and every constant both have;
//! - the text of a `libs/abi/src/lib.rs` (`FEOS_ABI_LIB`, default the base's): every
//!   call number and error number it defines, so a tree with more stages checks more;
//! - the call table of `docs/feos-abi-v2.md` section 3 (`FEOS_ABI_DOC`, default the
//!   tree's): every number from 31 to 108, including calls no kernel has yet.

#[cfg(test)]
#[path = "../../../src/relibc/src/platform/feos/abi.rs"]
mod relibc;

#[cfg(test)]
mod tests {
    use super::relibc;
    use feos_abi as kernel;
    use std::{collections::BTreeMap, env, fs, mem::{offset_of, size_of}, path::PathBuf};

    fn repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
    }

    fn kernel_lib() -> String {
        let path = env::var("FEOS_ABI_LIB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo().join("target/ports-base/libs/abi/src/lib.rs"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// `pub const NAME: TYPE = VALUE;` lines of `pub mod MODULE { ... }` in `text`.
    fn consts_of(text: &str, module: &str) -> BTreeMap<String, i64> {
        let start = text
            .find(&format!("pub mod {module} {{"))
            .unwrap_or_else(|| panic!("no `pub mod {module}`"));
        let mut out = BTreeMap::new();
        let mut depth = 0;
        for line in text[start..].lines() {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("pub const ") {
                let (name, rest) = rest.split_once(':').unwrap();
                if let Some((_, value)) = rest.split_once('=') {
                    let value = value.trim().trim_end_matches(';').replace('_', "");
                    let parsed = if let Some(hex) = value.strip_prefix("0x") {
                        i64::from_str_radix(hex, 16).ok()
                    } else {
                        value.parse::<i64>().ok()
                    };
                    if let Some(v) = parsed {
                        out.insert(name.trim().to_string(), v);
                    }
                }
            }
            if depth <= 0 {
                break;
            }
        }
        out
    }

    /// relibc's side, by name, from the same file the platform builds with.
    fn relibc_text() -> String {
        fs::read_to_string(repo().join("ports/src/relibc/src/platform/feos/abi.rs")).unwrap()
    }

    #[test]
    fn call_numbers_match_the_kernel() {
        let kernel = consts_of(&kernel_lib(), "nr");
        let ours = consts_of(&relibc_text(), "nr");
        let mut compared = 0;
        for (name, number) in &ours {
            if let Some(theirs) = kernel.get(name) {
                assert_eq!(number, theirs, "nr::{name}");
                compared += 1;
            }
        }
        // Everything up to 1c.6 is in the base: 55 of relibc's 96 numbers at least.
        assert!(compared >= 55, "only {compared} call numbers found in the kernel's feos-abi");
        // And every call the kernel has that relibc names is one of ours, at its number.
        for name in ["OPENAT", "SPAWN2", "THREAD_CREATE", "SYSINFO", "FUTEX_WAIT2", "GETDENTS"] {
            assert_eq!(ours.get(name), kernel.get(name), "nr::{name}");
        }
        println!("{compared} call numbers compared with the kernel's feos-abi");
    }

    #[test]
    fn call_numbers_match_the_design() {
        let path = env::var("FEOS_ABI_DOC")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo().join("docs/feos-abi-v2.md"));
        let doc = fs::read_to_string(path).unwrap();
        let start = doc.find("## 3. The calls").unwrap();
        let end = doc[start..].find("### 3.1").unwrap() + start;
        let ours = consts_of(&relibc_text(), "nr");
        let mut seen = 0;
        for row in doc[start..end].lines().filter(|l| l.starts_with("| ")) {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            for pair in cells.windows(2) {
                if let (Ok(number), Some(name)) =
                    (pair[0].parse::<i64>(), pair[1].strip_prefix('`').and_then(|n| n.strip_suffix('`')))
                {
                    assert_eq!(ours.get(name), Some(&number), "design's {number} `{name}`");
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 108 - 31 + 1, "the design's table has every number from 31 to 108");
    }

    #[test]
    fn error_numbers_match_the_kernel() {
        let kernel = consts_of(&kernel_lib(), "errno");
        let ours = consts_of(&relibc_text(), "errno");
        let mut compared = 0;
        for (name, number) in &kernel {
            if let Some(mine) = ours.get(name) {
                assert_eq!(mine, number, "errno::{name}");
                compared += 1;
            } else if name != "ENOTSUP" && name != "EWOULDBLOCK" {
                panic!("the kernel returns {name}, which relibc's feos abi doesn't name");
            }
        }
        assert!(compared >= 30);
    }

    macro_rules! same_layout {
        ($ours:ty, $theirs:ty $(, $a:ident = $b:ident)*) => {{
            assert_eq!(size_of::<$ours>(), size_of::<$theirs>(), stringify!($ours));
            $(assert_eq!(offset_of!($ours, $a), offset_of!($theirs, $b),
                concat!(stringify!($ours), "::", stringify!($a)));)*
        }};
    }

    #[test]
    fn structures_match_the_kernel() {
        same_layout!(relibc::PathAt, kernel::PathAt, dirfd = dirfd, pointer = pointer, length = length);
        same_layout!(relibc::Timespec, kernel::Timespec, sec = sec, nsec = nsec);
        same_layout!(relibc::Timeval, kernel::Timeval, sec = sec, usec = usec);
        same_layout!(relibc::Stat2, kernel::Stat2, dev = dev, ino = ino, mode = mode, nlink = nlink,
            uid = uid, gid = gid, rdev = rdev, size = size, blksize = blksize, blocks = blocks,
            atime = atime, mtime = mtime, ctime = ctime, btime = btime, flags = flags);
        same_layout!(relibc::Dirent, kernel::Dirent, ino = ino, off = off, reclen = reclen, kind = kind);
        assert_eq!(relibc::DIRENT_HEADER, kernel::Dirent::HEADER);
        same_layout!(relibc::StatVfs, kernel::StatVfs, bsize = bsize, frsize = frsize, blocks = blocks,
            bfree = bfree, bavail = bavail, files = files, ffree = ffree, favail = favail,
            fsid = fsid, flags = flags, namemax = namemax);
        same_layout!(relibc::Flock, kernel::Flock, kind = kind, whence = whence, start = start,
            length = length, pid = pid);
        same_layout!(relibc::UserStr, kernel::UserStr, pointer = pointer, length = length);
        same_layout!(relibc::SpawnRequest, kernel::SpawnRequest, path = path, argv = argv, argc = argc,
            envp = envp, envc = envc, actions = actions, action_count = action_count, flags = flags,
            pgroup = pgroup, sigmask = sigmask, sigdefault = sigdefault, reserved = reserved);
        same_layout!(relibc::SpawnAction, kernel::SpawnAction, kind = kind, fd = fd, to = to,
            open_flags = open_flags, mode = mode, path = path);
        same_layout!(relibc::Rusage, kernel::Rusage, utime = utime, stime = stime, maxrss = maxrss,
            minflt = minflt, nvcsw = nvcsw, nivcsw = nivcsw);
        same_layout!(relibc::SysInfo, kernel::SysInfo, abi_version = abi_version, page_size = page_size,
            cpus_online = cpus_online, cpus_configured = cpus_configured, total_pages = total_pages,
            free_pages = free_pages, uptime_ns = uptime_ns, arg_max = arg_max, fd_max = fd_max,
            task_max = task_max, path_max = path_max, name_max = name_max,
            memory_limit = memory_limit, release = release, machine = machine, flags = flags,
            features = features);
    }

    macro_rules! same {
        ($($a:expr => $b:expr),* $(,)?) => {
            $(assert_eq!($a as i64, $b as i64, stringify!($a));)*
        };
    }

    #[test]
    fn constants_match_the_kernel() {
        use kernel as k;
        use relibc as r;
        same! {
            r::ABI_VERSION => k::ABI_VERSION,
            r::PAGE_SIZE => k::PAGE_SIZE,
            r::USER_BASE => k::USER_BASE, r::USER_END => k::USER_END,
            r::USER_STACK_TOP => k::USER_STACK_TOP, r::USER_ARGS_SIZE => k::USER_ARGS_SIZE,
            r::USER_STACK_SIZE => k::USER_STACK_SIZE, r::USER_STACK_GUARD => k::USER_STACK_GUARD,
            r::o::RDONLY => k::o::RDONLY, r::o::WRONLY => k::o::WRONLY, r::o::RDWR => k::o::RDWR,
            r::o::ACCMODE => k::o::ACCMODE, r::o::CREAT => k::o::CREAT, r::o::EXCL => k::o::EXCL,
            r::o::NOCTTY => k::o::NOCTTY, r::o::TRUNC => k::o::TRUNC, r::o::APPEND => k::o::APPEND,
            r::o::NONBLOCK => k::o::NONBLOCK, r::o::DSYNC => k::o::DSYNC, r::o::ASYNC => k::o::ASYNC,
            r::o::DIRECT => k::o::DIRECT, r::o::LARGEFILE => k::o::LARGEFILE,
            r::o::DIRECTORY => k::o::DIRECTORY, r::o::NOFOLLOW => k::o::NOFOLLOW,
            r::o::NOATIME => k::o::NOATIME, r::o::CLOEXEC => k::o::CLOEXEC, r::o::SYNC => k::o::SYNC,
            r::o::PATH => k::o::PATH, r::o::TMPFILE => k::o::TMPFILE,
            r::at::FDCWD => k::at::FDCWD, r::at::SYMLINK_NOFOLLOW => k::at::SYMLINK_NOFOLLOW,
            r::at::REMOVEDIR => k::at::REMOVEDIR, r::at::EACCESS => k::at::EACCESS,
            r::at::SYMLINK_FOLLOW => k::at::SYMLINK_FOLLOW, r::at::NO_AUTOMOUNT => k::at::NO_AUTOMOUNT,
            r::at::EMPTY_PATH => k::at::EMPTY_PATH,
            r::f::DUPFD => k::f::DUPFD, r::f::GETFD => k::f::GETFD, r::f::SETFD => k::f::SETFD,
            r::f::GETFL => k::f::GETFL, r::f::SETFL => k::f::SETFL, r::f::GETLK => k::f::GETLK,
            r::f::SETLK => k::f::SETLK, r::f::SETLKW => k::f::SETLKW,
            r::f::DUPFD_CLOEXEC => k::f::DUPFD_CLOEXEC, r::f::FD_CLOEXEC => k::f::FD_CLOEXEC,
            r::s_if::MASK => k::s_if::MASK, r::s_if::SOCK => k::s_if::SOCK, r::s_if::LNK => k::s_if::LNK,
            r::s_if::REG => k::s_if::REG, r::s_if::BLK => k::s_if::BLK, r::s_if::DIR => k::s_if::DIR,
            r::s_if::CHR => k::s_if::CHR, r::s_if::FIFO => k::s_if::FIFO,
            r::prot::NONE => k::prot::NONE, r::prot::READ => k::prot::READ,
            r::prot::WRITE => k::prot::WRITE, r::prot::EXEC => k::prot::EXEC,
            r::map::SHARED => k::map::SHARED, r::map::PRIVATE => k::map::PRIVATE,
            r::map::SHARED_VALIDATE => k::map::SHARED_VALIDATE, r::map::FIXED => k::map::FIXED,
            r::map::ANONYMOUS => k::map::ANONYMOUS, r::map::NORESERVE => k::map::NORESERVE,
            r::map::POPULATE => k::map::POPULATE, r::map::STACK => k::map::STACK,
            r::map::FIXED_NOREPLACE => k::map::FIXED_NOREPLACE,
            r::clock::REALTIME => k::clock::REALTIME, r::clock::MONOTONIC => k::clock::MONOTONIC,
            r::clock::PROCESS_CPUTIME_ID => k::clock::PROCESS_CPUTIME_ID,
            r::clock::THREAD_CPUTIME_ID => k::clock::THREAD_CPUTIME_ID,
            r::clock::MONOTONIC_RAW => k::clock::MONOTONIC_RAW,
            r::clock::REALTIME_COARSE => k::clock::REALTIME_COARSE,
            r::clock::MONOTONIC_COARSE => k::clock::MONOTONIC_COARSE,
            r::clock::BOOTTIME => k::clock::BOOTTIME,
            r::seek::SET => k::seek::SET, r::seek::CURRENT => k::seek::CURRENT, r::seek::END => k::seek::END,
            r::FSYNC_DATA => k::FSYNC_DATA,
            r::spawn::RESETIDS => k::spawn::RESETIDS, r::spawn::SETPGROUP => k::spawn::SETPGROUP,
            r::spawn::SETSIGDEF => k::spawn::SETSIGDEF, r::spawn::SETSIGMASK => k::spawn::SETSIGMASK,
            r::spawn::SETSCHEDPARAM => k::spawn::SETSCHEDPARAM,
            r::spawn::SETSCHEDULER => k::spawn::SETSCHEDULER, r::spawn::USEVFORK => k::spawn::USEVFORK,
            r::spawn::SETSID => k::spawn::SETSID, r::spawn::EMPTY_PATH => k::spawn::EMPTY_PATH,
            r::spawn::ARG_MAX => k::spawn::ARG_MAX,
            r::spawn_action::CLOSE => k::spawn_action::CLOSE, r::spawn_action::DUP2 => k::spawn_action::DUP2,
            r::spawn_action::OPEN => k::spawn_action::OPEN, r::spawn_action::CHDIR => k::spawn_action::CHDIR,
            r::spawn_action::FCHDIR => k::spawn_action::FCHDIR,
            r::spawn_action::CLOSEFROM => k::spawn_action::CLOSEFROM,
            r::spawn_action::TCSETPGRP => k::spawn_action::TCSETPGRP,
            r::rusage::SELF => k::rusage::SELF, r::rusage::CHILDREN => k::rusage::CHILDREN,
            r::rusage::THREAD => k::rusage::THREAD,
            r::aux::NULL => k::aux::NULL, r::aux::PHDR => k::aux::PHDR, r::aux::PHENT => k::aux::PHENT,
            r::aux::PHNUM => k::aux::PHNUM, r::aux::PAGESZ => k::aux::PAGESZ, r::aux::BASE => k::aux::BASE,
            r::aux::FLAGS => k::aux::FLAGS, r::aux::ENTRY => k::aux::ENTRY, r::aux::UID => k::aux::UID,
            r::aux::EUID => k::aux::EUID, r::aux::GID => k::aux::GID, r::aux::EGID => k::aux::EGID,
            r::aux::PLATFORM => k::aux::PLATFORM, r::aux::HWCAP => k::aux::HWCAP,
            r::aux::CLKTCK => k::aux::CLKTCK, r::aux::SECURE => k::aux::SECURE,
            r::aux::RANDOM => k::aux::RANDOM, r::aux::HWCAP2 => k::aux::HWCAP2,
            r::aux::EXECFN => k::aux::EXECFN, r::aux::MINSIGSTKSZ => k::aux::MINSIGSTKSZ,
            r::aux::FEOS_ABI => k::aux::FEOS_ABI, r::aux::FEOS_FLAGS => k::aux::FEOS_FLAGS,
            r::aux::FEOS_FEATURES => k::aux::FEOS_FEATURES, r::aux::FEOS_EXE => k::aux::FEOS_EXE,
        }
        // relibc needs every stage through 1c.6, and no bit the kernel doesn't define.
        assert_eq!(r::FEATURES_REQUIRED & !k::FEATURES, 0);
    }
}
