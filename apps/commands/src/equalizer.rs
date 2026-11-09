//! A ten-band graphic equaliser, with a preamp and a balance control.
//!
//! The bands are Winamp's: 60 Hz to 16 kHz, each a peaking filter (RBJ's
//! cookbook biquad, an octave wide) that boosts or cuts by up to 12 dB. The
//! preamp moves everything by up to 12 dB before them. Balance turns one
//! channel down. It works on the decoder's own samples, at their own rate, so
//! [`crate::audio_worker`] runs it between decoding and the mixer.
//!
//! Two things keep it clean:
//!
//! - **Headroom.** Music is mastered to within a fraction of a dB of full scale,
//!   so a boosted band would push it past and be clipped flat, which is how a
//!   bass boost turns into distortion. The bands' curve is measured whenever it
//!   changes, and the level comes down by its highest peak: boosted frequencies
//!   rise relative to the rest instead of past the top. The preamp is left as
//!   set, as the listener's own level control.
//! - **Precision.** The filters run in `f64`. A 60 Hz band's poles sit within a
//!   few thousandths of the unit circle, where `f32` rounding is amplified to
//!   about -58 dB of noise and error, in the bass, where it's heard.

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
const Q: f64 = 1.41;
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

/// Points the curve is measured at, spread evenly in pitch from 20 Hz.
const CURVE_POINTS: usize = 240;

/// One second-order section, in transposed direct form II.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    state: [[f64; 2]; MAX_CHANNELS],
    active: bool,
}

