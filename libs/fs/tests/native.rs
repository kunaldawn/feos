//! The native filesystem: what it does, and that a crash at any write leaves it whole.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use feos_fs::native::{format, Native, BLOCK as FS_BLOCK, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use feos_fs::{BlockDevice, FileSystem, FsError, NodeId, RamDisk, Vfs, VfsTree, WritableBlockDevice, WritableFileSystem};

const SECTOR: usize = 512;
const SIZE: usize = 4 * 1024 * 1024;

fn fresh() -> Native<RamDisk> {
    let mut disk = RamDisk::new(SIZE, SECTOR).unwrap();
    format(&mut disk, 1000).unwrap();
    Native::mount(disk).unwrap()
}

fn tree(fs: Native<RamDisk>) -> Vfs {
    Vfs::new(Box::new(fs), false, "native")
}

fn remount(fs: Native<RamDisk>) -> Native<RamDisk> {
    let mut fs = fs;
    WritableFileSystem::flush(&mut fs).unwrap();
    Native::mount(fs.into_device()).unwrap()
}

fn assert_clean(fs: &mut Native<RamDisk>, context: &str) {
    let report = fs.fsck(false).unwrap();
    assert!(report.errors.is_empty(), "{context}: {:?}", report.errors);
}

#[test]
fn a_fresh_volume_is_detected_and_clean() {
    let mut disk = RamDisk::new(SIZE, SECTOR).unwrap();
    format(&mut disk, 1).unwrap();
    assert_eq!(feos_fs::detect(&disk).unwrap(), feos_fs::VolumeKind::Native);
    let mut fs = Native::mount(disk).unwrap();
    assert_clean(&mut fs, "fresh");
    assert_eq!(fs.child_count(NodeId::ROOT), 0);
    assert_eq!(fs.posix(NodeId::ROOT).unwrap().mode, S_IFDIR | 0o755);
    assert!(fs.stats().free_blocks > 0);
    assert_eq!(fs.stats().kind, "feos-native");
}

