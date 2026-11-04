//! Streaming sample-rate and channel conversion, in integers.
//!
//! The sound driver takes one fixed format, [`DRIVER_FORMAT`] (48 kHz stereo).
//! [`Resampler`] turns any decoded stream into that, or any other mono or
//! stereo format. It owns no heap memory: its state is about 2 KiB inline, and
//! the caller supplies all input and output.
//!
//! # Method
//!
//! Channels are mixed first, then the rate is converted by a polyphase
//! windowed-sinc filter: a Kaiser window (beta 8) with 16 zero crossings per
//! side, stored as 256 phases per zero crossing in Q30 and linearly interpolated
//! between phases. The arithmetic is entirely integer, and the rate ratio is
//! tracked exactly as a fraction, so there is no drift over long streams.
//!
//! # Quality
//!
//! Frequencies below are fractions of the lower of the two rates:
//!
//! | Response | Frequency |
//! |---|---|
//! | Within 0.1 dB | up to 0.40 (17.7 kHz at 44.1 kHz) |
//! | -6 dB | 0.46 |
//! | At least 80 dB down | above 0.54 |
//!
//! Gain is normalised per output sample, so DC passes exactly. 44.1 kHz to
//! 48 kHz costs 32 multiplies per sample, each channel. Equal rates bypass the
//! filter altogether and pass samples unchanged. When downsampling, the filter
//! widens with the ratio to keep its stopband, which is why the ratio is limited
//! to [`MAX_DOWN_RATIO`]: the state has to hold that much history.
//!
//! # Channel mixing
//!
//! Input channels are taken in WAVE order, which is what `feos-audio-codec`
//! produces. Mono goes to both stereo channels at unit gain. Stereo goes to
//! mono as the average. Surround folds to stereo with the centre and the
//! surrounds at -3 dB relative to the front pair, and without the LFE, all scaled
//! so that the mix cannot clip.

use crate::format::AudioFormat;

/// The only format the sound driver accepts.
pub const DRIVER_FORMAT: AudioFormat = match AudioFormat::stereo(48_000) {
    Some(format) => format,
    None => panic!("the driver format is valid"),
};

/// The largest input rate divided by output rate that [`Resampler`] accepts.
pub const MAX_DOWN_RATIO: u32 = 8;
/// The most input channels that can be mixed down.
pub const MAX_INPUT_CHANNELS: u16 = 8;

const ZERO_CROSSINGS: usize = 16;
const PHASE_BITS: u32 = 8;
/// Bits of fraction in a position in the table.
const POSITION_BITS: u32 = 16;
/// Input frames either side of the output position, at the largest ratio.
const MAX_WING: usize = ZERO_CROSSINGS * MAX_DOWN_RATIO as usize;
/// History per channel, stored twice so a window is always contiguous.
const HISTORY: usize = 2 * MAX_WING;

/// One wing of the symmetric filter in Q30: `c sinc(c t) kaiser(t / 16)` for
/// `t` from 0 to 16 in steps of 1/256, with `c` = 0.92. The tests recompute it.
static FILTER: [i32; (ZERO_CROSSINGS << PHASE_BITS) + 1] = include!("resample_table.in");

/// How far a call to [`Resampler::process`] got.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    /// Input samples used (all channels). Always a whole number of frames.
    pub consumed: usize,
    /// Output samples written (all channels). Always a whole number of frames.
    pub produced: usize,
}

/// Mix weights in Q15: for each input channel, its share of left and right.
type Mix = [(i32, i32); MAX_INPUT_CHANNELS as usize];

const fn downmix(table: &[(i32, i32)]) -> Mix {
    let mut mix = [(0, 0); MAX_INPUT_CHANNELS as usize];
    let mut index = 0;
    while index < table.len() {
        mix[index] = table[index];
        index += 1;
    }
    mix
}

/// A converter from one format to another.
pub struct Resampler {
    input_channels: usize,
    output_channels: usize,
    input_rate: u64,
    output_rate: u64,
    mix: Mix,
    /// Output steps, in input frames: whole part and numerator over the output
    /// rate.
    step: u64,
    remainder: u64,
    /// The fraction of an input frame past the centre, over the output rate.
    fraction: u64,
    /// New input frames needed before the next output frame can be made.
    need: u64,
    /// Filter half length, in input frames.
    wing: usize,
    /// `fraction` to table position scale, and table steps per input frame.
    denominator: u64,
    position_step: u64,
    /// Next write index in `history`, below `window`.
    head: usize,
    /// Window length, `2 * wing`.
    window: usize,
    /// Twice `window` samples per channel; every sample is written twice.
    history: [[i16; 2 * HISTORY]; 2],
    /// Stream totals, in frames, so that [`Self::flush`] knows when to stop.
    total_in: u64,
    total_out: u64,
}

