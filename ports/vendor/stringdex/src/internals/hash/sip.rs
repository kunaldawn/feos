#[derive(Default)]
pub(crate) struct Hasher;

impl super::Hasher for Hasher {
    fn hash(input: &[u8]) -> u64 {
        siphash_of_bytes(input, 0, 0)
    }
}

pub(crate) fn siphash_of_bytes(input: &[u8], k0: u64, k1: u64) -> u64 {
    // hash state
    let mut v: [u64; 4] = [0; 4];
    v[0] = k0 ^ 0x736f6d6570736575;
    v[1] = k1 ^ 0x646f72616e646f6d;
    v[2] = k0 ^ 0x6c7967656e657261;
    v[3] = k1 ^ 0x7465646279746573;
    let input_length = input.len();
    let (main_input, rest) = input.as_chunks();
    // main hash loop
    for mi in main_input.iter().copied().map(u64::from_le_bytes) {
        v[3] ^= mi;
        siphash_compress(&mut v);
        v[0] ^= mi;
    }
    let tail = {
        let mut tail = [0; _];
        tail[..rest.len()].copy_from_slice(rest);
        u64::from_le_bytes(tail)
    };
    let b = (u64::try_from(input_length & 0xff).unwrap() << 56) | tail;
    v[3] ^= b;
    siphash_compress(&mut v);
    v[0] ^= b;
    v[2] ^= 0xff;
    siphash_compress(&mut v);
    siphash_compress(&mut v);
    siphash_compress(&mut v);
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

fn siphash_compress(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_right(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_right(32);
}