#[test]
fn the_host_formatter_makes_the_same_volume() {
    // A boot test's disk comes from a script, as a container has no Rust to run. If this
    // host has no Python, there's no script to keep honest here either.
    let dir = std::env::temp_dir().join(format!("feos-mkfs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("volume.img");
    for size in [SIZE, 64 * 1024 * 1024 + 8192] {
        std::fs::File::create(&path).unwrap().set_len(size as u64).unwrap();
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/mkfs-feos.py");
        let Ok(status) = std::process::Command::new("python3").arg(script).arg(&path).arg("7").status() else {
            std::eprintln!("no python3: skipped");
            break;
        };
        assert!(status.success());
        let made = std::fs::read(&path).unwrap();
        let mut disk = RamDisk::new(size, SECTOR).unwrap();
        format(&mut disk, 7).unwrap();
        assert!(made == disk.into_data(), "the images differ at size {size}");
        let mut fs = Native::mount(RamDisk::from_bytes(made, SECTOR).unwrap()).unwrap();
        assert_clean(&mut fs, "from the script");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_survive_a_remount_with_their_metadata() {
    let mut fs = fresh();
    fs.set_time_source(|| Some(12345));
    let dir = fs.create_dir(NodeId::ROOT, "d\u{e9}").unwrap();
    let file = fs.create_file(dir, "file.txt").unwrap();
    assert_eq!(fs.write(file, 0, b"hello").unwrap(), 5);
    fs.set_mode(file, 0o640).unwrap();
    fs.set_owner(file, Some(1000), Some(100)).unwrap();

    let mut fs = remount(fs);
    let file = fs.resolve("/d\u{e9}/file.txt").unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap(), b"hello");
    let meta = fs.posix(file).unwrap();
    assert_eq!(meta.mode, S_IFREG | 0o640);
    assert_eq!((meta.uid, meta.gid, meta.nlink), (1000, 100, 1));
    assert_eq!(fs.times(file).modified, Some(12345));
    // A directory counts itself, `.`, and its subdirectories' `..`.
    assert_eq!(fs.posix(fs.resolve("/d\u{e9}").unwrap()).unwrap().nlink, 2);
    assert_eq!(fs.posix(NodeId::ROOT).unwrap().nlink, 3);
    assert_clean(&mut fs, "remount");
}

#[test]
fn large_files_go_through_every_kind_of_pointer() {
    let mut disk = RamDisk::new(4 * SIZE, SECTOR).unwrap();
    format(&mut disk, 1000).unwrap();
    let fs = Native::mount(disk).unwrap();
    let mut fs = fs;
    let file = fs.create_file(NodeId::ROOT, "big").unwrap();
    // Past the 12 direct and 1024 indirect blocks.
    let size = (12 + 1024 + 50) * FS_BLOCK + 123;
    let data: Vec<u8> = (0..size).map(|at| (at / 7) as u8).collect();
    assert_eq!(fs.write(file, 0, &data).unwrap(), size);
    let mut fs = remount(fs);
    let file = fs.resolve("/big").unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap(), data);
    assert_clean(&mut fs, "written");

    // Shrinks through each kind again, giving back what it held.
    let before = fs.stats().free_blocks;
    fs.set_len(file, 3 * FS_BLOCK + 7).unwrap();
    let mut fs = remount(fs);
    assert!(fs.stats().free_blocks > before + 1000);
    let file = fs.resolve("/big").unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap(), &data[..3 * FS_BLOCK + 7]);
    assert_clean(&mut fs, "shrunk");

    // Growing again reads as zeros, not what used to be there.
    fs.set_len(file, 4 * FS_BLOCK).unwrap();
    let back = fs.read_to_vec(file).unwrap();
    assert_eq!(&back[..3 * FS_BLOCK + 7], &data[..3 * FS_BLOCK + 7]);
    assert!(back[3 * FS_BLOCK + 7..].iter().all(|byte| *byte == 0));
    assert_clean(&mut fs, "grown");
}

#[test]
fn holes_read_as_zeros_and_take_no_room() {
    let mut fs = fresh();
    let file = fs.create_file(NodeId::ROOT, "sparse").unwrap();
    let before = fs.stats().free_blocks;
    assert_eq!(fs.write(file, 100 * FS_BLOCK, b"end").unwrap(), 3);
    assert!(before - fs.stats().free_blocks < 4);
    let mut out = [9u8; 16];
    assert_eq!(fs.try_read(file, 5 * FS_BLOCK, &mut out).unwrap(), 16);
    assert_eq!(out, [0u8; 16]);
    assert_clean(&mut fs, "sparse");
}

#[test]
fn hard_links_share_an_inode_until_the_last_goes() {
    let mut fs = fresh();
    let sub = fs.create_dir(NodeId::ROOT, "sub").unwrap();
    let first = fs.create_file(NodeId::ROOT, "first").unwrap();
    fs.write(first, 0, b"shared").unwrap();
    let second = fs.link(first, sub, "second").unwrap();
    assert_eq!(fs.posix(first).unwrap().ino, fs.posix(second).unwrap().ino);
    assert_eq!(fs.posix(first).unwrap().nlink, 2);
    fs.write(second, 0, b"SHARED").unwrap();
    assert_eq!(fs.read_to_vec(first).unwrap(), b"SHARED");
    assert_eq!(fs.link(sub, NodeId::ROOT, "no").unwrap_err(), FsError::IsDirectory);

    let mut fs = remount(fs);
    let (first, second) = (fs.resolve("/first").unwrap(), fs.resolve("/sub/second").unwrap());
    assert_eq!(fs.posix(second).unwrap().nlink, 2);
    assert_clean(&mut fs, "linked");
    let before = fs.stats().free_blocks;
    WritableFileSystem::remove(&mut fs, first).unwrap();
    assert_eq!(fs.read_to_vec(second).unwrap(), b"SHARED");
    assert_eq!(fs.posix(second).unwrap().nlink, 1);
    assert_clean(&mut fs, "one gone");
    WritableFileSystem::remove(&mut fs, second).unwrap();
    WritableFileSystem::flush(&mut fs).unwrap();
    assert_eq!(fs.stats().free_blocks, before + 1);
    assert_clean(&mut fs, "both gone");
}

#[test]
fn renames_keep_the_inode_and_refuse_cycles_and_collisions() {
    let mut fs = fresh();
    let a = fs.create_dir(NodeId::ROOT, "a").unwrap();
    let b = fs.create_dir(a, "b").unwrap();
    let file = fs.create_file(b, "f").unwrap();
    fs.write(file, 0, b"x").unwrap();
    let ino = fs.posix(file).unwrap().ino;
    assert_eq!(WritableFileSystem::rename(&mut fs, a, b, "no").unwrap_err(), FsError::InvalidName);
    assert_eq!(WritableFileSystem::rename(&mut fs, file, b, "f").unwrap_err(), FsError::NameExists);
    WritableFileSystem::rename(&mut fs, file, NodeId::ROOT, "moved").unwrap();
    WritableFileSystem::rename(&mut fs, b, NodeId::ROOT, "b2").unwrap();
    let mut fs = remount(fs);
    assert_eq!(fs.posix(fs.resolve("/moved").unwrap()).unwrap().ino, ino);
    assert!(fs.resolve("/a/b").is_none());
    assert!(fs.resolve("/b2").is_some());
    assert_clean(&mut fs, "renamed");
}

#[test]
fn directories_grow_and_names_are_checked() {
    let mut fs = fresh();
    let dir = fs.create_dir(NodeId::ROOT, "many").unwrap();
    for number in 0..100 {
        fs.create_file(dir, &format!("file-{number}")).unwrap();
    }
    assert_eq!(fs.create_file(dir, "a/b").unwrap_err(), FsError::InvalidName);
    assert_eq!(fs.create_file(dir, "..").unwrap_err(), FsError::InvalidName);
    assert_eq!(fs.create_file(dir, &"n".repeat(251)).unwrap_err(), FsError::InvalidName);
    assert!(fs.create_file(dir, &"n".repeat(250)).is_ok());
    assert_eq!(WritableFileSystem::remove(&mut fs, dir).unwrap_err(), FsError::NotEmpty);
    let mut fs = remount(fs);
    assert_eq!(fs.child_count(fs.resolve("/many").unwrap()), 101);
    assert_clean(&mut fs, "many");
}

#[test]
fn running_out_of_room_is_an_error_and_not_damage() {
    let mut fs = fresh();
    let file = fs.create_file(NodeId::ROOT, "fill").unwrap();
    let chunk = vec![1u8; 64 * 1024];
    let mut offset = 0;
    loop {
        match fs.write(file, offset, &chunk) {
            Ok(0) | Err(FsError::NoSpace) => break,
            Ok(count) => offset += count,
            Err(error) => panic!("{error}"),
        }
        assert!(offset < SIZE);
    }
    assert_clean(&mut fs, "full");
    WritableFileSystem::remove(&mut fs, file).unwrap();
    WritableFileSystem::flush(&mut fs).unwrap();
    let again = fs.create_file(NodeId::ROOT, "again").unwrap();
    assert_eq!(fs.write(again, 0, b"room").unwrap(), 4);
    assert_clean(&mut fs, "after");
}

#[test]
fn the_vfs_sees_links_modes_and_who_may_do_what() {
    let vfs = tree(fresh());
    vfs.create_dir("/home").unwrap();
    vfs.write_file("/home/note", b"secret", false).unwrap();
    vfs.symlink("/home/note", "/ln").unwrap();
    assert_eq!(vfs.read_link("/ln").unwrap(), "/home/note");
    let stat = vfs.lstat("/ln").unwrap();
    assert_eq!(stat.mode & S_IFMT, S_IFLNK);
    assert!(stat.posix);
    assert_eq!(vfs.stat("/ln").unwrap().mode & S_IFMT, S_IFREG);

    vfs.link("/home/note", "/hard").unwrap();
    assert_eq!(vfs.stat("/home/note").unwrap().nlink, 2);
    assert_eq!(vfs.stat("/hard").unwrap().ino, vfs.stat("/home/note").unwrap().ino);

    use feos_fs::vfs::{ACCESS_EXECUTE as X, ACCESS_READ as R, ACCESS_WRITE as W};
    vfs.chmod("/home/note", 0o640).unwrap();
    vfs.chown("/home/note", Some(1000), Some(50)).unwrap();
    assert_eq!(vfs.stat("/home/note").unwrap().mode & 0o7777, 0o640);
    // The owner, the group, and everyone else.
    assert!(vfs.check_access("/home/note", 1000, 1, R | W).is_ok());
    assert!(vfs.check_access("/home/note", 2000, 50, R).is_ok());
    assert_eq!(vfs.check_access("/home/note", 2000, 50, W), Err(FsError::PermissionDenied));
    assert_eq!(vfs.check_access("/home/note", 2000, 60, R), Err(FsError::PermissionDenied));
    // Root reads anything, and runs only what's executable by someone.
    assert!(vfs.check_access("/home/note", 0, 0, R | W).is_ok());
    assert_eq!(vfs.check_access("/home/note", 0, 0, X), Err(FsError::PermissionDenied));
    vfs.chmod("/home/note", 0o755).unwrap();
    assert!(vfs.check_access("/home/note", 0, 0, X).is_ok());
    // A directory in the way says no, whatever the file says.
    vfs.chmod("/home", 0o700).unwrap();
    assert_eq!(vfs.check_access("/home/note", 2000, 60, R), Err(FsError::PermissionDenied));
    assert!(vfs.check_access("/hard", 2000, 60, R).is_ok());
    // What's behind it can't be told from what isn't there.
    assert_eq!(vfs.check_access("/home/nothing", 2000, 60, 0), Err(FsError::PermissionDenied));
    assert_eq!(vfs.check_access("/nothing", 2000, 60, 0), Err(FsError::NotFound));
    assert!(vfs.fsck("/", false).unwrap().errors.is_empty());
}

#[test]
fn fat_reports_made_up_modes_and_refuses_to_change_them() {
    let mut disk = RamDisk::new(64 * 1024 * 1024, SECTOR).unwrap();
    feos_fs::format_fat32(&mut disk, "T").unwrap();
    let vfs = Vfs::new(Box::new(feos_fs::Fat32::mount(disk).unwrap()), false, "fat");
    vfs.write_file("/f", b"x", false).unwrap();
    vfs.create_dir("/d").unwrap();
    let stat = vfs.stat("/f").unwrap();
    assert!(!stat.posix);
    assert_eq!(stat.mode, S_IFREG | 0o644);
    assert_eq!((stat.uid, stat.gid), (0, 0));
    assert_eq!(vfs.stat("/d").unwrap().mode, S_IFDIR | 0o755);
    assert_eq!(vfs.chmod("/f", 0o600), Err(FsError::Unsupported));
    assert_eq!(vfs.chown("/f", Some(1), None), Err(FsError::Unsupported));
    assert_eq!(vfs.link("/f", "/g"), Err(FsError::Unsupported));
    // Permissionless: nobody's refused anything here.
    assert!(vfs.check_access("/f", 1000, 1000, feos_fs::vfs::ACCESS_WRITE).is_ok());
    assert_eq!(vfs.fsck("/", false).unwrap_err(), FsError::Unsupported);
}

#[test]
fn fsck_finds_and_repairs_what_a_bad_disk_could_do() {
    use feos_fs::native::BLOCK;
    let mut fs = fresh();
    let file = fs.create_file(NodeId::ROOT, "kept").unwrap();
    fs.write(file, 0, &[5; 3 * BLOCK]).unwrap();
    let doomed = fs.create_file(NodeId::ROOT, "orphan").unwrap();
    fs.write(doomed, 0, &[6; BLOCK]).unwrap();
    WritableFileSystem::flush(&mut fs).unwrap();
    let mut image = fs.into_device().into_data();

    // Clear the directory entry of `orphan`: its inode has no name now, and its blocks stay.
    let name = image
        .windows(6)
        // The last: the journal, which comes before the data, can have old copies.
        .rposition(|window| window == b"orphan")
        .expect("the entry is on disk");
    image[name - 6..name - 2].fill(0);
    // And clear a bit of the bitmap, marking a used block free. It's the first of data.
    let mut fs = Native::mount(RamDisk::from_bytes(image, SECTOR).unwrap()).unwrap();
    let report = fs.fsck(false).unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("no name"), "{:?}", report.errors);

    let report = fs.fsck(true).unwrap();
    assert!(report.clean());
    assert_eq!(report.repaired, 1);
    let mut fs = remount(fs);
    assert_clean(&mut fs, "repaired");
    assert_eq!(fs.read_to_vec(fs.resolve("/kept").unwrap()).unwrap(), [5; 3 * BLOCK]);
}

