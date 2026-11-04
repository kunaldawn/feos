//! Deterministic byte-mutation fuzzing for bundle opening and manifests.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_archive::{TarWriter, ZipWriter};
use feos_bundle::{Bundle, Manifest};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const ITERATIONS: usize = 4000;
const MAX_INPUT: usize = 32 * 1024;

const MANIFEST: &str = "id: demo\nname: Demo\nversion: 1.0.0\nentry: gui\ncategory: System\ndescription: \"A demo\"\nicon: icon.bmp\nargs: \"\"\nwindow:\n  width: 640\n  height: 420\n";

fn seeds() -> Vec<Vec<u8>> {
    let mut zip = vec![0u8; 16 * 1024];
    let mut writer = ZipWriter::new(&mut zip);
    let mut scratch = vec![0u8; 4 * 1024];
    writer
        .write_deflated("manifest.yaml", MANIFEST.as_bytes(), &mut scratch)
        .unwrap();
    writer
        .write_stored("icon.bmp", b"BM not really an image")
        .unwrap();
    let len = writer.finish().unwrap();
    zip.truncate(len);

    let mut tar = vec![0u8; 16 * 1024];
    let mut writer = TarWriter::new(&mut tar);
    writer
        .write_file("manifest.yaml", MANIFEST.as_bytes())
        .unwrap();
    let len = writer.finish().unwrap();
    tar.truncate(len);
    vec![zip, tar]
}

#[test]
fn bundle_open_survives_mutated_archives() {
    // `Bundle::icon` needs image decoding, which `feos-image` fuzzes itself and
    // whose buffers are its own: this covers the bundle layer.
    run_constrained(|| {
        fuzz_bytes(41, &seeds(), ITERATIONS, MAX_INPUT, |input| {
            let Ok(bundle) = Bundle::open(input) else {
                return;
            };
            if let Ok(manifest) = bundle.manifest() {
                assert!(!manifest.id.is_empty());
                let _ = bundle.icon(&manifest);
            }
        });
    });
}

#[test]
fn manifest_parse_survives_mutated_yaml() {
    run_constrained(|| {
        fuzz_bytes(
            42,
            &[MANIFEST.as_bytes().to_vec()],
            ITERATIONS,
            MAX_INPUT,
            |input| {
                if let Ok(text) = core::str::from_utf8(input) {
                    let _ = Manifest::parse(text);
                }
            },
        );
    });
}
