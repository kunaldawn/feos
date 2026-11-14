#[cfg(feature = "quickcheck")]
use quickcheck::quickcheck;

use crate::internals::{
    decode::{self, u40_from_be_bytes},
    encode::{self, u40_to_be_bytes},
};

fn base64_roundtrip(bytes: &[u8]) {
    let mut encoded = Vec::new();
    encode::write_base64_to_bytes(bytes, &mut encoded).unwrap();
    let mut out = Vec::with_capacity(bytes.len());
    decode::read_base64_from_bytes(&encoded, &mut out).unwrap();
    assert_eq!(&out, bytes);
}

#[test]
fn test_base64_roundtrip() {
    base64_roundtrip(b"");
    base64_roundtrip(b"\0");
    base64_roundtrip(b"a");
    base64_roundtrip(b"ab");
    base64_roundtrip(b"abc");
    base64_roundtrip(b"abcd");
    base64_roundtrip(b"abcde");
    base64_roundtrip(b"abcdef");
    base64_roundtrip(b"abcdefg");
    base64_roundtrip(b"abcdefgh");
}

fn hex_roundtrip(byte: u8) {
    let mut encoded = String::new();
    encode::write_hex_to_string(byte, &mut encoded);
    let out = decode::read_hex_from_string(&encoded).unwrap();
    assert_eq!(out, byte);
    assert_eq!(encoded.len(), 2);
}

#[test]
fn test_hex_roundtrip() {
    for i in 0..255 {
        hex_roundtrip(i);
    }
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_base64_roundtrip(bytes: Vec<u8>) -> bool {
        base64_roundtrip(&bytes);
        true
    }
}

fn roaringbitmap_roundtrip(domain: &[u32]) {
    let mut bitmap = Vec::new();
    encode::write_bitmap_to_bytes(domain, &mut bitmap).unwrap();
    let (bitmap, _) = decode::RoaringBitmap::from_bytes(&bitmap).unwrap();
    if domain.last().copied() > Some(1 << 16) {
        for item in domain.iter().copied() {
            assert!(bitmap.contains(item));
        }
    } else if let Some(last) = domain.last().copied() {
        for item in 0..=last {
            assert_eq!(bitmap.contains(item), domain.binary_search(&item).is_ok());
        }
    }
}

fn roaringbitmap_sizeopt(domain: &[u32]) {
    let mut bitmap = Vec::new();
    encode::write_bitmap_to_bytes(domain, &mut bitmap).unwrap();
    assert!(bitmap.len() < domain.len() * 4);
}

fn be_40bits_roundtrip(domain: &[u64]) {
    let bytes = domain
        .iter()
        .copied()
        .flat_map(u40_to_be_bytes)
        .collect::<Vec<_>>();

    let (chunks, []) = bytes.as_chunks() else {
        panic!("buf is not divisable by 48 bits");
    };
    let result = chunks
        .iter()
        .copied()
        .map(u40_from_be_bytes)
        .collect::<Vec<_>>();
    assert_eq!(&result, domain);
}

#[test]
fn test_be_40bits_roundtrip() {
    be_40bits_roundtrip(&[0, 1, 2, 3, 1 << 39]);
}

// Tests small bitmaps: don't bother checking sizeopt, because
// these are only here to serve as stress tests. The tree files
// special case tiny sets like these anyhow.
#[test]
fn test_roaringbitmap_roundtrip() {
    for domain in &[
        &[][..],
        &[0xfffc_0000, 0xffff_0000][..],
        &[0xfffc_0000, 0xffff_0001],
        &[0xfffc_0000, 0xfffd_0000],
        &[0xfffc_0000, 0xfffd_0001],
        &[0xffff_0000, 0xffff_0001],
    ][..]
    {
        roaringbitmap_roundtrip(domain);
    }
}

// Test large bitmaps have reasonable size
#[test]
fn test_roaringbitmap_roundtrip_and_sizeopt() {
    let mut dense = Vec::new();
    for i in 0u32..65535u32 {
        if i != 0 && (i >> i.trailing_zeros()) != 1 {
            dense.push(i);
        }
    }
    let mut dense2 = Vec::new();
    for i in 0u32..65536u32 {
        dense2.push(i);
    }
    let mut flipflop1 = Vec::new();
    for i in 0u32..65536 {
        if i & 1 == 0 {
            flipflop1.push(i);
        }
    }
    for domain in &[
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10][..],
        &dense,
        &dense2,
        &flipflop1,
    ][..]
    {
        roaringbitmap_roundtrip(domain);
        roaringbitmap_sizeopt(domain);
    }
}

#[test]
#[should_panic]
fn test_roaringbitmap_panic_1() {
    let mut bitmap = Vec::new();
    encode::write_bitmap_to_bytes(&[0xffff_0000, 0xfffd_0000], &mut bitmap).unwrap();
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_roaringbitmap_roundtrip(domain: Vec<u32>) -> bool {
        let mut domain = domain;
        domain.sort();
        domain.dedup();
        roaringbitmap_roundtrip(&domain);
        true
    }
}