impl Resampler {
    /// Creates a converter, or `None` for a conversion it cannot do: more than
    /// [`MAX_INPUT_CHANNELS`] in, more than two out, or an input rate above
    /// [`MAX_DOWN_RATIO`] times the output rate.
    pub fn new(input: AudioFormat, output: AudioFormat) -> Option<Self> {
        let input_channels = input.channels();
        let output_channels = output.channels();
        if input_channels > MAX_INPUT_CHANNELS || output_channels > 2 {
            return None;
        }
        let input_rate = input.sample_rate() as u64;
        let output_rate = output.sample_rate() as u64;
        if input_rate > output_rate * MAX_DOWN_RATIO as u64 {
            return None;
        }
        let denominator = input_rate.max(output_rate);
        let wing = ((ZERO_CROSSINGS as u64 * input_rate).div_ceil(output_rate) as usize)
            .max(ZERO_CROSSINGS)
            .min(MAX_WING);
        let mut resampler = Self {
            input_channels: input_channels as usize,
            output_channels: output_channels as usize,
            input_rate,
            output_rate,
            mix: mix_for(input_channels, output_channels),
            step: input_rate / output_rate,
            remainder: input_rate % output_rate,
            fraction: 0,
            need: 0,
            wing,
            denominator,
            position_step: (output_rate << (PHASE_BITS + POSITION_BITS)) / denominator,
            head: 0,
            window: 2 * wing,
            history: [[0; 2 * HISTORY]; 2],
            total_in: 0,
            total_out: 0,
        };
        resampler.reset();
        Some(resampler)
    }

    /// Creates a converter to [`DRIVER_FORMAT`].
    pub fn to_driver(input: AudioFormat) -> Option<Self> {
        Self::new(input, DRIVER_FORMAT)
    }

    /// Forgets all history, ready for a new stream or a seek.
    pub fn reset(&mut self) {
        self.history = [[0; 2 * HISTORY]; 2];
        self.head = 0;
        self.fraction = 0;
        // The first output frame lines up with the first input frame, which
        // has to be the frame just left of centre in the window: `wing` frames
        // after it are needed to complete the window.
        self.need = self.wing as u64 + 1;
        self.total_in = 0;
        self.total_out = 0;
    }

    /// Whether this converter skips the filter because the rates are equal.
    pub fn is_bypass(&self) -> bool {
        self.input_rate == self.output_rate
    }

    /// The most output samples `input_samples` of input can produce, for
    /// sizing `output`. A call to [`Self::process`] stops when either side
    /// runs out, so a smaller buffer is also fine.
    pub fn max_output_samples(&self, input_samples: usize) -> usize {
        let frames = (input_samples / self.input_channels) as u64;
        let frames = (frames.saturating_mul(self.output_rate) / self.input_rate).saturating_add(2);
        (frames as usize).saturating_mul(self.output_channels)
    }

    /// Mixes one input frame down to the output's channels.
    #[inline]
    fn mixed(&self, frame: &[i16]) -> [i16; 2] {
        let (mut left, mut right) = (0i32, 0i32);
        for (&sample, &(to_left, to_right)) in frame.iter().zip(&self.mix) {
            left += sample as i32 * to_left;
            right += sample as i32 * to_right;
        }
        // The weights of a channel sum to at most 1 in Q15.
        [
            ((left + (1 << 14)) >> 15).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            ((right + (1 << 14)) >> 15).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        ]
    }

    #[inline]
    fn push(&mut self, frame: [i16; 2]) {
        let window = self.window;
        for (channel, &sample) in frame.iter().enumerate().take(self.output_channels) {
            self.history[channel][self.head] = sample;
            self.history[channel][self.head + window] = sample;
        }
        self.head += 1;
        if self.head == window {
            self.head = 0;
        }
    }

    /// Weighs a filter wing into `weights`. The first weight is at distance
    /// `start`, in table positions, and each next one a step further out.
    #[inline]
    fn wing_weights(&self, start: u64, weights: &mut [i32]) -> i64 {
        let mut position = start;
        let mut sum = 0i64;
        for weight in weights.iter_mut() {
            let index = (position >> POSITION_BITS) as usize;
            let value = match (FILTER.get(index), FILTER.get(index + 1)) {
                (Some(&low), Some(&high)) => {
                    let blend = (position & ((1 << POSITION_BITS) - 1)) as i64;
                    low + (((high - low) as i64 * blend) >> POSITION_BITS) as i32
                }
                _ => 0,
            };
            *weight = value;
            sum += value as i64;
            position += self.position_step;
        }
        sum
    }

