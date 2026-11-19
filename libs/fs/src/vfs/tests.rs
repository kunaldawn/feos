use super::*;
use crate::{format_fat32, Fat32, RamDisk, StaticFs, StaticNode, BufferCache};
use std::sync::atomic::Ordering as AtomicOrdering;

fn vfs() -> Vfs {
    Vfs::in_memory(1024 * 1024)
}

static NODES: &[StaticNode] = &[
    StaticNode::dir("/", None, &[1, 2]),
    StaticNode::file("readme.txt", Some(0), b"hello\n"),
    StaticNode::dir("docs", Some(0), &[3]),
    StaticNode::file("guide.md", Some(2), b"# Guide\n"),
];
static BASE: StaticFs = StaticFs::new(NODES);

fn seeded() -> Vfs {
    Vfs::new(
        Box::new(RamFs::seeded(&BASE, 1024 * 1024).unwrap()),
        false,
        "ram",
    )
}

fn read_all(tree: &dyn VfsTree, path: &str) -> Vec<u8> {
    let size = tree.stat(path).unwrap().size as usize;
    let mut out = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let count = tree.read_at(path, done as u64, &mut out[done..]).unwrap();
        assert_ne!(count, 0);
        done += count;
    }
    out
}

/// A disk that outlives each mount of it, as a real one does.
#[derive(Clone)]
struct SharedDisk(std::sync::Arc<std::sync::Mutex<RamDisk>>);

impl SharedDisk {
    fn formatted(bytes: usize) -> Self {
        let mut disk = RamDisk::new(bytes, 512).unwrap();
        format_fat32(&mut disk, "T").unwrap();
        Self(std::sync::Arc::new(std::sync::Mutex::new(disk)))
    }
}

impl crate::BlockDevice for SharedDisk {
    fn block_size(&self) -> usize {
        512
    }
    fn block_count(&self) -> u64 {
        self.0.lock().unwrap().block_count()
    }
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        self.0.lock().unwrap().read_block(block, out)
    }
}

impl crate::WritableBlockDevice for SharedDisk {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        self.0.lock().unwrap().write_block(block, data)
    }
}

/// A FAT32 tree on a fresh disk, cached as a real boot does it.
fn fat_vfs(bytes: usize) -> Vfs {
    let mut disk = RamDisk::new(bytes, 512).unwrap();
    format_fat32(&mut disk, "TEST").unwrap();
    let cache = BufferCache::new(disk, 1024 * 1024).unwrap();
    Vfs::new(Box::new(Fat32::mount(cache).unwrap()), false, "disk")
}

#[test]
fn normalizes_paths() {
    for (path, expected) in [
        ("/", "/"),
        ("", "/"),
        ("/a//b/./c/", "/a/b/c"),
        ("/a/../..", "/"),
        ("a/b", "/a/b"),
    ] {
        assert_eq!(normalize_path(path).unwrap(), expected);
    }
    assert!(normalize_path(&"x".repeat(MAX_PATH_BYTES + 1)).is_err());
}

#[test]
fn a_change_is_visible_through_paths_and_nodes_at_once() {
    let vfs = seeded();
    let before = vfs.generation();
    vfs.write_file("/docs/new.txt", b"data", false).unwrap();
    assert!(vfs.generation() > before);

    // Another reader, through the read-only interface apps hold.
    let fs: &dyn FileSystem = &vfs;
    let node = fs.resolve("/docs/new.txt").unwrap();
    assert_eq!(fs.kind(node), Some(EntryKind::File));
    assert_eq!(fs.size(node), 4);
    assert_eq!(fs.read_to_vec(node).unwrap(), b"data");
    assert_eq!(fs.name(node), Some("new.txt"));
    assert_eq!(fs.parent(node), fs.resolve("/docs"));
    let docs = fs.resolve("/docs").unwrap();
    assert_eq!(fs.child_count(docs), 2);
    assert_eq!(fs.lookup(docs, "new.txt"), Some(node));
    let mut buffer = [0u8; 64];
    assert_eq!(fs.path(node, &mut buffer), Some("/docs/new.txt"));
    assert_eq!(fs.path(fs.root(), &mut buffer), Some("/"));

    // Removing it takes effect for the same node ID, and the listing notices.
    vfs.remove_file("/docs/new.txt").unwrap();
    assert_eq!(fs.kind(node), None);
    assert_eq!(fs.child_count(docs), 1);
}

#[test]
fn the_node_view_keeps_ids_stable_and_refuses_to_climb_above_root() {
    let vfs = seeded();
    let fs: &dyn FileSystem = &vfs;
    let first = fs.resolve("/docs/guide.md").unwrap();
    assert_eq!(fs.resolve("/docs/../docs/guide.md"), Some(first));
    assert_eq!(fs.resolve("/../"), None);
    assert_eq!(fs.resolve("/missing"), None);
    assert_eq!(fs.parent(fs.root()), None);
    assert!(fs.tree().is_some());
}

#[test]
fn errors_are_the_ones_callers_expect() {
    let vfs = vfs();
    vfs.create_dir("/d").unwrap();
    vfs.write_file("/f", b"x", false).unwrap();
    assert_eq!(vfs.create_dir("/d"), Err(FsError::NameExists));
    assert_eq!(vfs.create_file("/f"), Err(FsError::NameExists));
    assert_eq!(vfs.create_dir("/missing/d"), Err(FsError::NotFound));
    assert_eq!(vfs.create_file("/f/child"), Err(FsError::NotDirectory));
    assert_eq!(vfs.stat("/f/child"), Err(FsError::NotDirectory));
    assert_eq!(vfs.read_dir("/f").unwrap_err(), FsError::NotDirectory);
    let mut out = [0u8; 4];
    assert_eq!(vfs.read_at("/d", 0, &mut out), Err(FsError::IsDirectory));
    assert_eq!(vfs.write_file("/d", b"x", false), Err(FsError::IsDirectory));
    assert_eq!(vfs.remove_file("/d"), Err(FsError::IsDirectory));
    assert_eq!(vfs.remove_dir("/f"), Err(FsError::NotDirectory));
    vfs.write_file("/d/inner", b"", false).unwrap();
    assert_eq!(vfs.remove_dir("/d"), Err(FsError::NotEmpty));
    assert_eq!(vfs.remove_dir("/"), Err(FsError::Busy));
    assert_eq!(vfs.remove_file("/missing"), Err(FsError::NotFound));
    assert_eq!(vfs.rename("/d", "/d/inner2"), Err(FsError::Unsupported));
    assert_eq!(vfs.rename("/f", "/d/inner"), Err(FsError::NameExists));
}

#[test]
fn handles_read_write_seek_and_append() {
    let vfs = vfs();
    {
        let file = File::open(&vfs, "/f", OpenOptions::replacing()).unwrap();
        assert_eq!(file.write(b"hello").unwrap(), 5);
        assert_eq!(file.write(b" world").unwrap(), 6);
        let mut out = [0u8; 1];
        assert_eq!(file.read(&mut out), Err(FsError::Unsupported));
    }
    assert_eq!(read_all(&vfs, "/f"), b"hello world");

    let file = File::open(&vfs, "/f", OpenOptions::reading()).unwrap();
    let mut out = [0u8; 5];
    assert_eq!(file.read(&mut out).unwrap(), 5);
    assert_eq!(&out, b"hello");
    assert_eq!(file.seek(Seek::End(-5)).unwrap(), 6);
    assert_eq!(file.read(&mut out).unwrap(), 5);
    assert_eq!(&out, b"world");
    assert_eq!(file.read(&mut out).unwrap(), 0);
    assert_eq!(file.seek(Seek::Current(-11)).unwrap(), 0);
    assert_eq!(file.seek(Seek::Current(-1)), Err(FsError::OutOfBounds));
    assert_eq!(file.write(b"x"), Err(FsError::ReadOnly));
    assert_eq!(file.stat().unwrap().size, 11);

    {
        let log = File::open(&vfs, "/f", OpenOptions::appending()).unwrap();
        log.seek(Seek::Start(0)).unwrap();
        log.write(b"!").unwrap();
    }
    assert_eq!(read_all(&vfs, "/f"), b"hello world!");
    assert_eq!(
        vfs.open("/f", OpenOptions { create: true, exclusive: true, write: true, ..OpenOptions::default() }),
        Err(FsError::NameExists)
    );
    assert_eq!(vfs.open("/none", OpenOptions::reading()), Err(FsError::NotFound));
}

#[test]
fn a_handle_survives_a_rename_and_goes_stale_on_removal() {
    let vfs = vfs();
    vfs.write_file("/a", b"contents", false).unwrap();
    let handle = vfs.open("/a", OpenOptions::reading()).unwrap();
    vfs.rename("/a", "/b").unwrap();
    let mut out = [0u8; 8];
    assert_eq!(vfs.read_handle(handle, &mut out).unwrap(), 8);

    vfs.remove_file("/b").unwrap();
    assert_eq!(vfs.read_handle(handle, &mut out), Err(FsError::StaleHandle));
    vfs.close(handle).unwrap();
    assert_eq!(vfs.close(handle), Err(FsError::StaleHandle));

    // A reused slot doesn't answer to the old handle.
    vfs.write_file("/c", b"", false).unwrap();
    let next = vfs.open("/c", OpenOptions::reading()).unwrap();
    assert_eq!(next.slot, handle.slot);
    assert_eq!(vfs.close(handle), Err(FsError::StaleHandle));
    vfs.close(next).unwrap();
}

#[test]
fn handles_run_out_with_an_error_and_come_back() {
    let vfs = vfs();
    vfs.write_file("/f", b"", false).unwrap();
    let handles: Vec<Handle> = (0..MAX_HANDLES)
        .map(|_| vfs.open("/f", OpenOptions::reading()).unwrap())
        .collect();
    assert_eq!(vfs.open("/f", OpenOptions::reading()), Err(FsError::Busy));
    vfs.close(handles[10]).unwrap();
    assert!(vfs.open("/f", OpenOptions::reading()).is_ok());
}

#[test]
fn mounts_appear_in_listings_and_shadow_what_is_under_them() {
    let vfs = seeded();
    vfs.create_dir("/mnt").unwrap();
    let mut other = RamFs::new(1024);
    other.create_file(NodeId::ROOT, "on-other").unwrap();
    vfs.mount("/mnt/other", Box::new(other), false, "test").unwrap();
    assert_eq!(
        vfs.mount("/mnt/other", Box::new(RamFs::new(0)), false, "again"),
        Err(FsError::Busy)
    );
    assert_eq!(
        vfs.mount("/nowhere/x", Box::new(RamFs::new(0)), false, "lost"),
        Err(FsError::NotFound)
    );

    let names: Vec<String> = vfs.read_dir("/mnt").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["other"]);
    assert!(vfs.stat("/mnt/other/on-other").is_ok());
    let fs: &dyn FileSystem = &vfs;
    assert!(fs.resolve("/mnt/other/on-other").is_some());
    assert_eq!(vfs.mounts().len(), 2);

    // Things under a mount keep it from going away.
    assert_eq!(vfs.remove_dir("/mnt"), Err(FsError::Busy));
    assert_eq!(vfs.remove_dir("/mnt/other"), Err(FsError::Busy));
    assert_eq!(vfs.rename("/mnt", "/moved"), Err(FsError::Busy));
    assert_eq!(vfs.rename("/mnt/other", "/moved"), Err(FsError::Busy));
    let handle = vfs.open("/mnt/other/on-other", OpenOptions::reading()).unwrap();
    assert_eq!(vfs.unmount("/mnt/other"), Err(FsError::Busy));
    vfs.close(handle).unwrap();
    assert_eq!(vfs.unmount("/"), Err(FsError::Busy));
    vfs.unmount("/mnt/other").unwrap();
    assert_eq!(vfs.stat("/mnt/other"), Err(FsError::NotFound));
    assert_eq!(vfs.unmount("/mnt/other"), Err(FsError::NotFound));
}