#[test]
fn random_damage_never_panics() {
    let mut fs = fresh();
    let dir = fs.create_dir(NodeId::ROOT, "d").unwrap();
    for number in 0..10 {
        let file = fs.create_file(dir, &format!("f{number}")).unwrap();
        fs.write(file, 0, &vec![number as u8; 20_000 * (number + 1)]).unwrap();
    }
    WritableFileSystem::flush(&mut fs).unwrap();
    let image = fs.into_device().into_data();
    let mut state = 88172645463325252u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // The metadata is at the front.
    let metadata = 96 * FS_BLOCK;
    for round in 0..300 {
        let mut damaged = image.clone();
        for _ in 0..1 + round % 16 {
            let at = next() as usize % metadata;
            damaged[at] = next() as u8;
        }
        if let Ok(mut fs) = Native::mount(RamDisk::from_bytes(damaged, SECTOR).unwrap()) {
            let _ = fs.fsck(false);
            let limit = fs.node_id_limit();
            for id in 0..limit as u32 {
                let node = NodeId(id);
                if fs.is_file(node) {
                    let _ = fs.read_to_vec(node);
                }
            }
            if let Ok(probe) = fs.create_file(NodeId::ROOT, "probe") {
                let _ = fs.write(probe, 0, &[1; 9000]);
                let _ = WritableFileSystem::flush(&mut fs);
            }
        }
    }
}

