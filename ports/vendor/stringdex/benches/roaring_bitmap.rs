use criterion::{Criterion, black_box, criterion_group, criterion_main};
use stringdex::internals::decode::RoaringBitmap;

pub fn roaring_bitmapwithoutruns(c: &mut Criterion) {
    let dataset = std::fs::read("roaring-bitmap-testdata/bitmapwithoutruns.bin")
        .or_else(|_| std::fs::read("benches/roaring-bitmap-testdata/bitmapwithoutruns.bin"))
        .unwrap();
    c.bench_function("RoaringBitmap::from_bytes", |b| {
        b.iter_with_large_drop(|| black_box(RoaringBitmap::from_bytes(black_box(&dataset[..]))))
    });
    let data = RoaringBitmap::from_bytes(&dataset[..]).unwrap().0;
    c.bench_function("RoaringBitmap::contains", |b| {
        b.iter(|| {
            for i in (0..100000).step_by(1000) {
                assert!(black_box(data.contains(i)));
            }
            for i in 100000..200000 {
                assert!(black_box(data.contains(3 * i)));
            }
            for i in 700000..800000 {
                assert!(black_box(data.contains(i)));
            }
        })
    });
}
pub fn roaring_bitmapwithruns(c: &mut Criterion) {
    let dataset = std::fs::read("roaring-bitmap-testdata/bitmapwithruns.bin")
        .or_else(|_| std::fs::read("benches/roaring-bitmap-testdata/bitmapwithruns.bin"))
        .unwrap();
    c.bench_function("RoaringBitmap::from_bytes", |b| {
        b.iter_with_large_drop(|| black_box(RoaringBitmap::from_bytes(black_box(&dataset[..]))))
    });
    let data = RoaringBitmap::from_bytes(&dataset[..]).unwrap().0;
    c.bench_function("RoaringBitmap::contains", |b| {
        b.iter(|| {
            for i in (0..100000).step_by(1000) {
                assert!(black_box(data.contains(i)));
            }
            for i in 100000..200000 {
                assert!(black_box(data.contains(3 * i)));
            }
            for i in 700000..800000 {
                assert!(black_box(data.contains(i)));
            }
        })
    });
}

criterion_group!(benches, roaring_bitmapwithoutruns, roaring_bitmapwithruns);
criterion_main!(benches);