#[test]
fn unmounting_keeps_handles_on_later_mounts_working() {
    let vfs = vfs();
    vfs.mount("/a", Box::new(RamFs::new(1024)), false, "a").unwrap();
    vfs.mount("/b", Box::new(RamFs::new(1024)), false, "b").unwrap();
    vfs.write_file("/b/f", b"x", false).unwrap();
    let handle = vfs.open("/b/f", OpenOptions::reading()).unwrap();
    vfs.unmount("/a").unwrap();
    let mut out = [0u8; 1];
    assert_eq!(vfs.read_handle(handle, &mut out).unwrap(), 1);
    assert_eq!(out[0], b'x');
}

#[test]
fn a_read_only_mount_refuses_every_change() {
    let vfs = vfs();
    let mut ro = RamFs::new(1024);
    let file = ro.create_file(NodeId::ROOT, "f").unwrap();
    ro.write(file, 0, b"x").unwrap();
    vfs.mount("/ro", Box::new(ro), true, "ro").unwrap();
    assert!(vfs.stat("/ro/f").unwrap().read_only);
    assert_eq!(vfs.write_file("/ro/f", b"y", false), Err(FsError::ReadOnly));
    assert_eq!(vfs.write_at("/ro/f", 0, b"y"), Err(FsError::ReadOnly));
    assert_eq!(vfs.create_file("/ro/g"), Err(FsError::ReadOnly));
    assert_eq!(vfs.create_dir("/ro/d"), Err(FsError::ReadOnly));
    assert_eq!(vfs.remove_file("/ro/f"), Err(FsError::ReadOnly));
    assert_eq!(vfs.truncate("/ro/f", 0), Err(FsError::ReadOnly));
    assert_eq!(vfs.rename("/ro/f", "/ro/g"), Err(FsError::ReadOnly));
    assert_eq!(vfs.set_attributes("/ro/f", 0), Err(FsError::ReadOnly));
    assert_eq!(
        vfs.open("/ro/f", OpenOptions::appending()),
        Err(FsError::ReadOnly)
    );
    assert!(vfs.open("/ro/f", OpenOptions::reading()).is_ok());
    assert!(vfs.statfs("/ro/f").unwrap().read_only);
}

#[test]
fn the_read_only_attribute_protects_a_file() {
    let vfs = fat_vfs(40 * 1024 * 1024);
    vfs.write_file("/f", b"x", false).unwrap();
    vfs.set_attributes("/f", attr::READ_ONLY).unwrap();
    assert!(vfs.stat("/f").unwrap().read_only);
    assert_eq!(vfs.write_file("/f", b"y", false), Err(FsError::ReadOnly));
    assert_eq!(vfs.remove_file("/f"), Err(FsError::ReadOnly));
    vfs.set_attributes("/f", 0).unwrap();
    vfs.remove_file("/f").unwrap();
}

#[test]
fn rename_moves_a_tree_across_mounts_by_copying() {
    let vfs = seeded();
    vfs.mount("/tmp", Box::new(RamFs::new(1024 * 1024)), false, "tmp").unwrap();
    vfs.create_dir("/tmp/d").unwrap();
    vfs.write_file("/tmp/d/file", b"moved", false).unwrap();
    vfs.symlink("/readme.txt", "/tmp/d/link").unwrap();
    vfs.rename("/tmp/d", "/docs/d").unwrap();
    assert_eq!(read_all(&vfs, "/docs/d/file"), b"moved");
    assert_eq!(vfs.read_link("/docs/d/link").unwrap(), "/readme.txt");
    assert_eq!(vfs.stat("/tmp/d"), Err(FsError::NotFound));
}

#[test]
fn a_failed_copy_across_mounts_keeps_the_source() {
    let vfs = vfs();
    vfs.mount("/small", Box::new(RamFs::new(4)), false, "small").unwrap();
    vfs.write_file("/big", b"too large to fit", false).unwrap();
    assert_eq!(vfs.rename("/big", "/small/big"), Err(FsError::NoSpace));
    assert_eq!(read_all(&vfs, "/big"), b"too large to fit");
    assert_eq!(vfs.stat("/small/big"), Err(FsError::NotFound));
}

#[test]
fn symlinks_resolve_and_loops_end() {
    let vfs = seeded();
    vfs.symlink("/docs/guide.md", "/abs").unwrap();
    vfs.symlink("guide.md", "/docs/rel").unwrap();
    vfs.symlink("/docs", "/dirlink").unwrap();
    vfs.symlink("../readme.txt", "/docs/up").unwrap();
    vfs.symlink("/loop-b", "/loop-a").unwrap();
    vfs.symlink("/loop-a", "/loop-b").unwrap();
    vfs.symlink("/missing", "/dangling").unwrap();

    assert_eq!(read_all(&vfs, "/abs"), b"# Guide\n");
    assert_eq!(read_all(&vfs, "/docs/rel"), b"# Guide\n");
    assert_eq!(read_all(&vfs, "/dirlink/guide.md"), b"# Guide\n");
    assert_eq!(read_all(&vfs, "/docs/up"), b"hello\n");
    assert_eq!(vfs.stat("/abs").unwrap().kind, FileKind::File);
    assert_eq!(vfs.lstat("/abs").unwrap().kind, FileKind::Symlink);
    assert_eq!(vfs.read_link("/abs").unwrap(), "/docs/guide.md");
    assert_eq!(vfs.read_link("/readme.txt"), Err(FsError::Unsupported));
    assert_eq!(vfs.stat("/loop-a"), Err(FsError::SymlinkLoop));
    assert_eq!(vfs.stat("/dangling"), Err(FsError::NotFound));
    assert_eq!(vfs.lstat("/dangling").unwrap().kind, FileKind::Symlink);

    let listing = vfs.read_dir("/").unwrap();
    let link = listing.iter().find(|entry| entry.name == "abs").unwrap();
    assert_eq!(link.stat.kind, FileKind::Symlink);

    // Writing through a link changes the target; removing it doesn't.
    vfs.write_file("/abs", b"new", false).unwrap();
    assert_eq!(read_all(&vfs, "/docs/guide.md"), b"new");
    vfs.remove_file("/abs").unwrap();
    assert_eq!(read_all(&vfs, "/docs/guide.md"), b"new");

    // A hostile link can't reach past the root.
    vfs.symlink("../../../../readme.txt", "/docs/far").unwrap();
    assert_eq!(read_all(&vfs, "/docs/far"), b"hello\n");
    // Link targets that are too long are refused.
    assert_eq!(vfs.symlink(&"x".repeat(5000), "/long"), Err(FsError::InvalidName));
}

#[test]
fn symlinks_work_on_fat_and_survive_a_remount() {
    let disk = SharedDisk::formatted(40 * 1024 * 1024);
    {
        let vfs = Vfs::new(Box::new(Fat32::mount(disk.clone()).unwrap()), false, "d");
        vfs.write_file("/target", b"reached", false).unwrap();
        vfs.symlink("/target", "/link").unwrap();
        vfs.sync().unwrap();
    }
    let vfs = Vfs::new(Box::new(Fat32::mount(disk).unwrap()), false, "d");
    assert_eq!(read_all(&vfs, "/link"), b"reached");
    assert_eq!(vfs.lstat("/link").unwrap().kind, FileKind::Symlink);
}

#[test]
fn devices_behave() {
    let vfs = vfs();
    vfs.create_dir("/dev").unwrap();
    let mut dev = RamFs::new(0);
    dev.add_device(NodeId::ROOT, "null", DeviceKind::Null).unwrap();
    vfs.mount("/dev", Box::new(dev), false, "dev").unwrap();
    vfs.make_device("/dev/zero", DeviceKind::Zero).unwrap();
    vfs.write_file("/dev/null", b"gone", false).unwrap();
    assert_eq!(vfs.stat("/dev/null").unwrap().size, 0);
    let mut out = [9u8; 4];
    assert_eq!(vfs.read_at("/dev/zero", 0, &mut out).unwrap(), 4);
    assert_eq!(out, [0; 4]);
    // A device is known by its node, whatever path reaches it.
    let tree: &dyn VfsTree = &vfs;
    assert_eq!(tree.identify("//dev/./null").unwrap().1.device, Some(DeviceKind::Null));
    assert_eq!(tree.identify("/dev/../dev/zero").unwrap().1.device, Some(DeviceKind::Zero));
    vfs.write_file("/plain", b"bytes", false).unwrap();
    assert_eq!(tree.identify("/plain").unwrap().1.device, None);
}

#[test]
fn atomic_replace_leaves_no_temporary_and_keeps_contents() {
    let vfs = fat_vfs(40 * 1024 * 1024);
    vfs.create_dir("/etc").unwrap();
    vfs.write_file_atomic("/etc/settings", b"one").unwrap();
    vfs.write_file_atomic("/etc/settings", b"two two").unwrap();
    assert_eq!(read_all(&vfs, "/etc/settings"), b"two two");
    let names: Vec<String> = vfs.read_dir("/etc").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["settings"]);
}

#[test]
fn an_atomic_file_can_be_read_back_from_either_name() {
    let vfs = vfs();
    assert_eq!(read_atomic(&vfs, "/s"), Err(FsError::NotFound));
    vfs.write_file_atomic("/s", b"one").unwrap();
    assert_eq!(read_atomic(&vfs, "/s").unwrap(), b"one");
    // The crash window: old gone, new not yet renamed.
    vfs.rename("/s", "/.s.new").unwrap();
    assert_eq!(read_atomic(&vfs, "/s").unwrap(), b"one");
    vfs.write_file_atomic("/s", b"two").unwrap();
    assert_eq!(read_atomic(&vfs, "/s").unwrap(), b"two");
}

// A clock for each thread, as tests run side by side: with one between them, one test
// moving it forward landed in another's stamps.
std::thread_local! {
    static NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(1_700_000_000) };
}

fn fake_now() -> Option<crate::Timestamp> {
    Some(crate::Timestamp::from_secs(NOW.get()))
}

#[test]
fn timestamps_are_stored_and_updated_on_every_backend() {
    for vfs in [fat_vfs(40 * 1024 * 1024), vfs()] {
        NOW.set(1_700_000_000);
        vfs.set_time_source(fake_now);
        vfs.write_file("/f", b"x", false).unwrap();
        vfs.create_dir("/d").unwrap();
        let times = vfs.stat("/f").unwrap().times;
        assert_eq!(times.created, Some(crate::Timestamp::from_secs(1_700_000_000)));
        assert_eq!(times.modified, Some(crate::Timestamp::from_secs(1_700_000_000)));
        assert_eq!(vfs.stat("/d").unwrap().times.created, Some(crate::Timestamp::from_secs(1_700_000_000)));

        NOW.set(1_700_000_100);
        vfs.write_at("/f", 1, b"y").unwrap();
        let times = vfs.stat("/f").unwrap().times;
        assert_eq!(times.created, Some(crate::Timestamp::from_secs(1_700_000_000)));
        assert_eq!(times.modified, Some(crate::Timestamp::from_secs(1_700_000_100)));

        vfs.set_times(
            "/f",
            NodeTimes {
                modified: Some(crate::Timestamp::from_secs(1_600_000_000)),
                ..NodeTimes::NONE
            },
        )
        .unwrap();
        assert_eq!(vfs.stat("/f").unwrap().times.modified, Some(crate::Timestamp::from_secs(1_600_000_000)));
    }
}

