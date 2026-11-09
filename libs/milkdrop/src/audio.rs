//! What presets hear: the waveform, its spectrum, and MilkDrop's beat values.
//!
//! `bass`, `mid` and `treb` are each band's energy this frame relative to its
//! long-run average, so they hover around 1 whatever the volume, and jump when
//! a beat lands. The `_att` versions are the same, smoothed. Both follow
//! MilkDrop's own rates, adjusted for the frame rate.

use alloc::vec::Vec;

use feos_math as m;

/// Waveform samples per channel that presets see each frame.
pub const SAMPLES: usize = 576;
/// The spectrum's length, in bins from 0 Hz to half the sample rate.
pub const SPECTRUM: usize = 512;
const FFT: usize = SPECTRUM * 2;
/// Band edges in Hz: bass, mid and treble.
const BANDS: [(f32, f32); 3] = [(20.0, 250.0), (250.0, 2_000.0), (2_000.0, 8_000.0)];

/// Analyses audio a frame at a time.
pub struct Analyzer {
    /// The last [`FFT`] mono samples, oldest first.
    history: Vec<f32>,
    /// Left channel, -1 to 1.
    pub wave_left: Vec<f32>,
    /// Right channel, -1 to 1.
    pub wave_right: Vec<f32>,
    /// Magnitudes, roughly 0 to a few for loud music.
    pub spectrum: Vec<f32>,
    window: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    average: [f32; 3],
    long_average: [f32; 3],
    frames: u32,
    sample_rate: f32,
    /// This frame's energy relative to the long-run average.
    pub bass: f32,
    /// See [`Analyzer::bass`].
    pub mid: f32,
    /// See [`Analyzer::bass`].
    pub treb: f32,
    /// The smoothed relative energy.
    pub bass_att: f32,
    /// See [`Analyzer::bass_att`].
    pub mid_att: f32,
    /// See [`Analyzer::bass_att`].
    pub treb_att: f32,
}

fn zeroed(len: usize) -> Option<Vec<f32>> {
    let mut vector = Vec::new();
    vector.try_reserve_exact(len).ok()?;
    vector.resize(len, 0.0);
    Some(vector)
}

impl Analyzer {
    /// An analyser for audio at `sample_rate` Hz, hearing silence so far.
    pub fn new(sample_rate: u32) -> Option<Self> {
        let mut window = zeroed(FFT)?;
        for (index, weight) in window.iter_mut().enumerate() {
            // Hann.
            *weight = 0.5 - 0.5 * m::cos(2.0 * core::f64::consts::PI * index as f64 / FFT as f64) as f32;
        }
        let mut cos = zeroed(FFT / 2)?;
        let mut sin = zeroed(FFT / 2)?;
        for index in 0..FFT / 2 {
            let (s, c) = m::sin_cos(-2.0 * core::f64::consts::PI * index as f64 / FFT as f64);
            cos[index] = c as f32;
            sin[index] = s as f32;
        }
        Some(Self {
            history: zeroed(FFT)?,
            wave_left: zeroed(SAMPLES)?,
            wave_right: zeroed(SAMPLES)?,
            spectrum: zeroed(SPECTRUM)?,
            window,
            cos,
            sin,
            re: zeroed(FFT)?,
            im: zeroed(FFT)?,
            average: [0.0; 3],
            long_average: [0.0; 3],
            frames: 0,
            sample_rate: sample_rate.max(8_000) as f32,
            bass: 1.0,
            mid: 1.0,
            treb: 1.0,
            bass_att: 1.0,
            mid_att: 1.0,
            treb_att: 1.0,
        })
    }

    /// Takes the newest audio, interleaved stereo, and updates everything for
    /// a frame shown at `fps`. Fewer than [`SAMPLES`] frames are padded with
    /// silence at the old end.
    pub fn update(&mut self, interleaved: &[i16], fps: f32) {
        let frames = interleaved.len() / 2;
        // The waveform: the newest SAMPLES frames.
        let skip = frames.saturating_sub(SAMPLES);
        let pad = SAMPLES.saturating_sub(frames);
        for index in 0..SAMPLES {
            let (left, right) = if index < pad {
                (0.0, 0.0)
            } else {
                let at = (skip + index - pad) * 2;
                (interleaved[at] as f32 / 32768.0, interleaved[at + 1] as f32 / 32768.0)
            };
            self.wave_left[index] = left;
            self.wave_right[index] = right;
        }
        // The spectrum's input: history, then the newest frames.
        let fresh = frames.min(FFT);
        self.history.copy_within(fresh.., 0);
        let base = FFT - fresh;
        let first = frames - fresh;
        for index in 0..fresh {
            let at = (first + index) * 2;
            self.history[base + index] = (interleaved[at] as f32 + interleaved[at + 1] as f32) / 65536.0;
        }
        self.transform();
        self.beats(fps);
    }

