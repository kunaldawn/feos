//! Geometries and damage that the small built-in fixtures can't show: clusters of
//! several sectors, 4 KiB sectors, up-case tables, cycles, truncated chains and
//! bad boot records.

mod support;

use feos_fs::{
    detect, BlockDevice, EntryKind, Exfat, Fat32, FileSystem, FsError, MountTable, NodeId,
    VolumeKind, WritableFileSystem, MAX_PATH_DEPTH,
};
use support::*;

fn pattern(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u32).wrapping_mul(31).wrapping_add(salt as u32) as u8)
        .collect()
}

/// Reads in `chunk`-sized steps, as `coreutils` does.
fn read_chunks(fs: &dyn FileSystem, node: NodeId, chunk: usize) -> Result<Vec<u8>, FsError> {
    let mut out = Vec::new();
    let mut buffer = vec![0u8; chunk];
    loop {
        let count = fs.try_read(node, out.len(), &mut buffer)?;
        if count == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buffer[..count]);
    }
}

const FAT_GEOMETRIES: [FatGeometry; 4] = [FAT_512, FAT_MULTI, FAT_4K, FAT_4K_ON_512];
const EXFAT_GEOMETRIES: [ExfatGeometry; 3] = [EXFAT_512, EXFAT_MULTI, EXFAT_4K];

#[test]
fn fat32_reads_and_writes_every_geometry() {
    for geometry in FAT_GEOMETRIES {
        let mut image = FatImage::new(geometry);
        let cluster = image.cluster_size() as usize;
        let data = pattern(cluster * 5 + 123, 7);
        image.add_long_file(FAT_ROOT, "Fragmented File.bin", b"FRAGME~1BIN", &data);
        let docs = image.add_dir(FAT_ROOT, b"DOCS       ");
        image.add_file(docs, b"NOTE    TXT", b"note");

        assert_eq!(detect(&image.disk), Ok(VolumeKind::Fat32));
        let mut fs = Fat32::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        let node = fs.resolve("/Fragmented File.bin").unwrap();
        assert_eq!(read_chunks(&fs, node, 777).unwrap(), data);
        assert_eq!(read_chunks(&fs, node, cluster).unwrap(), data);
        let mut piece = [0u8; 64];
        let middle = cluster * 3 - 10;
        assert_eq!(fs.read(node, middle, &mut piece), 64);
        assert_eq!(&piece[..], &data[middle..middle + 64]);

        let more = pattern(cluster * 2 + 5, 9);
        let file = fs
            .create_file(fs.resolve("/DOCS").unwrap(), "new file.dat")
            .unwrap();
        fs.write(file, 0, &more).unwrap();
        assert_eq!(fs.read_to_vec(file).unwrap(), more);
        fs.flush().unwrap();
        drop(fs);

        let fs = Fat32::mount(&mut image.disk).unwrap();
        let file = fs.resolve("/docs/new file.dat").unwrap();
        assert_eq!(fs.read_to_vec(file).unwrap(), more);
    }
}

#[test]
fn exfat_reads_and_writes_every_geometry() {
    for geometry in EXFAT_GEOMETRIES {
        let mut image = ExfatImage::new(geometry);
        let cluster = image.cluster_size() as usize;
        let chained = pattern(cluster * 4 + 99, 3);
        let contiguous = pattern(cluster * 3 + 1, 5);
        image.add_file(EXFAT_ROOT, "chained.bin", &chained);
        image.add_contiguous_file(EXFAT_ROOT, "straight.bin", &contiguous);
        let docs = image.add_dir(EXFAT_ROOT, "docs");
        image.add_file(docs, "note.txt", b"note");

        assert_eq!(detect(&image.disk), Ok(VolumeKind::Exfat));
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        let node = fs.resolve("/chained.bin").unwrap();
        assert_eq!(read_chunks(&fs, node, 777).unwrap(), chained);
        let node = fs.resolve("/straight.bin").unwrap();
        assert_eq!(read_chunks(&fs, node, 777).unwrap(), contiguous);

        let more = pattern(cluster + 5, 9);
        let file = fs.create_file(NodeId::ROOT, "new file.dat").unwrap();
        fs.write(file, 0, &more).unwrap();
        fs.flush().unwrap();
        drop(fs);

        let fs = Exfat::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        let file = fs.resolve("/new file.dat").unwrap();
        assert_eq!(fs.read_to_vec(file).unwrap(), more);
        assert_eq!(
            fs.read_to_vec(fs.resolve("/docs/NOTE.TXT").unwrap())
                .unwrap(),
            b"note"
        );
    }
}

// ---------------------------------------------------------------------------
// exFAT integrity
// ---------------------------------------------------------------------------

#[test]
fn exfat_uses_the_up_case_table_for_lookup_and_hash() {
    let mut image = ExfatImage::new(EXFAT_512);
    image.add_file(EXFAT_ROOT, "caf\u{e9}.txt", b"coffee");
    let mut fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(!fs.is_damaged());
    let node = fs.resolve("/CAF\u{c9}.TXT").unwrap();
    assert_eq!(fs.read_to_vec(node).unwrap(), b"coffee");
    assert_eq!(
        fs.create_file(NodeId::ROOT, "CAF\u{c9}.txt"),
        Err(FsError::NameExists)
    );
}

#[test]
fn exfat_skips_a_set_with_a_bad_checksum_and_flags_the_volume() {
    let mut image = ExfatImage::new(EXFAT_512);
    let bad = image.add_file(EXFAT_ROOT, "bad.txt", b"bad");
    image.add_file(EXFAT_ROOT, "good.txt", b"good");
    // A flipped name byte, in the name entry after primary and stream.
    image.disk.write_at(bad + 64 + 2, b"z");

    let mut fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert_eq!(fs.resolve("/bad.txt"), None);
    assert_eq!(fs.resolve("/zad.txt"), None);
    assert!(fs.resolve("/good.txt").is_some());
    // Reading still works, and writing doesn't.
    assert_eq!(
        fs.create_file(NodeId::ROOT, "new.txt"),
        Err(FsError::Corrupt)
    );
}

