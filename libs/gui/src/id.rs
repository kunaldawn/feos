//! Stable, allocation-free widget identifiers.

use core::hash::{Hash, Hasher};

/// The identifier of one widget within a frame.
///
/// IDs are produced by [`Ui`](crate::Ui) from the widget order and from any
/// enclosing [`Ui::with_id`](crate::Ui::with_id) scope. They are stable across
/// frames as long as the structure of the interface is stable, which is what
/// immediate-mode toolkits require for focus and drag state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Id(u64);

impl Id {
    /// A sentinel that is never returned for a real widget.
    pub const NONE: Self = Self(0);

    /// Wraps a raw identifier.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns `true` for the sentinel ID.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Derives a child identifier from this one.
    pub fn child(self, salt: u64) -> Self {
        Self(mix(self.0 ^ salt) | 1)
    }

    /// Hashes a string into an identifier.
    pub fn from_name(value: &str) -> Self {
        let mut hasher = Fnv1a::default();
        value.hash(&mut hasher);
        Self(mix(hasher.finish()) | 1)
    }
}

/// Deterministic SplitMix64 finalizer.
pub(crate) const fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// FNV-1a, used only for stable string hashing (not for security).
#[derive(Default)]
struct Fnv1a(u64);

impl Hasher for Fnv1a {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = hash;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_nonzero_and_stable() {
        assert!(!Id::from_name("play").is_none());
        assert_eq!(Id::from_name("play"), Id::from_name("play"));
        assert_ne!(Id::from_name("play"), Id::from_name("pause"));
        assert!(!Id::NONE.child(1).is_none());
    }
}