#[test]
fn timestamps_survive_a_remount_on_fat() {
    let disk = SharedDisk::formatted(40 * 1024 * 1024);
    {
        let vfs = Vfs::new(Box::new(Fat32::mount(disk.clone()).unwrap()), false, "d");
        NOW.set(1_700_000_000);
        vfs.set_time_source(fake_now);
        vfs.write_file("/f", b"x", false).unwrap();
        vfs.set_attributes("/f", attr::HIDDEN).unwrap();
        vfs.sync().unwrap();
    }
    let vfs = Vfs::new(Box::new(Fat32::mount(disk).unwrap()), false, "d");
    let stat = vfs.stat("/f").unwrap();
    assert_eq!(stat.times.modified, Some(crate::Timestamp::from_secs(1_700_000_000)));
    assert_eq!(stat.times.created, Some(crate::Timestamp::from_secs(1_700_000_000)));
    assert_eq!(stat.attributes & attr::HIDDEN, attr::HIDDEN);
    // The date alone, for the access time.
    assert_eq!(stat.times.accessed, Some(crate::Timestamp::from_secs(1_700_000_000 / 86_400 * 86_400)));
}

#[test]
fn timestamp_tests_can_run_at_the_same_time() {
    // Sharing a clock made these fail within a few dozen rounds.
    let moving = std::thread::spawn(|| (0..64).for_each(|_| timestamps_are_stored_and_updated_on_every_backend()));
    let remount = std::thread::spawn(|| (0..64).for_each(|_| timestamps_survive_a_remount_on_fat()));
    moving.join().unwrap();
    remount.join().unwrap();
}

#[test]
fn statfs_reports_room_on_fat() {
    let vfs = fat_vfs(40 * 1024 * 1024);
    let before = vfs.statfs("/").unwrap();
    assert_eq!(before.kind, "fat32");
    assert!(before.total_blocks >= 65_525);
    assert!(before.free_blocks > 60_000);
    vfs.write_file("/f", &[1u8; 8192], false).unwrap();
    let after = vfs.statfs("/").unwrap();
    assert!(after.free_blocks < before.free_blocks);
    vfs.remove_file("/f").unwrap();
    assert_eq!(vfs.statfs("/").unwrap().free_blocks, before.free_blocks);
}

#[test]
fn sync_if_dirty_does_nothing_when_idle() {
    let vfs = vfs();
    assert!(!vfs.dirty.load(AtomicOrdering::Relaxed));
    vfs.write_file("/f", b"x", false).unwrap();
    assert!(vfs.dirty.load(AtomicOrdering::Relaxed));
    vfs.sync_if_dirty().unwrap();
    assert!(!vfs.dirty.load(AtomicOrdering::Relaxed));
}

#[test]
fn two_threads_share_the_tree() {
    let vfs: &'static Vfs = Box::leak(Box::new(fat_vfs(40 * 1024 * 1024)));
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            std::thread::spawn(move || {
                let dir = format!("/w{worker}");
                vfs.create_dir(&dir).unwrap();
                for round in 0..20 {
                    let path = format!("{dir}/{round}");
                    vfs.write_file(&path, path.as_bytes(), false).unwrap();
                    assert_eq!(read_all(vfs, &path), path.as_bytes());
                    if round % 2 == 0 {
                        vfs.remove_file(&path).unwrap();
                    }
                }
                vfs.sync().unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    for worker in 0..4 {
        assert_eq!(vfs.read_dir(&format!("/w{worker}")).unwrap().len(), 10);
    }
}

#[test]
fn deep_paths_and_a_long_name_do_not_panic() {
    let vfs = fat_vfs(40 * 1024 * 1024);
    let name = "n".repeat(255);
    vfs.write_file(&format!("/{name}"), b"x", false).unwrap();
    assert_eq!(
        vfs.write_file(&format!("/{}", "n".repeat(256)), b"x", false),
        Err(FsError::InvalidName)
    );
    let mut path = String::new();
    let mut result = Ok(());
    for _ in 0..40 {
        path.push_str("/d");
        result = vfs.create_dir(&path);
        if result.is_err() {
            break;
        }
    }
    assert_eq!(result, Err(FsError::TooDeep));
    assert!(vfs.stat(&"/x".repeat(5000)).is_err());
}

struct RamDevice {
    data: std::vec::Vec<u8>,
    block: usize,
}

impl feos_hal::BlockIo for RamDevice {
    fn block_size(&self) -> usize {
        self.block
    }
    fn block_count(&self) -> u64 {
        (self.data.len() / self.block) as u64
    }
    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), feos_hal::BlockIoError> {
        let at = start as usize * self.block;
        out.copy_from_slice(
            self.data
                .get(at..at + out.len())
                .ok_or(feos_hal::BlockIoError::OutOfRange)?,
        );
        Ok(())
    }
    fn write(&mut self, start: u64, data: &[u8]) -> Result<(), feos_hal::BlockIoError> {
        let at = start as usize * self.block;
        self.data
            .get_mut(at..at + data.len())
            .ok_or(feos_hal::BlockIoError::OutOfRange)?
            .copy_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), feos_hal::BlockIoError> {
        Ok(())
    }
}

fn embedded() -> Embedded {
    Embedded {
        tree: &BASE,
        root_image: None,
        images: &[],
    }
}

#[test]
fn with_no_disk_the_root_is_a_writable_copy_of_the_image() {
    let (vfs, log) = assemble::<RamDevice>(embedded(), Vec::new(), Vec::new(), None);
    assert!(log.iter().any(|line| line.contains("no disk")));
    assert_eq!(read_all(vfs, "/readme.txt"), b"hello\n");
    vfs.write_file("/readme.txt", b"changed", false).unwrap();
    assert_eq!(read_all(vfs, "/readme.txt"), b"changed");
    assert!(vfs.stat("/dev/null").is_ok());
    assert!(vfs.stat("/tmp").is_ok());
    assert_eq!(vfs.stat("/rom"), Err(FsError::NotFound));
    vfs.write_file("/dev/null", b"x", false).unwrap();
}

#[test]
fn a_blank_disk_is_formatted_and_seeded_once() {
    let device = RamDevice {
        data: vec![0; 40 * 1024 * 1024],
        block: 512,
    };
    let (vfs, log) = assemble(embedded(), vec![device], Vec::new(), None);
    assert!(log.iter().any(|line| line.contains("formatted")), "{log:?}");
    assert_eq!(read_all(vfs, "/docs/guide.md"), b"# Guide\n");
    assert_eq!(vfs.statfs("/").unwrap().kind, "fat32");
    assert!(vfs.stat("/rom/readme.txt").unwrap().read_only);
    assert_eq!(vfs.write_file("/rom/readme.txt", b"x", false), Err(FsError::ReadOnly));
    // What's seeded can be deleted.
    vfs.remove_file("/readme.txt").unwrap();
    assert!(vfs.lstat(SEED_MARKER_PATH).is_ok());
}

const SEED_MARKER_PATH: &str = "/.feos-seeded";

#[test]
fn a_removable_disk_is_never_the_root_and_never_formatted() {
    let blank = RamDevice {
        data: vec![0; 40 * 1024 * 1024],
        block: 512,
    };
    let removable: Vec<Box<dyn feos_hal::BlockIo>> = vec![Box::new(blank)];
    let (vfs, log) = assemble::<RamDevice>(embedded(), Vec::new(), removable, None);
    assert!(log.iter().any(|line| line.contains("no disk")), "{log:?}");
    assert!(log.iter().any(|line| line.contains("usb0: no filesystem recognised")), "{log:?}");
    assert!(!log.iter().any(|line| line.contains("formatted")), "{log:?}");
    assert_eq!(vfs.stat("/usb0"), Err(FsError::NotFound));
}

#[test]
fn a_disk_with_unknown_contents_is_never_touched() {
    let mut data = vec![0u8; 40 * 1024 * 1024];
    data[100_000] = 1;
    data[5] = 0x7e;
    let device = RamDevice {
        data: data.clone(),
        block: 512,
    };
    let (vfs, log) = assemble(embedded(), vec![device], Vec::new(), None);
    assert!(log.iter().any(|line| line.contains("left alone")), "{log:?}");
    assert_eq!(vfs.mounts()[0].source, "ram");
}

#[test]
fn partitions_are_found_and_later_ones_mounted_beside_the_root() {
    let total = 90 * 1024 * 1024usize;
    let mut disk = RamDisk::new(total, 512).unwrap();
    let start_a = 2048u32;
    let count_a = (40 * 1024 * 1024 / 512) as u32;
    let start_b = start_a + count_a;
    let count_b = count_a;
    {
        let mut part = crate::Partition::new(&mut disk, start_a as u64, count_a as u64).unwrap();
        format_fat32(&mut part, "A").unwrap();
    }
    {
        let mut part = crate::Partition::new(&mut disk, start_b as u64, count_b as u64).unwrap();
        format_fat32(&mut part, "B").unwrap();
    }
    let mut data = disk.into_data();
    for (index, (start, count)) in [(start_a, count_a), (start_b, count_b)].into_iter().enumerate() {
        let at = 446 + index * 16;
        data[at + 4] = 0x0c;
        data[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
        data[at + 12..at + 16].copy_from_slice(&count.to_le_bytes());
    }
    data[510] = 0x55;
    data[511] = 0xaa;
    let device = RamDevice { data, block: 512 };
    let (vfs, log) = assemble(embedded(), vec![device], Vec::new(), None);
    assert!(log.iter().any(|line| line.contains("virtio0p1: mounted at /")), "{log:?}");
    assert!(log.iter().any(|line| line.contains("virtio0p2: mounted at /disk1")), "{log:?}");
    vfs.write_file("/disk1/on-second", b"2", false).unwrap();
    vfs.write_file("/on-first", b"1", false).unwrap();
    assert_eq!(read_all(vfs, "/disk1/on-second"), b"2");
    vfs.sync().unwrap();
}

/// One tree of each kind with a file to rename: RAM, FAT32, exFAT and native.
fn every_kind() -> Vec<(&'static str, Vfs)> {
    let mut exfat = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
    crate::format_exfat(&mut exfat, "X").unwrap();
    let mut native = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
    crate::native::format(&mut native, 0).unwrap();
    vec![
        ("ram", vfs()),
        ("fat32", fat_vfs(64 * 1024 * 1024)),
        ("exfat", Vfs::new(Box::new(crate::Exfat::mount(exfat).unwrap()), false, "exfat")),
        ("native", Vfs::new(Box::new(crate::Native::mount(native).unwrap()), false, "native")),
    ]
}

#[test]
fn stat_has_dev() {
    let tree = vfs();
    tree.create_dir("/mnt").unwrap();
    tree.mount("/mnt", Box::new(RamFs::new(4096)), false, "ram").unwrap();
    tree.write_file("/a", b"a", false).unwrap();
    tree.write_file("/mnt/b", b"b", false).unwrap();
    let (a, b) = (tree.stat("/a").unwrap(), tree.stat("/mnt/b").unwrap());
    assert_ne!(a.dev, 0);
    assert_ne!(a.dev, b.dev, "each mount has its own number");
    assert_eq!(tree.stat("/").unwrap().dev, a.dev);
    // A number isn't given to a later mount when one goes.
    tree.unmount("/mnt").unwrap();
    tree.mount("/mnt", Box::new(RamFs::new(4096)), false, "ram").unwrap();
    assert!(![a.dev, b.dev].contains(&tree.stat("/mnt").unwrap().dev));
    let (id, stat) = tree.identify("/a").unwrap();
    assert_eq!((id.dev, id.ino), (stat.dev, stat.ino));
}

#[test]
fn ino_stable_across_rename() {
    for (kind, tree) in every_kind() {
        tree.create_dir("/d").unwrap();
        tree.write_file("/f", b"contents", false).unwrap();
        let before = tree.identify("/f").unwrap().0;
        tree.rename("/f", "/d/g").unwrap();
        let after = tree.identify("/d/g").unwrap().0;
        assert_eq!(before, after, "{kind}");
        let mut out = [0u8; 8];
        assert_eq!(tree.read_file(after, 0, &mut out).unwrap(), 8, "{kind}");
        assert_eq!(&out, b"contents", "{kind}");
        assert_eq!(tree.stat_file(after).unwrap().size, 8, "{kind}");
    }
}

#[test]
fn ino_not_recycled_while_referenced() {
    for (kind, tree) in every_kind() {
        tree.write_file("/held", b"x", false).unwrap();
        let held = tree.identify("/held").unwrap().0;
        tree.hold(held).unwrap();
        tree.remove_file("/held").unwrap();
        assert_eq!(tree.read_file(held, 0, &mut [0u8; 1]), Err(FsError::NotFound), "{kind}");
        // Make and remove plenty more: none of them is given the held number.
        for round in 0..40 {
            let path = format!("/new{round}");
            tree.write_file(&path, b"y", false).unwrap();
            assert_ne!(tree.identify(&path).unwrap().0, held, "{kind} round {round}");
            if round % 2 == 0 {
                tree.remove_file(&path).unwrap();
            }
        }
        // Once let go, it may be reused; the tree is whole either way.
        tree.release(held);
        tree.write_file("/after", b"z", false).unwrap();
        assert_eq!(read_all(&tree, "/after"), b"z", "{kind}");
    }
}

#[test]
fn a_held_file_keeps_its_mount() {
    let tree = vfs();
    tree.create_dir("/mnt").unwrap();
    tree.mount("/mnt", Box::new(RamFs::new(4096)), false, "ram").unwrap();
    tree.write_file("/mnt/f", b"f", false).unwrap();
    let file = tree.identify("/mnt/f").unwrap().0;
    tree.hold(file).unwrap();
    assert_eq!(tree.unmount("/mnt"), Err(FsError::Busy));
    tree.release(file);
    tree.unmount("/mnt").unwrap();
}

/// A hook that records what it's told, and refuses changes to one file.
struct Recorder {
    refuse: std::sync::Mutex<Option<FileId>>,
    seen: std::sync::Mutex<Vec<String>>,
}

impl FileHook for Recorder {
    fn may_change(&self, file: FileId) -> Result<(), FsError> {
        if *self.refuse.lock().unwrap() == Some(file) {
            self.seen.lock().unwrap().push(format!("refused {}", file.ino));
            return Err(FsError::TextBusy);
        }
        Ok(())
    }

    fn changed(&self, file: FileId, change: Change<'_>) {
        let text = match change {
            Change::Written { offset, data, size } => format!("wrote {} {offset}+{} size {size}", file.ino, data.len()),
            Change::Resized { size } => format!("resized {} {size}", file.ino),
            Change::Failed { size, .. } => format!("failed {} size {size}", file.ino),
            Change::Nothing => format!("nothing {}", file.ino),
        };
        self.seen.lock().unwrap().push(text);
    }

    fn removed(&self, file: FileId) {
        self.seen.lock().unwrap().push(format!("removed {}", file.ino));
    }

    fn unmounting(&self, _dev: u64) {}
}

fn recorder() -> &'static Recorder {
    Box::leak(Box::new(Recorder { refuse: std::sync::Mutex::new(None), seen: std::sync::Mutex::new(Vec::new()) }))
}

#[test]
fn write_hook_called() {
    let tree = vfs();
    let hook = recorder();
    tree.set_file_hook(hook);
    tree.write_file("/f", b"hello", false).unwrap();
    let ino = tree.identify("/f").unwrap().0.ino;
    hook.seen.lock().unwrap().clear();
    tree.write_at("/f", 3, b"p!!").unwrap();
    tree.truncate("/f", 2).unwrap();
    let handle = tree.open("/f", OpenOptions::appending()).unwrap();
    tree.write_handle(handle, b"yz").unwrap();
    tree.close(handle).unwrap();
    tree.write_file("/f", b"new", false).unwrap();
    let handle = tree.open("/f", OpenOptions::replacing()).unwrap();
    tree.close(handle).unwrap();
    tree.remove_file("/f").unwrap();
    let seen = hook.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            format!("wrote {ino} 3+3 size 6"),
            format!("resized {ino} 2"),
            format!("wrote {ino} 2+2 size 4"),
            format!("resized {ino} 0"),
            format!("wrote {ino} 0+3 size 3"),
            format!("resized {ino} 0"),
            format!("removed {ino}"),
        ]
    );
}

