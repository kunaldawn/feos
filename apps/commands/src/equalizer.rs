//! A ten-band graphic equaliser, with a preamp and a balance control.
//!
//! The bands are Winamp's: 60 Hz to 16 kHz, each a peaking filter (RBJ's
//! cookbook biquad, an octave wide) that boosts or cuts by up to 12 dB. The
//! preamp moves everything by up to 12 dB before them. Balance turns one
//! channel down. It works on the decoder's own samples, at their own rate, so
//! [`crate::audio_worker`] runs it between decoding and the mixer.

use feos_math as m;

/// The centre of each band, in Hz.
pub const BAND_HZ: [f32; BANDS] = [
    60.0, 170.0, 310.0, 600.0, 1_000.0, 3_000.0, 6_000.0, 12_000.0, 14_000.0, 16_000.0,
];
/// How many bands.
pub const BANDS: usize = 10;
/// The most a band or the preamp boosts or cuts, in dB.
pub const RANGE_DB: f32 = 12.0;
/// Each band's width: Q for about an octave.
const Q: f32 = 1.41;
/// Channels the equaliser keeps filter state for.
const MAX_CHANNELS: usize = 2;

/// What the listener has set.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EqSettings {
    /// Whether the bands and preamp apply.
    pub enabled: bool,
    /// The preamp, in dB.
    pub preamp_db: f32,
    /// Each band's boost (positive) or cut, in dB.
    pub bands_db: [f32; BANDS],
    /// -1 is the left only, 1 the right only.
    pub balance: f32,
}

impl EqSettings {
    /// Off, flat and centred.
    pub const FLAT: Self = Self {
        enabled: false,
        preamp_db: 0.0,
        bands_db: [0.0; BANDS],
        balance: 0.0,
    };

    /// Whether these change the sound at all.
    pub fn is_neutral(&self) -> bool {
        self.balance.abs() < 0.01
            && (!self.enabled || (self.preamp_db.abs() < 0.05 && self.bands_db.iter().all(|db| db.abs() < 0.05)))
    }
}

impl Default for EqSettings {
    fn default() -> Self {
        Self::FLAT
    }
}

/// One second-order section, in transposed direct form II.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    state: [[f32; 2]; MAX_CHANNELS],
    active: bool,
}