#[test]
fn exfat_skips_a_set_with_a_bad_name_hash() {
    let mut image = ExfatImage::new(EXFAT_512);
    let mut set = exfat_entry_set(ExfatEntry {
        name: "hash.txt",
        attributes: 0x20,
        cluster: 0,
        valid_length: 0,
        data_length: 0,
        contiguous: false,
    });
    set[1][4] ^= 0x55;
    let checksum = set_checksum(&set);
    set[0][2..4].copy_from_slice(&checksum.to_le_bytes());
    image.push_entries(EXFAT_ROOT, &set);

    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert_eq!(fs.resolve("/hash.txt"), None);
}

#[test]
fn exfat_rejects_an_up_case_table_with_a_bad_checksum() {
    let mut image = ExfatImage::new(EXFAT_512);
    image.add_file(EXFAT_ROOT, "plain.txt", b"data");
    let offset = image.upcase_checksum_offset;
    image.disk.write_at(offset, &[0xde, 0xad, 0xbe, 0xef]);
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert!(fs.resolve("/plain.txt").is_some());
}

#[test]
fn exfat_reads_zeros_past_valid_data_length() {
    let mut image = ExfatImage::new(EXFAT_512);
    let cluster = image.cluster_size() as usize;
    let secret = vec![0xaau8; cluster * 2];
    let start = image.write_chain(&secret);
    image.disk.write_at(image.cluster_offset(start), b"real");
    image.push_entries(
        EXFAT_ROOT,
        &exfat_entry_set(ExfatEntry {
            name: "prealloc.bin",
            attributes: 0x20,
            cluster: start,
            valid_length: 4,
            data_length: secret.len() as u64,
            contiguous: false,
        }),
    );

    let mut expected = vec![0u8; secret.len()];
    expected[..4].copy_from_slice(b"real");
    {
        let fs = Exfat::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/prealloc.bin").unwrap();
        assert_eq!(fs.size(node), secret.len());
        assert_eq!(fs.read_to_vec(node).unwrap(), expected);
        // Straddling the boundary, and entirely past it.
        let mut piece = [0xffu8; 8];
        assert_eq!(fs.read(node, 2, &mut piece), 8);
        assert_eq!(&piece, b"al\0\0\0\0\0\0");
        assert_eq!(fs.read(node, 600, &mut piece), 8);
        assert_eq!(piece, [0; 8]);
    }

    // Renaming must not promote the stale clusters to valid data.
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/prealloc.bin").unwrap();
        fs.rename(node, NodeId::ROOT, "moved.bin").unwrap();
    }
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        let node = fs.resolve("/moved.bin").unwrap();
        assert_eq!(fs.read_to_vec(node).unwrap(), expected);

        // Writing past the valid length zeroes the gap on disk, not just in reads.
        fs.write(node, 300, b"tail").unwrap();
        let mut after = expected.clone();
        after[300..304].copy_from_slice(b"tail");
        assert_eq!(fs.read_to_vec(node).unwrap(), after);
    }
    let start_offset = image.cluster_offset(start);
    let mut raw = [0xffu8; 16];
    image.disk.read_at(start_offset + 100, &mut raw);
    assert_eq!(raw, [0u8; 16]);
    let fs = Exfat::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/moved.bin").unwrap();
    assert_eq!(fs.read_to_vec(node).unwrap()[..4], *b"real");
}

#[test]
fn exfat_growing_a_file_does_not_expose_old_clusters() {
    let mut image = ExfatImage::new(EXFAT_512);
    let data = pattern(200, 1);
    let cluster = image.cluster_size() as usize;
    image.add_file(EXFAT_ROOT, "grow.bin", &data);
    let mut fs = Exfat::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/grow.bin").unwrap();
    fs.write(node, 0, &pattern(cluster, 2)).unwrap();
    fs.set_len(node, 100).unwrap();
    fs.set_len(node, cluster * 3).unwrap();
    let read = fs.read_to_vec(node).unwrap();
    assert_eq!(read[..100], pattern(100, 2)[..]);
    assert!(read[100..].iter().all(|&byte| byte == 0));
}

#[test]
fn exfat_treats_fat_entries_as_32_bits() {
    let mut image = ExfatImage::new(EXFAT_512);
    let cluster = image.cluster_size() as usize;
    let data = pattern(cluster * 3, 4);
    let offset = image.add_file(EXFAT_ROOT, "file.bin", &data);
    let mut raw = [0u8; 4];
    image.disk.read_at(offset + 32 + 20, &mut raw);
    let first = u32::from_le_bytes(raw);
    let second = image.disk.u32_at(image.fat_offset(first));
    // With only 28 bits read, this would be a valid link to `second`.
    image.set_fat(first, 0x1000_0000 | second);

    let fs = Exfat::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/file.bin").unwrap();
    assert_eq!(fs.read_to_vec(node), Err(FsError::Corrupt));
}

#[test]
fn exfat_entry_set_in_the_last_slot_can_be_written_and_removed() {
    let mut image = ExfatImage::new(EXFAT_512);
    // Bitmap and up-case entries take two slots of sixteen. Two names long enough
    // for two name entries take four each, leaving two three-entry sets to end
    // exactly on the last slot.
    assert_eq!(image.cluster_size(), 16 * 32);
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        fs.create_file(NodeId::ROOT, "a-name-of-eighteen1").unwrap();
        fs.create_file(NodeId::ROOT, "a-name-of-eighteen2").unwrap();
        fs.create_file(NodeId::ROOT, "x").unwrap();
        let last = fs.create_file(NodeId::ROOT, "y").unwrap();
        // Used to fail after the data was written, at the checksum update.
        fs.write(last, 0, b"data").unwrap();
        fs.rename(last, NodeId::ROOT, "renamed").unwrap();
        fs.flush().unwrap();
    }
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        let node = fs.resolve("/renamed").unwrap();
        assert_eq!(fs.read_to_vec(node).unwrap(), b"data");
        fs.remove(node).unwrap();
    }
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert_eq!(fs.resolve("/renamed"), None);
}

// ---------------------------------------------------------------------------
// Boot records
// ---------------------------------------------------------------------------

