//! Sample rate and channel layout.

/// The format of an interleaved PCM buffer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AudioFormat {
    sample_rate: u32,
    channels: u16,
}

impl AudioFormat {
    /// Creates a format, returning `None` for a zero rate or channel count.
    pub const fn new(sample_rate: u32, channels: u16) -> Option<Self> {
        if sample_rate == 0 || channels == 0 {
            None
        } else {
            Some(Self {
                sample_rate,
                channels,
            })
        }
    }

    /// Single-channel format.
    pub const fn mono(sample_rate: u32) -> Option<Self> {
        Self::new(sample_rate, 1)
    }

    /// Two-channel format.
    pub const fn stereo(sample_rate: u32) -> Option<Self> {
        Self::new(sample_rate, 2)
    }

    /// Samples per second.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Number of interleaved channels per frame.
    pub const fn channels(self) -> u16 {
        self.channels
    }

    /// Number of whole frames needed to hold `ms` milliseconds.
    pub const fn frames_for_ms(self, ms: u64) -> u64 {
        (self.sample_rate as u64)
            .saturating_mul(ms)
            .saturating_div(1000)
    }

    /// Duration of `frames` frames in milliseconds.
    pub const fn duration_ms(self, frames: usize) -> u64 {
        (frames as u64)
            .saturating_mul(1000)
            .saturating_div(self.sample_rate as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_parameters() {
        assert!(AudioFormat::new(0, 1).is_none());
        assert!(AudioFormat::new(48_000, 0).is_none());
    }

    #[test]
    fn duration_round_trips() {
        let format = AudioFormat::stereo(48_000).unwrap();
        assert_eq!(format.frames_for_ms(1_000), 48_000);
        assert_eq!(format.duration_ms(48_000), 1_000);
        assert_eq!(format.frames_for_ms(500), 24_000);
    }

    #[test]
    fn channel_helpers() {
        assert_eq!(AudioFormat::mono(8_000).unwrap().channels(), 1);
        assert_eq!(AudioFormat::stereo(8_000).unwrap().channels(), 2);
    }
}