impl Biquad {
    fn peaking(rate: f32, hz: f32, db: f32) -> Self {
        // A band at or above Nyquist can't be shaped; leave it out.
        if db.abs() < 0.05 || hz >= rate * 0.45 {
            return Self::default();
        }
        let a = m::f32::pow(10.0, db / 40.0);
        let w0 = 2.0 * core::f32::consts::PI * hz / rate;
        let (sin, cos) = m::f32::sin_cos(w0);
        let alpha = sin / (2.0 * Q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: -2.0 * cos / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha / a) / a0,
            state: [[0.0; 2]; MAX_CHANNELS],
            active: true,
        }
    }

    #[inline]
    fn run(&mut self, channel: usize, x: f32) -> f32 {
        let state = &mut self.state[channel];
        let y = self.b0 * x + state[0];
        state[0] = self.b1 * x - self.a1 * y + state[1];
        state[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// The filters for one stream.
pub struct Equalizer {
    rate: f32,
    channels: usize,
    settings: EqSettings,
    preamp: f32,
    gains: [f32; MAX_CHANNELS],
    filters: [Biquad; BANDS],
}

impl Equalizer {
    /// An equaliser for interleaved audio at `rate` Hz with `channels`
    /// channels (1 or 2; more are passed through after the first two).
    pub fn new(rate: u32, channels: u16, settings: EqSettings) -> Self {
        let mut equalizer = Self {
            rate: rate.max(1) as f32,
            channels: channels.max(1) as usize,
            settings,
            preamp: 1.0,
            gains: [1.0; MAX_CHANNELS],
            filters: [Biquad::default(); BANDS],
        };
        equalizer.set(settings);
        equalizer
    }

    /// The rate and channels this was made for.
    pub fn format(&self) -> (u32, usize) {
        (self.rate as u32, self.channels)
    }

    /// The settings in use.
    pub fn settings(&self) -> EqSettings {
        self.settings
    }

    /// Changes the settings. Filter state carries over, so it doesn't click.
    pub fn set(&mut self, settings: EqSettings) {
        let old = self.filters;
        self.settings = settings;
        let enabled = settings.enabled;
        self.preamp = if enabled {
            m::f32::pow(10.0, settings.preamp_db.clamp(-RANGE_DB, RANGE_DB) / 20.0)
        } else {
            1.0
        };
        for (band, filter) in self.filters.iter_mut().enumerate() {
            let db = if enabled {
                settings.bands_db[band].clamp(-RANGE_DB, RANGE_DB)
            } else {
                0.0
            };
            *filter = Biquad::peaking(self.rate, BAND_HZ[band], db);
            filter.state = old[band].state;
        }
        let balance = settings.balance.clamp(-1.0, 1.0);
        self.gains = [(1.0 - balance).min(1.0), (1.0 + balance).min(1.0)];
        if self.channels == 1 {
            self.gains = [1.0; MAX_CHANNELS];
        }
    }

    /// Forgets the filters' history, as after a seek.
    pub fn reset(&mut self) {
        for filter in &mut self.filters {
            filter.state = [[0.0; 2]; MAX_CHANNELS];
        }
    }

    /// Equalises interleaved `samples` in place.
    pub fn process(&mut self, samples: &mut [i16]) {
        if self.settings.is_neutral() {
            return;
        }
        let channels = self.channels;
        for frame in samples.chunks_exact_mut(channels) {
            for (channel, sample) in frame.iter_mut().enumerate().take(MAX_CHANNELS) {
                let mut x = *sample as f32 * self.preamp;
                for filter in &mut self.filters {
                    if filter.active {
                        x = filter.run(channel, x);
                    }
                }
                x *= self.gains[channel];
                *sample = x.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
    }
}

/// Winamp-style presets, in dB for each band.
pub const PRESETS: &[(&str, [f32; BANDS])] = &[
    ("Flat", [0.0; BANDS]),
    ("Rock", [5.0, 3.0, -3.0, -5.0, -2.0, 2.0, 5.0, 7.0, 7.0, 7.0]),
    ("Pop", [-1.0, 3.0, 5.0, 5.5, 3.5, -1.0, -1.5, -1.5, -1.0, -1.0]),
    ("Dance", [6.0, 4.5, 1.5, 0.0, 0.0, -3.5, -4.5, -4.5, 0.0, 0.0]),
    ("Techno", [5.0, 3.5, 0.0, -3.5, -3.0, 0.0, 5.0, 6.0, 6.0, 5.5]),
    ("Classical", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -4.5, -4.5, -4.5, -6.0]),
    ("Club", [0.0, 0.0, 5.0, 3.5, 3.5, 3.5, 2.0, 0.0, 0.0, 0.0]),
    ("Full Bass", [6.0, 6.0, 6.0, 3.5, 1.0, -2.5, -5.0, -6.0, -6.5, -6.5]),
    ("Full Treble", [-6.0, -6.0, -6.0, -2.5, 1.5, 6.5, 9.5, 9.5, 9.5, 10.0]),
    ("Laptop Speakers", [3.0, 6.5, 3.0, -2.0, -1.5, 1.0, 3.0, 6.0, 8.0, 9.0]),
    ("Live", [-3.0, 0.0, 2.5, 3.0, 3.0, 3.0, 2.5, 1.5, 1.5, 1.5]),
    ("Reggae", [0.0, 0.0, -0.5, -3.5, 0.0, 4.0, 4.0, 0.0, 0.0, 0.0]),
    ("Soft", [3.0, 1.0, -1.0, -1.5, -1.0, 1.5, 5.0, 6.0, 7.0, 7.5]),
];

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn sine(hz: f32, rate: u32, frames: usize, channels: usize) -> Vec<i16> {
        let mut out = Vec::new();
        for index in 0..frames {
            let value = ((2.0 * core::f32::consts::PI * hz * index as f32 / rate as f32).sin() * 8000.0) as i16;
            for _ in 0..channels {
                out.push(value);
            }
        }
        out
    }

    fn rms(samples: &[i16], channels: usize, channel: usize) -> f32 {
        let tail: Vec<f32> = samples
            .chunks_exact(channels)
            .skip(samples.len() / channels / 2)
            .map(|frame| frame[channel] as f32)
            .collect();
        (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt()
    }

    #[test]
    fn neutral_settings_leave_the_sound_alone() {
        let mut equalizer = Equalizer::new(44_100, 2, EqSettings::FLAT);
        let original = sine(1000.0, 44_100, 2000, 2);
        let mut samples = original.clone();
        equalizer.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn a_boosted_band_is_louder_and_its_neighbours_barely_move() {
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.bands_db[4] = 12.0; // 1 kHz
        let mut equalizer = Equalizer::new(44_100, 2, settings);
        let mut hit = sine(1000.0, 44_100, 8000, 2);
        let before = rms(&hit, 2, 0);
        equalizer.process(&mut hit);
        let gain = 20.0 * (rms(&hit, 2, 0) / before).log10();
        assert!((gain - 12.0).abs() < 1.0, "1 kHz moved {gain} dB");

        let mut equalizer = Equalizer::new(44_100, 2, settings);
        let mut far = sine(60.0, 44_100, 16000, 2);
        let before = rms(&far, 2, 0);
        equalizer.process(&mut far);
        let gain = 20.0 * (rms(&far, 2, 0) / before).log10();
        assert!(gain.abs() < 1.0, "60 Hz moved {gain} dB");
    }

    #[test]
    fn preamp_and_balance() {
        let settings = EqSettings {
            enabled: true,
            preamp_db: -6.0,
            bands_db: [0.0; BANDS],
            balance: 1.0,
        };
        let mut equalizer = Equalizer::new(48_000, 2, settings);
        let mut samples = sine(440.0, 48_000, 4000, 2);
        let before = rms(&samples, 2, 1);
        equalizer.process(&mut samples);
        assert!(rms(&samples, 2, 0) < 1.0, "the left is silent");
        let gain = 20.0 * (rms(&samples, 2, 1) / before).log10();
        assert!((gain + 6.0).abs() < 0.2, "the right moved {gain} dB");
    }

    #[test]
    fn bands_above_nyquist_are_skipped_and_loud_boosts_clip() {
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.preamp_db = 12.0;
        settings.bands_db = [12.0; BANDS];
        let mut equalizer = Equalizer::new(22_050, 1, settings);
        assert!(!equalizer.filters[9].active, "16 kHz can't exist at 22 kHz");
        let mut samples = sine(60.0, 22_050, 4000, 1);
        equalizer.process(&mut samples);
        assert!(samples.iter().any(|s| *s == i16::MAX));
    }

    #[test]
    fn presets_stay_in_range() {
        for (name, bands) in PRESETS {
            assert!(bands.iter().all(|db| db.abs() <= RANGE_DB), "{name}");
        }
    }
}
