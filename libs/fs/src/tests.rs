//! Self-contained FAT32 and exFAT fixtures.
//!
//! The images are built in memory so the test suite has no dependency on
//! `mkfs`, loop devices or root privileges.

use crate::*;

const SECTOR: usize = 512;

// ---------------------------------------------------------------------------
// FAT32 fixture
// ---------------------------------------------------------------------------

const FAT_TOTAL_SECTORS: u64 = 66_600;
const FAT_RESERVED_SECTORS: u64 = 32;
const FAT_COPIES: u64 = 2;
const FAT_ROOT_CLUSTER: u32 = 2;
const FAT_HELLO_CLUSTER: u32 = 3;
const FAT_DOCS_CLUSTER: u32 = 4;
const FAT_NOTES_CLUSTER: u32 = 5;
const FAT_LONG_CLUSTER: u32 = 6;
const FAT_BIG_CLUSTER: u32 = 7;

fn fat_layout() -> (u64, u64) {
    let mut fat_sectors = 512u64;
    loop {
        let clusters = FAT_TOTAL_SECTORS - FAT_RESERVED_SECTORS - FAT_COPIES * fat_sectors;
        let needed = ((clusters + 2) * 4 + SECTOR as u64 - 1) / SECTOR as u64;
        if needed <= fat_sectors {
            return (fat_sectors, clusters);
        }
        fat_sectors = needed;
    }
}

fn fat_cluster_offset(fat_sectors: u64, cluster: u32) -> usize {
    let data_start = (FAT_RESERVED_SECTORS + FAT_COPIES * fat_sectors) * SECTOR as u64;
    (data_start + (cluster as u64 - 2) * SECTOR as u64) as usize
}