#[test]
fn fat32_rejects_bad_boot_sectors() {
    let cases: &[(usize, &[u8], FsError)] = &[
        (11, &0u16.to_le_bytes(), FsError::InvalidBootSector),
        (11, &513u16.to_le_bytes(), FsError::InvalidBootSector),
        (11, &8192u16.to_le_bytes(), FsError::InvalidBootSector),
        (13, &[0], FsError::InvalidBootSector),
        (13, &[3], FsError::InvalidBootSector),
        (14, &0u16.to_le_bytes(), FsError::InvalidBootSector),
        (16, &[0], FsError::InvalidBootSector),
        (17, &1u16.to_le_bytes(), FsError::UnsupportedVolume),
        (36, &0u32.to_le_bytes(), FsError::UnsupportedVolume),
        (36, &1u32.to_le_bytes(), FsError::Corrupt),
        (36, &0x7fff_ffffu32.to_le_bytes(), FsError::Corrupt),
        (32, &0x7fff_ffffu32.to_le_bytes(), FsError::Corrupt),
        (40, &0x0085u16.to_le_bytes(), FsError::InvalidBootSector),
        (44, &0u32.to_le_bytes(), FsError::Corrupt),
        (44, &u32::MAX.to_le_bytes(), FsError::Corrupt),
        (510, &[0x55, 0x00], FsError::InvalidBootSector),
    ];
    for &(offset, bytes, expected) in cases {
        let mut image = FatImage::new(FAT_512);
        image.disk.write_at(offset as u64, bytes);
        assert_eq!(
            Fat32::mount(&mut image.disk).err(),
            Some(expected),
            "offset {offset}"
        );
    }

    let mut image = FatImage::new(FAT_512);
    image.disk.write_at(32, &0u32.to_le_bytes());
    image.disk.write_at(19, &0u16.to_le_bytes());
    assert_eq!(
        Fat32::mount(&mut image.disk).err(),
        Some(FsError::InvalidBootSector)
    );
}

#[test]
fn exfat_rejects_bad_boot_regions() {
    let cases: &[(usize, &[u8], FsError)] = &[
        (108, &[8], FsError::InvalidBootSector),
        (108, &[13], FsError::InvalidBootSector),
        // Adding this to the sector shift used to overflow a u8.
        (109, &[0xff], FsError::InvalidBootSector),
        (109, &[17], FsError::InvalidBootSector),
        (110, &[0], FsError::InvalidBootSector),
        (110, &[3], FsError::InvalidBootSector),
        (72, &0u64.to_le_bytes(), FsError::InvalidBootSector),
        (72, &(1u64 << 40).to_le_bytes(), FsError::Corrupt),
        (80, &0u32.to_le_bytes(), FsError::InvalidBootSector),
        (84, &0u32.to_le_bytes(), FsError::InvalidBootSector),
        (88, &0u32.to_le_bytes(), FsError::InvalidBootSector),
        (92, &0u32.to_le_bytes(), FsError::InvalidBootSector),
        (84, &0x00ff_ffffu32.to_le_bytes(), FsError::Corrupt),
        (92, &0x7fff_ffffu32.to_le_bytes(), FsError::Corrupt),
        (96, &0u32.to_le_bytes(), FsError::Corrupt),
        (96, &u32::MAX.to_le_bytes(), FsError::Corrupt),
    ];
    for &(offset, bytes, expected) in cases {
        let mut image = ExfatImage::new(EXFAT_512);
        // Both copies, so the fallback can't rescue the mount.
        for base in [0u64, 12] {
            image.disk.write_at(base * 512 + offset as u64, bytes);
            image.reseal_boot(base);
        }
        assert_eq!(
            Exfat::mount(&mut image.disk).err(),
            Some(expected),
            "offset {offset}"
        );
    }
}

#[test]
fn exfat_two_fats_must_fit_before_the_heap() {
    let mut image = ExfatImage::new(EXFAT_MULTI);
    let mut fat_length = [0u8; 4];
    image.disk.read_at(84, &mut fat_length);
    let fat_length = u32::from_le_bytes(fat_length);
    let mut heap = [0u8; 4];
    image.disk.read_at(88, &mut heap);
    let heap = u32::from_le_bytes(heap);
    // Make the second FAT overlap the cluster heap.
    let fat_offset = heap - fat_length;
    for base in [0u64, 12] {
        image
            .disk
            .write_at(base * 512 + 80, &fat_offset.to_le_bytes());
        image.disk.write_at(base * 512 + 110, &[2]);
        image.reseal_boot(base);
    }
    assert_eq!(Exfat::mount(&mut image.disk).err(), Some(FsError::Corrupt));
}

#[test]
fn exfat_falls_back_to_the_backup_boot_region() {
    for geometry in EXFAT_GEOMETRIES {
        let mut image = ExfatImage::new(geometry);
        image.add_file(EXFAT_ROOT, "kept.txt", b"kept");
        // A damaged checksum, then a wiped first sector.
        image.disk.write_at(50, &[0xff]);
        {
            let mut fs = Exfat::mount(&mut image.disk).unwrap();
            assert!(fs.is_damaged());
            let node = fs.resolve("/kept.txt").unwrap();
            assert_eq!(fs.read_to_vec(node).unwrap(), b"kept");
            assert_eq!(fs.remove(node), Err(FsError::Corrupt));
        }
        image.disk.write_at(0, &[0u8; 512]);
        assert_eq!(detect(&image.disk), Ok(VolumeKind::Exfat));
        assert!(Exfat::mount(&mut image.disk).unwrap().is_damaged());
    }
}

#[test]
fn exfat_bad_backup_region_is_not_fatal_or_written() {
    let mut image = ExfatImage::new(EXFAT_512);
    let backup = 12 * 512;
    image.disk.write_at(backup + 50, &[0xff]);
    let mut before = [0u8; 2];
    image.disk.read_at(backup + 106, &mut before);
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        assert!(!fs.is_damaged());
        fs.create_file(NodeId::ROOT, "new").unwrap();
    }
    let mut after = [0u8; 2];
    image.disk.read_at(backup + 106, &mut after);
    assert_eq!(before, after);
}

