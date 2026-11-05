use super::*;
use crate::{format_fat32, Fat32, RamDisk, StaticFs, StaticNode, BufferCache};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

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

static NOW: AtomicU64 = AtomicU64::new(1_700_000_000);

fn fake_now() -> Option<u64> {
    Some(NOW.load(AtomicOrdering::Relaxed))
}

#[test]
fn timestamps_are_stored_and_updated_on_every_backend() {
    for vfs in [fat_vfs(40 * 1024 * 1024), vfs()] {
        NOW.store(1_700_000_000, AtomicOrdering::Relaxed);
        vfs.set_time_source(fake_now);
        vfs.write_file("/f", b"x", false).unwrap();
        vfs.create_dir("/d").unwrap();
        let times = vfs.stat("/f").unwrap().times;
        assert_eq!(times.created, Some(1_700_000_000));
        assert_eq!(times.modified, Some(1_700_000_000));
        assert_eq!(vfs.stat("/d").unwrap().times.created, Some(1_700_000_000));

        NOW.store(1_700_000_100, AtomicOrdering::Relaxed);
        vfs.write_at("/f", 1, b"y").unwrap();
        let times = vfs.stat("/f").unwrap().times;
        assert_eq!(times.created, Some(1_700_000_000));
        assert_eq!(times.modified, Some(1_700_000_100));

        vfs.set_times(
            "/f",
            NodeTimes {
                modified: Some(1_600_000_000),
                ..NodeTimes::NONE
            },
        )
        .unwrap();
        assert_eq!(vfs.stat("/f").unwrap().times.modified, Some(1_600_000_000));
    }
}

#[test]
fn timestamps_survive_a_remount_on_fat() {
    let disk = SharedDisk::formatted(40 * 1024 * 1024);
    {
        let vfs = Vfs::new(Box::new(Fat32::mount(disk.clone()).unwrap()), false, "d");
        NOW.store(1_700_000_000, AtomicOrdering::Relaxed);
        vfs.set_time_source(fake_now);
        vfs.write_file("/f", b"x", false).unwrap();
        vfs.set_attributes("/f", attr::HIDDEN).unwrap();
        vfs.sync().unwrap();
    }
    let vfs = Vfs::new(Box::new(Fat32::mount(disk).unwrap()), false, "d");
    let stat = vfs.stat("/f").unwrap();
    assert_eq!(stat.times.modified, Some(1_700_000_000));
    assert_eq!(stat.times.created, Some(1_700_000_000));
    assert_eq!(stat.attributes & attr::HIDDEN, attr::HIDDEN);
    // The date alone, for the access time.
    assert_eq!(stat.times.accessed, Some(1_700_000_000 / 86_400 * 86_400));
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
