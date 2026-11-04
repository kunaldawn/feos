//! Deterministic fuzzing of mounting, walking, reading and writing FAT32 and
//! exFAT volumes, with metadata mutated in place.

mod support;

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_fs::{detect, Exfat, Fat32, FileSystem, FsError, NodeId, WritableFileSystem};
use fuzz::{run_constrained, LimitAllocator, Rng};
use support::*;

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const ITERATIONS: usize = 600;

/// Walks everything iteratively with a bound, reading each file.
fn exercise(fs: &dyn FileSystem) {
    let mut stack = vec![NodeId::ROOT];
    let mut visited = 0usize;
    let mut path = [0u8; 512];
    let mut buffer = [0u8; 700];
    while let Some(node) = stack.pop() {
        visited += 1;
        assert!(visited <= 200_000, "the walk must terminate");
        let _ = fs.name(node);
        let _ = fs.path(node, &mut path);
        let size = fs.size(node);
        if fs.is_file(node) {
            // Short reads are all valid, but none may run past the file's size.
            if let Ok(count) = fs.try_read(node, 0, &mut buffer) {
                assert!(count <= size);
            }
            if size > 1 {
                let _ = fs.try_read(node, size - 1, &mut buffer);
                let _ = fs.try_read(node, size / 2, &mut buffer);
            }
            if size <= 1 << 20 {
                let _ = fs.read_to_vec(node);
            }
        }
        for index in 0..fs.child_count(node) {
            if let Some(child) = fs.child_at(node, index) {
                stack.push(child);
            }
        }
    }
}

fn mutate_ranges(
    disk: &mut SparseDisk,
    rng: &mut Rng,
    ranges: &[(u64, u64)],
    undo: &mut Vec<(u64, [u8; 8])>,
) {
    for _ in 0..1 + rng.below(6) {
        let (start, len) = ranges[rng.below(ranges.len())];
        let offset = start + rng.next_u64() % len;
        let mut old = [0u8; 8];
        disk.read_at(offset, &mut old);
        undo.push((offset, old));
        match rng.below(5) {
            0 => {
                let mut byte = [old[0] ^ (1 << rng.below(8))];
                disk.write_at(offset, std::slice::from_mut(&mut byte[0]));
            }
            1 => disk.write_at(offset, &[rng.next_u64() as u8]),
            2 => disk.write_at(offset, &0xffff_ffffu32.to_le_bytes()),
            3 => disk.write_at(offset, &0u32.to_le_bytes()),
            _ => {
                // A plausible cluster number, for chains and start clusters.
                let cluster = 2 + rng.below(64) as u32;
                disk.write_at(offset, &cluster.to_le_bytes());
            }
        }
    }
}

fn undo_all(disk: &mut SparseDisk, undo: &mut Vec<(u64, [u8; 8])>) {
    while let Some((offset, old)) = undo.pop() {
        disk.write_at(offset, &old);
    }
}

fn fat_seed(geometry: FatGeometry) -> FatImage {
    let mut image = FatImage::new(geometry);
    let cluster = image.cluster_size() as usize;
    image.add_long_file(
        FAT_ROOT,
        "A long file name.bin",
        b"ALONGF~1BIN",
        &vec![0x5a; cluster * 3 + 7],
    );
    image.add_file(FAT_ROOT, b"SMALL   TXT", b"small");
    let docs = image.add_dir(FAT_ROOT, b"DOCS       ");
    image.add_file(docs, b"NOTE    TXT", &vec![0x33; cluster + 1]);
    let inner = image.add_dir(docs, b"INNER      ");
    image.add_file(inner, b"DEEP    TXT", b"deep");
    image
}

fn fat_ranges(image: &FatImage) -> Vec<(u64, u64)> {
    let sector = image.geometry.sector as u64;
    let cluster = image.cluster_size();
    vec![
        (0, 512),
        (image.fat_offset(0, 0), 256),
        (image.cluster_offset(FAT_ROOT), cluster.min(1024)),
        (image.cluster_offset(FAT_ROOT) + sector.min(cluster), 512),
        (image.cluster_offset(FAT_ROOT + 1), cluster * 24),
    ]
}