fn fat_set_entry(data: &mut [u8], fat_sectors: u64, cluster: u32, value: u32) {
    let first = FAT_RESERVED_SECTORS * SECTOR as u64;
    let stride = fat_sectors * SECTOR as u64;
    for copy in 0..FAT_COPIES {
        let offset = (first + copy * stride + cluster as u64 * 4) as usize;
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

fn fat_short_entry(base: &str, ext: &str, attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    let mut name = [b' '; 8];
    for (index, byte) in base.bytes().take(8).enumerate() {
        name[index] = byte;
    }
    let mut extension = [b' '; 3];
    for (index, byte) in ext.bytes().take(3).enumerate() {
        extension[index] = byte;
    }
    entry[..8].copy_from_slice(&name);
    entry[8..11].copy_from_slice(&extension);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&((cluster & 0xffff) as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

fn fat_lfn_checksum(short: &[u8; 32]) -> u8 {
    let mut sum = 0u8;
    for &byte in &short[..11] {
        sum = sum.rotate_right(1).wrapping_add(byte);
    }
    sum
}

fn fat_push_lfn(out: &mut Vec<u8>, name: &str, short: &[u8; 32]) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let total = units.len() + 1;
    let count = (total + 12) / 13;
    let checksum = fat_lfn_checksum(short);
    for sequence in (1..=count).rev() {
        let mut entry = [0u8; 32];
        entry[0] = sequence as u8;
        if sequence == count {
            entry[0] |= 0x40;
        }
        entry[11] = 0x0f;
        entry[13] = checksum;
        for index in 0..13 {
            let unit_index = (sequence - 1) * 13 + index;
            let value = if unit_index < units.len() {
                units[unit_index]
            } else if unit_index == units.len() {
                0
            } else {
                0xffff
            };
            let offset = match index {
                0..=4 => 1 + index * 2,
                5..=10 => 14 + (index - 5) * 2,
                _ => 28 + (index - 11) * 2,
            };
            entry[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&entry);
    }
}

fn build_fat32() -> (Vec<u8>, Vec<u8>) {
    let (fat_sectors, clusters) = fat_layout();
    assert!(clusters >= 65_525, "fixture must be FAT32");

    let mut data = vec![0u8; FAT_TOTAL_SECTORS as usize * SECTOR];
    {
        let boot = &mut data[..SECTOR];
        boot[0] = 0xeb;
        boot[1] = 0x58;
        boot[2] = 0x90;
        boot[3..11].copy_from_slice(b"FEOS    ");
        boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        boot[13] = 1; // one sector per cluster
        boot[14..16].copy_from_slice(&(FAT_RESERVED_SECTORS as u16).to_le_bytes());
        boot[16] = FAT_COPIES as u8;
        boot[17..19].copy_from_slice(&0u16.to_le_bytes());
        boot[19..21].copy_from_slice(&0u16.to_le_bytes());
        boot[21] = 0xf8;
        boot[22..24].copy_from_slice(&0u16.to_le_bytes());
        boot[32..36].copy_from_slice(&(FAT_TOTAL_SECTORS as u32).to_le_bytes());
        boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
        boot[44..48].copy_from_slice(&FAT_ROOT_CLUSTER.to_le_bytes());
        boot[48..50].copy_from_slice(&1u16.to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xaa;
    }

    fat_set_entry(&mut data, fat_sectors, 0, 0x0fff_fff8);
    fat_set_entry(&mut data, fat_sectors, 1, 0x0fff_ffff);
    for cluster in [
        FAT_ROOT_CLUSTER,
        FAT_HELLO_CLUSTER,
        FAT_DOCS_CLUSTER,
        FAT_NOTES_CLUSTER,
        FAT_LONG_CLUSTER,
        9,
    ] {
        fat_set_entry(&mut data, fat_sectors, cluster, 0x0fff_ffff);
    }
    fat_set_entry(&mut data, fat_sectors, FAT_BIG_CLUSTER, 8);
    fat_set_entry(&mut data, fat_sectors, 8, 9);
    fat_set_entry(&mut data, fat_sectors, 9, 0x0fff_ffff);

    let hello = b"Hello, FAT32!\n";
    let long = b"long file contents\n";
    let notes = b"# notes\n";
    let big: Vec<u8> = (0..1500usize).map(|index| (index % 251) as u8).collect();

    let mut root = Vec::new();
    root.extend_from_slice(&fat_short_entry("FEOSTEST", "", 0x08, 0, 0));

    let hello_short = fat_short_entry("HELLO", "TXT", 0x20, FAT_HELLO_CLUSTER, hello.len() as u32);
    fat_push_lfn(&mut root, "Hello.txt", &hello_short);
    root.extend_from_slice(&hello_short);

    root.extend_from_slice(&fat_short_entry("DOCS", "", 0x10, FAT_DOCS_CLUSTER, 0));

    let long_short = fat_short_entry("LONGFI~1", "TXT", 0x20, FAT_LONG_CLUSTER, long.len() as u32);
    fat_push_lfn(&mut root, "Long Name Test.txt", &long_short);
    root.extend_from_slice(&long_short);

    root.extend_from_slice(&fat_short_entry("BIG", "BIN", 0x20, FAT_BIG_CLUSTER, 1500));
    assert!(root.len() <= SECTOR);

    let mut docs = Vec::new();
    docs.extend_from_slice(&fat_short_entry(".", "", 0x10, FAT_DOCS_CLUSTER, 0));
    docs.extend_from_slice(&fat_short_entry("..", "", 0x10, 0, 0));
    let notes_short = fat_short_entry("NOTES", "MD", 0x20, FAT_NOTES_CLUSTER, notes.len() as u32);
    fat_push_lfn(&mut docs, "notes.md", &notes_short);
    docs.extend_from_slice(&notes_short);
    assert!(docs.len() <= SECTOR);

    let root_offset = fat_cluster_offset(fat_sectors, FAT_ROOT_CLUSTER);
    data[root_offset..root_offset + root.len()].copy_from_slice(&root);
    let docs_offset = fat_cluster_offset(fat_sectors, FAT_DOCS_CLUSTER);
    data[docs_offset..docs_offset + docs.len()].copy_from_slice(&docs);

    let hello_offset = fat_cluster_offset(fat_sectors, FAT_HELLO_CLUSTER);
    data[hello_offset..hello_offset + hello.len()].copy_from_slice(hello);
    let long_offset = fat_cluster_offset(fat_sectors, FAT_LONG_CLUSTER);
    data[long_offset..long_offset + long.len()].copy_from_slice(long);
    let notes_offset = fat_cluster_offset(fat_sectors, FAT_NOTES_CLUSTER);
    data[notes_offset..notes_offset + notes.len()].copy_from_slice(notes);

    for (index, &byte) in big.iter().enumerate() {
        let cluster = FAT_BIG_CLUSTER + (index / SECTOR) as u32;
        let offset = fat_cluster_offset(fat_sectors, cluster) + index % SECTOR;
        data[offset] = byte;
    }

    (data, big)
}

#[test]
fn fat32_mounts_lists_and_reads() {
    let (image, big) = build_fat32();
    let fs = Fat32::mount(MemoryBlockDevice::new(&image)).unwrap();

    assert_eq!(fs.resolve("/").unwrap(), NodeId::ROOT);
    assert_eq!(fs.child_count(NodeId::ROOT), 4);
    assert_eq!(fs.resolve("/docs").unwrap(), fs.resolve("/DOCS").unwrap());
    assert_eq!(
        fs.resolve("/Hello.TXT").unwrap(),
        fs.resolve("/hello.txt").unwrap()
    );

    let hello = fs.resolve("/Hello.txt").unwrap();
    assert_eq!(fs.kind(hello), Some(EntryKind::File));
    assert_eq!(fs.size(hello), 14);
    assert_eq!(read_all(&fs, hello), b"Hello, FAT32!\n");

    let long = fs.resolve("/Long Name Test.txt").unwrap();
    assert_eq!(read_all(&fs, long), b"long file contents\n");

    let notes = fs.resolve("/DOCS/notes.md").unwrap();
    assert_eq!(read_all(&fs, notes), b"# notes\n");

    let big_node = fs.resolve("/big.bin").unwrap();
    assert_eq!(fs.size(big_node), 1500);
    assert_eq!(read_all(&fs, big_node), big);

    let mut buffer = [0u8; 17];
    let read = fs.read(big_node, 512, &mut buffer);
    assert_eq!(read, 17);
    assert_eq!(&buffer[..read], &big[512..529]);

    let mut last = [0u8; 8];
    let read = fs.read(big_node, 1499, &mut last);
    assert_eq!(read, 1);
    assert_eq!(last[0], big[1499]);

    let mut path = [0u8; 64];
    assert_eq!(fs.path(long, &mut path), Some("/Long Name Test.txt"));
}

#[test]
fn fat32_is_detected_and_mounted_from_image() {
    let (image, _) = build_fat32();
    let device = MemoryBlockDevice::new(&image);
    assert_eq!(detect(&device).unwrap(), VolumeKind::Fat32);

    let fs = mount_image(&image).unwrap();
    assert!(fs.resolve("/docs/notes.md").is_some());
    assert_eq!(fs.name(fs.resolve("/docs").unwrap()), Some("DOCS"));
}

// ---------------------------------------------------------------------------
// exFAT fixture
// ---------------------------------------------------------------------------

const EXFAT_VOLUME_SECTORS: u64 = 256;
const EXFAT_FAT_OFFSET: u32 = 24;
const EXFAT_FAT_LENGTH: u32 = 4;
const EXFAT_HEAP_OFFSET: u32 = 32;
const EXFAT_ROOT_CLUSTER: u32 = 2;
const EXFAT_HELLO_CLUSTER: u32 = 3;
const EXFAT_DOCS_CLUSTER: u32 = 4;
const EXFAT_BITMAP_CLUSTER: u32 = 5;
const EXFAT_BIG_CLUSTER: u32 = 6;
const EXFAT_CAFE_CLUSTER: u32 = 9;
const EXFAT_README_CLUSTER: u32 = 10;

fn exfat_cluster_offset(cluster: u32) -> usize {
    (EXFAT_HEAP_OFFSET as u64 * SECTOR as u64 + (cluster as u64 - 2) * SECTOR as u64) as usize
}

fn exfat_set_fat(data: &mut [u8], cluster: u32, value: u32) {
    let offset = EXFAT_FAT_OFFSET as usize * SECTOR + cluster as usize * 4;
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn exfat_boot_checksum(region: &[u8]) -> u32 {
    let mut checksum = 0u32;
    for (index, &byte) in region.iter().enumerate() {
        if index == 106 || index == 107 || index == 112 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(byte as u32);
    }
    checksum
}

fn exfat_name_hash(name: &str) -> u16 {
    let mut hash = 0u16;
    for unit in name.encode_utf16() {
        let unit = if unit <= 0x7f {
            (unit as u8).to_ascii_uppercase() as u16
        } else {
            unit
        };
        hash = hash.rotate_right(1).wrapping_add(unit & 0xff);
        hash = hash.rotate_right(1).wrapping_add(unit >> 8);
    }
    hash
}

fn exfat_volume_label(name: &str) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0] = 0x83;
    let units: Vec<u16> = name.encode_utf16().take(11).collect();
    entry[1] = units.len() as u8;
    for (index, unit) in units.iter().enumerate() {
        entry[2 + index * 2..4 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    entry
}

fn exfat_bitmap(cluster: u32, length: u64) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0] = 0x81;
    entry[20..24].copy_from_slice(&cluster.to_le_bytes());
    entry[24..32].copy_from_slice(&length.to_le_bytes());
    entry
}

fn exfat_push_file(
    out: &mut Vec<u8>,
    name: &str,
    attributes: u16,
    cluster: u32,
    size: u64,
    flags: u8,
) {
    let units: Vec<u16> = name.encode_utf16().collect();
    assert!(units.len() <= 255);
    let name_entries = (units.len() + 14) / 15;
    let secondary = 1 + name_entries;

    let mut entries: Vec<[u8; 32]> = Vec::new();

    let mut primary = [0u8; 32];
    primary[0] = 0x85;
    primary[1] = secondary as u8;
    primary[4..6].copy_from_slice(&attributes.to_le_bytes());
    entries.push(primary);

    let mut stream = [0u8; 32];
    stream[0] = 0xc0;
    stream[1] = flags;
    stream[3] = units.len() as u8;
    stream[4..6].copy_from_slice(&exfat_name_hash(name).to_le_bytes());
    stream[8..16].copy_from_slice(&size.to_le_bytes());
    stream[20..24].copy_from_slice(&cluster.to_le_bytes());
    stream[24..32].copy_from_slice(&size.to_le_bytes());
    entries.push(stream);

    for chunk in units.chunks(15) {
        let mut entry = [0u8; 32];
        entry[0] = 0xc1;
        for (index, unit) in chunk.iter().enumerate() {
            entry[2 + index * 2..4 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        entries.push(entry);
    }

    let mut checksum = 0u16;
    for (entry_index, entry) in entries.iter().enumerate() {
        for byte_index in 0..32 {
            if entry_index == 0 && (byte_index == 2 || byte_index == 3) {
                continue;
            }
            checksum = checksum
                .rotate_right(1)
                .wrapping_add(entry[byte_index] as u16);
        }
    }
    entries[0][2..4].copy_from_slice(&checksum.to_le_bytes());

    for entry in entries {
        out.extend_from_slice(&entry);
    }
}

fn build_exfat() -> (Vec<u8>, Vec<u8>) {
    let mut data = vec![0u8; EXFAT_VOLUME_SECTORS as usize * SECTOR];
    let cluster_count = (EXFAT_VOLUME_SECTORS - EXFAT_HEAP_OFFSET as u64) as u32;
    assert_eq!(
        cluster_count as u64 * SECTOR as u64,
        (EXFAT_VOLUME_SECTORS - EXFAT_HEAP_OFFSET as u64) * SECTOR as u64
    );

    {
        let boot = &mut data[..SECTOR];
        boot[0] = 0xeb;
        boot[1] = 0x76;
        boot[2] = 0x90;
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[72..80].copy_from_slice(&EXFAT_VOLUME_SECTORS.to_le_bytes());
        boot[80..84].copy_from_slice(&EXFAT_FAT_OFFSET.to_le_bytes());
        boot[84..88].copy_from_slice(&EXFAT_FAT_LENGTH.to_le_bytes());
        boot[88..92].copy_from_slice(&EXFAT_HEAP_OFFSET.to_le_bytes());
        boot[92..96].copy_from_slice(&cluster_count.to_le_bytes());
        boot[96..100].copy_from_slice(&EXFAT_ROOT_CLUSTER.to_le_bytes());
        boot[100..104].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[108] = 9; // 512-byte sectors
        boot[109] = 0; // one sector per cluster
        boot[110] = 1;
        boot[111] = 0x80;
        boot[510] = 0x55;
        boot[511] = 0xaa;
    }

    let checksum = exfat_boot_checksum(&data[..11 * SECTOR]);
    for index in 0..(SECTOR / 4) {
        let offset = 11 * SECTOR + index * 4;
        data[offset..offset + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    let main_region: Vec<u8> = data[..12 * SECTOR].to_vec();
    data[12 * SECTOR..24 * SECTOR].copy_from_slice(&main_region);

    exfat_set_fat(&mut data, 0, 0xffff_fff8);
    exfat_set_fat(&mut data, 1, 0xffff_ffff);
    for cluster in [
        EXFAT_ROOT_CLUSTER,
        EXFAT_HELLO_CLUSTER,
        EXFAT_DOCS_CLUSTER,
        EXFAT_BITMAP_CLUSTER,
        EXFAT_CAFE_CLUSTER,
        EXFAT_README_CLUSTER,
    ] {
        exfat_set_fat(&mut data, cluster, 0xffff_ffff);
    }
    exfat_set_fat(&mut data, EXFAT_BIG_CLUSTER, 7);
    exfat_set_fat(&mut data, 7, 8);
    exfat_set_fat(&mut data, 8, 0xffff_ffff);

    let hello = b"Hello, exFAT!\n";
    let cafe_text = "café text\n".as_bytes();
    let readme = b"# exFAT docs\n";
    let big: Vec<u8> = (0..1300usize).map(|index| (index % 239) as u8).collect();

    let mut root = Vec::new();
    root.extend_from_slice(&exfat_volume_label("FEOSTEST"));
    root.extend_from_slice(&exfat_bitmap(EXFAT_BITMAP_CLUSTER, 28));
    exfat_push_file(
        &mut root,
        "hello.txt",
        0x20,
        EXFAT_HELLO_CLUSTER,
        hello.len() as u64,
        0x01,
    );
    exfat_push_file(
        &mut root,
        "docs",
        0x10,
        EXFAT_DOCS_CLUSTER,
        SECTOR as u64,
        0x03,
    );
    exfat_push_file(
        &mut root,
        "big.bin",
        0x20,
        EXFAT_BIG_CLUSTER,
        big.len() as u64,
        0x01,
    );
    exfat_push_file(
        &mut root,
        "café.txt",
        0x20,
        EXFAT_CAFE_CLUSTER,
        cafe_text.len() as u64,
        0x01,
    );
    assert!(root.len() <= SECTOR);

    let mut docs = Vec::new();
    exfat_push_file(
        &mut docs,
        "readme.md",
        0x20,
        EXFAT_README_CLUSTER,
        readme.len() as u64,
        0x03,
    );
    assert!(docs.len() <= SECTOR);

    let root_offset = exfat_cluster_offset(EXFAT_ROOT_CLUSTER);
    data[root_offset..root_offset + root.len()].copy_from_slice(&root);
    let docs_offset = exfat_cluster_offset(EXFAT_DOCS_CLUSTER);
    data[docs_offset..docs_offset + docs.len()].copy_from_slice(&docs);

    let hello_offset = exfat_cluster_offset(EXFAT_HELLO_CLUSTER);
    data[hello_offset..hello_offset + hello.len()].copy_from_slice(hello);
    let cafe_offset = exfat_cluster_offset(EXFAT_CAFE_CLUSTER);
    data[cafe_offset..cafe_offset + cafe_text.len()].copy_from_slice(cafe_text);
    let readme_offset = exfat_cluster_offset(EXFAT_README_CLUSTER);
    data[readme_offset..readme_offset + readme.len()].copy_from_slice(readme);

    let bitmap_offset = exfat_cluster_offset(EXFAT_BITMAP_CLUSTER);
    // Clusters 2..=10 are in use (root, files, docs, bitmap, big, café,
    // readme). The bitmap is indexed from cluster 2, not from zero.
    data[bitmap_offset] = 0xff;
    data[bitmap_offset + 1] = 0x01;

    for (index, &byte) in big.iter().enumerate() {
        let cluster = EXFAT_BIG_CLUSTER + (index / SECTOR) as u32;
        let offset = exfat_cluster_offset(cluster) + index % SECTOR;
        data[offset] = byte;
    }

    (data, big)
}

#[test]
fn exfat_mounts_lists_and_reads() {
    let (image, big) = build_exfat();
    let fs = Exfat::mount(MemoryBlockDevice::new(&image)).unwrap();

    assert_eq!(fs.resolve("/").unwrap(), NodeId::ROOT);
    assert_eq!(fs.child_count(NodeId::ROOT), 4);
    assert_eq!(fs.resolve("/docs").unwrap(), fs.resolve("/DOCS").unwrap());

    let hello = fs.resolve("/hello.txt").unwrap();
    assert_eq!(fs.size(hello), 14);
    assert_eq!(read_all(&fs, hello), b"Hello, exFAT!\n");
    assert_eq!(
        fs.resolve("/HELLO.TXT").unwrap(),
        hello,
        "exFAT lookups are case-insensitive"
    );

    let docs = fs.resolve("/docs").unwrap();
    assert_eq!(fs.child_count(docs), 1);
    let readme = fs.resolve("/docs/readme.md").unwrap();
    assert_eq!(read_all(&fs, readme), b"# exFAT docs\n");

    let cafe = fs.resolve("/café.txt").unwrap();
    assert_eq!(read_all(&fs, cafe), "café text\n".as_bytes());
    assert_eq!(fs.resolve("/CAFÉ.TXT").unwrap(), cafe);

    let big_node = fs.resolve("/big.bin").unwrap();
    assert_eq!(fs.size(big_node), 1300);
    assert_eq!(read_all(&fs, big_node), big);

    let mut buffer = [0u8; 20];
    let read = fs.read(big_node, 511, &mut buffer);
    assert_eq!(read, 20);
    assert_eq!(&buffer[..read], &big[511..531]);

    let mut last = [0u8; 16];
    let read = fs.read(big_node, 1299, &mut last);
    assert_eq!(read, 1);
    assert_eq!(last[0], big[1299]);
}

#[test]
fn exfat_is_detected_and_mounted_from_image() {
    let (image, _) = build_exfat();
    let device = MemoryBlockDevice::new(&image);
    assert_eq!(detect(&device).unwrap(), VolumeKind::Exfat);

    let fs = mount_image(&image).unwrap();
    assert!(fs.resolve("/docs/README.MD").is_some());
    assert_eq!(fs.name(fs.resolve("/docs").unwrap()), Some("docs"));
}

#[test]
fn fat32_writes_renames_removes_and_persists() {
    let (mut image, _) = build_fat32();
    let data: Vec<u8> = (0..1500usize).map(|index| (index % 97) as u8).collect();

    {
        let mut fs = Fat32::mount(MemoryBlockDeviceMut::new(&mut image)).unwrap();

        let file = fs.create_file(NodeId::ROOT, "new-file.txt").unwrap();
        assert_eq!(fs.write(file, 0, &data).unwrap(), data.len());
        assert_eq!(read_all(&fs, file), data);

        fs.set_len(file, 2000).unwrap();
        assert_eq!(fs.size(file), 2000);
        let mut tail = [0u8; 10];
        assert_eq!(fs.read(file, 1990, &mut tail), 10);
        assert!(tail.iter().all(|&byte| byte == 0));

        assert_eq!(fs.write(file, 2500, b"tail").unwrap(), 4);
        assert_eq!(fs.size(file), 2504);

        let dir = fs.create_dir(NodeId::ROOT, "newdir").unwrap();
        let nested = fs.create_file(dir, "nested.txt").unwrap();
        assert_eq!(fs.write(nested, 0, b"nested").unwrap(), 6);
        fs.rename(nested, NodeId::ROOT, "renamed.txt").unwrap();
        assert!(fs.resolve("/newdir/nested.txt").is_none());
        assert_eq!(
            read_all(&fs, fs.resolve("/renamed.txt").unwrap()),
            b"nested"
        );

        let notes = fs.resolve("/DOCS/notes.md").unwrap();
        fs.remove(notes).unwrap();
        assert!(fs.resolve("/DOCS/notes.md").is_none());

        assert!(matches!(
            fs.create_file(NodeId::ROOT, "new-file.txt"),
            Err(FsError::NameExists)
        ));
        assert!(matches!(fs.remove(fs.resolve("/DOCS").unwrap()), Ok(())));
        assert!(matches!(fs.remove(fs.resolve("/newdir").unwrap()), Ok(())));

        fs.flush().unwrap();
    }

    let fs = Fat32::mount(MemoryBlockDevice::new(&image)).unwrap();
    let file = fs.resolve("/new-file.txt").unwrap();
    assert_eq!(fs.size(file), 2504);
    assert_eq!(read_all(&fs, file), {
        let mut expected = data.clone();
        expected.resize(2000, 0);
        expected.resize(2500, 0);
        expected.extend_from_slice(b"tail");
        expected
    });
    assert_eq!(
        read_all(&fs, fs.resolve("/renamed.txt").unwrap()),
        b"nested"
    );
    assert!(fs.resolve("/newdir").is_none());
    assert!(fs.resolve("/DOCS/notes.md").is_none());
    assert_eq!(fs.name(file), Some("new-file.txt"));
}

#[test]
fn exfat_writes_renames_removes_and_persists() {
    let (mut image, _) = build_exfat();
    let data: Vec<u8> = (0..2000usize).map(|index| (index % 113) as u8).collect();
    let extended: Vec<u8> = (0..900usize).map(|index| (index % 61) as u8).collect();

    {
        let mut fs = Exfat::mount(MemoryBlockDeviceMut::new(&mut image)).unwrap();

        let file = fs.create_file(NodeId::ROOT, "naïve.txt").unwrap();
        assert_eq!(fs.write(file, 0, &data).unwrap(), data.len());
        assert_eq!(read_all(&fs, file), data);

        // `readme.md` is a contiguous (NoFatChain) stream in the fixture.
        // Writing past its first cluster exercises the contiguous-to-FAT
        // conversion path.
        let readme = fs.resolve("/docs/readme.md").unwrap();
        assert_eq!(fs.write(readme, 0, &extended).unwrap(), extended.len());
        assert_eq!(read_all(&fs, readme), extended);

        let dir = fs.create_dir(NodeId::ROOT, "dír").unwrap();
        let child = fs.create_file(dir, "child.txt").unwrap();
        assert_eq!(fs.write(child, 0, b"child").unwrap(), 5);
        fs.rename(child, NodeId::ROOT, "moved.txt").unwrap();
        assert!(fs.resolve("/dír/child.txt").is_none());
        assert_eq!(read_all(&fs, fs.resolve("/moved.txt").unwrap()), b"child");
        fs.remove(dir).unwrap();

        let cafe = fs.resolve("/café.txt").unwrap();
        fs.remove(cafe).unwrap();
        assert!(fs.resolve("/café.txt").is_none());

        fs.flush().unwrap();
    }

    let fs = Exfat::mount(MemoryBlockDevice::new(&image)).unwrap();
    let file = fs.resolve("/naïve.txt").unwrap();
    assert_eq!(fs.size(file), 2000);
    assert_eq!(read_all(&fs, file), data);
    assert_eq!(
        read_all(&fs, fs.resolve("/docs/readme.md").unwrap()),
        extended
    );
    assert_eq!(read_all(&fs, fs.resolve("/moved.txt").unwrap()), b"child");
    assert!(fs.resolve("/dír").is_none());
    assert!(fs.resolve("/café.txt").is_none());
    assert_eq!(fs.name(file), Some("naïve.txt"));
}

#[test]
fn long_names_span_directory_clusters() {
    let name = format!("{}.txt", "a".repeat(150));

    let (mut fat_image, _) = build_fat32();
    {
        let mut fs = Fat32::mount(MemoryBlockDeviceMut::new(&mut fat_image)).unwrap();
        let file = fs.create_file(NodeId::ROOT, &name).unwrap();
        fs.write(file, 0, b"fat32 long name").unwrap();
        fs.flush().unwrap();
    }
    let fs = Fat32::mount(MemoryBlockDevice::new(&fat_image)).unwrap();
    assert_eq!(
        read_all(&fs, fs.resolve(&name).unwrap()),
        b"fat32 long name"
    );

    let exfat_name = format!("{}.txt", "é".repeat(100));
    let (mut exfat_image, _) = build_exfat();
    {
        let mut fs = mount_image_mut(&mut exfat_image).unwrap();
        let file = fs.create_file(NodeId::ROOT, &exfat_name).unwrap();
        fs.write(file, 0, b"exfat long name").unwrap();
        fs.flush().unwrap();
    }
    let fs = Exfat::mount(MemoryBlockDevice::new(&exfat_image)).unwrap();
    assert_eq!(
        read_all(&fs, fs.resolve(&exfat_name).unwrap()),
        b"exfat long name"
    );
}

#[test]
fn truncates_and_overwrites_existing_files() {
    let (mut fat_image, fat_big) = build_fat32();
    {
        let mut fs = Fat32::mount(MemoryBlockDeviceMut::new(&mut fat_image)).unwrap();
        let file = fs.resolve("/big.bin").unwrap();
        fs.set_len(file, 700).unwrap();
        fs.write(file, 100, b"XYZ").unwrap();
        fs.flush().unwrap();
    }
    let fs = Fat32::mount(MemoryBlockDevice::new(&fat_image)).unwrap();
    let file = fs.resolve("/big.bin").unwrap();
    assert_eq!(fs.size(file), 700);
    let mut expected = fat_big[..700].to_vec();
    expected[100..103].copy_from_slice(b"XYZ");
    assert_eq!(read_all(&fs, file), expected);

    let (mut exfat_image, exfat_big) = build_exfat();
    {
        let mut fs = Exfat::mount(MemoryBlockDeviceMut::new(&mut exfat_image)).unwrap();
        let file = fs.resolve("/big.bin").unwrap();
        fs.set_len(file, 700).unwrap();
        fs.write(file, 100, b"XYZ").unwrap();
        fs.flush().unwrap();
    }
    let fs = Exfat::mount(MemoryBlockDevice::new(&exfat_image)).unwrap();
    let file = fs.resolve("/big.bin").unwrap();
    assert_eq!(fs.size(file), 700);
    let mut expected = exfat_big[..700].to_vec();
    expected[100..103].copy_from_slice(b"XYZ");
    assert_eq!(read_all(&fs, file), expected);
}

#[test]
fn mount_table_exposes_multiple_volumes() {
    let (fat_image, _) = build_fat32();
    let (exfat_image, _) = build_exfat();
    let mut table = MountTable::new();
    table
        .mount("fat32", mount_image(&fat_image).unwrap())
        .unwrap();
    table
        .mount("exfat", mount_image(&exfat_image).unwrap())
        .unwrap();

    assert_eq!(table.child_count(NodeId::ROOT), 2);
    assert!(table.resolve("/fat32/Hello.txt").is_some());
    assert!(table.resolve("/exfat/hello.txt").is_some());
    assert!(table.resolve("/exfat/café.txt").is_some());
    assert!(table.resolve("/fat32/missing").is_none());

    let notes = table.resolve("/exfat/docs/readme.md").unwrap();
    let mut path = [0u8; 64];
    assert_eq!(table.path(notes, &mut path), Some("/exfat/docs/readme.md"));
    assert_eq!(
        read_all(&table, table.resolve("/fat32/docs/notes.md").unwrap()),
        b"# notes\n"
    );
}

fn read_all(fs: &dyn FileSystem, node: NodeId) -> Vec<u8> {
    let size = fs.size(node);
    let mut data = vec![0u8; size];
    let read = fs.read(node, 0, &mut data);
    data.truncate(read);
    data
}
