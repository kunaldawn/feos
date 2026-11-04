//! Deterministic byte-mutation fuzzing for inflate, gzip and zlib.

#[path = "support/fuzz.rs"]
mod fuzz;

use feos_compress::{deflate, gzip, inflate, zlib, Deflater};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator, Rng};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const PLAIN: &[u8] = include_bytes!("data/plain.bin");
const RAW: &[u8] = include_bytes!("data/raw.deflate");
const ZLIB: &[u8] = include_bytes!("data/zlib.bin");
const GZIP: &[u8] = include_bytes!("data/gzip.bin");
const GZIP_NAMED: &[u8] = include_bytes!("data/gzip_named.bin");

const ITERATIONS: usize = 4000;
const MAX_INPUT: usize = 16 * 1024;

#[test]
fn inflate_survives_mutated_streams() {
    run_constrained(|| {
        let mut out = vec![0u8; 64 * 1024];
        let mut small = vec![0u8; 64];
        fuzz_bytes(1, &[RAW.to_vec()], ITERATIONS, MAX_INPUT, |input| {
            if let Ok(result) = inflate::inflate_with_consumed(input, &mut out) {
                assert!(result.written <= out.len());
                assert!(result.consumed <= input.len());
            }
            let _ = inflate::inflate(input, &mut small);
        });
    });
}

#[test]
fn gzip_survives_mutated_members() {
    run_constrained(|| {
        let mut out = vec![0u8; 64 * 1024];
        let seeds = [GZIP.to_vec(), GZIP_NAMED.to_vec()];
        fuzz_bytes(2, &seeds, ITERATIONS, MAX_INPUT, |input| {
            let _ = gzip::decompress(input, &mut out);
        });
    });
}

#[test]
fn zlib_survives_mutated_streams() {
    run_constrained(|| {
        let mut out = vec![0u8; 64 * 1024];
        fuzz_bytes(3, &[ZLIB.to_vec()], ITERATIONS, MAX_INPUT, |input| {
            let _ = zlib::decompress(input, &mut out);
        });
    });
}

#[test]
fn compressed_output_always_inflates_back() {
    run_constrained(|| {
        let mut deflater = Deflater::new();
        let mut compressed = vec![0u8; 80 * 1024];
        let mut out = vec![0u8; 64 * 1024];
        let mut rng = Rng::new(4);
        let mut input = Vec::with_capacity(32 * 1024);
        for _ in 0..200 {
            input.clear();
            input.extend_from_slice(&PLAIN[..PLAIN.len().min(8192)]);
            fuzz::mutate(&mut rng, &mut input, 32 * 1024);
            let len = deflate::compress_with(&mut deflater, &input, &mut compressed).unwrap();
            let written = inflate::inflate(&compressed[..len], &mut out).unwrap();
            assert_eq!(&out[..written], &input[..]);
        }
    });
}
