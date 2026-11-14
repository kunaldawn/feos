use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng as _};
use rand_chacha::ChaCha8Rng;
use stringdex::internals::{decode::read_base64_from_bytes, encode::write_base64_to_bytes};

fn gen_base64_data(rng: &mut impl Rng, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    rng.fill_bytes(&mut data);
    let mut encoded = Vec::new();
    write_base64_to_bytes(&data, &mut encoded);
    encoded
}

pub fn base64_decode(c: &mut Criterion) {
    let mut rng = ChaCha8Rng::seed_from_u64(1034);

    for (size, param_name) in [(128, "128 bytes"), (1024, "1kb"), (300 * 1024, "300kb")] {
        c.bench_with_input(
            BenchmarkId::new("read_base64_from_bytes", param_name),
            &gen_base64_data(&mut rng, size),
            |b, data| {
                b.iter_with_large_drop(|| {
                    let mut out = Vec::new();
                    let _ = read_base64_from_bytes(data, &mut out);
                    out
                });
            },
        );
    }
}

criterion_group!(benches, base64_decode);
criterion_main!(benches);
