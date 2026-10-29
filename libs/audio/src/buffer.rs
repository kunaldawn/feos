//! An interleaved PCM buffer with mixing and fade helpers.
//!
//! `AudioBuffer` is the audio counterpart of `feos_gfx::Canvas`: it borrows a
//! caller-provided sample buffer and offers immediate, clipping-aware
//! operations.

use crate::format::AudioFormat;
use crate::sample::Sample;
use crate::wave::Oscillator;

/// A 16-bit interleaved PCM buffer.
///
/// A frame contains one sample per channel. Stereo frames are stored as
/// `[left, right]`, so a 480-frame stereo buffer needs 960 samples.
pub struct AudioBuffer<'a> {
    samples: &'a mut [i16],
    format: AudioFormat,
}

impl<'a> AudioBuffer<'a> {
    /// Creates a buffer over `samples`.
    ///
    /// Returns `None` if `samples` does not contain a whole number of frames.
    /// An empty buffer is allowed and reports zero frames.
    pub fn new(samples: &'a mut [i16], format: AudioFormat) -> Option<Self> {
        let channels = format.channels() as usize;
        if !samples.len().is_multiple_of(channels) {
            return None;
        }

        Some(Self { samples, format })
    }

    /// The buffer's sample rate and channel layout.
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Samples per second.
    pub const fn sample_rate(&self) -> u32 {
        self.format.sample_rate()
    }

    /// Number of interleaved channels.
    pub const fn channels(&self) -> usize {
        self.format.channels() as usize
    }