#[test]
fn exfat_reads_the_active_fat() {
    let mut image = ExfatImage::new(EXFAT_MULTI);
    let cluster = image.cluster_size() as usize;
    let data = pattern(cluster * 2 + 1, 6);
    let offset = image.add_file(EXFAT_ROOT, "file.bin", &data);
    let mut raw = [0u8; 4];
    image.disk.read_at(offset + 32 + 20, &mut raw);
    let first = u32::from_le_bytes(raw);

    // The heap's alignment to a cluster boundary leaves room for a second FAT.
    let mut fat_length = [0u8; 4];
    image.disk.read_at(84, &mut fat_length);
    let fat_length = u32::from_le_bytes(fat_length);
    let mut heap = [0u8; 4];
    image.disk.read_at(88, &mut heap);
    let room = u32::from_le_bytes(heap) - 24;
    assert!(room >= 2 * fat_length);
    let fat_bytes = fat_length as u64 * 512;
    // Copy the FAT into the second slot and damage the first.
    let main = image.fat_offset(0);
    let mut fat = vec![0u8; fat_bytes as usize];
    image.disk.read_at(main, &mut fat);
    image.disk.write_at(main + fat_bytes, &fat);
    image
        .disk
        .write_at(image.fat_offset(first), &0x5u32.to_le_bytes());
    for base in [0u64, 12] {
        image.disk.write_at(base * 512 + 110, &[2]);
        image.disk.write_at(base * 512 + 106, &1u16.to_le_bytes());
        image.reseal_boot(base);
    }
    let fs = Exfat::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/file.bin").unwrap();
    assert_eq!(fs.read_to_vec(node).unwrap(), data);
}

// ---------------------------------------------------------------------------
// Chains
// ---------------------------------------------------------------------------

/// Returns the image and the file's first three clusters.
fn fat_with_file() -> (FatImage, [u32; 3], Vec<u8>) {
    let mut image = FatImage::new(FAT_512);
    let data = pattern(image.cluster_size() as usize * 3, 8);
    let first = image.add_file(FAT_ROOT, b"FILE    BIN", &data);
    let second = image.disk.u32_at(image.fat_offset(0, first));
    let third = image.disk.u32_at(image.fat_offset(0, second));
    (image, [first, second, third], data)
}

#[test]
fn fat32_truncated_and_broken_chains_are_errors() {
    for value in [0u32, FAT_END, 0x0fff_fff7, 1, 0x0fff_0000] {
        let (mut image, clusters, _) = fat_with_file();
        image.set_fat(clusters[1], value);
        let fs = Fat32::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/FILE.BIN").unwrap();
        let mut buffer = [0u8; 16];
        // Even a read of an intact first cluster fails: the file is corrupt.
        assert_eq!(
            fs.try_read(node, 0, &mut buffer),
            Err(FsError::Corrupt),
            "{value:#x}"
        );
        assert_eq!(fs.read(node, 0, &mut buffer), 0);
        assert_eq!(fs.read_to_vec(node), Err(FsError::Corrupt));
    }
}

#[test]
fn fat32_cycles_are_errors_not_repeated_data() {
    for (from, to) in [(0usize, 0usize), (1, 0), (1, 1), (2, 0), (2, 1), (2, 2)] {
        let (mut image, clusters, _) = fat_with_file();
        image.set_fat(clusters[from], clusters[to]);
        let fs = Fat32::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/FILE.BIN").unwrap();
        assert_eq!(fs.read_to_vec(node), Err(FsError::Corrupt), "{from}->{to}");
    }
}

#[test]
fn exfat_truncated_chains_and_cycles_are_errors() {
    let mut image = ExfatImage::new(EXFAT_512);
    let data = pattern(image.cluster_size() as usize * 3, 8);
    let offset = image.add_file(EXFAT_ROOT, "file.bin", &data);
    let mut raw = [0u8; 4];
    image.disk.read_at(offset + 32 + 20, &mut raw);
    let first = u32::from_le_bytes(raw);
    let second = image.disk.u32_at(image.fat_offset(first));

    for value in [EXFAT_END, 0, 0xffff_fff7, 1, first, second] {
        image.set_fat(second, value);
        let fs = Exfat::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/file.bin").unwrap();
        assert_eq!(fs.read_to_vec(node), Err(FsError::Corrupt), "{value:#x}");
    }
}

#[test]
fn longer_chains_than_the_size_are_fine() {
    let (mut image, _, data) = fat_with_file();
    image.disk.write_at(
        image.cluster_offset(FAT_ROOT) + 28,
        &((data.len() as u32) - 600).to_le_bytes(),
    );
    let fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/FILE.BIN").unwrap();
    assert_eq!(fs.read_to_vec(node).unwrap(), data[..data.len() - 600]);
}

#[test]
fn chain_verdict_resets_when_the_file_changes() {
    let (mut image, clusters, _) = fat_with_file();
    image.set_fat(clusters[1], FAT_END);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/FILE.BIN").unwrap();
    assert_eq!(fs.read_to_vec(node), Err(FsError::Corrupt));
    // Cutting the file to what the chain really holds makes it consistent.
    fs.set_len(node, 100).unwrap();
    assert_eq!(fs.read_to_vec(node).unwrap().len(), 100);
}

#[test]
fn impossible_file_sizes_are_skipped_at_mount() {
    let mut image = FatImage::new(FAT_512);
    image.push_entries(FAT_ROOT, &[short_entry(b"HUGE    BIN", 0x20, 3, u32::MAX)]);
    image.push_entries(
        FAT_ROOT,
        &[short_entry(b"WILD    BIN", 0x20, u32::MAX, 512)],
    );
    image.add_file(FAT_ROOT, b"OK      TXT", b"ok");
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert_eq!(fs.resolve("/HUGE.BIN"), None);
    assert_eq!(fs.resolve("/WILD.BIN"), None);
    assert!(fs.resolve("/OK.TXT").is_some());

    let mut image = ExfatImage::new(EXFAT_512);
    for (name, cluster, length, contiguous) in [
        ("huge", 5u32, 1u64 << 62, false),
        ("wild", u32::MAX, 512, false),
        ("zero", 0, 512, false),
        ("past", 500, 4096 * 4096, true),
    ] {
        image.push_entries(
            EXFAT_ROOT,
            &exfat_entry_set(ExfatEntry {
                name,
                attributes: 0x20,
                cluster,
                valid_length: length,
                data_length: length,
                contiguous,
            }),
        );
    }
    image.add_file(EXFAT_ROOT, "ok.txt", b"ok");
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    for name in ["/huge", "/wild", "/zero", "/past"] {
        assert_eq!(fs.resolve(name), None, "{name}");
    }
    assert!(fs.resolve("/ok.txt").is_some());
}