#[test]
fn write_to_exec_mapped_refused() {
    let tree = vfs();
    let hook = recorder();
    tree.set_file_hook(hook);
    tree.write_file("/bin", b"\x7fELF code", false).unwrap();
    tree.write_file("/other", b"other", false).unwrap();
    let file = tree.identify("/bin").unwrap().0;
    *hook.refuse.lock().unwrap() = Some(file);
    // Every way of changing its bytes is refused, and nothing changed.
    assert_eq!(tree.write_at("/bin", 0, b"X"), Err(FsError::TextBusy));
    assert_eq!(tree.write_file("/bin", b"X", false), Err(FsError::TextBusy));
    assert_eq!(tree.write_file("/bin", b"X", true), Err(FsError::TextBusy));
    assert_eq!(tree.truncate("/bin", 0), Err(FsError::TextBusy));
    assert_eq!(tree.open("/bin", OpenOptions::replacing()), Err(FsError::TextBusy));
    let handle = tree.open("/bin", OpenOptions::appending()).unwrap();
    assert_eq!(tree.write_handle(handle, b"X"), Err(FsError::TextBusy));
    tree.close(handle).unwrap();
    assert_eq!(read_all(&tree, "/bin"), b"\x7fELF code");
    // Other files, and reading this one, are as ever.
    tree.write_at("/other", 0, b"O").unwrap();
    assert_eq!(read_all(&tree, "/other"), b"Other");
    // Removing a running program's name is allowed, as on Linux.
    tree.remove_file("/bin").unwrap();
}

#[test]
fn write_back_skips_hook_and_never_grows() {
    let tree = vfs();
    let hook = recorder();
    tree.set_file_hook(hook);
    tree.write_file("/f", b"hello world", false).unwrap();
    let file = tree.identify("/f").unwrap().0;
    hook.seen.lock().unwrap().clear();
    // Written back by identity, across a rename, without the hook hearing of it: the
    // bytes were the cache's already.
    tree.rename("/f", "/g").unwrap();
    let mut buffer = [0u8; 64];
    tree.write_back(file, 6, &mut buffer, &mut |out| {
        out[..9].copy_from_slice(b"WORLD+tai");
        9
    })
    .unwrap();
    assert_eq!(read_all(&tree, "/g"), b"hello WORLD", "clipped at the end");
    assert!(hook.seen.lock().unwrap().is_empty());
    // Nothing to write is nothing written; a file that's gone is NotFound.
    tree.write_back(file, 0, &mut buffer, &mut |_| 0).unwrap();
    tree.remove_file("/g").unwrap();
    assert_eq!(tree.write_back(file, 0, &mut buffer, &mut |_| 1), Err(FsError::NotFound));
}

#[test]
fn trailing_slash_rules() {
    let vfs = seeded();
    vfs.symlink("/docs", "/dirlink").unwrap();
    vfs.symlink("/readme.txt", "/filelink").unwrap();
    assert!(path::has_trailing_slash("/docs/"));
    assert!(!path::has_trailing_slash("/docs"));
    assert!(!path::has_trailing_slash("/"));
    assert!(!path::has_trailing_slash("//"));
    // A file named with a slash after it isn't one; a directory is, and so is a link to
    // one, followed even where the call wouldn't follow a last link.
    assert_eq!(path::lookup(&vfs, "/readme.txt/", false), Err(FsError::NotDirectory));
    assert_eq!(path::lookup(&vfs, "/filelink/", false), Err(FsError::NotDirectory));
    assert_eq!(path::lookup(&vfs, "/docs/", false).unwrap().canonical, "/docs");
    let through = path::lookup(&vfs, "/dirlink/", false).unwrap();
    assert_eq!((through.canonical.as_str(), through.stat.kind), ("/docs", FileKind::Directory));
    // Without the slash, the link itself.
    let link = path::lookup(&vfs, "/dirlink", false).unwrap();
    assert_eq!((link.canonical.as_str(), link.stat.kind), ("/dirlink", FileKind::Symlink));
    assert_eq!(path::lookup(&vfs, "/readme.txt", false).unwrap().stat.kind, FileKind::File);
    // `mkdir("a/")` makes `a`.
    vfs.create_dir("/made/").unwrap();
    assert_eq!(vfs.stat("/made").unwrap().kind, FileKind::Directory);
    assert_eq!(path::last_component("/a/b//"), Some("b"));
    assert_eq!(path::last_component("/"), None);
}

#[test]
fn dotdot_is_physical() {
    let vfs = seeded();
    vfs.create_dir("/real").unwrap();
    vfs.create_dir("/real/sub").unwrap();
    vfs.symlink("/real/sub", "/docs/hop").unwrap();
    // `..` from a link is the parent of where it leads, not of the link.
    let up = path::lookup(&vfs, "/docs/hop/..", true).unwrap();
    assert_eq!(up.canonical, "/real");
    // A dirfd opened through the link remembers where it led; `..` from it is the same.
    let dir = path::lookup(&vfs, "/docs/hop", true).unwrap();
    assert_eq!(dir.canonical, "/real/sub");
    let joined = path::join(&dir.canonical, "../sub/../..").unwrap();
    assert_eq!(path::lookup(&vfs, &joined, true).unwrap().canonical, "/");
    assert_eq!(path::join("/real", "/abs").unwrap(), "/abs");
    assert_eq!(path::join("/", "x").unwrap(), "/x");
    // And `..` at the root stays there.
    assert_eq!(path::lookup(&vfs, "/../..", true).unwrap().canonical, "/");
}

#[test]
fn forty_links_is_eloop() {
    let vfs = seeded();
    // `/l0` leads to the file through 40 links in a row; `/l0` via one more is 41.
    vfs.symlink("/readme.txt", "/l40").unwrap();
    for index in (0..40).rev() {
        vfs.symlink(&format!("/l{}", index + 1), &format!("/l{index}")).unwrap();
    }
    assert_eq!(MAX_SYMLINK_HOPS, 40);
    // `/l1` is 40 links from the file, `/l0` 41.
    assert_eq!(path::lookup(&vfs, "/l1", true).unwrap().canonical, "/readme.txt");
    assert_eq!(path::lookup(&vfs, "/l0", true), Err(FsError::SymlinkLoop));
    assert_eq!(path::lookup(&vfs, "/l0", false).unwrap().stat.kind, FileKind::Symlink);
}

