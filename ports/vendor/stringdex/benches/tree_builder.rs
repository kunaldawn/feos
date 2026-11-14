use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use stringdex::internals::tree::{encode_search_tree_naive, encode_search_tree_ukkonen};

pub fn tree_builder_naive_vs_multiple(c: &mut Criterion) {
    let dataset = std::fs::read("words")
        .or_else(|_| std::fs::read("benches/words"))
        .unwrap();
    let dataset: Vec<&[u8]> = dataset.split(|c| *c == b'\n').collect();
    c.benchmark_group("words")
        .sample_size(20)
        .measurement_time(Duration::from_secs(30))
        .bench_function("encode_search_tree_ukkonen", |b| {
            b.iter_with_large_drop(|| encode_search_tree_ukkonen(dataset.iter().copied()))
        })
        .bench_function("encode_search_tree_naive", |b| {
            b.iter_with_large_drop(|| encode_search_tree_naive(dataset.iter().copied()))
        });
}

pub fn tree_builder_naive_vs_multiple_mini(c: &mut Criterion) {
    let dataset = std::fs::read("words.mini")
        .or_else(|_| std::fs::read("benches/words.mini"))
        .unwrap();
    let dataset: Vec<&[u8]> = dataset.split(|c| *c == b'\n').collect();
    c.benchmark_group("words.mini")
        .bench_function("encode_search_tree_ukkonen", |b| {
            b.iter_with_large_drop(|| encode_search_tree_ukkonen(dataset.iter().copied()))
        })
        .bench_function("encode_search_tree_naive", |b| {
            b.iter_with_large_drop(|| encode_search_tree_naive(dataset.iter().copied()))
        });
}

criterion_group!(
    benches,
    tree_builder_naive_vs_multiple,
    tree_builder_naive_vs_multiple_mini
);
criterion_main!(benches);