#[test]
fn sequential_reads_do_not_walk_the_chain_again() {
    let mut image = FatImage::new(FAT_512);
    // 8000 clusters use 63 FAT sectors, more than the sector cache holds, so
    // restarting each read from the first cluster would thrash it.
    let data = pattern(512 * 8000, 2);
    image.add_file(FAT_ROOT, b"BIG     BIN", &data);
    let probe = image.disk.probe.clone();
    let fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.resolve("/BIG.BIN").unwrap();

    let mut buffer = vec![0u8; 4096];
    assert_eq!(fs.try_read(node, 0, &mut buffer), Ok(4096));
    let start = probe.read_count();
    let mut offset = 4096usize;
    let mut out = data[..4096].to_vec();
    while offset < data.len() {
        let count = fs.try_read(node, offset, &mut buffer).unwrap();
        out.extend_from_slice(&buffer[..count]);
        offset += count;
    }
    assert_eq!(out, data);
    let reads = probe.read_count() - start;
    // One read per data sector, plus each FAT sector about once.
    assert!(reads < 8000 + 2 * 63, "{reads} device reads");

    // Backing up works too: the hint is only used going forward.
    let mut piece = [0u8; 32];
    assert_eq!(fs.try_read(node, 1000, &mut piece), Ok(32));
    assert_eq!(&piece[..], &data[1000..1032]);
}

#[test]
fn metadata_reads_come_from_the_sector_cache() {
    let mut image = FatImage::new(FAT_512);
    image.add_file(FAT_ROOT, b"A       TXT", b"a");
    let probe = image.disk.probe.clone();
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.create_file(NodeId::ROOT, "many.bin").unwrap();
    let before = probe.read_count();
    // Each cluster reads FAT entries, all in a sector or two.
    fs.write(node, 0, &pattern(512 * 100, 3)).unwrap();
    let reads = probe.read_count() - before;
    assert!(reads < 100, "{reads} device reads");
    assert_eq!(fs.read_to_vec(node).unwrap(), pattern(512 * 100, 3));
}

#[test]
fn cache_stays_coherent_with_writes_of_every_shape() {
    let mut image = FatImage::new(FAT_4K_ON_512);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let file = fs.create_file(NodeId::ROOT, "data.bin").unwrap();
    let mut model = vec![0u8; 0];
    let cluster = 8192usize;
    for (offset, len, salt) in [
        (0, 100, 1u8),
        (50, 20, 2),
        (cluster - 10, 20, 3),
        (cluster * 2, cluster, 4),
        (10, 1, 5),
        (cluster * 2 + 1, 2, 6),
    ] {
        let piece = pattern(len, salt);
        fs.write(file, offset, &piece).unwrap();
        if model.len() < offset + len {
            model.resize(offset + len, 0);
        }
        model[offset..offset + len].copy_from_slice(&piece);
        assert_eq!(fs.read_to_vec(file).unwrap(), model);
    }
}

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

#[test]
fn fat32_directory_loops_are_skipped_not_followed() {
    // The root points at itself.
    let mut image = FatImage::new(FAT_512);
    image.add_file(FAT_ROOT, b"FILE    TXT", b"x");
    image.set_fat(FAT_ROOT, FAT_ROOT);
    // Fill the cluster so the walk can't stop at an end marker.
    for index in 0..15u32 {
        let name = format!("F{index:<7}TXT");
        let name: [u8; 11] = name.as_bytes().try_into().unwrap();
        image.push_entries(FAT_ROOT, &[short_entry(&name, 0x20, 0, 0)]);
    }
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert_eq!(fs.child_count(NodeId::ROOT), 16);

    // A subdirectory that links back to an earlier cluster of itself.
    let mut image = FatImage::new(FAT_512);
    let sub = image.add_dir(FAT_ROOT, b"SUB        ");
    for index in 0..40u32 {
        let name = format!("F{index:<7}TXT");
        let name: [u8; 11] = name.as_bytes().try_into().unwrap();
        image.push_entries(sub, &[short_entry(&name, 0x20, 0, 0)]);
    }
    let second = image.disk.u32_at(image.fat_offset(0, sub));
    image.set_fat(second, sub);
    image.add_file(FAT_ROOT, b"AFTER   TXT", b"x");
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert!(fs.resolve("/AFTER.TXT").is_some());
    assert!(fs.child_count(fs.resolve("/SUB").unwrap()) <= 32);
}

#[test]
fn fat32_directory_cycle_through_a_parent_terminates() {
    let mut image = FatImage::new(FAT_512);
    let child = image.add_dir(FAT_ROOT, b"CHILD      ");
    // A child that claims the root as a subdirectory.
    image.push_entries(child, &[short_entry(b"LOOP       ", 0x10, FAT_ROOT, 0)]);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    let looped = fs.resolve("/CHILD/LOOP").unwrap();
    assert_eq!(fs.child_count(looped), 0);
}

#[test]
fn one_bad_directory_does_not_fail_the_mount() {
    let mut image = FatImage::new(FAT_512);
    let bad = image.add_dir(FAT_ROOT, b"BAD        ");
    for index in 0..20u32 {
        let name = format!("F{index:<7}TXT");
        let name: [u8; 11] = name.as_bytes().try_into().unwrap();
        image.push_entries(bad, &[short_entry(&name, 0x20, 0, 0)]);
    }
    image.set_fat(bad, 0x0fff_0000);
    image.add_file(FAT_ROOT, b"GOOD    TXT", b"good");
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    let good = fs.resolve("/GOOD.TXT").unwrap();
    assert_eq!(fs.read_to_vec(good).unwrap(), b"good");
    assert_eq!(fs.write(good, 0, b"x"), Err(FsError::Corrupt));

    let mut image = ExfatImage::new(EXFAT_512);
    let bad = image.add_dir(EXFAT_ROOT, "bad");
    for index in 0..11 {
        image.add_file(bad, &format!("file{index}"), b"");
    }
    let second = image.disk.u32_at(image.fat_offset(bad));
    assert_ne!(image.disk.u32_at(image.fat_offset(second)), EXFAT_END);
    image.set_fat(second, second);
    image.add_file(EXFAT_ROOT, "good.txt", b"good");
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert!(fs.resolve("/good.txt").is_some());
}