#[test]
fn cookies_survive_resnapshot() {
    let vfs = seeded();
    for index in 0..20 {
        vfs.create_file(&format!("/docs/f{index:02}")).unwrap();
    }
    let first = Snapshot::take(&vfs, "/docs").unwrap();
    // `.`, `..`, guide.md and the twenty.
    assert_eq!(first.len(), 23);
    assert_eq!(first.get(0).unwrap().name, ".");
    assert_eq!(first.get(1).unwrap().name, "..");
    assert_eq!(first.get(1).unwrap().ino, vfs.stat("/").unwrap().ino);
    assert_eq!(first.get(0).unwrap().ino, vfs.stat("/docs").unwrap().ino);
    let again = Snapshot::take(&vfs, "/docs").unwrap();
    for cookie in 0..first.len() {
        assert_eq!(first.get(cookie), again.get(cookie));
    }
    assert_eq!(again.get(23), None);
    let kinds: Vec<FileKind> = (0..first.len()).map(|at| first.get(at).unwrap().kind).collect();
    assert_eq!(kinds.iter().filter(|kind| **kind == FileKind::File).count(), 21);
    assert_eq!(Snapshot::take(&vfs, "/readme.txt").err(), Some(FsError::NotDirectory));
}

#[test]
fn orphans_cleaned_at_mount() {
    use crate::WritableFileSystem;
    let vfs = seeded();
    vfs.create_dir("/m").unwrap();
    let mut ram = RamFs::new(1024 * 1024);
    let root = ram.root();
    let orphans = ram.create_dir(root, ORPHANS).unwrap();
    ram.create_file(orphans, "2-7").unwrap();
    ram.create_file(root, "kept").unwrap();
    vfs.mount("/m", Box::new(ram), false, "ram").unwrap();
    assert!(vfs.read_dir("/m/.feos-orphans").unwrap().is_empty());
    assert_eq!(vfs.stat("/m/kept").unwrap().kind, FileKind::File);
    // The root's are cleaned the same way, by whoever builds the tree.
    vfs.write_file("/x", b"x", false).unwrap();
    let left = vfs.orphan("/x").unwrap();
    assert_eq!(vfs.stat(&left).unwrap().size, 1);
    assert_eq!(vfs.clean_orphans("/").unwrap(), 1);
    assert_eq!(vfs.stat(&left), Err(FsError::NotFound));
}

#[test]
fn orphans_hidden_from_listing() {
    let vfs = seeded();
    vfs.write_file("/docs/victim", b"still here", false).unwrap();
    let before = vfs.resolve_path("/docs/victim", false).unwrap();
    let at = vfs.orphan("/docs/victim").unwrap();
    assert_eq!(at, format!("/.feos-orphans/{}-{}", before.id.dev, before.id.ino));
    // Gone from where it was, kept where it went, and the same file by identity.
    assert_eq!(vfs.stat("/docs/victim"), Err(FsError::NotFound));
    assert_eq!(read_all(&vfs, &at), b"still here");
    let mut out = [0u8; 16];
    let got = vfs.read_file(before.id, 0, &mut out).unwrap();
    assert_eq!(&out[..got], b"still here");
    assert_eq!(vfs.write_file_at(before.id, 5, b"THERE").unwrap(), 5);
    vfs.truncate_file(before.id, 10).unwrap();
    assert_eq!(read_all(&vfs, &at), b"stillTHERE");
    // The root's listing doesn't show where; a program's lookup can't reach it.
    assert!(vfs.read_dir("/").unwrap().iter().all(|entry| entry.name != ORPHANS));
    assert!(Snapshot::take(&vfs, "/").unwrap().len() >= 2);
    assert!((0..Snapshot::take(&vfs, "/").unwrap().len()).all(|at| Snapshot::take(&vfs, "/").unwrap().get(at).unwrap().name != ORPHANS));
    assert_eq!(path::lookup(&vfs, "/.feos-orphans", true).err(), Some(FsError::NotFound));
    assert_eq!(path::lookup(&vfs, &at, true).err(), Some(FsError::NotFound));
    vfs.symlink("/.feos-orphans", "/sneaky").unwrap();
    assert_eq!(path::lookup(&vfs, "/sneaky/", true).err(), Some(FsError::NotFound));
    // A directory can't be orphaned, nor the root.
    assert_eq!(vfs.orphan("/docs"), Err(FsError::IsDirectory));
    assert_eq!(vfs.orphan("/"), Err(FsError::Busy));
}

/// A process as the tests' `/proc` sees it: pid 7, running `/bin/prog`, with descriptors
/// 0 to 2 (a pipe and the console), 5 (a file) and 9 (a directory).
struct FakeProcess {
    pid: Option<u32>,
}

impl FakeProcess {
    const FDS: [(u32, &'static str); 5] =
        [(0, "pipe:[3]"), (1, "/dev/console"), (2, "/dev/console"), (5, "/docs/guide.md"), (9, "/docs")];
}

impl devfs::ProcessView for FakeProcess {
    fn pid(&self) -> Option<u32> {
        self.pid
    }
    fn exe(&self) -> Option<String> {
        self.pid.map(|_| String::from("/bin/prog"))
    }
    fn descriptor_count(&self) -> usize {
        if self.pid.is_some() { Self::FDS.len() } else { 0 }
    }
    fn nth_descriptor(&self, index: usize) -> Option<u32> {
        self.pid?;
        Self::FDS.get(index).map(|(fd, _)| *fd)
    }
    fn descriptor_target(&self, fd: u32) -> Option<String> {
        self.pid?;
        Self::FDS.iter().find(|(open, _)| *open == fd).map(|(_, target)| String::from(*target))
    }
}

static PID_7: FakeProcess = FakeProcess { pid: Some(7) };
static KERNEL_APP: FakeProcess = FakeProcess { pid: None };

/// The boot tree's synthesised mounts over a seeded root, `/proc` as `view` sees it.
fn with_devfs(view: &'static FakeProcess) -> Vfs {
    let vfs = seeded();
    vfs.create_dir("/bin").unwrap();
    vfs.write_file("/bin/prog", b"\x7fELF", false).unwrap();
    vfs.mount("/tmp", Box::new(RamFs::with_owners(1024 * 1024, 0o1777)), false, "tmp").unwrap();
    vfs.mount("/dev", Box::new(devfs::DevFs), false, "dev").unwrap();
    vfs.mount("/dev/shm", Box::new(RamFs::with_owners(1024 * 1024, 0o1777)), false, "shm").unwrap();
    vfs.mount("/proc", Box::new(devfs::ProcFs::new(view)), false, "proc").unwrap();
    vfs
}

#[test]
fn devfs_nodes_and_modes() {
    let vfs = with_devfs(&PID_7);
    for (path, kind, mode) in [
        ("/dev/null", DeviceKind::Null, 0o666),
        ("/dev/zero", DeviceKind::Zero, 0o666),
        ("/dev/full", DeviceKind::Full, 0o666),
        ("/dev/tty", DeviceKind::Tty, 0o666),
        ("/dev/console", DeviceKind::Console, 0o600),
    ] {
        let stat = vfs.stat(path).unwrap();
        assert_eq!(stat.device, Some(kind), "{path}");
        assert_eq!(stat.mode, devfs::S_IFCHR | mode, "{path}");
        assert_eq!((stat.uid, stat.posix), (0, true), "{path}");
    }
    assert_eq!(DeviceKind::Null.rdev(), 1 << 8 | 3);
    assert_eq!(DeviceKind::Console.rdev(), 5 << 8 | 1);
    // Nothing else of a random device until `GETRANDOM`.
    assert_eq!(vfs.stat("/dev/urandom"), Err(FsError::NotFound));
    assert_eq!(vfs.stat("/dev/random"), Err(FsError::NotFound));
    // The links into `/proc/self/fd`.
    for (name, target) in [("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2"), ("fd", "/proc/self/fd")] {
        let path = format!("/dev/{name}");
        assert_eq!(vfs.lstat(&path).unwrap().kind, FileKind::Symlink, "{path}");
        assert_eq!(vfs.read_link(&path).unwrap(), target);
    }
    // `/dev/fd` leads to the caller's descriptors; `/dev/stdout` to the console here.
    assert_eq!(vfs.resolve_path("/dev/fd", true).unwrap().canonical, "/proc/7/fd");
    assert_eq!(vfs.resolve_path("/dev/stdout", true).unwrap().canonical, "/dev/console");
    // Reads and writes.
    let null = vfs.identify("/dev/null").unwrap().0;
    let zero = vfs.identify("/dev/zero").unwrap().0;
    let full = vfs.identify("/dev/full").unwrap().0;
    let mut out = [9u8; 8];
    assert_eq!(vfs.read_file(null, 0, &mut out).unwrap(), 0);
    assert_eq!(vfs.write_file_at(null, 0, b"gone").unwrap(), 4);
    assert_eq!(vfs.read_file(zero, 1 << 40, &mut out).unwrap(), 8);
    assert_eq!(out, [0; 8]);
    assert_eq!(vfs.write_file_at(zero, 0, b"gone").unwrap(), 4);
    out.fill(9);
    assert_eq!(vfs.read_file(full, 0, &mut out).unwrap(), 8);
    assert_eq!(out, [0; 8]);
    assert_eq!(vfs.write_file_at(full, 0, b"x"), Err(FsError::NoSpace));
    // Emptying a device, as every `> /dev/null` does, works and changes nothing.
    vfs.truncate_file(null, 0).unwrap();
    vfs.write_file("/dev/null", b"shell", false).unwrap();
    // The listing, and the shared-memory directory on top.
    let names: Vec<String> = vfs.read_dir("/dev").unwrap().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["console", "fd", "full", "null", "shm", "stderr", "stdin", "stdout", "tty", "zero"]);
    let shm = vfs.stat("/dev/shm").unwrap();
    assert_eq!((shm.kind, shm.mode & 0o7777, shm.posix), (FileKind::Directory, 0o1777, true));
    let tmp = vfs.stat("/tmp").unwrap();
    assert_eq!(tmp.mode, crate::native::S_IFDIR | 0o1777);
    assert_eq!(vfs.statfs("/proc").unwrap().kind, "proc");
}

#[test]
fn devfs_refuses_changes() {
    let vfs = with_devfs(&PID_7);
    assert_eq!(vfs.create_file("/dev/evil"), Err(FsError::NotPermitted));
    assert_eq!(vfs.write_file("/dev/evil", b"x", false), Err(FsError::NotPermitted));
    assert_eq!(vfs.create_dir("/dev/evil"), Err(FsError::NotPermitted));
    assert_eq!(vfs.remove_file("/dev/null"), Err(FsError::NotPermitted));
    assert_eq!(vfs.remove_file("/dev/stdout"), Err(FsError::NotPermitted));
    assert_eq!(vfs.rename("/dev/null", "/dev/nul"), Err(FsError::NotPermitted));
    assert_eq!(vfs.chmod("/dev/null", 0o777), Err(FsError::NotPermitted));
    assert_eq!(vfs.chown("/dev/null", Some(5), None), Err(FsError::NotPermitted));
    assert_eq!(vfs.symlink("/x", "/dev/link"), Err(FsError::NotPermitted));
    assert_eq!(vfs.make_device("/dev/new", DeviceKind::Null), Err(FsError::NotPermitted));
    assert!(vfs.stat("/dev/null").is_ok());
    // `/proc` the same, through a link or not.
    assert_eq!(vfs.create_file("/proc/new"), Err(FsError::NotPermitted));
    assert_eq!(vfs.create_dir("/proc/self/new"), Err(FsError::NotPermitted));
    assert_eq!(vfs.create_file("/dev/fd/7"), Err(FsError::NotPermitted));
    assert_eq!(vfs.remove_file("/proc/self"), Err(FsError::NotPermitted));
    assert_eq!(vfs.remove_file("/proc/7/fd/5"), Err(FsError::NotPermitted));
    assert_eq!(vfs.remove_dir("/proc/7/fd"), Err(FsError::NotPermitted));
    assert_eq!(vfs.rename("/proc/self", "/proc/me"), Err(FsError::NotPermitted));
    // The file a descriptor link leads to is still the file's own business.
    assert_eq!(vfs.stat("/proc/self/fd/5").unwrap().size, 8);
    // But `/dev/shm` is a RAM directory like any other.
    vfs.write_file("/dev/shm/segment", b"shared", false).unwrap();
    assert_eq!(read_all(&vfs, "/dev/shm/segment"), b"shared");
    vfs.remove_file("/dev/shm/segment").unwrap();
}

#[test]
fn procfs_lists_fds() {
    let vfs = with_devfs(&PID_7);
    assert_eq!(vfs.read_link("/proc/self").unwrap(), "7");
    assert_eq!(vfs.resolve_path("/proc/self", true).unwrap().canonical, "/proc/7");
    let names: Vec<String> = vfs.read_dir("/proc").unwrap().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["7", "self"]);
    let names: Vec<String> = vfs.read_dir("/proc/self").unwrap().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["exe", "fd"]);
    // `exe` leads to the program, which is there.
    assert_eq!(vfs.read_link("/proc/self/exe").unwrap(), "/bin/prog");
    let exe = vfs.resolve_path("/proc/self/exe", true).unwrap();
    assert_eq!((exe.canonical.as_str(), exe.stat.size), ("/bin/prog", 4));
    // Each open descriptor, a link to where it leads, in order.
    let listed = vfs.read_dir("/proc/self/fd").unwrap();
    let names: Vec<&str> = listed.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["0", "1", "2", "5", "9"]);
    assert!(listed.iter().all(|entry| entry.stat.kind == FileKind::Symlink));
    for (fd, target) in FakeProcess::FDS {
        assert_eq!(vfs.read_link(&format!("/proc/7/fd/{fd}")).unwrap(), target);
    }
    assert_eq!(vfs.resolve_path("/dev/fd/5", true).unwrap().canonical, "/docs/guide.md");
    assert_eq!(vfs.resolve_path("/proc/self/fd/9/guide.md", true).unwrap().canonical, "/docs/guide.md");
    // A pipe's leads nowhere a path can go: the kernel opens those itself.
    assert_eq!(vfs.resolve_path("/dev/stdin", true).err(), Some(FsError::NotFound));
    assert_eq!(vfs.resolve_path("/dev/stdin", false).unwrap().canonical, "/dev/stdin");
    let link = path::lookup(&vfs, "/proc/self/fd/0", false).unwrap();
    assert_eq!(devfs::descriptor_link(&link.canonical, 7), Some(0));
    assert_eq!(devfs::descriptor_link("/proc/7/fd/00", 7), None);
    assert_eq!(devfs::descriptor_link("/proc/8/fd/0", 7), None);
    // Closed descriptors and other names aren't there.
    assert_eq!(vfs.stat("/proc/self/fd/3"), Err(FsError::NotFound));
    assert_eq!(vfs.lstat("/proc/self/fd/05"), Err(FsError::NotFound));
    assert_eq!(vfs.stat("/proc/self/maps"), Err(FsError::NotFound));
    // Inode numbers are the same in every look.
    let ino = vfs.lstat("/proc/self/fd/5").unwrap().ino;
    assert_eq!(listed.iter().find(|entry| entry.name == "5").unwrap().stat.ino, ino);
    let snapshot = Snapshot::take(&vfs, "/proc/self/fd").unwrap();
    assert_eq!(snapshot.len(), 7);
    assert_eq!(snapshot.get(0).unwrap().name, ".");
}