#[test]
fn space_freed_but_not_committed_is_not_reused() {
    // A file that's durable, deleted, and then a crash before the delete is. If its blocks
    // had gone to something else meanwhile, it'd come back with that in it.
    let mut fs = fresh();
    let file = fs.create_file(NodeId::ROOT, "old").unwrap();
    fs.write(file, 0, &[1; 200 * FS_BLOCK]).unwrap();
    WritableFileSystem::flush(&mut fs).unwrap();
    let free_before = fs.stats().free_blocks;

    WritableFileSystem::remove(&mut fs, file).unwrap();
    // Those blocks aren't counted free until the delete has happened.
    assert_eq!(fs.stats().free_blocks, free_before);
    let new = fs.create_file(NodeId::ROOT, "new").unwrap();
    fs.write(new, 0, &[2; 400 * FS_BLOCK]).unwrap();

    // The power goes: data has reached the device, and no commit has.
    let mut fs = Native::mount(fs.into_device()).unwrap();
    assert!(fs.resolve("/new").is_none());
    let old = fs.resolve("/old").expect("the delete never happened");
    assert_eq!(fs.read_to_vec(old).unwrap(), vec![1u8; 200 * FS_BLOCK]);
    assert_clean(&mut fs, "after the crash");

    // Once it has committed, they're free again.
    WritableFileSystem::remove(&mut fs, old).unwrap();
    assert_eq!(fs.stats().free_blocks, free_before);
    WritableFileSystem::flush(&mut fs).unwrap();
    assert!(fs.stats().free_blocks > free_before + 190);
}

