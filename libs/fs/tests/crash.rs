//! Cuts the power at every interesting moment of a filesystem's life.
//!
//! A recording disk logs each write and flush as a workload runs. Each test then
//! rebuilds what a real disk might hold at some moment: everything up to the last
//! flush for certain, and any subset of what came after, because a disk's cache
//! can reorder freely between flushes. The mount-time check must turn every one of
//! those states into a sound volume, with every synced file intact.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use feos_fs::vfs::VfsTree;
use feos_fs::{
    format_fat32, BlockDevice, BufferCache, EntryKind, Fat32, FileSystem, FsError, NodeId, RamDisk,
    Vfs, WritableBlockDevice, WritableFileSystem,
};

const DISK_BYTES: usize = 36 * 1024 * 1024;
const BLOCK: usize = 512;

#[derive(Clone)]
enum Event {
    Write(u64, Vec<u8>),
    Flush,
}

/// A disk that logs what reaches it.
struct Recorder {
    size: usize,
    /// Current contents, so that reads see writes.
    blocks: BTreeMap<u64, Vec<u8>>,
    base: Arc<Vec<u8>>,
    log: Arc<Mutex<Vec<Event>>>,
}

impl BlockDevice for Recorder {
    fn block_size(&self) -> usize {
        BLOCK
    }
    fn block_count(&self) -> u64 {
        (self.size / BLOCK) as u64
    }
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        match self.blocks.get(&block) {
            Some(data) => out.copy_from_slice(data),
            None => {
                let at = block as usize * BLOCK;
                out.copy_from_slice(&self.base[at..at + BLOCK]);
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

/// What's promised to be on the disk from a point in the log onward.
struct Promise {
    /// Log length when the sync that made it durable returned.
    from_event: usize,
    path: &'static str,
    contents: Vec<u8>,
}

struct Run {
    base: Arc<Vec<u8>>,
    log: Vec<Event>,
    promises: Vec<Promise>,
}

fn formatted_base() -> Arc<Vec<u8>> {
    let mut disk = RamDisk::new(DISK_BYTES, BLOCK).unwrap();
    format_fat32(&mut disk, "CRASH").unwrap();
    Arc::new(disk.into_data())
}

/// Runs `workload` on a cached volume, logging everything that reaches the disk.
fn record(workload: impl FnOnce(&Vfs, &mut dyn FnMut(&'static str, &[u8]))) -> Run {
    let base = formatted_base();
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder {
        size: DISK_BYTES,
        blocks: BTreeMap::new(),
        base: base.clone(),
        log: log.clone(),
    };
    let cache = BufferCache::new(recorder, 256 * 1024).unwrap();
    let vfs = Vfs::new(Box::new(Fat32::mount(cache).unwrap()), false, "disk");
    let mut promises = Vec::new();
    {
        let log = log.clone();
        let tree = &vfs;
        let mut promise = |path: &'static str, contents: &[u8]| {
            tree.sync().unwrap();
            promises.push(Promise {
                from_event: log.lock().unwrap().len(),
                path,
                contents: contents.to_vec(),
            });
        };
        workload(&vfs, &mut promise);
    }
    vfs.sync().unwrap();
    let log = log.lock().unwrap().clone();
    Run {
        base,
        log,
        promises,
    }
}

/// A small, fixed generator: failures have to reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The disk after the power fails at `cut`. Writes after the last flush land or
/// don't according to `rng`.
fn disk_at(run: &Run, cut: usize, rng: &mut Rng, keep_all: bool) -> RamDisk {
    let mut image = (*run.base).clone();
    let last_flush = run.log[..cut]
        .iter()
        .rposition(|event| matches!(event, Event::Flush))
        .map_or(0, |index| index + 1);
    for (index, event) in run.log[..cut].iter().enumerate() {
        if let Event::Write(block, data) = event {
            if index < last_flush || keep_all || rng.next() % 2 == 0 {
                let at = *block as usize * BLOCK;
                image[at..at + data.len()].copy_from_slice(data);
            }
        }
    }
    RamDisk::from_bytes(image, BLOCK).unwrap()
}

/// Walks a tree, reading every file: nothing may fail, and nothing may be damaged.
fn walk(fs: &dyn FileSystem, node: NodeId, context: &str) {
    for index in 0..fs.child_count(node) {
        let child = fs.child_at(node, index).unwrap();
        match fs.kind(child).unwrap() {
            EntryKind::Directory => walk(fs, child, context),
            EntryKind::File => {
                let data = fs.read_to_vec(child);
                assert!(
                    data.is_ok(),
                    "{context}: {:?} unreadable: {data:?}",
                    fs.name(child)
                );
            }
        }
    }
}

/// Everything a crash at this point must not have broken.
fn verify(run: &Run, cut: usize, disk: RamDisk, context: &str) {
    let mut fs = Fat32::mount(disk).unwrap_or_else(|error| panic!("{context}: mount: {error}"));
    assert!(!fs.is_damaged(), "{context}: damaged at mount: {:?}", fs.damage_reason());
    fs.check().unwrap_or_else(|error| panic!("{context}: check: {error}"));

    walk(&fs, NodeId::ROOT, context);
    for promise in &run.promises {
        if promise.from_event <= cut {
            let node = fs.resolve(promise.path);
            let node = node.unwrap_or_else(|| panic!("{context}: lost {}", promise.path));
            assert_eq!(
                fs.read_to_vec(node).unwrap(),
                promise.contents,
                "{context}: {} changed",
                promise.path
            );
        }
    }

    // It works: a new file goes in, and the check has nothing more to say.
    let probe = fs.create_file(NodeId::ROOT, "probe-after-crash").unwrap();
    fs.write(probe, 0, &[7u8; 10_000]).unwrap();
    fs.flush().unwrap();
    let free = fs.stats().free_blocks;
    let disk = fs.into_device();
    // For checking a sample with a real `fsck.vfat`, which a host may not have.
    if let Ok(path) = std::env::var("FEOS_CRASH_DUMP") {
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) % 40 == 0 {
            let name = format!("{path}/{}.img", context.replace(['/', ' '], "_"));
            std::fs::write(name, disk.data()).unwrap();
        }
    }
    let mut again = Fat32::mount(disk).unwrap();
    assert!(!again.check().unwrap().was_dirty, "{context}: flush left the volume dirty");
    assert_eq!(again.stats().free_blocks, free, "{context}: free count wrong");
}

fn workload(vfs: &Vfs, promise: &mut dyn FnMut(&'static str, &[u8])) {
    let stable: Vec<u8> = (0..20_000u32).map(|value| value as u8).collect();
    vfs.create_dir("/docs").unwrap();
    vfs.write_file("/docs/stable.bin", &stable, false).unwrap();
    promise("/docs/stable.bin", &stable);

    // Growth, in steps that cross clusters.
    vfs.write_file("/docs/log.txt", b"", false).unwrap();
    for round in 0..8u8 {
        vfs.write_file("/docs/log.txt", &[round; 3000], true).unwrap();
        if round == 4 {
            vfs.sync().unwrap();
        }
    }
    // A long name, and a file that gets deleted.
    vfs.write_file("/docs/a rather long file name with spaces.txt", &[1; 5000], false)
        .unwrap();
    vfs.write_file("/docs/doomed.bin", &[2; 9000], false).unwrap();
    vfs.remove_file("/docs/doomed.bin").unwrap();

    // Renames: within a directory and across, of a file and of a directory.
    vfs.rename("/docs/log.txt", "/docs/renamed.txt").unwrap();
    vfs.create_dir("/moved").unwrap();
    vfs.rename("/docs/renamed.txt", "/moved/renamed.txt").unwrap();
    vfs.create_dir("/docs/sub").unwrap();
    vfs.write_file("/docs/sub/inner", b"inner", false).unwrap();
    vfs.sync().unwrap();
    vfs.rename("/docs/sub", "/moved/sub").unwrap();

    // Shrinking, then replacing.
    vfs.truncate("/moved/renamed.txt", 100).unwrap();
    vfs.write_file("/moved/renamed.txt", &[9; 7000], false).unwrap();

    // A directory big enough to need a second cluster.
    vfs.create_dir("/many").unwrap();
    for number in 0..150 {
        let name = format!("/many/entry-with-a-long-name-{number:03}.txt");
        vfs.write_file(&name, b"x", false).unwrap();
    }
    for number in (0..150).step_by(2) {
        let name = format!("/many/entry-with-a-long-name-{number:03}.txt");
        vfs.remove_file(&name).unwrap();
    }
    let last = b"written last";
    vfs.write_file("/last.txt", last, false).unwrap();
    promise("/last.txt", last);
}

#[test]
fn every_cut_recovers_when_writes_land_in_order() {
    let run = record(workload);
    assert!(run.log.len() > 100, "the workload should do real I/O");
    let mut rng = Rng(1);
    // Every event is too slow in a debug build; a prime stride still lands on
    // each kind of moment as the workload cycles.
    let step = (run.log.len() / 150).max(1);
    for cut in (0..=run.log.len()).step_by(step) {
        let disk = disk_at(&run, cut, &mut rng, true);
        verify(&run, cut, disk, &format!("in-order cut at {cut}/{}", run.log.len()));
    }
}

#[test]
fn every_flush_boundary_recovers_when_writes_reorder() {
    let run = record(workload);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let boundaries: Vec<usize> = run
        .log
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, Event::Flush))
        .map(|(index, _)| index)
        .collect();
    // Just before a flush is when the most writes are in doubt.
    let step = (boundaries.len() / 100).max(1);
    for &cut in boundaries.iter().step_by(step) {
        for attempt in 0..3 {
            let disk = disk_at(&run, cut, &mut rng, false);
            let context = format!("reordered cut at {cut}/{} try {attempt}", run.log.len());
            verify(&run, cut, disk, &context);
        }
    }
}

#[test]
fn a_rename_interrupted_after_its_commit_keeps_one_name() {
    let run = record(|vfs, promise| {
        vfs.create_dir("/a").unwrap();
        vfs.create_dir("/b").unwrap();
        vfs.write_file("/a/file", b"payload", false).unwrap();
        vfs.create_dir("/a/dir").unwrap();
        vfs.write_file("/a/dir/child", b"child", false).unwrap();
        promise("/a/file", b"payload");
        vfs.rename("/a/file", "/b/file").unwrap();
        vfs.rename("/a/dir", "/b/dir").unwrap();
    });
    let mut rng = Rng(7);
    for cut in 0..=run.log.len() {
        let disk = disk_at(&run, cut, &mut rng, true);
        let mut fs = Fat32::mount(disk).unwrap();
        fs.check().unwrap();
        assert!(!fs.is_damaged());
        if run.promises[0].from_event > cut {
            continue;
        }
        let old = fs.resolve("/a/file").is_some();
        let new = fs.resolve("/b/file").is_some();
        assert!(old ^ new, "cut {cut}: old name {old}, new name {new}");
        let old_dir = fs.resolve("/a/dir/child").is_some();
        let new_dir = fs.resolve("/b/dir/child").is_some();
        assert!(old_dir ^ new_dir, "cut {cut}: old dir {old_dir}, new dir {new_dir}");
    }
}

#[test]
fn a_clean_shutdown_needs_no_check_and_a_dirty_one_is_flagged() {
    let run = record(workload);
    let mut rng = Rng(3);
    let disk = disk_at(&run, run.log.len(), &mut rng, true);
    let mut fs = Fat32::mount(disk).unwrap();
    assert!(!fs.check().unwrap().was_dirty);

    // Once the first change has landed, the flags say so.
    let first_data = run
        .log
        .iter()
        .position(|event| matches!(event, Event::Flush))
        .unwrap();
    let disk = disk_at(&run, first_data + 1, &mut rng, true);
    let mut fs = Fat32::mount(disk).unwrap();
    assert!(fs.check().unwrap().was_dirty);
}

#[test]
fn lost_clusters_are_reclaimed_and_counted() {
    let mut disk = RamDisk::new(DISK_BYTES, BLOCK).unwrap();
    format_fat32(&mut disk, "T").unwrap();
    let mut fs = Fat32::mount(disk).unwrap();
    let before = fs.stats().free_blocks;
    let file = fs.create_file(NodeId::ROOT, "f").unwrap();
    fs.write(file, 0, &[1; 8192]).unwrap();
    // The power fails before the entry is written back: clusters with no owner.
    let disk = fs.into_device();
    let mut data = disk.into_data();
    let root_entry = data
        .chunks_exact(32)
        .position(|entry| entry.starts_with(b"F          "))
        .unwrap();
    for byte in &mut data[root_entry * 32..root_entry * 32 + 32] {
        *byte = 0xe5;
    }
    let mut fs = Fat32::mount(RamDisk::from_bytes(data, BLOCK).unwrap()).unwrap();
    let report = fs.check().unwrap();
    assert!(report.was_dirty);
    assert!(report.lost_clusters > 0);
    assert_eq!(fs.stats().free_blocks, before);
}

/// A long-name entry, not the FAT bytes that can look like one.
fn is_fragment(entry: &[u8]) -> bool {
    let sequence = entry[0] & 0xa0 == 0 && entry[0] != 0;
    sequence && entry[11] == 0x0f && entry[12] == 0 && entry[26] == 0 && entry[27] == 0
}

#[test]
fn entries_past_the_end_marker_are_cleared() {
    let mut disk = RamDisk::new(DISK_BYTES, BLOCK).unwrap();
    format_fat32(&mut disk, "T").unwrap();
    let mut fs = Fat32::mount(disk).unwrap();
    let dir = fs.create_dir(NodeId::ROOT, "d").unwrap();
    for number in 0..40 {
        fs.create_file(dir, &format!("F{number}.TXT")).unwrap();
    }
    let mut data = fs.into_device().into_data();
    // The directory's first block of names never landed: it reads as the end.
    let start = data
        .chunks_exact(32)
        .position(|entry| entry.starts_with(b"F0      TXT"))
        .unwrap()
        * 32;
    data[start..start + 16 * 32].fill(0);
    let mut fs = Fat32::mount(RamDisk::from_bytes(data, BLOCK).unwrap()).unwrap();
    fs.check().unwrap();
    let data = fs.into_device().into_data();
    assert!(!data
        .chunks_exact(32)
        .any(|entry| entry.starts_with(b"F39     TXT")));
}

#[test]
fn name_fragments_with_no_owner_are_cleared() {
    let mut disk = RamDisk::new(DISK_BYTES, BLOCK).unwrap();
    format_fat32(&mut disk, "T").unwrap();
    let mut fs = Fat32::mount(disk).unwrap();
    let name = "a name long enough to need several fragments of long-name entries.txt";
    fs.create_file(NodeId::ROOT, name).unwrap();
    // Left dirty, with the first fragment lost: the others start nothing.
    let mut data = fs.into_device().into_data();
    let first = data
        .chunks_exact(32)
        .position(|entry| is_fragment(entry) && entry[0] & 0x40 != 0)
        .unwrap();
    data[first * 32] = 0xe5;

    let mut fs = Fat32::mount(RamDisk::from_bytes(data, BLOCK).unwrap()).unwrap();
    let report = fs.check().unwrap();
    assert!(report.orphan_names > 0);
    let data = fs.into_device().into_data();
    let live_fragments = data
        .chunks_exact(32)
        .filter(|entry| is_fragment(entry))
        .count();
    assert_eq!(live_fragments, 0);
}

#[test]
fn exfat_recovers_at_every_cut_with_writes_in_order_or_not() {
    const SIZE: usize = 8 * 1024 * 1024;
    let mut disk = RamDisk::new(SIZE, BLOCK).unwrap();
    feos_fs::format_exfat(&mut disk, "CRASH").unwrap();
    let base = Arc::new(disk.into_data());
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder {
        size: SIZE,
        blocks: BTreeMap::new(),
        base: base.clone(),
        log: log.clone(),
    };
    let cache = BufferCache::new(recorder, 256 * 1024).unwrap();
    let vfs = Vfs::new(Box::new(feos_fs::Exfat::mount(cache).unwrap()), false, "disk");
    vfs.create_dir("/docs").unwrap();
    vfs.write_file("/docs/stable", &[5; 9000], false).unwrap();
    vfs.sync().unwrap();
    let promised_from = log.lock().unwrap().len();
    vfs.write_file("/docs/log", &[1; 3000], false).unwrap();
    vfs.write_file("/docs/log", &[2; 3000], true).unwrap();
    vfs.rename("/docs/log", "/moved").unwrap();
    vfs.create_dir("/sub").unwrap();
    vfs.rename("/sub", "/docs/sub").unwrap();
    vfs.write_file("/doomed", &[3; 20_000], false).unwrap();
    vfs.remove_file("/doomed").unwrap();
    vfs.truncate("/moved", 10).unwrap();
    for number in 0..40 {
        vfs.write_file(&format!("/a-long-name-for-an-entry-set-{number}"), b"x", false).unwrap();
    }
    vfs.sync().unwrap();
    let run = Run {
        base,
        log: log.lock().unwrap().clone(),
        promises: Vec::new(),
    };

    let mut rng = Rng(11);
    let mut repaired = 0;
    // Every cut, in order and with the writes since the last flush landing at random.
    for cut in 0..=run.log.len() {
        for keep_all in [true, false, false] {
            let disk = disk_at(&run, cut, &mut rng, keep_all);
            let mut fs = feos_fs::Exfat::mount(disk).unwrap();
            let context = format!("exFAT cut {cut}/{} in-order {keep_all}", run.log.len());
            assert!(!fs.is_damaged(), "{context}: a crash left it read-only");
            let report = fs.check().unwrap_or_else(|error| panic!("{context}: {error}"));
            repaired += u32::from(!report.found_nothing());
            assert!(!fs.is_read_only(), "{context}");
            walk(&fs, NodeId::ROOT, &context);
            if cut >= promised_from {
                let node = fs.resolve("/docs/stable");
                let node = node.unwrap_or_else(|| panic!("{context}: lost a synced file"));
                assert_eq!(fs.read_to_vec(node).unwrap(), [5; 9000], "{context}");
            }
            // It accepts writes, the check has nothing more to say, and a new mount
            // agrees with what this one thinks is free.
            let probe = fs.create_file(NodeId::ROOT, "probe").unwrap();
            fs.write(probe, 0, &[7; 10_000]).unwrap();
            fs.flush().unwrap();
            let free = fs.stats().free_blocks;
            let disk = fs.into_device();
            let mut again = feos_fs::Exfat::mount(disk).unwrap();
            assert!(!again.is_damaged(), "{context}");
            assert!(!again.check().unwrap().was_dirty, "{context}: flush left it dirty");
            assert_eq!(again.stats().free_blocks, free, "{context}: free count wrong");
            let node = again.resolve("/probe").unwrap();
            assert_eq!(again.read_to_vec(node).unwrap(), [7; 10_000], "{context}");
        }
    }
    assert!(repaired > 0);
}

/// Tearing a set by hand, in the ways a crash could if writes reach the disk out of order.
mod torn_sets {
    use super::*;
    use feos_fs::{CheckReport, Exfat};

    const SIZE: usize = 8 * 1024 * 1024;

    /// A dirty volume with a file, and where that file's set is on disk.
    fn volume() -> (Vec<u8>, usize) {
        let mut disk = RamDisk::new(SIZE, BLOCK).unwrap();
        feos_fs::format_exfat(&mut disk, "TORN").unwrap();
        let mut fs = Exfat::mount(disk).unwrap();
        let kept = fs.create_file(NodeId::ROOT, "kept.txt").unwrap();
        fs.write(kept, 0, &[4; 5000]).unwrap();
        let file = fs.create_file(NodeId::ROOT, "torn.txt").unwrap();
        fs.write(file, 0, &[9; 5000]).unwrap();
        fs.flush().unwrap();
        let mut image = fs.into_device().into_data();
        // The volume flags are at offset 106 of the boot sector; bit 1 is dirty.
        image[106] |= 2;
        let at = find_set(&image, "torn.txt");
        (image, at)
    }

    fn find_set(image: &[u8], name: &str) -> usize {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let found = image
            .chunks(32)
            .position(|entry| entry[0] == 0xc1 && entry[2..].starts_with(&units))
            .expect("the set is on disk");
        (found - 2) * 32
    }

    fn repaired(image: Vec<u8>) -> (Exfat<RamDisk>, CheckReport) {
        let mut fs = Exfat::mount(RamDisk::from_bytes(image, BLOCK).unwrap()).unwrap();
        assert!(!fs.is_damaged());
        let report = fs.check().unwrap();
        (fs, report)
    }

    fn still_fine(fs: &mut Exfat<RamDisk>) {
        let kept = fs.resolve("/kept.txt").unwrap();
        assert_eq!(fs.read_to_vec(kept).unwrap(), [4; 5000]);
        assert!(!fs.is_read_only());
        let probe = fs.create_file(NodeId::ROOT, "probe").unwrap();
        fs.write(probe, 0, &[1; 100]).unwrap();
        fs.flush().unwrap();
    }

    #[test]
    fn only_the_checksum_wrong_keeps_the_file_and_its_data() {
        let (mut image, at) = volume();
        image[at + 2] ^= 0xff;
        let (mut fs, report) = repaired(image);
        assert_eq!(report.repaired_sets, 1);
        let node = fs.resolve("/torn.txt").unwrap();
        assert_eq!(fs.read_to_vec(node).unwrap(), [9; 5000]);
        still_fine(&mut fs);
        // The fix reached the disk.
        let image = fs.into_device().into_data();
        let fs = Exfat::mount(RamDisk::from_bytes(image, BLOCK).unwrap()).unwrap();
        assert!(!fs.is_damaged());
        assert!(fs.resolve("/torn.txt").is_some());
    }

    #[test]
    fn a_stream_entry_that_never_landed_clears_the_set() {
        let (mut image, at) = volume();
        image[at + 32] = 0;
        let (mut fs, report) = repaired(image.clone());
        assert_eq!(report.removed_entries, 1);
        assert_eq!(fs.resolve("/torn.txt"), None);
        assert!(report.lost_clusters > 0);
        still_fine(&mut fs);

        // Likewise a name that doesn't match its hash.
        let (mut image, at) = volume();
        image[at + 64 + 2] ^= 0x01;
        let (mut fs, report) = repaired(image);
        assert_eq!(report.removed_entries, 1);
        assert_eq!(fs.resolve("/torn.txt"), None);
        still_fine(&mut fs);
    }

    #[test]
    fn a_set_cut_short_clears_what_there_is() {
        let (mut image, at) = volume();
        image[at + 64] &= 0x7f;
        let (mut fs, report) = repaired(image);
        assert_eq!(report.removed_entries, 1);
        assert_eq!(fs.resolve("/torn.txt"), None);
        still_fine(&mut fs);
    }

    #[test]
    fn secondaries_with_no_primary_are_cleared() {
        let (mut image, at) = volume();
        image[at] &= 0x7f;
        let (mut fs, report) = repaired(image);
        assert_eq!(report.orphan_names, 2);
        assert_eq!(fs.resolve("/torn.txt"), None);
        still_fine(&mut fs);
    }

    #[test]
    fn on_a_clean_volume_a_torn_set_is_damage_and_stays_read_only() {
        let (mut image, at) = volume();
        image[106] &= !2;
        image[at + 2] ^= 0xff;
        let mut fs = Exfat::mount(RamDisk::from_bytes(image, BLOCK).unwrap()).unwrap();
        assert!(fs.is_damaged());
        assert!(fs.is_read_only());
        assert!(fs.create_file(NodeId::ROOT, "no").is_err());
    }
}

/// FAT12 and FAT16 are read-only, so these are built by hand here.
mod old_fat {
    use feos_fs::{detect, Fat16, FileSystem, FsError, RamDisk, VolumeKind};

    const SECTOR: usize = 512;
    const RESERVED: usize = 1;
    const FATS: usize = 2;
    const ROOT_ENTRIES: usize = 64;

    fn dir_entry(name: &[u8; 11], attr: u8, start: u16, size: u32) -> [u8; 32] {
        let mut entry = [0u8; 32];
        entry[..11].copy_from_slice(name);
        entry[11] = attr;
        // 2024-03-15 12:30:10.
        entry[22..24].copy_from_slice(&((12u16 << 11) | (30 << 5) | 5).to_le_bytes());
        entry[24..26].copy_from_slice(&(((2024u16 - 1980) << 9) | (3 << 5) | 15).to_le_bytes());
        entry[26..28].copy_from_slice(&start.to_le_bytes());
        entry[28..32].copy_from_slice(&size.to_le_bytes());
        entry
    }

    fn set_fat(image: &mut [u8], fat_sectors: usize, wide: bool, cluster: usize, value: u16) {
        for copy in 0..FATS {
            let base = (RESERVED + copy * fat_sectors) * SECTOR;
            if wide {
                image[base + cluster * 2..base + cluster * 2 + 2]
                    .copy_from_slice(&value.to_le_bytes());
            } else {
                let at = base + cluster + cluster / 2;
                let old = u16::from_le_bytes([image[at], image[at + 1]]);
                let new = if cluster & 1 != 0 {
                    (old & 0x000f) | (value << 4)
                } else {
                    (old & 0xf000) | (value & 0x0fff)
                };
                image[at..at + 2].copy_from_slice(&new.to_le_bytes());
            }
        }
    }

    /// A volume with `/DOC.TXT` over two clusters and `/SUB/INNER.BIN`.
    fn image(clusters: usize, spc: usize) -> (Vec<u8>, Vec<u8>) {
        let wide = clusters >= 4085;
        let fat_bytes = if wide { (clusters + 2) * 2 } else { (clusters + 2) * 3 / 2 + 1 };
        let fat_sectors = fat_bytes.div_ceil(SECTOR);
        let root_sectors = (ROOT_ENTRIES * 32).div_ceil(SECTOR);
        let data_start = RESERVED + FATS * fat_sectors + root_sectors;
        let total = data_start + clusters * spc;
        let mut image = vec![0u8; total * SECTOR];

        let boot = &mut image[..512];
        boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
        boot[3..11].copy_from_slice(b"MSDOS5.0");
        boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        boot[13] = spc as u8;
        boot[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        boot[16] = FATS as u8;
        boot[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
        if total < 65_536 {
            boot[19..21].copy_from_slice(&(total as u16).to_le_bytes());
        } else {
            boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
        }
        boot[21] = 0xf8;
        boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xaa;

        let end = if wide { 0xffff } else { 0x0fff };
        let cluster_bytes = spc * SECTOR;
        let contents: Vec<u8> = (0..cluster_bytes + 100).map(|value| (value % 199) as u8).collect();
        set_fat(&mut image, fat_sectors, wide, 2, 3);
        set_fat(&mut image, fat_sectors, wide, 3, end);
        set_fat(&mut image, fat_sectors, wide, 4, end);
        set_fat(&mut image, fat_sectors, wide, 5, end);

        let root = (RESERVED + FATS * fat_sectors) * SECTOR;
        let doc = dir_entry(b"DOC     TXT", 0x20, 2, contents.len() as u32);
        let sub = dir_entry(b"SUB        ", 0x10, 4, 0);
        image[root..root + 32].copy_from_slice(&doc);
        image[root + 32..root + 64].copy_from_slice(&sub);

        let cluster = |number: usize| (data_start + (number - 2) * spc) * SECTOR;
        let at = cluster(2);
        image[at..at + contents.len()].copy_from_slice(&contents);
        let at = cluster(4);
        image[at..at + 32].copy_from_slice(&dir_entry(b".          ", 0x10, 4, 0));
        image[at + 32..at + 64].copy_from_slice(&dir_entry(b"..         ", 0x10, 0, 0));
        image[at + 64..at + 96].copy_from_slice(&dir_entry(b"INNER   BIN", 0x20, 5, 5));
        let at = cluster(5);
        image[at..at + 5].copy_from_slice(b"inner");
        (image, contents)
    }

    fn check(clusters: usize, spc: usize, kind: &str) {
        let (bytes, contents) = image(clusters, spc);
        let disk = RamDisk::from_bytes(bytes, SECTOR).unwrap();
        assert_eq!(detect(&disk).unwrap(), VolumeKind::Fat16);
        let fs = Fat16::mount(disk).unwrap();
        assert_eq!(fs.stats().kind, kind);
        assert!(fs.stats().read_only);
        assert!(!fs.is_damaged());
        let doc = fs.resolve("/DOC.TXT").unwrap();
        assert_eq!(fs.read_to_vec(doc).unwrap(), contents);
        let mut partial = [0u8; 10];
        assert_eq!(fs.read(doc, spc * SECTOR + 5, &mut partial), 10);
        assert_eq!(partial[0], contents[spc * SECTOR + 5]);
        let inner = fs.resolve("/SUB/INNER.BIN").unwrap();
        assert_eq!(fs.read_to_vec(inner).unwrap(), b"inner");
        // 2024-03-15 12:30:10.
        assert_eq!(fs.times(inner).modified, Some(1_710_505_810));
    }

    #[test]
    fn fat12_reads() {
        check(2000, 2, "fat12");
    }

    #[test]
    fn fat16_reads() {
        check(5000, 1, "fat16");
    }

    #[test]
    fn a_fat32_sized_volume_is_not_taken_for_fat16() {
        let (mut bytes, _) = image(5000, 1);
        // More clusters than the old formats can have.
        bytes[19..21].fill(0);
        bytes[32..36].copy_from_slice(&200_000u32.to_le_bytes());
        bytes.resize(200_000 * SECTOR, 0);
        let disk = RamDisk::from_bytes(bytes, SECTOR).unwrap();
        assert_eq!(Fat16::mount(disk).err(), Some(FsError::UnsupportedVolume));
    }

    #[test]
    fn every_damaged_byte_of_the_metadata_fails_safely() {
        let (good, _) = image(2000, 2);
        let metadata = 512 * 10;
        for at in (0..metadata).step_by(7) {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[at] ^= flip;
                let disk = RamDisk::from_bytes(bad, SECTOR).unwrap();
                if let Ok(fs) = Fat16::mount(disk) {
                    for id in 0..fs.node_id_limit() as u32 {
                        let _ = fs.read_to_vec(feos_fs::NodeId(id));
                    }
                }
            }
        }
    }

    #[test]
    fn a_chain_that_loops_is_an_error() {
        let (mut bytes, _) = image(5000, 1);
        // DOC.TXT's second cluster points back at its first.
        let fat_sectors = (5002usize * 2).div_ceil(SECTOR);
        set_fat(&mut bytes, fat_sectors, true, 3, 2);
        let fs = Fat16::mount(RamDisk::from_bytes(bytes, SECTOR).unwrap()).unwrap();
        let doc = fs.resolve("/DOC.TXT").unwrap();
        assert!(fs.read_to_vec(doc).is_err());
    }
}