    /// Number of whole frames in the buffer.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels()
    }

    /// Duration of the buffer in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.format.duration_ms(self.frames())
    }

    /// Returns `true` when the buffer contains no frames.
    pub fn is_empty(&self) -> bool {
        self.frames() == 0
    }

    /// The complete interleaved sample buffer.
    pub fn samples(&self) -> &[i16] {
        self.samples
    }

    /// Mutable access to the complete interleaved sample buffer.
    pub fn samples_mut(&mut self) -> &mut [i16] {
        self.samples
    }

    /// Returns the raw samples for one frame.
    pub fn frame(&self, index: usize) -> Option<&[i16]> {
        let channels = self.channels();
        let start = index.checked_mul(channels)?;
        let end = start.checked_add(channels)?;
        self.samples.get(start..end)
    }

    /// Returns mutable raw samples for one frame.
    pub fn frame_mut(&mut self, index: usize) -> Option<&mut [i16]> {
        let channels = self.channels();
        let start = index.checked_mul(channels)?;
        let end = start.checked_add(channels)?;
        self.samples.get_mut(start..end)
    }

    /// Reads one sample.
    pub fn sample(&self, frame: usize, channel: usize) -> Option<Sample> {
        self.frame(frame)
            .and_then(|frame| frame.get(channel))
            .map(|&value| Sample::from_i16(value))
    }

    /// Writes one sample, returning `false` if the coordinates are out of
    /// range.
    pub fn set_sample(&mut self, frame: usize, channel: usize, value: Sample) -> bool {
        match self
            .frame_mut(frame)
            .and_then(|frame| frame.get_mut(channel))
        {
            Some(sample) => {
                *sample = value.raw();
                true
            }
            None => false,
        }
    }

    /// Fills the whole buffer with `value`.
    pub fn clear(&mut self, value: Sample) {
        self.samples.fill(value.raw());
    }

    /// Replaces one frame with `values`.
    ///
    /// Extra values are ignored and missing channels are left unchanged.
    /// Returns `false` if `frame` is out of range.
    pub fn write_frame(&mut self, frame: usize, values: &[Sample]) -> bool {
        let Some(destination) = self.frame_mut(frame) else {
            return false;
        };

        for (slot, value) in destination.iter_mut().zip(values) {
            *slot = value.raw();
        }
        true
    }

    /// Adds `values` to one frame, scaled by `gain`, saturating at the sample
    /// limits.
    ///
    /// Extra values are ignored and missing channels are left unchanged.
    /// Returns `false` if `frame` is out of range.
    pub fn mix_frame(&mut self, frame: usize, values: &[Sample], gain: Sample) -> bool {
        let Some(destination) = self.frame_mut(frame) else {
            return false;
        };

        for (slot, value) in destination.iter_mut().zip(values) {
            *slot = slot.saturating_add(value.gain(gain).raw());
        }
        true
    }

    /// Scales every sample by `gain` in place.
    pub fn apply_gain(&mut self, gain: Sample) {
        for sample in self.samples.iter_mut() {
            *sample = Sample::from_i16(*sample).gain(gain).raw();
        }
    }

    /// Fades the first `frames` frames in from silence to full scale.
    ///
    /// Frames beyond the fade are untouched.
    pub fn fade_in(&mut self, frames: usize) {
        let total = frames.min(self.frames());
        if total == 0 {
            return;
        }

        let channels = self.channels();
        for frame in 0..total {
            let gain = fade_gain(frame, total, false);
            let start = frame * channels;
            for sample in &mut self.samples[start..start + channels] {
                *sample = Sample::from_i16(*sample).gain(gain).raw();
            }
        }
    }

    /// Fades the last `frames` frames out to silence.
    ///
    /// Frames before the fade are untouched.
    pub fn fade_out(&mut self, frames: usize) {
        let total = frames.min(self.frames());
        if total == 0 {
            return;
        }

        let channels = self.channels();
        let first = self.frames() - total;
        for offset in 0..total {
            let gain = fade_gain(offset, total, true);
            let start = (first + offset) * channels;
            for sample in &mut self.samples[start..start + channels] {
                *sample = Sample::from_i16(*sample).gain(gain).raw();
            }
        }
    }

    /// The largest absolute sample value in the buffer.
    pub fn peak(&self) -> u16 {
        self.samples
            .iter()
            .map(|&sample| Sample::from_i16(sample).magnitude())
            .max()
            .unwrap_or(0)
    }

    /// Returns `true` when every sample is zero.
    pub fn is_silent(&self) -> bool {
        self.samples.iter().all(|&sample| sample == 0)
    }

    /// Mixes another buffer into this one.
    ///
    /// Both buffers must have the same format. Mixing covers the shorter of
    /// the two buffers. Returns `false` when the formats differ.
    pub fn mix(&mut self, source: &AudioBuffer<'_>, gain: Sample) -> bool {
        if self.format != source.format {
            return false;
        }

        let channels = self.channels();
        let frames = self.frames().min(source.frames());
        for frame in 0..frames {
            let start = frame * channels;
            for channel in 0..channels {
                let index = start + channel;
                let mixed = Sample::from_i16(source.samples[index]).gain(gain);
                self.samples[index] = self.samples[index].saturating_add(mixed.raw());
            }
        }
        true
    }

    /// Replaces the buffer with `oscillator`'s output, using the buffer's
    /// sample rate.
    pub fn render_oscillator(&mut self, oscillator: &mut Oscillator, gain: Sample) {
        let channels = self.channels();
        let sample_rate = self.sample_rate();

        for frame in 0..self.frames() {
            let value = oscillator.next_sample(sample_rate).gain(gain);
            let start = frame * channels;
            for channel in 0..channels {
                self.samples[start + channel] = value.raw();
            }
        }
    }

    /// Adds `oscillator`'s output to the buffer, using the buffer's sample
    /// rate.
    pub fn mix_oscillator(&mut self, oscillator: &mut Oscillator, gain: Sample) {
        let channels = self.channels();
        let sample_rate = self.sample_rate();

        for frame in 0..self.frames() {
            let value = oscillator.next_sample(sample_rate).gain(gain);
            let start = frame * channels;
            for channel in 0..channels {
                self.samples[start + channel] =
                    self.samples[start + channel].saturating_add(value.raw());
            }
        }
    }
}