    /// Filters the window around the current position to one output frame.
    fn filter(&self, output: &mut [i16]) {
        let wing = self.wing;
        let scale = PHASE_BITS + POSITION_BITS;
        let start_left = (self.fraction << scale) / self.denominator;
        let start_right = ((self.output_rate - self.fraction) << scale) / self.denominator;
        let mut left = [0i32; MAX_WING];
        let mut right = [0i32; MAX_WING];
        let sum = self.wing_weights(start_left, &mut left[..wing])
            + self.wing_weights(start_right, &mut right[..wing]);
        if sum == 0 {
            output.fill(0);
            return;
        }
        for (channel, slot) in output.iter_mut().enumerate() {
            let window = &self.history[channel][self.head..self.head + self.window];
            let (older, newer) = window.split_at(wing);
            let mut total = 0i64;
            for (&weight, &sample) in left[..wing].iter().zip(older.iter().rev()) {
                total += weight as i64 * sample as i64;
            }
            for (&weight, &sample) in right[..wing].iter().zip(newer) {
                total += weight as i64 * sample as i64;
            }
            // Rounded division, for unit DC gain at every phase.
            let rounded = (total + total.signum() * sum.abs() / 2) / sum;
            *slot = rounded.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        }
    }

    /// Moves to the next output frame.
    #[inline]
    fn advance(&mut self) {
        self.need += self.step;
        self.fraction += self.remainder;
        if self.fraction >= self.output_rate {
            self.fraction -= self.output_rate;
            self.need += 1;
        }
        self.total_out += 1;
    }

    /// Converts as much as the buffers allow. Leftover input (less than a frame
    /// or more than `output` could take) stays with the caller; call again with
    /// the rest. There is no limit on how the stream is split across calls: the
    /// result is the same.
    pub fn process(&mut self, input: &[i16], output: &mut [i16]) -> Progress {
        let mut progress = Progress::default();
        let mut frames = input.chunks_exact(self.input_channels);
        let mut slots = output.chunks_exact_mut(self.output_channels);

        if self.is_bypass() {
            for (frame, slot) in (&mut frames).zip(&mut slots) {
                let mixed = self.mixed(frame);
                slot.copy_from_slice(&mixed[..slot.len()]);
                progress.consumed += self.input_channels;
                progress.produced += self.output_channels;
            }
            return progress;
        }

        loop {
            while self.need > 0 {
                let Some(frame) = frames.next() else {
                    return progress;
                };
                let mixed = self.mixed(frame);
                self.push(mixed);
                self.need -= 1;
                self.total_in += 1;
                progress.consumed += self.input_channels;
            }
            let Some(slot) = slots.next() else {
                return progress;
            };
            self.filter(slot);
            self.advance();
            progress.produced += self.output_channels;
        }
    }

    /// Finishes a stream: writes what is still held back by the filter's delay,
    /// and returns the samples written. Call until it returns 0. After that the
    /// total output is exactly `ceil(input frames * output rate / input rate)`.
    pub fn flush(&mut self, output: &mut [i16]) -> usize {
        if self.is_bypass() {
            return 0;
        }
        let target = self
            .total_in
            .saturating_mul(self.output_rate)
            .div_ceil(self.input_rate);
        let mut produced = 0;
        for slot in output.chunks_exact_mut(self.output_channels) {
            if self.total_out >= target {
                break;
            }
            while self.need > 0 {
                self.push([0, 0]);
                self.need -= 1;
            }
            self.filter(slot);
            self.advance();
            produced += self.output_channels;
        }
        produced
    }
}