    fn transform(&mut self) {
        let bits = FFT.trailing_zeros();
        for index in 0..FFT {
            let reversed = index.reverse_bits() >> (usize::BITS - bits);
            self.re[reversed] = self.history[index] * self.window[index];
            self.im[reversed] = 0.0;
        }
        let mut size = 2;
        while size <= FFT {
            let half = size / 2;
            let stride = FFT / size;
            let mut start = 0;
            while start < FFT {
                for k in 0..half {
                    let (wr, wi) = (self.cos[k * stride], self.sin[k * stride]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = self.re[b] * wr - self.im[b] * wi;
                    let ti = self.re[b] * wi + self.im[b] * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
                start += size;
            }
            size *= 2;
        }
        // A full-scale sine lands at about FFT/4 after the Hann window; this
        // puts loud music's peaks at a few.
        let scale = 16.0 / FFT as f32;
        for bin in 0..SPECTRUM {
            let power = self.re[bin] * self.re[bin] + self.im[bin] * self.im[bin];
            self.spectrum[bin] = m::f32::sqrt(power) * scale;
        }
    }

    fn beats(&mut self, fps: f32) {
        let fps = fps.clamp(5.0, 120.0);
        let hz_per_bin = self.sample_rate / FFT as f32;
        let mut immediate = [0.0f32; 3];
        for (band, &(low, high)) in BANDS.iter().enumerate() {
            let first = ((low / hz_per_bin) as usize).max(1);
            let last = ((high / hz_per_bin) as usize).clamp(first + 1, SPECTRUM);
            immediate[band] = self.spectrum[first..last].iter().sum::<f32>();
        }
        // MilkDrop's rates are tuned for 30 fps.
        let adjust = |rate: f32| m::f32::pow(rate, 30.0 / fps);
        self.frames = self.frames.saturating_add(1);
        let mut relative = [1.0f32; 3];
        let mut relative_average = [1.0f32; 3];
        for band in 0..3 {
            let rate = adjust(if immediate[band] > self.average[band] { 0.2 } else { 0.5 });
            self.average[band] = self.average[band] * rate + immediate[band] * (1.0 - rate);
            let rate = adjust(if self.frames < 50 { 0.9 } else { 0.992 });
            self.long_average[band] = self.long_average[band] * rate + immediate[band] * (1.0 - rate);
            if self.long_average[band] >= 0.001 {
                relative[band] = immediate[band] / self.long_average[band];
                relative_average[band] = self.average[band] / self.long_average[band];
            }
        }
        [self.bass, self.mid, self.treb] = relative;
        [self.bass_att, self.mid_att, self.treb_att] = relative_average;
    }

    /// The overall loudness, as the beat values see it.
    pub fn volume(&self) -> f32 {
        (self.bass + self.mid + self.treb) / 3.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn tone(hz: f32, frames: usize, offset: usize, amplitude: f32) -> Vec<i16> {
        let mut out = Vec::new();
        for index in 0..frames {
            let t = (offset + index) as f32 / 44_100.0;
            let value = (amplitude * (2.0 * core::f32::consts::PI * hz * t).sin() * 32767.0) as i16;
            out.push(value);
            out.push(value);
        }
        out
    }

    #[test]
    fn a_tone_peaks_in_its_bin() {
        let mut analyzer = Analyzer::new(44_100).unwrap();
        analyzer.update(&tone(1_000.0, 1024, 0, 0.8), 30.0);
        let peak = (0..SPECTRUM)
            .max_by(|a, b| analyzer.spectrum[*a].total_cmp(&analyzer.spectrum[*b]))
            .unwrap();
        let hz = peak as f32 * 44_100.0 / FFT as f32;
        assert!((hz - 1_000.0).abs() < 50.0, "peak at {hz} Hz");
        assert!(analyzer.wave_left.iter().any(|s| *s > 0.7));
    }

    #[test]
    fn silence_reads_as_one_and_a_beat_rises_above_it() {
        let mut analyzer = Analyzer::new(44_100).unwrap();
        analyzer.update(&[0; 1152], 30.0);
        assert_eq!((analyzer.bass, analyzer.mid, analyzer.treb), (1.0, 1.0, 1.0));
        // Quiet bass for a while, then a loud hit.
        let mut offset = 0;
        for _ in 0..120 {
            analyzer.update(&tone(80.0, 1470, offset, 0.1), 30.0);
            offset += 1470;
        }
        assert!((analyzer.bass - 1.0).abs() < 0.3, "steady bass {}", analyzer.bass);
        analyzer.update(&tone(80.0, 1470, offset, 0.9), 30.0);
        assert!(analyzer.bass > 2.0, "beat {}", analyzer.bass);
        assert!(analyzer.bass_att > 1.0 && analyzer.bass_att < analyzer.bass);
    }

    #[test]
    fn short_input_pads_rather_than_panics() {
        let mut analyzer = Analyzer::new(48_000).unwrap();
        analyzer.update(&[1000, -1000, 5], 60.0);
        assert_eq!(analyzer.wave_left[SAMPLES - 1], 1000.0 / 32768.0);
        assert_eq!(analyzer.wave_left[0], 0.0);
        analyzer.update(&[], 60.0);
    }
}
