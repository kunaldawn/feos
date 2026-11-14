pub(super) mod sip;

pub trait Hasher {
    fn hash(input: &[u8]) -> u64;
}
