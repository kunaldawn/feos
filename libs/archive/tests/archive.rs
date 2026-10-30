use feos_archive::tar::{EntryType, TarReader, TarWriter};
use feos_archive::zip::{ZipReader, ZipWriter};
use feos_archive::{detect, ArchiveError, Format};

const HELLO: &[u8] = b"hello from the feos archive library\n";
const LONG: &str = "very-long-directory-name-segment-one/very-long-directory-name-segment-two/very-long-file-name-with-more-than-one-hundred-characters.txt";

fn tar_entries(data: &[u8]) -> Vec<(String, EntryType, Vec<u8>, String)> {
    let mut reader = TarReader::new(data);
    let mut result = Vec::new();
    let mut buffer = [0u8; 512];
    while let Some(entry) = reader.next().unwrap() {
        let path = entry.path(&mut buffer).unwrap().to_string();
        let link = entry.link().unwrap().to_string();
        result.push((path, entry.entry_type, entry.data.to_vec(), link));
    }
    result
}

#[test]
fn detects_formats() {
    assert_eq!(detect(include_bytes!("data/ustar.tar")), Some(Format::Tar));
    assert_eq!(
        detect(include_bytes!("data/entries.zip")),
        Some(Format::Zip)
    );
    assert_eq!(
        detect(include_bytes!("../../compress/tests/data/gzip.bin")),
        Some(Format::Gzip)
    );
    assert_eq!(detect(b"not an archive"), None);
}

#[test]
fn reads_all_tar_flavours() {
    for data in [
        &include_bytes!("data/ustar.tar")[..],
        &include_bytes!("data/gnu.tar")[..],
        &include_bytes!("data/pax.tar")[..],
    ] {
        let entries = tar_entries(data);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].0, "hello.txt");
        assert_eq!(entries[0].1, EntryType::Regular);
        assert_eq!(entries[0].2, HELLO);
        assert_eq!(entries[1].0, "sub/");
        assert!(matches!(entries[1].1, EntryType::Directory));
        assert_eq!(entries[2].0, "sub/data.bin");
        assert_eq!(entries[2].2.len(), 1024);
        assert_eq!(entries[3].0, "link");
        assert_eq!(entries[3].1, EntryType::Symlink);
        assert_eq!(entries[3].3, "hello.txt");
        assert_eq!(entries[4].0, LONG);
        assert_eq!(entries[4].2, HELLO);
    }
}

#[test]
fn tar_writer_roundtrip() {
    let mut buffer = vec![0u8; 8192];
    let written = {
        let mut writer = TarWriter::new(&mut buffer);
        writer.write_dir("dir/").unwrap();
        writer.write_file("dir/file.txt", HELLO).unwrap();
        writer
            .write_file_with("long.txt", b"abc", 0o600, 1_700_000_000)
            .unwrap();
        writer.write_symlink("shortcut", "dir/file.txt").unwrap();
        let long_path = format!("{LONG}");
        writer.write_file(&long_path, HELLO).unwrap();
        writer.finish().unwrap()
    };

    let entries = tar_entries(&buffer[..written]);
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].0, "dir/");
    assert_eq!(entries[1].0, "dir/file.txt");
    assert_eq!(entries[1].2, HELLO);
    assert_eq!(entries[2].0, "long.txt");
    assert_eq!(entries[2].2, b"abc");
    assert_eq!(entries[3].3, "dir/file.txt");
    assert_eq!(entries[4].0, LONG);
}

#[test]
fn tar_writer_reports_small_buffer() {
    let mut buffer = [0u8; 128];
    let mut writer = TarWriter::new(&mut buffer);
    assert!(matches!(
        writer.write_file("file.txt", HELLO),
        Err(ArchiveError::BufferTooSmall { .. })
    ));
}

#[test]
fn reads_zip_entries() {
    let data = include_bytes!("data/entries.zip");
    let mut reader = ZipReader::open(data).unwrap();
    assert_eq!(reader.len(), 4);

    let mut out = [0u8; 4096];

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "stored.txt");
    assert!(!entry.is_dir());
    let written = entry.extract(&mut out).unwrap();
    assert_eq!(&out[..written], HELLO);

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "deflate.txt");
    let written = entry.extract(&mut out).unwrap();
    assert_eq!(written, 1024);
    for (index, byte) in out[..written].iter().enumerate() {
        assert_eq!(*byte, index as u8);
    }

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "folder/");
    assert!(entry.is_dir());
    assert_eq!(entry.extract(&mut out).unwrap(), 0);

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "folder/nested.txt");
    let written = entry.extract(&mut out).unwrap();
    assert_eq!(&out[..written], HELLO);
    assert!(reader.next().unwrap().is_none());
}

#[test]
fn zip_writer_roundtrip() {
    let mut buffer = vec![0u8; 16384];
    let mut scratch = vec![0u8; 8192];
    let written = {
        let mut writer = ZipWriter::new(&mut buffer);
        writer.write_dir("folder/").unwrap();
        writer.write_stored("folder/stored.txt", HELLO).unwrap();
        writer
            .write_deflated("folder/deflate.txt", HELLO, &mut scratch)
            .unwrap();
        writer.finish().unwrap()
    };

    assert_eq!(detect(&buffer[..written]), Some(Format::Zip));
    let mut reader = ZipReader::open(&buffer[..written]).unwrap();
    assert_eq!(reader.len(), 3);

    let mut out = [0u8; 1024];
    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "folder/");
    assert!(entry.is_dir());

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "folder/stored.txt");
    let written = entry.extract(&mut out).unwrap();
    assert_eq!(&out[..written], HELLO);

    let entry = reader.next().unwrap().unwrap();
    assert_eq!(entry.name().unwrap(), "folder/deflate.txt");
    let written = entry.extract(&mut out).unwrap();
    assert_eq!(&out[..written], HELLO);
    assert!(reader.next().unwrap().is_none());
}

#[test]
fn zip_writer_reports_small_buffer() {
    let mut buffer = [0u8; 64];
    let mut writer = ZipWriter::new(&mut buffer);
    assert!(matches!(
        writer.write_stored("file.txt", HELLO),
        Err(ArchiveError::BufferTooSmall { .. })
    ));
}

#[test]
fn rejects_corrupt_zip_crc() {
    let mut data = include_bytes!("data/entries.zip").to_vec();
    // The stored entry's CRC field sits near the start of the central
    // directory; find it through the reader instead of hard-coding offsets.
    let entry = {
        let mut reader = ZipReader::open(&data).unwrap();
        reader.next().unwrap().unwrap()
    };
    assert_eq!(entry.name().unwrap(), "stored.txt");
    let crc = entry.crc32 ^ 1;
    // Locate the local header CRC of the first entry.
    data[14..18].copy_from_slice(&crc.to_le_bytes());
    // Also patch the central copy so extraction reaches the CRC check.
    let eocd = data.len() - 22;
    let central_offset = u32::from_le_bytes([
        data[eocd + 16],
        data[eocd + 17],
        data[eocd + 18],
        data[eocd + 19],
    ]) as usize;
    data[central_offset + 16..central_offset + 20].copy_from_slice(&crc.to_le_bytes());

    let mut reader = ZipReader::open(&data).unwrap();
    let entry = reader.next().unwrap().unwrap();
    let mut out = [0u8; 256];
    assert_eq!(entry.extract(&mut out), Err(ArchiveError::Checksum));
}
