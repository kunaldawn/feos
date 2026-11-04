//! Deterministic byte-mutation fuzzing for ZIP and TAR.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_archive::tar::{TarReader, TarWriter};
use feos_archive::zip::{ZipReader, ZipWriter};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const ITERATIONS: usize = 6000;
const MAX_INPUT: usize = 64 * 1024;

fn zip_seed() -> Vec<u8> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut writer = ZipWriter::new(&mut buffer);
    writer.write_stored("manifest.yaml", b"id: demo\n").unwrap();
    let mut scratch = vec![0u8; 8 * 1024];
    writer
        .write_deflated("dir/data.txt", &b"compressible ".repeat(64), &mut scratch)
        .unwrap();
    writer.write_stored("empty", b"").unwrap();
    let len = writer.finish().unwrap();
    buffer.truncate(len);
    buffer
}

fn tar_seed() -> Vec<u8> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut writer = TarWriter::new(&mut buffer);
    writer.write_dir("dir/").unwrap();
    writer.write_file("dir/data.txt", b"hello tar").unwrap();
    writer.write_symlink("link", "dir/data.txt").unwrap();
    let len = writer.finish().unwrap();
    buffer.truncate(len);
    buffer
}

#[test]
fn zip_reader_survives_mutated_archives() {
    run_constrained(|| {
        let mut out = vec![0u8; 16 * 1024];
        fuzz_bytes(11, &[zip_seed()], ITERATIONS, MAX_INPUT, |input| {
            let Ok(mut reader) = ZipReader::open(input) else {
                return;
            };
            while let Ok(Some(entry)) = reader.next() {
                let _ = entry.name();
                let _ = entry.is_dir();
                // A consumer sizes a buffer from this, so it has to be plausible.
                if entry.method == 0 {
                    assert_eq!(entry.compressed_size, entry.uncompressed_size);
                }
                assert!(entry.uncompressed_size as u64 <= (input.len() as u64 + 1) * 1032);
                let _ = entry.extract(&mut out);
            }
        });
    });
}

#[test]
fn tar_reader_survives_mutated_archives() {
    run_constrained(|| {
        let mut path = [0u8; 512];
        fuzz_bytes(12, &[tar_seed()], ITERATIONS, MAX_INPUT, |input| {
            let mut reader = TarReader::new(input);
            let mut last = 0usize;
            while let Ok(Some(entry)) = reader.next() {
                let _ = entry.path(&mut path);
                let _ = entry.link();
                assert!(entry.data.len() <= input.len());
                // Every step makes progress, so a walk can't loop.
                assert!(reader.position() > last);
                last = reader.position();
            }
        });
    });
}