fn fuzz_fat(geometry: FatGeometry, seed: u64) {
    let mut image = fat_seed(geometry);
    let ranges = fat_ranges(&image);
    let mut rng = Rng::new(seed);
    let mut undo = Vec::new();
    for iteration in 0..fuzz::scaled(ITERATIONS) {
        mutate_ranges(&mut image.disk, &mut rng, &ranges, &mut undo);
        let _ = detect(&image.disk);
        if let Ok(fs) = Fat32::mount(&image.disk) {
            exercise(&fs);
        }
        // Writing changes the image, so snapshot for this rarer path.
        if iteration % 16 == 0 {
            let saved = image.disk.blocks.clone();
            if let Ok(mut fs) = Fat32::mount(&mut image.disk) {
                write_some(&mut fs);
            }
            image.disk.blocks = saved;
        }
        undo_all(&mut image.disk, &mut undo);
    }
}

fn write_some(fs: &mut dyn WritableFileSystem) {
    let mut list = Vec::new();
    for index in 0..fs.child_count(NodeId::ROOT).min(8) {
        list.extend(fs.child_at(NodeId::ROOT, index));
    }
    for &node in &list {
        let _ = fs.write(node, 3, b"written");
        let _ = fs.set_len(node, 9);
    }
    if let Ok(file) = fs.create_file(NodeId::ROOT, "created file.txt") {
        let _ = fs.write(file, 700, b"gap");
        let _ = fs.rename(file, NodeId::ROOT, "CREATED FILE.TXT");
    }
    if let Ok(dir) = fs.create_dir(NodeId::ROOT, "subdir") {
        let _ = fs.create_file(dir, "inner");
    }
    for &node in &list {
        let _ = fs.rename(node, NodeId::ROOT, "renamed");
        let _ = fs.remove(node);
    }
    let _ = fs.flush();
}

#[test]
fn fat32_survives_mutated_metadata() {
    run_constrained(|| fuzz_fat(FAT_512, 61));
}

#[test]
fn fat32_with_multi_sector_clusters_survives_mutated_metadata() {
    run_constrained(|| fuzz_fat(FAT_MULTI, 62));
}

#[test]
fn fat32_with_4k_sectors_survives_mutated_metadata() {
    run_constrained(|| fuzz_fat(FAT_4K, 63));
}

fn exfat_seed(geometry: ExfatGeometry) -> ExfatImage {
    let mut image = ExfatImage::new(geometry);
    let cluster = image.cluster_size() as usize;
    image.add_file(EXFAT_ROOT, "chained.bin", &vec![0x5a; cluster * 3 + 7]);
    image.add_contiguous_file(EXFAT_ROOT, "straight.bin", &vec![0x33; cluster * 2]);
    let docs = image.add_dir(EXFAT_ROOT, "docs");
    image.add_file(docs, "note.txt", b"note");
    let inner = image.add_dir(docs, "inner");
    image.add_file(inner, "caf\u{e9}.txt", b"deep");
    image
}

fn exfat_ranges(image: &ExfatImage) -> Vec<(u64, u64)> {
    let cluster = image.cluster_size();
    vec![
        (0, 512),
        (image.fat_offset(0), 128),
        (image.cluster_offset(EXFAT_ROOT), cluster.min(512)),
        (image.cluster_offset(EXFAT_ROOT), 256),
        (image.cluster_offset(EXFAT_ROOT + 1), cluster * 24),
    ]
}