#[test]
fn exfat_directory_loop_is_skipped() {
    let mut image = ExfatImage::new(EXFAT_512);
    image.add_file(EXFAT_ROOT, "kept.txt", b"kept");
    // Fill the root's first two clusters, so that the walk reaches the link.
    for index in 0..11 {
        image.add_file(EXFAT_ROOT, &format!("more{index}"), b"");
    }
    let next = image.disk.u32_at(image.fat_offset(EXFAT_ROOT));
    assert_ne!(next, EXFAT_END);
    assert_ne!(image.disk.u32_at(image.fat_offset(next)), EXFAT_END);
    image.set_fat(next, EXFAT_ROOT);
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert!(fs.resolve("/kept.txt").is_some());
}

#[test]
fn deep_trees_stop_at_the_depth_limit() {
    let mut image = FatImage::new(FAT_512);
    let mut dir = FAT_ROOT;
    for _ in 0..MAX_PATH_DEPTH + 8 {
        dir = image.add_dir(dir, b"D          ");
    }
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    let mut node = NodeId::ROOT;
    let mut depth = 0;
    while let Some(child) = fs.child_at(node, 0) {
        node = child;
        depth += 1;
    }
    assert_eq!(depth, MAX_PATH_DEPTH);
    let mut path = [0u8; 256];
    assert!(fs.path(node, &mut path).is_some());

    let mut image = ExfatImage::new(ExfatGeometry {
        clusters: 2048,
        ..EXFAT_512
    });
    let mut dir = EXFAT_ROOT;
    for _ in 0..MAX_PATH_DEPTH + 8 {
        dir = image.add_dir(dir, "d");
    }
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(fs.is_damaged());
    assert!(fs.node_count() <= MAX_PATH_DEPTH + 1);
}

#[test]
fn changes_cannot_nest_past_the_depth_limit() {
    let mut image = FatImage::new(FAT_512);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let mut dir = NodeId::ROOT;
    for _ in 0..MAX_PATH_DEPTH - 1 {
        dir = fs.create_dir(dir, "d").unwrap();
    }
    assert_eq!(fs.create_dir(dir, "deeper"), Err(FsError::TooDeep));
    // A file fits at the limit, and doesn't need a level for children.
    let file = fs.create_file(dir, "file").unwrap();
    let mut path = [0u8; 256];
    assert!(fs.path(file, &mut path).is_some());

    // A rename can't carry a subtree past the limit either.
    let branch = fs.create_dir(NodeId::ROOT, "branch").unwrap();
    fs.create_dir(branch, "leaf").unwrap();
    let parent = fs.parent(dir).unwrap();
    assert_eq!(fs.rename(branch, parent, "moved"), Err(FsError::TooDeep));
    fs.flush().unwrap();
    drop(fs);
    assert!(!Fat32::mount(&mut image.disk).unwrap().is_damaged());
}

#[test]
fn node_slots_are_reused() {
    let mut image = FatImage::new(FAT_512);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let keep = fs.create_file(NodeId::ROOT, "keep").unwrap();
    let limit = fs.node_id_limit();
    for _ in 0..200 {
        let node = fs.create_file(NodeId::ROOT, "temp").unwrap();
        fs.remove(node).unwrap();
    }
    assert!(fs.node_id_limit() <= limit + 1);
    assert_eq!(fs.resolve("/keep"), Some(keep));
    assert_eq!(fs.node_count(), 2);
}

#[test]
fn mount_table_sees_nodes_above_the_live_count() {
    let mut image = FatImage::new(FAT_512);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let first = fs.create_file(NodeId::ROOT, "first").unwrap();
    fs.create_file(NodeId::ROOT, "second").unwrap();
    let third = fs.create_file(NodeId::ROOT, "third").unwrap();
    fs.write(third, 0, b"third").unwrap();
    fs.remove(first).unwrap();
    // Live count 3, but the highest ID is still 3 (one past is 4).
    assert!(fs.node_id_limit() > fs.node_count());

    let mut table = MountTable::new();
    table.mount("vol", Box::new(fs)).unwrap();
    let node = table.resolve("/vol/third").unwrap();
    assert_eq!(table.read_to_vec(node).unwrap(), b"third");
    let directory = table.resolve("/vol").unwrap();
    assert_eq!(table.child_count(directory), 2);
    for index in 0..table.child_count(directory) {
        assert!(table.child_at(directory, index).is_some());
    }
    assert!(!table.is_damaged());
    assert_eq!(
        table.try_read(directory, 0, &mut [0u8; 4]),
        Err(FsError::NotFile)
    );
    assert_eq!(
        table.try_read(NodeId(9999), 0, &mut [0u8; 4]),
        Err(FsError::NotFound)
    );
}

#[test]
fn static_fs_gets_the_default_try_read() {
    use feos_fs::{StaticFs, StaticNode};
    static NODES: &[StaticNode] = &[
        StaticNode::dir("/", None, &[1]),
        StaticNode::file("a", Some(0), b"abc"),
    ];
    let fs = StaticFs::new(NODES);
    let mut out = [0u8; 2];
    assert_eq!(fs.try_read(NodeId(1), 1, &mut out), Ok(2));
    assert_eq!(fs.try_read(NodeId(0), 0, &mut out), Err(FsError::NotFile));
    assert_eq!(fs.try_read(NodeId(9), 0, &mut out), Err(FsError::NotFound));
    assert_eq!(fs.read_to_vec(NodeId(1)).unwrap(), b"abc");
    assert_eq!(fs.read_to_vec(NodeId(9)), Err(FsError::NotFound));
    assert_eq!(fs.kind(NodeId(1)), Some(EntryKind::File));
}

// ---------------------------------------------------------------------------
// Running out of space
// ---------------------------------------------------------------------------