const fn mix_for(input: u16, output: u16) -> Mix {
    const FULL: i32 = 1 << 15;
    // Each side's weights sum to one: L + 0.707 C + 0.707 S, scaled to fit.
    let table: Mix = match input {
        1 => downmix(&[(FULL, FULL)]),
        2 => downmix(&[(FULL, 0), (0, FULL)]),
        // FL FR FC
        3 => {
            let (f, c) = (19196, 13572);
            downmix(&[(f, 0), (0, f), (c, c)])
        }
        // FL FR BL BR
        4 => {
            let (f, s) = (19196, 13572);
            downmix(&[(f, 0), (0, f), (s, 0), (0, s)])
        }
        // FL FR FC BL BR
        5 => {
            let (f, s) = (13573, 9597);
            downmix(&[(f, 0), (0, f), (s, s), (s, 0), (0, s)])
        }
        // FL FR FC LFE BL BR
        6 => {
            let (f, s) = (13573, 9597);
            downmix(&[(f, 0), (0, f), (s, s), (0, 0), (s, 0), (0, s)])
        }
        // FL FR FC LFE BC SL SR: the back centre at -6 dB to both sides.
        7 => {
            let (f, s, b) = (11244, 7950, 5622);
            downmix(&[(f, 0), (0, f), (s, s), (0, 0), (b, b), (s, 0), (0, s)])
        }
        // FL FR FC LFE BL BR SL SR
        8 => {
            let (f, s) = (10498, 7423);
            downmix(&[(f, 0), (0, f), (s, s), (0, 0), (s, 0), (0, s), (s, 0), (0, s)])
        }
        _ => downmix(&[]),
    };
    if output != 1 {
        return table;
    }
    // Mono: the average of the two sides.
    let mut mono = [(0, 0); MAX_INPUT_CHANNELS as usize];
    let mut index = 0;
    while index < input as usize && index < mono.len() {
        let weight = if input == 1 {
            FULL
        } else {
            (table[index].0 + table[index].1) / 2
        };
        mono[index] = (weight, 0);
        index += 1;
    }
    mono
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(rate: u32, channels: u16) -> AudioFormat {
        AudioFormat::new(rate, channels).unwrap()
    }

    /// Converts a whole stream, in chunks of `chunk` input frames.
    fn convert(from: AudioFormat, to: AudioFormat, input: &[i16], chunk: usize) -> Vec<i16> {
        let mut resampler = Resampler::new(from, to).unwrap();
        let mut output = vec![0i16; resampler.max_output_samples(input.len()) + 1024];
        let mut written = 0;
        for piece in input.chunks(chunk * from.channels() as usize) {
            let mut used = 0;
            while used < piece.len() {
                let progress = resampler.process(&piece[used..], &mut output[written..]);
                used += progress.consumed;
                written += progress.produced;
                assert!(progress.consumed > 0 || progress.produced > 0);
            }
        }
        loop {
            let produced = resampler.flush(&mut output[written..]);
            if produced == 0 {
                break;
            }
            written += produced;
        }
        output.truncate(written);
        output
    }

    fn sine(rate: u32, hz: f64, amplitude: f64, frames: usize) -> Vec<i16> {
        (0..frames)
            .map(|n| {
                let phase = 2.0 * core::f64::consts::PI * hz * n as f64 / rate as f64;
                (amplitude * phase.sin()).round() as i16
            })
            .collect()
    }

    /// The level of `hz` in `samples`, by correlation, and of what is left.
    fn analyse(samples: &[i16], rate: u32, hz: f64) -> (f64, f64) {
        let omega = 2.0 * core::f64::consts::PI * hz / rate as f64;
        // Least squares for a sin + b cos, with no leakage over a part-cycle.
        let (mut ys, mut yc, mut ss, mut cc, mut sc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (n, &s) in samples.iter().enumerate() {
            let (sine, cosine) = (omega * n as f64).sin_cos();
            ys += s as f64 * sine;
            yc += s as f64 * cosine;
            ss += sine * sine;
            cc += cosine * cosine;
            sc += sine * cosine;
        }
        let det = ss * cc - sc * sc;
        let a = (ys * cc - yc * sc) / det;
        let b = (yc * ss - ys * sc) / det;
        let count = samples.len() as f64;
        let mut residual = 0.0;
        for (n, &s) in samples.iter().enumerate() {
            let fit = a * (omega * n as f64).sin() + b * (omega * n as f64).cos();
            residual += (s as f64 - fit).powi(2);
        }
        ((a * a + b * b).sqrt(), (2.0 * residual / count).sqrt())
    }

    fn db(ratio: f64) -> f64 {
        20.0 * ratio.max(1e-9).log10()
    }

    /// Skips the filter's edges, where the signal starts and stops abruptly.
    fn middle(samples: &[i16]) -> &[i16] {
        &samples[512..samples.len() - 512]
    }

    #[test]
    fn filter_table_is_what_the_documentation_says() {
        fn bessel_i0(x: f64) -> f64 {
            let (mut sum, mut term) = (1.0, 1.0);
            for k in 1..60 {
                term *= (x / (2.0 * k as f64)).powi(2);
                sum += term;
            }
            sum
        }
        let (c, beta) = (0.92f64, 8.0f64);
        for (index, &entry) in FILTER.iter().enumerate() {
            let t = index as f64 / 256.0;
            let x = core::f64::consts::PI * c * t;
            let sinc = if index == 0 { 1.0 } else { x.sin() / x };
            let window = bessel_i0(beta * (1.0 - (t / 16.0).powi(2)).max(0.0).sqrt()) / bessel_i0(beta);
            let expected = c * sinc * window * (1u64 << 30) as f64;
            assert!(
                (entry as f64 - expected).abs() < 2.0,
                "entry {index}: {entry} vs {expected}"
            );
        }
    }

    #[test]
    fn rejects_what_it_cannot_do() {
        assert!(Resampler::new(format(48_000, 9), format(48_000, 2)).is_none());
        assert!(Resampler::new(format(48_000, 2), format(48_000, 3)).is_none());
        assert!(Resampler::new(format(384_001, 2), format(48_000, 2)).is_none());
        assert!(Resampler::new(format(384_000, 2), format(48_000, 2)).is_some());
        assert!(Resampler::new(format(1, 2), format(48_000, 2)).is_some());
        assert!(Resampler::to_driver(format(u32::MAX, 2)).is_none());
    }

    #[test]
    fn equal_rates_bypass_the_filter() {
        let input = [1, -2, 3, -4, 5, -6];
        let output = convert(format(48_000, 2), format(48_000, 2), &input, 1);
        assert_eq!(output, input);
        assert!(Resampler::to_driver(format(48_000, 2)).unwrap().is_bypass());
    }

    #[test]
    fn channel_conversion() {
        let mono = convert(format(48_000, 1), format(48_000, 2), &[100, -200], 1);
        assert_eq!(mono, [100, 100, -200, -200]);
        let stereo = convert(format(48_000, 2), format(48_000, 1), &[100, 300, -200, -400], 1);
        assert_eq!(stereo, [200, -300]);

        // Full scale on every channel cannot clip, for any layout.
        for channels in 3..=8u16 {
            let input = vec![i16::MAX; channels as usize];
            let output = convert(format(8000, channels), format(8000, 2), &input, 1);
            assert_eq!(output.len(), 2);
            let lfe_free = channels != 6 && channels != 7 && channels != 8;
            assert!(output[0] > 32_000 || !lfe_free, "{channels}: {output:?}");
            assert_eq!(output[0], output[1], "{channels}");
            let input = vec![i16::MIN; channels as usize];
            let output = convert(format(8000, channels), format(8000, 2), &input, 1);
            assert!(output[0] < -30_000, "{channels}: {output:?}");
        }
        // 5.1 sends nothing from the LFE.
        let output = convert(format(8000, 6), format(8000, 2), &[0, 0, 0, 30_000, 0, 0], 1);
        assert_eq!(output, [0, 0]);
        // Left stays left.
        let output = convert(format(8000, 6), format(8000, 2), &[10_000, 0, 0, 0, 0, 0], 1);
        assert!(output[0] > 4000 && output[1] == 0);
    }

    #[test]
    fn dc_passes_exactly() {
        for (from, to) in [(44_100, 48_000), (48_000, 44_100), (8000, 48_000), (192_000, 48_000)] {
            let input = vec![12_345i16; 8192];
            let output = convert(format(from, 1), format(to, 1), &input, 777);
            for &sample in middle(&output) {
                assert_eq!(sample, 12_345, "{from} to {to}");
            }
        }
    }

    #[test]
    fn output_length_is_exact() {
        for (from, to, frames, expected) in [
            (44_100, 48_000, 44_100, 48_000),
            (44_100, 48_000, 441, 480),
            (44_100, 48_000, 1, 2),
            (48_000, 44_100, 480, 441),
            (48_000, 8000, 7, 2),
            (8000, 48_000, 3, 18),
            (44_100, 48_000, 0, 0),
        ] {
            let input = vec![0i16; frames];
            let output = convert(format(from, 1), format(to, 1), &input, 100);
            assert_eq!(output.len(), expected, "{from} to {to}, {frames} frames");
        }
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let input = sine(44_100, 997.0, 20_000.0, 4410);
        let whole = convert(format(44_100, 1), DRIVER_FORMAT, &input, input.len());
        assert_eq!(whole.len(), 4800 * 2);
        for chunk in [1, 2, 7, 64, 1000] {
            assert_eq!(
                convert(format(44_100, 1), DRIVER_FORMAT, &input, chunk),
                whole,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn small_output_buffers_make_progress() {
        let input = sine(8000, 300.0, 20_000.0, 800);
        let expected = convert(format(8000, 1), DRIVER_FORMAT, &input, 800);
        let mut resampler = Resampler::to_driver(format(8000, 1)).unwrap();
        let mut result = Vec::new();
        let mut slot = [0i16; 2];
        let mut used = 0;
        while used < input.len() {
            let progress = resampler.process(&input[used..], &mut slot);
            used += progress.consumed;
            result.extend_from_slice(&slot[..progress.produced]);
        }
        loop {
            let produced = resampler.flush(&mut slot);
            if produced == 0 {
                break;
            }
            result.extend_from_slice(&slot[..produced]);
        }
        assert_eq!(result, expected);
        // A buffer too small for one frame takes nothing, and does not hang.
        let mut tiny = [0i16; 1];
        let progress = resampler.process(&input, &mut tiny);
        assert_eq!(progress.produced, 0);
    }

    /// The stream must keep its timing: first output frame, first input frame.
    #[test]
    fn output_is_aligned_with_input() {
        let rate = 44_100;
        let input = sine(rate, 1000.0, 20_000.0, 4410);
        let output = convert(format(rate, 1), format(48_000, 1), &input, 4410);
        let expected = sine(48_000, 1000.0, 20_000.0, output.len());
        for (&a, &b) in middle(&output).iter().zip(middle(&expected)) {
            assert!((a as i32 - b as i32).abs() <= 2, "{a} vs {b}");
        }
    }

    #[test]
    fn passband_is_flat_and_clean() {
        for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100), (22_050, 48_000), (96_000, 48_000)] {
            let lower = from.min(to) as f64;
            for fraction in [0.02, 0.1, 0.25, 0.4] {
                let hz = lower * fraction;
                let input = sine(from, hz, 20_000.0, 16_384);
                let output = convert(format(from, 1), format(to, 1), &input, 512);
                let (level, residual) = analyse(middle(&output), to, hz);
                let gain = db(level / 20_000.0);
                let noise = db(residual / 20_000.0);
                assert!(gain.abs() < 0.15, "{from}->{to} at {hz} Hz: gain {gain:.3} dB");
                assert!(noise < -70.0, "{from}->{to} at {hz} Hz: noise {noise:.1} dB");
            }
        }
    }

    #[test]
    fn stopband_is_rejected() {
        // Downsampling: an input above the new Nyquist must vanish.
        for (from, to, hz) in [
            (96_000, 48_000, 30_000.0),
            (192_000, 48_000, 40_000.0),
            (384_000, 48_000, 100_000.0),
            (48_000, 8000, 6000.0),
        ] {
            let input = sine(from, hz, 20_000.0, 32_768);
            let output = convert(format(from, 1), format(to, 1), &input, 512);
            let (level, residual) = analyse(middle(&output), to, 1000.0);
            let rejection = db((level + residual) / 20_000.0);
            assert!(rejection < -72.0, "{from}->{to} at {hz} Hz: {rejection:.1} dB");
        }
        // Upsampling: the image of a tone just under the old Nyquist would be
        // just over it. It has to be gone from the new band.
        let input = sine(8000, 3000.0, 20_000.0, 16_384);
        let output = convert(format(8000, 1), format(48_000, 1), &input, 512);
        let (_, residual) = analyse(middle(&output), 48_000, 3000.0);
        assert!(db(residual / 20_000.0) < -72.0);
    }

    #[test]
    fn extreme_samples_saturate_without_overflow() {
        let input: Vec<i16> = (0..4096)
            .map(|n| if n % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        for (from, to) in [(44_100, 48_000), (384_000, 48_000), (1, 48_000), (48_000, 6000)] {
            let frames = if from == 1 { 4 } else { 4096 };
            let _ = convert(format(from, 1), format(to, 2), &input[..frames], 64);
        }
    }

    #[test]
    fn reset_starts_over() {
        let input = sine(44_100, 500.0, 10_000.0, 500);
        let mut resampler = Resampler::to_driver(format(44_100, 1)).unwrap();
        let mut first = vec![0i16; 2048];
        let a = resampler.process(&input, &mut first).produced;
        resampler.reset();
        let mut second = vec![0i16; 2048];
        let b = resampler.process(&input, &mut second).produced;
        assert_eq!(a, b);
        assert_eq!(first, second);
    }
}
