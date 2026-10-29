//! Signed 16-bit PCM samples.

/// One signed 16-bit PCM sample.
///
/// `Sample` is the audio counterpart of `feos_gfx::Color`: it keeps the
/// internal representation opaque and provides saturating arithmetic and
/// conversions.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Sample(i16);

impl Sample {
    /// Silence (zero).
    pub const SILENCE: Self = Self(0);
    /// Maximum positive amplitude.
    pub const MAX: Self = Self(i16::MAX);
    /// Maximum negative amplitude.
    pub const MIN: Self = Self(i16::MIN);

    /// Wraps a raw signed 16-bit value.
    pub const fn from_i16(value: i16) -> Self {
        Self(value)
    }

    /// Converts a signed 8-bit sample, widening it to the high byte.
    pub const fn from_i8(value: i8) -> Self {
        Self((value as i16) << 8)
    }

    /// Converts an unsigned 8-bit sample (128 is silence).
    pub const fn from_u8(value: u8) -> Self {
        Self(((value as i16) - 128) << 8)
    }

    /// Converts a normalized `-1.0..=1.0` value, clamping to the sample range.
    pub fn from_f32(value: f32) -> Self {
        let scaled = value.clamp(-1.0, 1.0) * 32_768.0;
        Self(scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16)
    }

    /// Returns the raw signed 16-bit value.
    pub const fn raw(self) -> i16 {
        self.0
    }

    /// Converts to a normalized `-1.0..1.0` value.
    pub fn to_f32(self) -> f32 {
        self.0 as f32 / 32768.0
    }

    /// Absolute amplitude (`0..=32768`).
    pub const fn magnitude(self) -> u16 {
        self.0.unsigned_abs()
    }

    /// Returns `true` when the sample is exactly zero.
    pub const fn is_silence(self) -> bool {
        self.0 == 0
    }

    /// Scales the sample by `gain`, where [`Sample::MAX`] means unchanged and
    /// [`Sample::SILENCE`] means muted. Negative gains invert the phase.
    pub fn gain(self, gain: Self) -> Self {
        let scaled = (self.0 as i32 * gain.0 as i32) / i16::MAX as i32;
        Self(scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
    }

    /// Adds two samples, saturating instead of wrapping.
    pub const fn mix(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl From<i16> for Sample {
    fn from(value: i16) -> Self {
        Self(value)
    }
}

impl From<Sample> for i16 {
    fn from(sample: Sample) -> Self {
        sample.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_pack_and_unpack() {
        assert_eq!(Sample::from_i16(-1234).raw(), -1234);
        assert_eq!(Sample::from_i8(-128).raw(), -32768);
        assert_eq!(Sample::from_i8(127).raw(), 32512);
        assert_eq!(Sample::from_u8(128), Sample::SILENCE);
        assert_eq!(Sample::from_u8(0).raw(), -32768);
    }

    #[test]
    fn float_conversion_clamps() {
        assert_eq!(Sample::from_f32(2.0), Sample::MAX);
        assert_eq!(Sample::from_f32(-2.0), Sample::MIN);
        assert!(Sample::from_f32(0.5).raw() > 16_000);
    }

    #[test]
    fn gain_scales_and_inverts() {
        let half = Sample::from_i16(16_384);
        assert_eq!(half.gain(Sample::MAX), half);
        assert_eq!(half.gain(Sample::SILENCE), Sample::SILENCE);
        assert_eq!(half.gain(Sample::MIN), Sample::from_i16(-16_384));
    }

    #[test]
    fn mix_saturates() {
        assert_eq!(Sample::MAX.mix(Sample::MAX), Sample::MAX);
        assert_eq!(Sample::MIN.mix(Sample::MIN), Sample::MIN);
        assert_eq!(
            Sample::from_i16(10).mix(Sample::from_i16(-3)),
            Sample::from_i16(7)
        );
    }

    #[test]
    fn magnitude_of_minimum_is_32768() {
        assert_eq!(Sample::MIN.magnitude(), 32_768);
    }
}