#[test]
fn procfs_hides_other_pids() {
    let vfs = with_devfs(&PID_7);
    for path in ["/proc/1", "/proc/8/exe", "/proc/1/fd/0", "/proc/07", "/proc/-7"] {
        assert_eq!(vfs.lstat(path), Err(FsError::NotFound), "{path}");
    }
    assert_eq!(vfs.lstat("/proc/7").unwrap().kind, FileKind::Directory);
    // A caller that isn't a program sees an empty `/proc`.
    let vfs = with_devfs(&KERNEL_APP);
    assert!(vfs.read_dir("/proc").unwrap().is_empty());
    assert_eq!(vfs.stat("/proc").unwrap().kind, FileKind::Directory);
    for path in ["/proc/self", "/proc/7", "/dev/fd/0", "/dev/stdout"] {
        assert_eq!(vfs.stat(path), Err(FsError::NotFound), "{path}");
    }
}

/// A process whose descriptor 3 leads somewhere a little different at every look, as when
/// another thread `dup3`s over it while the tree walks, and whose descriptor 4 is a file opened by
/// a path longer than a link may be.
struct ShiftingProcess {
    looks: std::sync::atomic::AtomicUsize,
}

impl ShiftingProcess {
    const LONG: usize = MAX_LINK_TARGET + 100;
}

impl devfs::ProcessView for ShiftingProcess {
    fn pid(&self) -> Option<u32> {
        Some(7)
    }
    fn exe(&self) -> Option<String> {
        Some(String::from("/bin/prog"))
    }
    fn descriptor_count(&self) -> usize {
        2
    }
    fn nth_descriptor(&self, index: usize) -> Option<u32> {
        [3, 4].get(index).copied()
    }
    fn descriptor_target(&self, fd: u32) -> Option<String> {
        match fd {
            // Shorter at every look: a link read as a size and then its bytes comes up
            // short every time.
            3 => {
                let look = self.looks.fetch_add(1, AtomicOrdering::Relaxed);
                Some(format!("/docs{}", "/.".repeat(400usize.saturating_sub(look))))
            }
            4 => Some(format!("/{}", "a".repeat(Self::LONG))),
            _ => None,
        }
    }
}

static SHIFTING: ShiftingProcess = ShiftingProcess { looks: std::sync::atomic::AtomicUsize::new(0) };

#[test]
fn procfs_links_stay_links() {
    let vfs = seeded();
    vfs.mount("/proc", Box::new(devfs::ProcFs::new(&SHIFTING)), false, "proc").unwrap();
    // However the target moves between looks, the node is a link to one of them, never a
    // file of its own (which a program could open for writing, or map).
    for _ in 0..8 {
        assert_eq!(path::lookup(&vfs, "/proc/self/fd/3", false).unwrap().stat.kind, FileKind::Symlink);
        assert_eq!(vfs.lstat("/proc/7/fd/3").unwrap().kind, FileKind::Symlink);
        assert!(vfs.read_link("/proc/7/fd/3").unwrap().starts_with("/docs/."));
        assert_eq!(vfs.resolve_path("/proc/7/fd/3", true).unwrap().canonical, "/docs");
    }
    // A target too long for a link is still a link, one that leads nowhere a path goes,
    // so following it reaches the descriptor (the kernel's to do), not a file.
    assert_eq!(vfs.lstat("/proc/7/fd/4").unwrap().kind, FileKind::Symlink);
    assert_eq!(vfs.read_link("/proc/7/fd/4").unwrap(), devfs::TOO_LONG);
    assert_eq!(vfs.resolve_path("/proc/7/fd/4", true).err(), Some(FsError::NotFound));
    let both = OpenOptions { read: true, write: true, ..OpenOptions::default() };
    assert_eq!(vfs.open("/proc/7/fd/4", both).err(), Some(FsError::NotFound));
    let listed = vfs.read_dir("/proc/7/fd").unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|entry| entry.stat.kind == FileKind::Symlink));
}

#[test]
fn sticky_tmp_refuses_foreign_unlink() {
    let vfs = with_devfs(&PID_7);
    vfs.write_file("/tmp/roots", b"r", false).unwrap();
    vfs.write_file("/tmp/users", b"u", false).unwrap();
    vfs.chown("/tmp/users", Some(1000), Some(100)).unwrap();
    vfs.create_dir("/tmp/open").unwrap();
    vfs.chmod("/tmp/open", 0o777).unwrap();
    vfs.write_file("/tmp/open/roots", b"r", false).unwrap();
    assert_eq!(vfs.stat("/tmp/users").unwrap().uid, 1000);
    // Not another's, in a sticky directory, even with write permission on it.
    assert_eq!(vfs.check_remove("/tmp/roots", 1000, 100), Err(FsError::NotPermitted));
    assert_eq!(vfs.check_remove("/tmp/roots", 1001, 1000), Err(FsError::NotPermitted));
    // One's own, or anything as root.
    assert_eq!(vfs.check_remove("/tmp/users", 1000, 100), Ok(()));
    assert_eq!(vfs.check_remove("/tmp/roots", 0, 0), Ok(()));
    // A directory without the bit is write permission alone.
    assert_eq!(vfs.check_remove("/tmp/open/roots", 1000, 100), Ok(()));
    vfs.chmod("/tmp/open", 0o755).unwrap();
    assert_eq!(vfs.check_remove("/tmp/open/roots", 1000, 100), Err(FsError::PermissionDenied));
    // The directory's owner may tidy it.
    vfs.create_dir("/tmp/theirs").unwrap();
    vfs.chown("/tmp/theirs", Some(1000), Some(100)).unwrap();
    vfs.chmod("/tmp/theirs", 0o1777).unwrap();
    vfs.write_file("/tmp/theirs/roots", b"r", false).unwrap();
    assert_eq!(vfs.check_remove("/tmp/theirs/roots", 1000, 100), Ok(()));
    assert_eq!(vfs.check_remove("/tmp/theirs/roots", 1001, 100), Err(FsError::NotPermitted));
    // Where nothing keeps owners there's nothing to refuse.
    assert_eq!(vfs.check_remove("/docs/guide.md", 1000, 100), Ok(()));
}

/// Each kind of tree, with a clock to the nanosecond that only moves when told to.
fn every_kind_timed() -> Vec<(&'static str, Vfs)> {
    let kinds = every_kind();
    for (_, tree) in &kinds {
        tree.set_time_source(fine_now);
    }
    kinds
}

std::thread_local! {
    static FINE_NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(1_700_000_001_234_567_891) };
}

fn fine_now() -> Option<crate::Timestamp> {
    Some(crate::Timestamp(FINE_NOW.get()))
}

/// Moves the fine clock on by `nanos`.
fn tick(nanos: u64) {
    FINE_NOW.set(FINE_NOW.get() + nanos);
}