// Crashing at every write.

#[derive(Clone)]
enum Event {
    Write(u64, Vec<u8>),
    Flush,
}

struct Recorder {
    blocks: BTreeMap<u64, Vec<u8>>,
    base: Arc<Vec<u8>>,
    log: Arc<Mutex<Vec<Event>>>,
}

impl BlockDevice for Recorder {
    fn block_size(&self) -> usize {
        SECTOR
    }
    fn block_count(&self) -> u64 {
        (self.base.len() / SECTOR) as u64
    }
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        match self.blocks.get(&block) {
            Some(data) => out.copy_from_slice(data),
            None => {
                let at = block as usize * SECTOR;
                out.copy_from_slice(&self.base[at..at + SECTOR]);
            }
        }
        Ok(())
    }
}

impl WritableBlockDevice for Recorder {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        self.blocks.insert(block, data.to_vec());
        self.log.lock().unwrap().push(Event::Write(block, data.to_vec()));
        Ok(())
    }
    fn flush(&mut self) -> Result<(), FsError> {
        self.log.lock().unwrap().push(Event::Flush);
        Ok(())
    }
}

struct Promise {
    /// When the sync that made it durable returned.
    from_event: usize,
    /// When the next change to this path began, which could reach the disk from then on.
    until_event: usize,
    path: &'static str,
    /// `None`: it mustn't exist from here on.
    contents: Option<Vec<u8>>,
    /// Permission bits and owner that went with it.
    mode: Option<(u16, u32)>,
}

struct Run {
    base: Arc<Vec<u8>>,
    log: Vec<Event>,
    promises: Vec<Promise>,
    /// Between these events, a rename is in progress: exactly one of the names exists.
    one_of: Vec<(usize, usize, &'static str, &'static str)>,
}

