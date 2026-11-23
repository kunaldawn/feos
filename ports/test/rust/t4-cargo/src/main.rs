//! Stage T4: a cargo-built program for feos with vendored crates.io dependencies.
//! Prints "t4-cargo: ok" when every check passed; a failed check panics (status 101).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Report {
    name: String,
    arch: String,
    os: String,
    numbers: Vec<u64>,
    tags: BTreeMap<String, bool>,
    nested: Option<Box<Report>>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // serde + serde_json: a derived round trip.
    let mut tags = BTreeMap::new();
    tags.insert("cargo".to_string(), true);
    tags.insert("vendored".to_string(), true);
    let report = Report {
        name: "t4-cargo".into(),
        arch: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        numbers: (1..=10).map(|n| n * n).collect(),
        tags,
        nested: Some(Box::new(Report {
            name: "inner".into(),
            arch: String::new(),
            os: String::new(),
            numbers: vec![u64::MAX],
            tags: BTreeMap::new(),
            nested: None,
        })),
    };
    let json = serde_json::to_string(&report).expect("serialize");
    let back: Report = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, report);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["numbers"][9], 100);
    assert_eq!(value["nested"]["numbers"][0], u64::MAX);
    println!("json: {}", serde_json::to_string(&value["tags"]).unwrap());
    println!("serde: ok ({} bytes)", json.len());

    // memchr: SIMD searches (SSE2 on x86_64, NEON on AArch64, chosen at run time).
    let mut hay = vec![b'a'; 1 << 20];
    hay[777_777] = b'z';
    hay[999_999] = b'z';
    assert_eq!(memchr::memchr(b'z', &hay), Some(777_777));
    assert_eq!(memchr::memrchr(b'z', &hay), Some(999_999));
    assert_eq!(memchr::memmem::find(b"hello feos world", b"feos"), Some(6));
    assert_eq!(memchr::memchr_iter(b'z', &hay).count(), 2);
    println!("memchr: ok");

    // itoa.
    let mut buf = itoa::Buffer::new();
    assert_eq!(buf.format(-1234567890123i64), "-1234567890123");
    assert_eq!(buf.format(u128::MAX), "340282366920938463463374607431768211455");
    println!("itoa: ok");

    // Arguments, and threads with the dependencies inside them.
    let joined: Vec<_> = (0..4)
        .map(|i| std::thread::spawn(move || serde_json::to_string(&vec![i; 3]).unwrap()))
        .collect();
    let out: Vec<String> = joined.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(out[3], "[3,3,3]");
    println!("args: {:?}", args);
    println!("os: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    println!("t4-cargo: ok");
}