#[test]
fn rename_replaces_file_atomically() {
    // The crash half is `tests/crash.rs`'s, for the filesystems on disks; this is what
    // anyone looking sees, on every kind.
    for (kind, tree) in every_kind() {
        tree.write_file("/to", b"old", false).unwrap();
        tree.write_file("/from", b"new!", false).unwrap();
        let moving = tree.identify("/from").unwrap().0;
        let handle = tree.open("/to", OpenOptions::reading()).unwrap();
        assert_eq!(tree.rename_replacing("/from", "/to", false, None, &|_| false), Ok(None), "{kind}");
        assert_eq!(read_all(&tree, "/to"), b"new!", "{kind}");
        assert_eq!(tree.identify("/to").unwrap().0, moving, "{kind}: the file keeps its identity");
        assert_eq!(tree.stat("/from"), Err(FsError::NotFound), "{kind}");
        // The replaced file is gone, its handles with it.
        assert_eq!(tree.read_handle(handle, &mut [0u8; 4]), Err(FsError::StaleHandle), "{kind}");
        // A file over a directory, and a directory over a file, are refused.
        tree.create_dir("/dir").unwrap();
        assert_eq!(tree.rename_replacing("/to", "/dir", false, None, &|_| false), Err(FsError::IsDirectory), "{kind}");
        assert_eq!(tree.rename_replacing("/dir", "/to", false, None, &|_| false), Err(FsError::NotDirectory), "{kind}");
        // Onto itself, nothing happens.
        assert_eq!(tree.rename_replacing("/to", "/to", false, None, &|_| false), Ok(None), "{kind}");
        assert_eq!(read_all(&tree, "/to"), b"new!", "{kind}");
        // Under itself is refused, as is going across mounts.
        assert_eq!(tree.rename_replacing("/dir", "/dir/in", false, None, &|_| false), Err(FsError::InvalidMove), "{kind}");
        tree.create_dir("/mnt").unwrap();
        tree.mount("/mnt", Box::new(RamFs::new(4096)), false, "ram").unwrap();
        assert_eq!(tree.rename_replacing("/to", "/mnt/to", false, None, &|_| false), Err(FsError::CrossDevice), "{kind}");
        assert_eq!(tree.rename_replacing("/to", "/mnt", false, None, &|_| false), Err(FsError::Busy), "{kind}");
        assert_eq!(tree.rename_replacing("/mnt", "/elsewhere", false, None, &|_| false), Err(FsError::Busy), "{kind}");
    }
}

#[test]
fn rename_dir_over_empty_dir() {
    for (kind, tree) in every_kind() {
        tree.create_dir("/a").unwrap();
        tree.write_file("/a/inside", b"in", false).unwrap();
        tree.create_dir("/empty").unwrap();
        tree.create_dir("/full").unwrap();
        tree.write_file("/full/x", b"x", false).unwrap();
        assert_eq!(tree.rename_replacing("/a", "/full", false, None, &|_| false), Err(FsError::NotEmpty), "{kind}");
        assert_eq!(tree.rename_replacing("/a", "/empty", false, None, &|_| false), Ok(None), "{kind}");
        assert_eq!(read_all(&tree, "/empty/inside"), b"in", "{kind}");
        assert_eq!(tree.stat("/a"), Err(FsError::NotFound), "{kind}");
        // Nor of one it's in, which isn't empty while it's there.
        tree.create_dir("/empty/sub").unwrap();
        assert_eq!(tree.rename_replacing("/empty/sub", "/empty", false, None, &|_| false), Err(FsError::NotEmpty), "{kind}");
        tree.sync().unwrap();
    }
}

#[test]
fn rename_noreplace() {
    for (kind, tree) in every_kind() {
        tree.write_file("/a", b"a", false).unwrap();
        tree.write_file("/b", b"b", false).unwrap();
        assert_eq!(tree.rename_replacing("/a", "/b", true, None, &|_| false), Err(FsError::NameExists), "{kind}");
        assert_eq!(read_all(&tree, "/a"), b"a", "{kind}");
        assert_eq!(read_all(&tree, "/b"), b"b", "{kind}");
        assert_eq!(tree.rename_replacing("/a", "/c", true, None, &|_| false), Ok(None), "{kind}");
        assert_eq!(read_all(&tree, "/c"), b"a", "{kind}");
        // A name onto itself exists too, as Linux's `renameat2` says.
        assert_eq!(tree.rename_replacing("/c", "/c", true, None, &|_| false), Err(FsError::NameExists), "{kind}");
        assert_eq!(tree.rename_replacing("/c", "/c", false, None, &|_| false), Ok(None), "{kind}");
    }
}

/// A tree with owners: root's `/bin` and `/.feos-orphans`, and `/home/a`, user 1000's,
/// where a directory has already been swapped for a link to each, as a racing thread
/// would between a call's check and its change.
fn swapped_tree() -> Vfs {
    let tree = Vfs::new(Box::new(RamFs::with_owners(1 << 20, 0o755)), false, "ram");
    tree.create_dir("/bin").unwrap();
    tree.write_file("/bin/sh", b"root's", false).unwrap();
    tree.create_dir(&format!("/{ORPHANS}")).unwrap();
    tree.chmod(&format!("/{ORPHANS}"), 0o700).unwrap();
    tree.write_file(&format!("/{ORPHANS}/1-9"), b"someone's", false).unwrap();
    tree.create_dir("/home").unwrap();
    tree.create_dir("/home/a").unwrap();
    tree.chown("/home/a", Some(1000), Some(100)).unwrap();
    tree.write_file("/home/a/mine", b"mine", false).unwrap();
    tree.chown("/home/a/mine", Some(1000), Some(100)).unwrap();
    tree.symlink("/bin", "/home/a/d").unwrap();
    tree.symlink(&format!("/{ORPHANS}"), "/home/a/o").unwrap();
    tree
}

#[test]
fn program_calls_judge_what_the_walk_finds() {
    let tree = swapped_tree();
    let user = Caller { uid: 1000, gid: 100 };
    let in_use = |_| false;
    // Into root's directory, or out of it, through the link: no.
    assert_eq!(tree.rename_replacing("/home/a/mine", "/home/a/d/sh", false, Some(user), &in_use), Err(FsError::PermissionDenied));
    assert_eq!(tree.rename_replacing("/home/a/d/sh", "/home/a/sh", false, Some(user), &in_use), Err(FsError::PermissionDenied));
    assert_eq!(read_all(&tree, "/bin/sh"), b"root's");
    // The orphans can't be reached even by root's own calls.
    let root = Caller { uid: 0, gid: 0 };
    assert_eq!(tree.rename_replacing("/home/a/o/1-9", "/home/a/got", false, Some(root), &in_use), Err(FsError::NotFound));
    assert_eq!(tree.rename_replacing("/home/a/mine", "/home/a/o/x", false, Some(root), &in_use), Err(FsError::NotFound));
    // Without a caller, it's a kernel app's, and nothing's judged.
    assert_eq!(tree.rename_replacing("/home/a/mine", "/home/a/d/mine", false, None, &in_use), Ok(None));
    tree.rename_replacing("/bin/mine", "/home/a/mine", false, None, &in_use).unwrap();
    // A user's own rename goes.
    assert_eq!(tree.rename_replacing("/home/a/mine", "/home/a/ours", false, Some(user), &in_use), Ok(None));

    let ours = tree.resolve_path("/home/a/ours", false).unwrap().id;
    assert_eq!(tree.link_as("/home/a/ours", "/home/a/d/ours", ours, user), Err(FsError::PermissionDenied));
    let orphan = tree.resolve_path(&format!("/{ORPHANS}/1-9"), false).unwrap().id;
    assert_eq!(tree.link_as("/home/a/o/1-9", "/home/a/got", orphan, root), Err(FsError::NotFound));
    // The file found reachable, or nothing.
    let sh = tree.resolve_path("/bin/sh", false).unwrap().id;
    assert_eq!(tree.link_as("/home/a/ours", "/home/a/again", sh, user), Err(FsError::NotFound));
    assert_eq!(tree.link_as("/home/a/ours", "/home/a/again", ours, user), Err(FsError::Unsupported), "no hard links in RAM");

    assert_eq!(tree.symlink_as("/x", "/home/a/d/planted", user), Err(FsError::PermissionDenied));
    assert_eq!(tree.symlink_as("/x", "/home/a/o/planted", root), Err(FsError::NotFound));
    assert_eq!(tree.lstat("/bin/planted").map(|_| ()), Err(FsError::NotFound));
    // A user's link is theirs as it's made.
    tree.symlink_as("/x", "/home/a/link", user).unwrap();
    let link = tree.lstat("/home/a/link").unwrap();
    assert_eq!((link.kind, link.uid, link.gid), (FileKind::Symlink, 1000, 100));
}

#[test]
fn creations_are_judged_and_owned_in_one_step() {
    let tree = swapped_tree();
    let user = Caller { uid: 1000, gid: 100 };
    let root = Caller { uid: 0, gid: 0 };
    // Into root's directory through the link: no, and nothing made there.
    assert_eq!(tree.create_file_as("/home/a/d/new", 0o644, user).map(|_| ()), Err(FsError::PermissionDenied));
    assert_eq!(tree.create_dir_as("/home/a/d/newdir", Some(0o755), user), Err(FsError::PermissionDenied));
    assert_eq!(tree.lstat("/bin/new").map(|_| ()), Err(FsError::NotFound));
    assert_eq!(tree.lstat("/bin/newdir").map(|_| ()), Err(FsError::NotFound));
    // The orphans, even for root.
    assert_eq!(tree.create_file_as("/home/a/o/new", 0o644, root).map(|_| ()), Err(FsError::NotFound));
    assert_eq!(tree.create_dir_as("/home/a/o/newdir", None, root), Err(FsError::NotFound));
    // A user's own: theirs, with its mode, from the start, and what was made is said.
    let made = tree.create_file_as("/home/a/new", 0o640, user).unwrap();
    let stat = tree.lstat("/home/a/new").unwrap();
    assert_eq!((stat.kind, stat.uid, stat.gid, stat.mode & 0o7777), (FileKind::File, 1000, 100, 0o640));
    assert_eq!((made.canonical.as_str(), made.id), ("/home/a/new", tree.resolve_path("/home/a/new", false).unwrap().id));
    assert_eq!(tree.create_file_as("/home/a/new", 0o640, user).map(|_| ()), Err(FsError::NameExists));
    tree.create_dir_as("/home/a/sub", Some(0o700), user).unwrap();
    let stat = tree.lstat("/home/a/sub").unwrap();
    assert_eq!((stat.kind, stat.uid, stat.gid, stat.mode & 0o7777), (FileKind::Directory, 1000, 100, 0o700));
    // Root's are root's, and a mode of `None` leaves the filesystem's.
    tree.create_dir_as("/bin/rootdir", None, root).unwrap();
    let stat = tree.lstat("/bin/rootdir").unwrap();
    assert_eq!((stat.uid, stat.mode & 0o7777), (0, tree.lstat("/home").unwrap().mode & 0o7777));
}

#[test]
fn changes_by_identity_ask_of_the_file_changed() {
    let tree = swapped_tree();
    let sh = tree.resolve_path("/bin/sh", false).unwrap().id;
    let mode = tree.stat("/bin/sh").unwrap().mode;
    let owner_only = |stat: &Stat| if stat.uid == 1000 { Ok(()) } else { Err(FsError::NotPermitted) };
    assert_eq!(tree.chmod_file(sh, 0o4777, &owner_only), Err(FsError::NotPermitted));
    assert_eq!(tree.stat("/bin/sh").unwrap().mode, mode);
    let times = NodeTimes { modified: Some(crate::Timestamp(1)), ..NodeTimes::NONE };
    assert_eq!(tree.set_times_file(sh, times, &owner_only), Err(FsError::NotPermitted));
    assert_ne!(tree.stat("/bin/sh").unwrap().times.modified, Some(crate::Timestamp(1)));
    // A directory and a link by identity too, each judged by its own facts.
    let home = tree.resolve_path("/home/a", false).unwrap().id;
    tree.chmod_file(home, 0o700, &owner_only).unwrap();
    assert_eq!(tree.stat("/home/a").unwrap().mode & 0o7777, 0o700);
    let link = tree.resolve_path("/home/a/d", false).unwrap().id;
    let is_link = |stat: &Stat| if stat.kind == FileKind::Symlink { Ok(()) } else { Err(FsError::NotPermitted) };
    tree.set_times_file(link, times, &is_link).unwrap();
    assert_eq!(tree.lstat("/home/a/d").unwrap().times.modified, Some(crate::Timestamp(1)));
}