#[test]
fn fat32_out_of_space_frees_a_new_chain() {
    let mut image = FatImage::new(FAT_512);
    image.exhaust_except(3);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let file = fs.create_file(NodeId::ROOT, "too-big").unwrap();
    let too_big = pattern(512 * 4, 1);
    assert_eq!(fs.write(file, 0, &too_big), Err(FsError::NoSpace));
    assert_eq!(fs.size(file), 0);
    // Everything came back, so exactly the free amount still fits.
    let fits = pattern(512 * 3, 2);
    fs.write(file, 0, &fits).unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap(), fits);
}

#[test]
fn fat32_out_of_space_trims_an_extended_chain() {
    let mut image = FatImage::new(FAT_512);
    image.exhaust_except(3);
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let file = fs.create_file(NodeId::ROOT, "grow").unwrap();
    let start = pattern(512, 3);
    fs.write(file, 0, &start).unwrap();
    assert_eq!(
        fs.write(file, 512, &pattern(512 * 3, 4)),
        Err(FsError::NoSpace)
    );
    assert_eq!(fs.read_to_vec(file).unwrap(), start);
    let more = pattern(512 * 2, 5);
    fs.write(file, 512, &more).unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap()[512..], more[..]);
}

#[test]
fn exfat_out_of_space_returns_clusters() {
    let geometry = ExfatGeometry {
        clusters: 16,
        ..EXFAT_512
    };
    let mut image = ExfatImage::new(geometry);
    let mut fs = Exfat::mount(&mut image.disk).unwrap();
    let file = fs.create_file(NodeId::ROOT, "too-big").unwrap();
    // Three clusters hold the bitmap, up-case table and root.
    let free = 13;
    assert_eq!(
        fs.write(file, 0, &pattern(512 * (free + 1), 1)),
        Err(FsError::NoSpace)
    );
    let fits = pattern(512 * free, 2);
    fs.write(file, 0, &fits).unwrap();
    assert_eq!(fs.read_to_vec(file).unwrap(), fits);

    // Same for a contiguous stream that had to move to a chain.
    assert_eq!(fs.write(file, 512 * free, &[1u8; 1]), Err(FsError::NoSpace));
    assert_eq!(fs.read_to_vec(file).unwrap(), fits);
}