fn workload() -> Run {
    let mut disk = RamDisk::new(SIZE, SECTOR).unwrap();
    format(&mut disk, 1).unwrap();
    let base = Arc::new(disk.into_data());
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder {
        blocks: BTreeMap::new(),
        base: base.clone(),
        log: log.clone(),
    };
    let vfs = Vfs::new(Box::new(Native::mount(recorder).unwrap()), false, "native");
    let promises = std::cell::RefCell::new(Vec::<Promise>::new());
    let now = || log.lock().unwrap().len();
    let promise = |path: &'static str, contents: Option<&[u8]>, mode: Option<(u16, u32)>| {
        vfs.sync().unwrap();
        promises.borrow_mut().push(Promise {
            from_event: now(),
            until_event: usize::MAX,
            path,
            contents: contents.map(<[u8]>::to_vec),
            mode,
        });
    };
    // Says a path is about to change, so what was promised about it holds until here.
    let changing = |path: &'static str| {
        let at = now();
        for promise in promises.borrow_mut().iter_mut() {
            if promise.path == path && promise.until_event == usize::MAX {
                promise.until_event = at;
            }
        }
    };

    vfs.create_dir("/docs").unwrap();
    vfs.write_file("/docs/first", &[1; 5000], false).unwrap();
    promise("/docs/first", Some(&[1; 5000]), None);

    changing("/docs/first");
    vfs.chmod("/docs/first", 0o600).unwrap();
    vfs.chown("/docs/first", Some(1000), None).unwrap();
    promise("/docs/first", Some(&[1; 5000]), Some((0o600, 1000)));

    // A big one: indirect blocks, the bitmap, more than one commit perhaps.
    let big: Vec<u8> = (0..300_000u32).map(|at| (at / 11) as u8).collect();
    vfs.write_file("/big", &big, false).unwrap();
    promise("/big", Some(&big), None);

    vfs.link("/big", "/docs/big-again").unwrap();
    vfs.symlink("/big", "/sym").unwrap();
    promise("/docs/big-again", Some(&big), None);

    // Whenever it lands, it's all of the rename.
    let rename_from = now();
    changing("/docs/first");
    vfs.rename("/docs/first", "/renamed").unwrap();
    promise("/renamed", Some(&[1; 5000]), Some((0o600, 1000)));
    promise("/docs/first", None, None);
    let rename_until = now();

    vfs.write_file("/config", b"version one", false).unwrap();
    promise("/config", Some(b"version one"), None);

    changing("/big");
    changing("/docs/big-again");
    vfs.truncate("/big", 1000).unwrap();
    promise("/big", Some(&big[..1000]), None);
    promise("/docs/big-again", Some(&big[..1000]), None);

    changing("/docs/big-again");
    vfs.remove_file("/docs/big-again").unwrap();
    promise("/docs/big-again", None, None);

    for number in 0..40 {
        vfs.write_file(&format!("/many-{number}"), &[number as u8; 300], false).unwrap();
    }
    vfs.create_dir("/gone").unwrap();
    vfs.write_file("/gone/x", b"x", false).unwrap();
    vfs.remove_file("/gone/x").unwrap();
    vfs.remove_dir("/gone").unwrap();
    promise("/gone", None, None);
    // Nothing synced after this. A crash may lose it, but not half of it.
    vfs.write_file("/unsynced", &[9; 40_000], false).unwrap();
    changing("/renamed");
    vfs.remove_file("/renamed").unwrap();
    vfs.sync().unwrap();

    let promises = promises.into_inner();
    let log = log.lock().unwrap().clone();
    Run {
        base,
        log,
        promises,
        one_of: vec![(rename_from, rename_until, "/docs/first", "/renamed")],
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn disk_at(run: &Run, cut: usize, rng: &mut Rng, keep_all: bool) -> RamDisk {
    let mut image = (*run.base).clone();
    let last_flush = run.log[..cut]
        .iter()
        .rposition(|event| matches!(event, Event::Flush))
        .map_or(0, |index| index + 1);
    for (index, event) in run.log[..cut].iter().enumerate() {
        if let Event::Write(block, data) = event {
            if index < last_flush || keep_all || rng.next() % 2 == 0 {
                let at = *block as usize * SECTOR;
                image[at..at + data.len()].copy_from_slice(data);
            }
        }
    }
    RamDisk::from_bytes(image, SECTOR).unwrap()
}

fn verify(run: &Run, cut: usize, disk: RamDisk, context: &str) {
    let mut fs = Native::mount(disk).unwrap_or_else(|error| panic!("{context}: mount: {error}"));
    assert!(!fs.is_damaged(), "{context}: damaged");
    assert!(!fs.is_read_only(), "{context}: read-only");
    let report = fs.fsck(false).unwrap();
    assert!(report.errors.is_empty(), "{context}: {:?}", report.errors);

    for (from, until, one, other) in &run.one_of {
        if (*from..=*until).contains(&cut) {
            let found = (fs.resolve(one).is_some(), fs.resolve(other).is_some());
            assert!(found.0 != found.1, "{context}: {one} and {other}: {found:?}");
        }
    }

    // Every promise that holds at this point.
    for promise in run
        .promises
        .iter()
        .filter(|promise| promise.from_event <= cut && cut <= promise.until_event)
    {
        let path = promise.path;
        let node = fs.resolve(path);
        match &promise.contents {
            Some(contents) => {
                let node = node.unwrap_or_else(|| panic!("{context}: lost {path}"));
                assert_eq!(&fs.read_to_vec(node).unwrap(), contents, "{context}: {path}");
                if let Some((mode, uid)) = promise.mode {
                    let meta = fs.posix(node).unwrap();
                    assert_eq!((meta.mode & 0o7777, meta.uid), (mode, uid), "{context}: {path}");
                }
            }
            None => assert!(node.is_none(), "{context}: {path} came back"),
        }
    }

    // It works: a new file goes in, and the next mount agrees.
    let probe = fs.create_file(NodeId::ROOT, "probe").unwrap();
    fs.write(probe, 0, &[7; 20_000]).unwrap();
    WritableFileSystem::flush(&mut fs).unwrap();
    let free = fs.stats().free_blocks;
    let disk = fs.into_device();
    let mut again = Native::mount(disk).unwrap_or_else(|error| panic!("{context}: remount: {error}"));
    assert_eq!(again.stats().free_blocks, free, "{context}: free count");
    let report = again.fsck(false).unwrap();
    assert!(report.errors.is_empty(), "{context}: after: {:?}", report.errors);
    assert_eq!(again.read_to_vec(again.resolve("/probe").unwrap()).unwrap(), [7; 20_000], "{context}");
}

#[test]
fn every_cut_recovers_with_writes_in_order() {
    let run = workload();
    assert!(run.log.len() > 500, "only {} events", run.log.len());
    let mut rng = Rng(1);
    for cut in 0..=run.log.len() {
        let disk = disk_at(&run, cut, &mut rng, true);
        verify(&run, cut, disk, &format!("cut {cut}/{} in order", run.log.len()));
    }
}

#[test]
fn every_cut_recovers_with_unflushed_writes_landing_at_random() {
    let run = workload();
    let mut rng = Rng(0x1234_5678_9abc);
    for cut in 0..=run.log.len() {
        for round in 0..3 {
            let disk = disk_at(&run, cut, &mut rng, false);
            verify(&run, cut, disk, &format!("cut {cut}/{} round {round}", run.log.len()));
        }
    }
}

#[test]
fn a_journal_with_a_bad_commit_is_ignored() {
    // A transaction whose commit block never landed: everything before it stands.
    let run = workload();
    let commits: Vec<usize> = run
        .log
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, Event::Write(_, data) if data.starts_with(b"FJRNCMIT")))
        .map(|(index, _)| index)
        .collect();
    assert!(commits.len() > 5);
    let mut rng = Rng(9);
    for commit in commits {
        // Up to, but not including, that commit block, whatever else made it.
        let disk = disk_at(&run, commit, &mut rng, true);
        verify(&run, commit, disk, &format!("before commit at {commit}"));
        // And just after it: all of it, even with nothing of the checkpoint on disk.
        let disk = disk_at(&run, commit + 1, &mut rng, true);
        verify(&run, commit + 1, disk, &format!("after commit at {commit}"));
    }
}

#[test]
fn replaying_twice_changes_nothing() {
    let run = workload();
    let commit = run
        .log
        .iter()
        .rposition(|event| matches!(event, Event::Write(_, data) if data.starts_with(b"FJRNCMIT")))
        .unwrap();
    let mut rng = Rng(3);
    let disk = disk_at(&run, commit + 2, &mut rng, true);
    let image = disk.into_data();
    let once = Native::mount(RamDisk::from_bytes(image, SECTOR).unwrap()).unwrap();
    let first = once.into_device().into_data();
    let twice = Native::mount(RamDisk::from_bytes(first.clone(), SECTOR).unwrap()).unwrap();
    assert_eq!(twice.into_device().into_data(), first);
}
