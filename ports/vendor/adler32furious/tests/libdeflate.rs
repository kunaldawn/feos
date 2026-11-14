//! Differential tests against libdeflate's own Adler-32.

use adler32furious::{Adler32, Implementation, adler32, implementations};
use proptest::prelude::*;

fn with(imp: Implementation, data: &[u8]) -> u32 {
    let mut sum = Adler32::new();
    imp.update(&mut sum, data);
    sum.checksum()
}

fn reference(data: &[u8]) -> u32 {
    let mut a = libdeflater::Adler32::new();
    a.update(data);
    a.sum()
}

#[test]
fn large_buffers() {
    // Long enough to hit the realignment paths, which only kick in above 64 KiB.
    let mut x = 0x9E37_79B9u32;
    let data: Vec<u8> = (0..(1 << 20) + 77)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    for offset in [0, 1, 31, 63] {
        let d = &data[offset..];
        let want = reference(d);
        assert_eq!(adler32(d), want, "offset={offset}");
        for imp in implementations() {
            assert_eq!(with(imp, d), want, "{} offset={offset}", imp.name());
        }
    }
}

proptest! {
    #[test]
    fn matches_libdeflate(
        data in proptest::collection::vec(any::<u8>(), 0..20_000),
        offset in 0usize..64,
    ) {
        let data = &data[offset.min(data.len())..];
        let want = reference(data);
        prop_assert_eq!(adler32(data), want);
        for imp in implementations() {
            prop_assert_eq!(with(imp, data), want, "{}", imp.name());
        }
    }

    #[test]
    fn split_update_and_combine(
        data in proptest::collection::vec(any::<u8>(), 0..20_000),
        split in any::<prop::sample::Index>(),
    ) {
        let (a, b) = data.split_at(split.index(data.len() + 1));
        let want = reference(&data);

        let mut sum = Adler32::new();
        sum.update(a);
        sum.update(b);
        prop_assert_eq!(sum.checksum(), want);

        let mut sum = Adler32::from_checksum(adler32(a));
        sum.combine(Adler32::from_checksum(adler32(b)), b.len() as u64);
        prop_assert_eq!(sum.checksum(), want);
    }
}