fn fuzz_exfat(geometry: ExfatGeometry, seed: u64) {
    let mut image = exfat_seed(geometry);
    let ranges = exfat_ranges(&image);
    let mut rng = Rng::new(seed);
    let mut undo = Vec::new();
    for iteration in 0..fuzz::scaled(ITERATIONS) {
        mutate_ranges(&mut image.disk, &mut rng, &ranges, &mut undo);
        // Most boot edits only fail the checksum, so reseal half of them to
        // reach the layout checks.
        if iteration % 2 == 0 {
            image.reseal_boot(0);
        }
        let _ = detect(&image.disk);
        if let Ok(fs) = Exfat::mount(&image.disk) {
            exercise(&fs);
        }
        if iteration % 16 == 0 {
            let saved = image.disk.blocks.clone();
            if let Ok(mut fs) = Exfat::mount(&mut image.disk) {
                write_some(&mut fs);
            }
            image.disk.blocks = saved;
        }
        undo_all(&mut image.disk, &mut undo);
        image.reseal_boot(0);
    }
}

#[test]
fn exfat_survives_mutated_metadata() {
    run_constrained(|| fuzz_exfat(EXFAT_512, 71));
}

#[test]
fn exfat_with_multi_sector_clusters_survives_mutated_metadata() {
    run_constrained(|| fuzz_exfat(EXFAT_MULTI, 72));
}

#[test]
fn exfat_with_4k_sectors_survives_mutated_metadata() {
    run_constrained(|| fuzz_exfat(EXFAT_4K, 73));
}

/// A self-looping root whose every slot holds an entry. Without loop detection,
/// entries are added until the heap runs out, which aborts this binary.
#[test]
fn looping_directories_cannot_exhaust_the_heap() {
    run_constrained(|| {
        let mut image = FatImage::new(FAT_MULTI);
        let per_cluster = image.cluster_size() as usize / 32;
        for index in 0..per_cluster {
            let name = format!("F{:<7}TXT", index % 10_000);
            let name: [u8; 11] = name.as_bytes().try_into().unwrap();
            image.push_entries(FAT_ROOT, &[short_entry(&name, 0x20, 0, 0)]);
        }
        image.set_fat(FAT_ROOT, FAT_ROOT);
        let fs = Fat32::mount(&image.disk).unwrap();
        assert!(fs.is_damaged());
        assert_eq!(fs.node_count(), per_cluster + 1);
    });
}

/// More entries than the node budget allows, spread over directories that are
/// each within the format's limit: a clean error, not an abort.
#[test]
fn huge_volumes_hit_the_node_budget_not_the_heap_limit() {
    run_constrained(|| {
        let mut image = FatImage::new(FAT_MULTI);
        let per_cluster = image.cluster_size() as usize / 32;
        let mut entries = Vec::with_capacity(per_cluster);
        for index in 0..per_cluster {
            let name = format!("F{index:<7}TXT");
            let name: [u8; 11] = name.as_bytes().try_into().unwrap();
            entries.push(short_entry(&name, 0x20, 0, 0));
        }
        // 1000 directories of 128 entries take about 21 MiB of nodes and names,
        // over the 16 MiB budget. The sparse disk itself needs only about 5 MiB.
        for index in 0..1000u32 {
            let name = format!("D{index:<7}   ");
            let name: [u8; 11] = name.as_bytes().try_into().unwrap();
            let dir = image.add_dir(FAT_ROOT, &name);
            image.push_entries(dir, &entries);
        }
        match Fat32::mount(&image.disk) {
            Err(FsError::TooManyNodes) => {}
            Err(error) => panic!("{error}"),
            Ok(fs) => panic!("{} nodes fit the budget", fs.node_count()),
        }
    });
}

/// A file larger than the heap is legitimate. `read_to_vec` says so politely.
#[test]
fn files_larger_than_the_heap_are_an_error_not_an_abort() {
    run_constrained(|| {
        let mut image = FatImage::new(FAT_MULTI);
        let size = 200u32 * 1024 * 1024;
        image.push_entries(FAT_ROOT, &[short_entry(b"HUGE    BIN", 0x20, 3, size)]);
        let fs = Fat32::mount(&image.disk).unwrap();
        assert!(!fs.is_damaged());
        let node = fs.resolve("/HUGE.BIN").unwrap();
        assert_eq!(fs.size(node), size as usize);
        assert_eq!(fs.read_to_vec(node), Err(FsError::OutOfMemory));
    });
}