/// Gain for step `index` of a `total`-step fade.
fn fade_gain(index: usize, total: usize, fade_out: bool) -> Sample {
    if total == 1 {
        return if fade_out {
            Sample::SILENCE
        } else {
            Sample::MAX
        };
    }

    let step = if fade_out { total - 1 - index } else { index };
    let gain = (step as i64 * i16::MAX as i64 / (total as i64 - 1)) as i16;
    Sample::from_i16(gain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wave::Waveform;

    fn mono(sample_rate: u32) -> AudioFormat {
        AudioFormat::mono(sample_rate).unwrap()
    }

    fn stereo(sample_rate: u32) -> AudioFormat {
        AudioFormat::stereo(sample_rate).unwrap()
    }

    #[test]
    fn rejects_partial_frames() {
        let mut samples = [0i16; 5];
        assert!(AudioBuffer::new(&mut samples, stereo(8_000)).is_none());
    }

    #[test]
    fn empty_buffer_is_valid() {
        let mut samples = [];
        let buffer = AudioBuffer::new(&mut samples, mono(8_000)).unwrap();
        assert!(buffer.is_empty());
        assert_eq!(buffer.frames(), 0);
    }

    #[test]
    fn clear_and_access_samples() {
        let mut samples = [0i16; 8];
        let mut buffer = AudioBuffer::new(&mut samples, mono(8_000)).unwrap();

        buffer.clear(Sample::from_i16(7));
        assert_eq!(buffer.frames(), 8);
        assert_eq!(buffer.sample(3, 0), Some(Sample::from_i16(7)));
        assert_eq!(buffer.sample(8, 0), None);
        assert!(buffer.set_sample(3, 0, Sample::SILENCE));
        assert!(!buffer.set_sample(8, 0, Sample::SILENCE));
    }

    #[test]
    fn frames_and_channels_are_interleaved() {
        let mut samples = [0i16; 6];
        let mut buffer = AudioBuffer::new(&mut samples, stereo(8_000)).unwrap();

        assert_eq!(buffer.frames(), 3);
        assert!(buffer.write_frame(0, &[Sample::from_i16(1), Sample::from_i16(2)]));
        assert!(buffer.write_frame(1, &[Sample::from_i16(3), Sample::from_i16(4)]));

        assert_eq!(buffer.samples(), &[1, 2, 3, 4, 0, 0]);
        assert_eq!(buffer.frame(1), Some(&[3, 4][..]));
        assert!(!buffer.write_frame(3, &[Sample::MAX]));
    }

    #[test]
    fn mix_frame_saturates() {
        let mut samples = [0i16; 2];
        let mut buffer = AudioBuffer::new(&mut samples, stereo(8_000)).unwrap();

        assert!(buffer.mix_frame(0, &[Sample::MAX, Sample::MIN], Sample::MAX));
        assert!(buffer.mix_frame(0, &[Sample::MAX, Sample::MIN], Sample::MAX));

        assert_eq!(buffer.sample(0, 0), Some(Sample::MAX));
        assert_eq!(buffer.sample(0, 1), Some(Sample::MIN));
    }

    #[test]
    fn mix_requires_same_format() {
        let mut mono_samples = [0i16; 4];
        let mut stereo_samples = [0i16; 8];
        let mut a = AudioBuffer::new(&mut mono_samples, mono(8_000)).unwrap();
        let b = AudioBuffer::new(&mut stereo_samples, stereo(8_000)).unwrap();

        assert!(!a.mix(&b, Sample::MAX));
    }

    #[test]
    fn mix_scales_and_saturates() {
        let mut destination_samples = [0i16; 4];
        let mut source_samples = [100i16, -100, 20_000, -20_000];
        let mut destination = AudioBuffer::new(&mut destination_samples, mono(8_000)).unwrap();
        let source = AudioBuffer::new(&mut source_samples, mono(8_000)).unwrap();

        assert!(destination.mix(&source, Sample::from_i16(16_000)));
        assert_eq!(destination.sample(0, 0), Some(Sample::from_i16(48)));
        assert_eq!(destination.sample(1, 0), Some(Sample::from_i16(-48)));
        assert_eq!(destination.sample(2, 0), Some(Sample::from_i16(9_765)));
        assert_eq!(destination.sample(3, 0), Some(Sample::from_i16(-9_765)));
    }

    #[test]
    fn fades_reach_full_scale_and_silence() {
        let mut samples = [0i16; 4];
        let mut buffer = AudioBuffer::new(&mut samples, mono(8_000)).unwrap();
        buffer.clear(Sample::MAX);

        buffer.fade_in(2);
        assert_eq!(buffer.sample(0, 0), Some(Sample::SILENCE));
        assert_eq!(buffer.sample(1, 0), Some(Sample::MAX));
        assert_eq!(buffer.sample(2, 0), Some(Sample::MAX));

        buffer.fade_out(2);
        assert_eq!(buffer.sample(2, 0), Some(Sample::MAX));
        assert_eq!(buffer.sample(3, 0), Some(Sample::SILENCE));
    }

    #[test]
    fn peak_and_silence_cover_every_channel() {
        let mut samples = [0i16; 4];
        let mut buffer = AudioBuffer::new(&mut samples, stereo(8_000)).unwrap();

        assert!(buffer.is_silent());
        buffer.set_sample(0, 1, Sample::from_i16(-500));
        assert!(!buffer.is_silent());
        assert_eq!(buffer.peak(), 500);
    }

    #[test]
    fn oscillator_renders_and_mixes() {
        let mut samples = [0i16; 4];
        let mut buffer = AudioBuffer::new(&mut samples, mono(8_000)).unwrap();
        let mut oscillator = Oscillator::new(Waveform::Square, 2_000, Sample::from_i16(1_000));

        buffer.render_oscillator(&mut oscillator, Sample::MAX);
        assert_eq!(buffer.samples(), &[1_000, 1_000, -1_000, -1_000]);

        buffer.mix_oscillator(&mut oscillator, Sample::MAX);
        assert_eq!(buffer.samples(), &[2_000, 2_000, -2_000, -2_000]);
    }
}