#[test]
fn rename_over_file_in_use_orphans_it() {
    for (kind, tree) in every_kind() {
        tree.create_dir("/d").unwrap();
        tree.write_file("/d/running", b"the old program", false).unwrap();
        tree.write_file("/d/new", b"the new one", false).unwrap();
        let old = tree.identify("/d/running").unwrap().0;
        tree.hold(old).unwrap();
        let kept = tree.rename_replacing("/d/new", "/d/running", false, None, &|file| file == old).unwrap();
        let (file, at) = kept.unwrap_or_else(|| panic!("{kind}: not kept"));
        assert_eq!(file, old, "{kind}");
        assert_eq!(at, format!("/{ORPHANS}/{}-{}", old.dev, old.ino), "{kind}");
        assert_eq!(read_all(&tree, "/d/running"), b"the new one", "{kind}");
        // The old one still works, by identity and where it's kept, and nobody lists it.
        let mut out = [0u8; 15];
        assert_eq!(tree.read_file(old, 0, &mut out), Ok(15), "{kind}");
        assert_eq!(&out, b"the old program", "{kind}");
        assert_eq!(read_all(&tree, &at), b"the old program", "{kind}");
        assert!(!tree.read_dir("/").unwrap().iter().any(|entry| entry.name == ORPHANS), "{kind}");
        // Let go, it goes as an orphan does.
        tree.release(old);
        tree.remove_file(&at).unwrap();
        assert_eq!(tree.read_file(old, 0, &mut out), Err(FsError::NotFound), "{kind}");
        // One nothing uses isn't kept.
        tree.write_file("/d/other", b"o", false).unwrap();
        assert_eq!(tree.rename_replacing("/d/other", "/d/running", false, None, &|_| false), Ok(None), "{kind}");
    }
}

#[test]
fn ram_times_are_nanoseconds() {
    let tree = vfs();
    tree.set_time_source(fine_now);
    tree.write_file("/f", b"x", false).unwrap();
    let first = tree.stat("/f").unwrap().times.modified.unwrap();
    assert_eq!(first, crate::Timestamp(FINE_NOW.get()));
    tick(10_000_000);
    tree.write_at("/f", 1, b"y").unwrap();
    let second = tree.stat("/f").unwrap().times.modified.unwrap();
    assert_eq!(second.0 - first.0, 10_000_000);
    // Set to the nanosecond, kept to the nanosecond.
    let set = crate::Timestamp(1_600_000_000_987_654_321);
    tree.set_times("/f", NodeTimes { modified: Some(set), accessed: Some(set), ..NodeTimes::NONE }).unwrap();
    let times = tree.stat("/f").unwrap().times;
    assert_eq!((times.modified, times.accessed), (Some(set), Some(set)));
}

#[test]
fn native_times_are_nanoseconds() {
    use crate::WritableFileSystem;
    let disk = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
    let mut disk = disk;
    crate::native::format(&mut disk, 0).unwrap();
    let mut fs = crate::Native::mount(disk).unwrap();
    fs.set_time_source(fine_now);
    let file = fs.create_file(NodeId::ROOT, "f").unwrap();
    let made = crate::Timestamp(FINE_NOW.get());
    tick(10_000_000);
    fs.write(file, 0, b"x").unwrap();
    let written = crate::Timestamp(FINE_NOW.get());
    let set = crate::Timestamp(1_600_000_000_987_654_321);
    fs.set_times(file, NodeTimes { accessed: Some(set), ..NodeTimes::NONE }).unwrap();
    fs.flush().unwrap();
    // All of it on the disk, as the next mount finds.
    let fs = crate::Native::mount(fs.into_device()).unwrap();
    let times = fs.times(fs.lookup(NodeId::ROOT, "f").unwrap());
    assert_eq!(times.created, Some(made));
    assert_eq!(times.modified, Some(written));
    assert_eq!(times.accessed, Some(set));
    assert_eq!(times.changed, Some(written));
}

#[test]
fn native_old_image_reads_zero_nanoseconds() {
    use crate::partition::crc32;
    use crate::WritableFileSystem;
    let mut disk = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
    crate::native::format(&mut disk, 0).unwrap();
    let mut fs = crate::Native::mount(disk).unwrap();
    fs.set_time_source(fine_now);
    let file = fs.create_file(NodeId::ROOT, "f").unwrap();
    fs.write(file, 0, b"x").unwrap();
    fs.flush().unwrap();
    // Made into what version 1 wrote: the version, and nothing past the pointers.
    let mut image = fs.into_device().into_data();
    let word = |image: &[u8], at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
    image[8..12].copy_from_slice(&1u32.to_le_bytes());
    let crc = crc32(&image[..508]);
    image[508..512].copy_from_slice(&crc.to_le_bytes());
    let (inode_start, inode_blocks) = (word(&image, 32) as usize, word(&image, 36) as usize);
    for inode in (inode_start * 4096..(inode_start + inode_blocks) * 4096).step_by(128) {
        if image[inode..inode + 128].iter().all(|&byte| byte == 0) {
            continue;
        }
        image[inode + 104..inode + 124].fill(0);
        let crc = crc32(&image[inode..inode + 124]);
        image[inode + 124..inode + 128].copy_from_slice(&crc.to_le_bytes());
    }
    let fs = crate::Native::mount(RamDisk::from_bytes(image, 512).unwrap()).unwrap();
    let times = fs.times(fs.lookup(NodeId::ROOT, "f").unwrap());
    let whole = crate::Timestamp::from_secs(FINE_NOW.get() / 1_000_000_000);
    assert_eq!(times.modified, Some(whole));
    assert_eq!(times.created, Some(whole));
    // No access time of its own: the modification time stands in.
    assert_eq!(times.accessed, Some(whole));
    // Mounted to write, it says it's version 2 from now on.
    assert_eq!(word(fs.into_device().data(), 8), 2);
}

#[test]
fn exfat_times_10ms() {
    let mut disk = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
    crate::format_exfat(&mut disk, "X").unwrap();
    let disk = SharedDisk(std::sync::Arc::new(std::sync::Mutex::new(disk)));
    let tree = Vfs::new(Box::new(crate::Exfat::mount(disk.clone()).unwrap()), false, "exfat");
    // An odd second, and a little past: the two-second step, then the 10 ms count.
    FINE_NOW.set(1_700_000_001_234_567_891);
    tree.set_time_source(fine_now);
    tree.write_file("/f", b"x", false).unwrap();
    let kept = crate::Timestamp(1_700_000_001_230_000_000);
    assert_eq!(tree.stat("/f").unwrap().times.modified, Some(kept));
    assert_eq!(tree.stat("/f").unwrap().times.created, Some(kept));
    let set = crate::Timestamp(1_600_000_000_987_654_321);
    tree.set_times("/f", NodeTimes { modified: Some(set), ..NodeTimes::NONE }).unwrap();
    tree.sync().unwrap();
    drop(tree);
    let tree = Vfs::new(Box::new(crate::Exfat::mount(disk).unwrap()), false, "exfat");
    let times = tree.stat("/f").unwrap().times;
    assert_eq!(times.modified, Some(crate::Timestamp(1_600_000_000_980_000_000)));
    assert_eq!(times.created, Some(kept));
    // exFAT keeps no status change time.
    assert_eq!(times.changed, None);
}

#[test]
fn time_update_rules() {
    let mut kinds = every_kind_timed();
    // `ram` keeps no owners, so has no modes to change: this one does.
    let owned = Vfs::new(Box::new(RamFs::with_owners(1 << 20, 0o755)), false, "ram");
    owned.set_time_source(fine_now);
    kinds.push(("ram-owned", owned));
    for (kind, tree) in kinds {
        // FAT and exFAT keep no status change time, and their times are coarser: FAT's to
        // two seconds, exFAT's to ten milliseconds, each rounded down.
        let coarse = kind == "fat32" || kind == "exfat";
        let times = |path: &str| tree.stat(path).unwrap().times;
        let step = || {
            tick(10_000_000_000);
            FINE_NOW.get()
        };
        let near = |got: Option<crate::Timestamp>, at: u64| match got {
            Some(got) if coarse => got.0 <= at && at - got.0 < 2_000_000_000,
            Some(got) => got.0 == at,
            None => false,
        };
        let changed = |got: Option<crate::Timestamp>, at: u64| if coarse { got.is_none() } else { near(got, at) };
        tree.create_dir("/d").unwrap();
        let at = step();
        tree.write_file("/d/f", b"x", false).unwrap();
        assert!(near(times("/d").modified, at), "{kind}: create sets the directory's mtime");
        assert!(changed(times("/d").changed, at), "{kind}: and its ctime");
        // A write sets mtime and ctime, and nothing changes the access time: a day on,
        // so FAT's access day would show it.
        let before = times("/d/f");
        tick(86_400_000_000_000);
        let at = step();
        tree.write_at("/d/f", 1, b"y").unwrap();
        let after = times("/d/f");
        assert!(near(after.modified, at) && changed(after.changed, at), "{kind}: write");
        assert_eq!(after.accessed, before.accessed, "{kind}: a write isn't a read");
        // Reads change nothing.
        step();
        let mut out = [0u8; 2];
        tree.read_at("/d/f", 0, &mut out).unwrap();
        assert_eq!(times("/d/f"), after, "{kind}: a read");
        // A chmod sets ctime alone, where there are modes to set.
        let at = step();
        if coarse || kind == "ram" {
            assert!(tree.chmod("/d/f", 0o600).is_err(), "{kind}: no modes");
        } else {
            tree.chmod("/d/f", 0o600).unwrap();
            let now = times("/d/f");
            assert_eq!(now.changed.map(|t| t.0), Some(at), "{kind}: chmod sets ctime");
            assert_eq!(now.modified, after.modified, "{kind}: and not mtime");
        }
        // A rename sets both directories' mtime and ctime, and the file's ctime.
        tree.create_dir("/e").unwrap();
        let at = step();
        tree.rename_replacing("/d/f", "/e/g", false, None, &|_| false).unwrap();
        for directory in ["/d", "/e"] {
            assert!(near(times(directory).modified, at), "{kind}: rename, {directory}'s mtime");
            assert!(changed(times(directory).changed, at), "{kind}: rename, {directory}'s ctime");
        }
        assert!(changed(times("/e/g").changed, at), "{kind}: rename, the file");
        assert_eq!(times("/e/g").modified, after.modified, "{kind}: rename keeps mtime");
        // An unlink sets the directory's.
        let at = step();
        tree.remove_file("/e/g").unwrap();
        assert!(near(times("/e").modified, at), "{kind}: unlink");
        assert!(changed(times("/e").changed, at), "{kind}: unlink");
    }
}