#[test]
fn exfat_failed_contiguous_extension_keeps_the_stream_intact() {
    let geometry = ExfatGeometry {
        clusters: 16,
        ..EXFAT_512
    };
    let mut image = ExfatImage::new(geometry);
    let data = pattern(512 * 2, 7);
    image.add_contiguous_file(EXFAT_ROOT, "straight", &data);
    // Block the clusters right after the stream, then leave only two free.
    let offset = image.write_contiguous(&[0u8; 512]);
    assert!(offset > 0);
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        let hog = fs.create_file(NodeId::ROOT, "hog").unwrap();
        fs.write(hog, 0, &pattern(512 * 7, 1)).unwrap();
        let node = fs.resolve("/straight").unwrap();
        assert_eq!(
            fs.write(node, 512 * 2, &pattern(512 * 4, 2)),
            Err(FsError::NoSpace)
        );
        assert_eq!(fs.read_to_vec(node).unwrap(), data);
        // The freed clusters are usable.
        let small = fs.create_file(NodeId::ROOT, "small").unwrap();
        fs.write(small, 0, &pattern(512, 3)).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Rename and remove
// ---------------------------------------------------------------------------

#[test]
fn fat32_rename_changes_case_and_keeps_metadata() {
    let mut image = FatImage::new(FAT_512);
    let mut entry = short_entry(b"README  TXT", 0x23, 0, 0); // read-only, hidden, archive
    entry[13..20].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
    entry[22..26].copy_from_slice(&[8, 9, 10, 11]);
    image.push_entries(FAT_ROOT, &[entry]);
    {
        let mut fs = Fat32::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/README.TXT").unwrap();
        fs.rename(node, NodeId::ROOT, "ReadMe.txt").unwrap();
        assert_eq!(fs.name(node), Some("ReadMe.txt"));
    }
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert_eq!(fs.child_count(NodeId::ROOT), 1);
    let node = fs.child_at(NodeId::ROOT, 0).unwrap();
    assert_eq!(fs.name(node), Some("ReadMe.txt"));
    drop(fs);

    // Find the live short entry and compare its metadata.
    let root = image.cluster_offset(FAT_ROOT);
    let mut found = None;
    for slot in 0..16 {
        let mut raw = [0u8; 32];
        image.disk.read_at(root + slot * 32, &mut raw);
        if raw[0] != 0xe5 && raw[0] != 0 && raw[11] & 0x3f != 0x0f {
            found = Some(raw);
        }
    }
    let found = found.unwrap();
    assert_eq!(found[11], 0x23);
    assert_eq!(found[13..20], [1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(found[22..26], [8, 9, 10, 11]);
}

#[test]
fn exact_same_name_rename_is_a_no_op_and_clashes_are_errors() {
    let mut image = FatImage::new(FAT_512);
    image.add_file(FAT_ROOT, b"A       TXT", b"a");
    image.add_file(FAT_ROOT, b"B       TXT", b"b");
    let mut fs = Fat32::mount(&mut image.disk).unwrap();
    let a = fs.resolve("/A.TXT").unwrap();
    fs.rename(a, NodeId::ROOT, "A.TXT").unwrap();
    assert_eq!(
        fs.rename(a, NodeId::ROOT, "b.txt"),
        Err(FsError::NameExists)
    );
}

#[test]
fn exfat_rename_changes_case_and_keeps_metadata() {
    let mut image = ExfatImage::new(EXFAT_512);
    let data = pattern(600, 4);
    let cluster = image.write_chain(&data);
    let offset = image.push_entries(
        EXFAT_ROOT,
        &exfat_entry_set(ExfatEntry {
            name: "readme.txt",
            attributes: 0x23,
            cluster,
            valid_length: data.len() as u64,
            data_length: data.len() as u64,
            contiguous: false,
        }),
    );
    let mut before = [0u8; 32];
    image.disk.read_at(offset, &mut before);
    {
        let mut fs = Exfat::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/readme.txt").unwrap();
        fs.rename(node, NodeId::ROOT, "README.txt").unwrap();
        assert_eq!(fs.name(node), Some("README.txt"));
    }
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(!fs.is_damaged());
    let node = fs.child_at(NodeId::ROOT, 0).unwrap();
    assert_eq!(fs.name(node), Some("README.txt"));
    assert_eq!(fs.read_to_vec(node).unwrap(), data);
    drop(fs);

    let root = image.cluster_offset(EXFAT_ROOT);
    let mut after = None;
    for slot in 0..16 {
        let mut raw = [0u8; 32];
        image.disk.read_at(root + slot * 32, &mut raw);
        if raw[0] == 0x85 {
            after = Some(raw);
        }
    }
    let after = after.unwrap();
    // Attributes and all three timestamps.
    assert_eq!(after[4..6], before[4..6]);
    assert_eq!(after[8..20], before[8..20]);
}

#[test]
fn removal_deletes_the_entry_before_freeing_the_chain() {
    let (mut image, clusters, _) = fat_with_file();
    let probe = image.disk.probe.clone();
    let root = image.cluster_offset(FAT_ROOT);
    let fat = image.fat_offset(0, clusters[0]);
    {
        let mut fs = Fat32::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/FILE.BIN").unwrap();
        probe.fail_writes_after(0);
        assert_eq!(fs.remove(node), Err(FsError::Io));
    }
    probe.fail_writes_after(-1);
    // The entry failed first, so the chain must be untouched.
    assert_ne!(image.disk.u32_at(fat), 0);
    let mut raw = [0u8; 1];
    image.disk.read_at(root, &mut raw);
    assert_eq!(raw[0], b'F');
}

// ---------------------------------------------------------------------------
// FAT32 details
// ---------------------------------------------------------------------------

#[test]
fn fat32_uses_the_active_fat_when_mirroring_is_off() {
    let (mut image, clusters, data) = fat_with_file();
    // FAT 1 is live and correct. FAT 0 is stale and says the chain ends early.
    let stale = image.fat_offset(0, clusters[0]);
    image.disk.write_at(stale, &FAT_END.to_le_bytes());
    image.disk.write_at(40, &0x0081u16.to_le_bytes());
    {
        let mut fs = Fat32::mount(&mut image.disk).unwrap();
        let node = fs.resolve("/FILE.BIN").unwrap();
        assert_eq!(fs.read_to_vec(node).unwrap(), data);

        // Changes go to the live FAT only.
        let new = fs.create_file(NodeId::ROOT, "new.txt").unwrap();
        fs.write(new, 0, b"new").unwrap();
    }
    assert_eq!(image.disk.u32_at(stale), FAT_END);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    let new = fs.resolve("/new.txt").unwrap();
    assert_eq!(fs.read_to_vec(new).unwrap(), b"new");
}

#[test]
fn fat32_writes_preserve_reserved_fat_bits() {
    let mut image = FatImage::new(FAT_512);
    // Mark a free entry's reserved nibble, which a free scan must look past.
    let target = image.next_entry_offset(FAT_ROOT);
    assert!(target > 0);
    let cluster = 3u32;
    let offset = image.fat_offset(0, cluster);
    image.disk.write_at(offset, &0xa000_0000u32.to_le_bytes());
    {
        let mut fs = Fat32::mount(&mut image.disk).unwrap();
        let file = fs.create_file(NodeId::ROOT, "x").unwrap();
        fs.write(file, 0, b"x").unwrap();
    }
    let value = image.disk.u32_at(offset);
    assert_eq!(value & 0xf000_0000, 0xa000_0000);
    assert_ne!(value & 0x0fff_ffff, 0);
}

#[test]
fn fat32_rejects_long_names_with_bad_sequences() {
    // A gap in the sequence numbers, with a matching checksum.
    let short = short_entry(b"SHORT   TXT", 0x20, 0, 0);
    let mut entries = lfn_entries("a name long enough for three entries here!", &short);
    assert!(entries.len() >= 3);
    entries.remove(1);
    entries.push(short);
    let mut image = FatImage::new(FAT_512);
    image.push_entries(FAT_ROOT, &entries);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.resolve("/SHORT.TXT").is_some());

    // Starting partway through, with the final entry's marker but no first.
    let short = short_entry(b"OTHER   TXT", 0x20, 0, 0);
    let mut entries = lfn_entries("a name long enough for three entries here!", &short);
    entries.pop();
    entries.push(short);
    let mut image = FatImage::new(FAT_512);
    image.push_entries(FAT_ROOT, &entries);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    assert!(fs.resolve("/OTHER.TXT").is_some());
}

#[test]
fn names_are_kept_raw_with_a_path_separator_the_only_exception() {
    let short = short_entry(b"ESCAPE  TXT", 0x20, 0, 0);
    let mut entries = lfn_entries("bad\u{1b}[2Jname\u{7f}\u{9b}.txt", &short);
    entries.push(short);
    let mut image = FatImage::new(FAT_512);
    image.push_entries(FAT_ROOT, &entries);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.child_at(NodeId::ROOT, 0).unwrap();
    assert_eq!(fs.name(node), Some("bad\u{1b}[2Jname\u{7f}\u{9b}.txt"));
    assert_eq!(fs.resolve("/bad\u{1b}[2Jname\u{7f}\u{9b}.txt"), Some(node));

    let short = short_entry(b"SLASH   TXT", 0x20, 0, 0);
    let mut entries = lfn_entries("a/b\\c.txt", &short);
    entries.push(short);
    let mut image = FatImage::new(FAT_512);
    image.push_entries(FAT_ROOT, &entries);
    let fs = Fat32::mount(&mut image.disk).unwrap();
    let node = fs.child_at(NodeId::ROOT, 0).unwrap();
    assert_eq!(fs.name(node), Some("a_b\\c.txt"));

    let mut image = ExfatImage::new(EXFAT_512);
    image.add_file(EXFAT_ROOT, "x\u{1b}y", b"");
    let fs = Exfat::mount(&mut image.disk).unwrap();
    assert!(!fs.is_damaged());
    let node = fs.child_at(NodeId::ROOT, 0).unwrap();
    assert_eq!(fs.name(node), Some("x\u{1b}y"));
}

#[test]
fn device_block_size_is_reported_through_probes() {
    let image = FatImage::new(FAT_4K);
    assert_eq!(image.disk.block_size(), 4096);
}