impl Biquad {
    fn peaking(rate: f64, hz: f64, db: f64) -> Self {
        // A band at or above Nyquist can't be shaped; leave it out.
        if db.abs() < 0.05 || hz >= rate * 0.45 {
            return Self::default();
        }
        let a = m::pow(10.0, db / 40.0);
        let w0 = 2.0 * core::f64::consts::PI * hz / rate;
        let (sin, cos) = m::sin_cos(w0);
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

    /// How much this section scales a sine at `w` radians per sample.
    fn magnitude(&self, w: f64) -> f64 {
        if !self.active {
            return 1.0;
        }
        let (sin1, cos1) = m::sin_cos(w);
        let (sin2, cos2) = m::sin_cos(2.0 * w);
        let (num_re, num_im) = (self.b0 + self.b1 * cos1 + self.b2 * cos2, self.b1 * sin1 + self.b2 * sin2);
        let (den_re, den_im) = (1.0 + self.a1 * cos1 + self.a2 * cos2, self.a1 * sin1 + self.a2 * sin2);
        m::sqrt((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im))
    }

    #[inline]
    fn run(&mut self, channel: usize, x: f64) -> f64 {
        let state = &mut self.state[channel];
        let y = self.b0 * x + state[0];
        state[0] = self.b1 * x - self.a1 * y + state[1];
        state[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// The filters for one stream.
pub struct Equalizer {
    rate: f64,
    channels: usize,
    settings: EqSettings,
    /// The preamp and the headroom together, as one gain.
    preamp: f64,
    gains: [f64; MAX_CHANNELS],
    filters: [Biquad; BANDS],
}

impl Equalizer {
    /// An equaliser for interleaved audio at `rate` Hz with `channels`
    /// channels (1 or 2; more are passed through after the first two).
    pub fn new(rate: u32, channels: u16, settings: EqSettings) -> Self {
        let mut equalizer = Self {
            rate: rate.max(1) as f64,
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
        let preamp = if enabled {
            m::pow(10.0, settings.preamp_db.clamp(-RANGE_DB, RANGE_DB) as f64 / 20.0)
        } else {
            1.0
        };
        for (band, filter) in self.filters.iter_mut().enumerate() {
            let db = if enabled {
                settings.bands_db[band].clamp(-RANGE_DB, RANGE_DB)
            } else {
                0.0
            };
            *filter = Biquad::peaking(self.rate, BAND_HZ[band] as f64, db as f64);
            filter.state = old[band].state;
        }
        self.preamp = preamp / self.peak_gain().max(1.0);
        let balance = settings.balance.clamp(-1.0, 1.0) as f64;
        self.gains = [(1.0 - balance).min(1.0), (1.0 + balance).min(1.0)];
        if self.channels == 1 {
            self.gains = [1.0; MAX_CHANNELS];
        }
    }

    /// The bands' highest gain, over the audible range.
    fn peak_gain(&self) -> f64 {
        if self.filters.iter().all(|filter| !filter.active) {
            return 1.0;
        }
        let top = (self.rate * 0.49).min(20_000.0);
        let span = m::ln(top / 20.0);
        let mut peak: f64 = 0.0;
        let mut measure = |hz: f64| {
            let w = 2.0 * core::f64::consts::PI * hz / self.rate;
            let gain: f64 = self.filters.iter().map(|filter| filter.magnitude(w)).product();
            peak = peak.max(gain);
        };
        for point in 0..=CURVE_POINTS {
            measure(20.0 * m::exp(span * point as f64 / CURVE_POINTS as f64));
        }
        for hz in BAND_HZ {
            if (hz as f64) < top {
                measure(hz as f64);
            }
        }
        peak
    }

    /// The gain the headroom takes off, in dB (0 or less).
    pub fn headroom_db(&self) -> f32 {
        if !self.settings.enabled {
            return 0.0;
        }
        -(20.0 * m::log10(self.peak_gain().max(1.0))) as f32
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
                let mut x = *sample as f64 * self.preamp;
                for filter in &mut self.filters {
                    if filter.active {
                        x = filter.run(channel, x);
                    }
                }
                x *= self.gains[channel];
                // Rounded, not truncated, so silence stays silence and there's no
                // half-step bias. Only a preamp boost can still reach the clamp.
                *sample = m::round(x).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
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

    fn gain_db(settings: EqSettings, hz: f32, rate: u32) -> f32 {
        let mut equalizer = Equalizer::new(rate, 2, settings);
        let mut samples = sine(hz, rate, rate as usize, 2);
        let before = rms(&samples, 2, 0);
        equalizer.process(&mut samples);
        20.0 * (rms(&samples, 2, 0) / before).log10()
    }

    #[test]
    fn a_boosted_band_rises_12_db_over_the_rest_and_its_neighbours_barely_move() {
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.bands_db[4] = 12.0; // 1 kHz
        let boosted = gain_db(settings, 1000.0, 44_100);
        let far = gain_db(settings, 60.0, 44_100);
        assert!((boosted - far - 12.0).abs() < 0.3, "1 kHz rose {} dB over 60 Hz", boosted - far);
        // The headroom took the boost back off the top.
        assert!(boosted.abs() < 0.3, "1 kHz came out at {boosted} dB");
        let equalizer = Equalizer::new(44_100, 2, settings);
        assert!((equalizer.headroom_db() + 12.0).abs() < 0.3, "{}", equalizer.headroom_db());
    }

    #[test]
    fn a_bass_boost_on_full_scale_music_does_not_clip() {
        for (name, bands) in PRESETS {
            let settings = EqSettings { enabled: true, preamp_db: 0.0, bands_db: *bands, balance: 0.0 };
            let mut equalizer = Equalizer::new(44_100, 2, settings);
            // Bass, mid and treble together, peaking just under full scale.
            let mut samples: Vec<i16> = (0..44_100)
                .flat_map(|index| {
                    let t = index as f32 / 44_100.0;
                    let x = (2.0 * core::f32::consts::PI * 60.0 * t).sin() * 0.6
                        + (2.0 * core::f32::consts::PI * 1000.0 * t).sin() * 0.2
                        + (2.0 * core::f32::consts::PI * 9000.0 * t).sin() * 0.15;
                    let sample = (x * 32_000.0) as i16;
                    [sample, sample]
                })
                .collect();
            equalizer.process(&mut samples);
            let loudest = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            assert!(loudest < 32_767, "{name} clipped");
        }
    }

    #[test]
    fn cuts_need_no_headroom_and_bass_is_precise() {
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.bands_db = [-6.0; BANDS];
        assert_eq!(Equalizer::new(48_000, 2, settings).headroom_db(), 0.0);
        // A lone 60 Hz boost is exactly as the filter's maths says, where the
        // poles are closest to the unit circle.
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.bands_db[0] = 9.0;
        let at_60 = gain_db(settings, 60.0, 48_000);
        let at_3k = gain_db(settings, 3000.0, 48_000);
        assert!((at_60 - at_3k - 9.0).abs() < 0.1, "60 Hz rose {} dB", at_60 - at_3k);
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
    fn bands_above_nyquist_are_skipped_and_a_preamp_boost_can_still_clip() {
        let mut settings = EqSettings::FLAT;
        settings.enabled = true;
        settings.preamp_db = 12.0;
        settings.bands_db = [12.0; BANDS];
        let mut equalizer = Equalizer::new(22_050, 1, settings);
        assert!(!equalizer.filters[9].active, "16 kHz can't exist at 22 kHz");
        // The preamp is the listener's level control: it isn't taken back.
        let mut samples = sine(1000.0, 22_050, 4000, 1);
        for sample in &mut samples {
            *sample = sample.saturating_mul(4);
        }
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
