//! Plays sound faster or slower without changing its pitch: WSOLA, waveform-similarity
//! overlap-add.
//!
//! Output is built from overlapping segments of the input. For a tempo of 1.5 they're
//! taken from one and a half times as far apart as they're laid down. Laid end to end
//! like that, the waves wouldn't line up where they meet, so each segment's start is
//! moved a little from where the tempo says, to wherever it looks most like the sound
//! that would have come next. Then a short crossfade joins them.
//!
//! It's simple and has its limits: a tempo far from 1 is audibly choppy on speech, and
//! there's no phase locking between channels beyond using the same offsets for all of them.

use alloc::vec::Vec;

/// A segment: its second half crossfades into the next one's first half.
const SEGMENT_MS: usize = 40;
/// How far from its ideal place a segment's start can move.
const SEARCH_MS: usize = 10;

/// A time stretcher for interleaved 16-bit audio.
pub struct Wsola {
    channels: usize,
    /// Frames in half a segment, which is also what each step puts out.
    half: usize,
    search: usize,
    /// Input per step, in frames, in 16.16 fixed point.
    step: u64,
    /// Where the next segment ideally starts, in 16.16 fixed point, from `input[0]`.
    ideal: u64,
    /// The input not yet stepped past.
    input: Vec<i16>,
    /// The frame in `input` that the last segment's overlap came from, and so what
    /// the sound would go on to do.
    continuation: Option<usize>,
    /// The second half of the last segment, waiting to fade out.
    tail: Vec<i16>,
    output: Vec<i16>,
    read: usize,
}

impl Wsola {
    /// A stretcher that plays at `tempo` times normal speed, or `None` when memory is
    /// short.
    pub fn new(sample_rate: u32, channels: u16, tempo: f32) -> Option<Self> {
        let channels = (channels as usize).max(1);
        let half = (sample_rate as usize * SEGMENT_MS / 2000).max(16);
        let search = (sample_rate as usize * SEARCH_MS / 1000).max(4);
        let step = (half as f64 * tempo.clamp(0.25, 4.0) as f64 * 65536.0) as u64;
        let mut tail = Vec::new();
        tail.try_reserve_exact(half * channels).ok()?;
        let mut input = Vec::new();
        input.try_reserve((half * 8 + search * 2) * channels).ok()?;
        let mut output = Vec::new();
        output.try_reserve(half * 4 * channels).ok()?;
        Some(Self {
            channels,
            half,
            search,
            step,
            ideal: 0,
            input,
            continuation: None,
            tail,
            output,
            read: 0,
        })
    }

    /// Frames of input it would take to make the next output.
    fn needed(&self) -> usize {
        (self.ideal >> 16) as usize + self.search + self.half * 2
    }

    /// Adds interleaved input, and makes what can be made of it. `false` when memory is
    /// short, and the input is dropped.
    pub fn push(&mut self, samples: &[i16]) -> bool {
        if self.input.try_reserve(samples.len()).is_err() {
            return false;
        }
        self.input.extend_from_slice(samples);
        let mut made = true;
        while self.input.len() / self.channels >= self.needed() {
            made &= self.step_once();
        }
        made
    }

    /// Output waiting, in samples.
    pub fn available(&self) -> usize {
        self.output.len() - self.read
    }

    /// Moves output into `out`, in whole frames, and says how many samples that was.
    pub fn pull(&mut self, out: &mut [i16]) -> usize {
        let count = (self.available().min(out.len()) / self.channels) * self.channels;
        out[..count].copy_from_slice(&self.output[self.read..self.read + count]);
        self.read += count;
        if self.read == self.output.len() {
            self.output.clear();
            self.read = 0;
        }
        count
    }

    /// Forgets everything, for a seek.
    pub fn reset(&mut self) {
        self.ideal = 0;
        self.input.clear();
        self.continuation = None;
        self.tail.clear();
        self.output.clear();
        self.read = 0;
    }

    /// Finds where around `ideal` the input looks most like `continuation`.
    fn best_start(&self, ideal: usize, continuation: usize) -> usize {
        let channels = self.channels;
        let from = ideal.saturating_sub(self.search);
        let to = ideal + self.search;
        let mut best = ideal;
        let mut best_score = i64::MIN;
        // A thinned-out comparison of one channel is plenty to find the peak.
        for start in (from..=to).step_by(2) {
            let mut score = 0i64;
            for frame in (0..self.half).step_by(4) {
                let a = self.input[(continuation + frame) * channels] as i64;
                let b = self.input[(start + frame) * channels] as i64;
                score += a * b;
            }
            if score > best_score {
                best_score = score;
                best = start;
            }
        }
        best
    }

    fn step_once(&mut self) -> bool {
        let (channels, half) = (self.channels, self.half);
        let ideal = (self.ideal >> 16) as usize;
        let start = match self.continuation {
            Some(continuation) => self.best_start(ideal, continuation),
            None => ideal,
        };
        if self.output.try_reserve(half * channels).is_err() {
            return false;
        }
        // Fade the last segment's tail out and this segment's start in. The first has
        // nothing to fade from.
        if self.tail.is_empty() {
            self.output
                .extend_from_slice(&self.input[start * channels..(start + half) * channels]);
        } else {
            for frame in 0..half {
                let fade_in = frame as i32;
                let fade_out = (half - frame) as i32;
                for channel in 0..channels {
                    let old = self.tail[frame * channels + channel] as i32;
                    let new = self.input[(start + frame) * channels + channel] as i32;
                    self.output.push(((old * fade_out + new * fade_in) / half as i32) as i16);
                }
            }
        }
        self.tail.clear();
        self.tail.extend_from_slice(
            &self.input[(start + half) * channels..(start + 2 * half) * channels],
        );
        // The sound's own next half segment, for the next search to match.
        let continuation = start + half;
        self.ideal += self.step;

        // Drop what no search can reach any more.
        let keep_from = ((self.ideal >> 16) as usize)
            .saturating_sub(self.search)
            .min(continuation);
        self.input.drain(..keep_from * channels);
        self.ideal -= (keep_from as u64) << 16;
        self.continuation = Some(continuation - keep_from);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, rate: f64, frames: usize, channels: usize) -> Vec<i16> {
        let mut out = Vec::new();
        for frame in 0..frames {
            let value = (2.0 * core::f64::consts::PI * frequency * frame as f64 / rate).sin() * 10000.0;
            for _ in 0..channels {
                out.push(value as i16);
            }
        }
        out
    }

    fn run(input: &[i16], rate: u32, channels: u16, tempo: f32) -> Vec<i16> {
        let mut wsola = Wsola::new(rate, channels, tempo).unwrap();
        let mut all = Vec::new();
        let mut out = vec![0i16; 4096];
        for chunk in input.chunks(1000 * channels as usize) {
            assert!(wsola.push(chunk));
            loop {
                let count = wsola.pull(&mut out);
                if count == 0 {
                    break;
                }
                all.extend_from_slice(&out[..count]);
            }
        }
        all
    }

    /// Rising zero crossings a second, on the first channel.
    fn frequency(samples: &[i16], rate: f64, channels: usize) -> f64 {
        let channel: Vec<i16> = samples.iter().step_by(channels).copied().collect();
        let crossings = channel.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count();
        crossings as f64 * rate / channel.len() as f64
    }

    #[test]
    fn length_follows_the_tempo_and_pitch_stays() {
        for tempo in [0.5f32, 0.75, 1.25, 1.5, 2.0] {
            let input = sine(440.0, 48000.0, 96000, 2);
            let output = run(&input, 48000, 2, tempo);
            let expected = input.len() as f64 / tempo as f64;
            let ratio = output.len() as f64 / expected;
            assert!((0.95..=1.02).contains(&ratio), "tempo {tempo}: length ratio {ratio}");
            let heard = frequency(&output, 48000.0, 2);
            assert!((heard - 440.0).abs() < 4.0, "tempo {tempo}: heard {heard} Hz");
        }
    }

    #[test]
    fn joins_do_not_click() {
        // A tone with no wave-matching would jump by thousands where segments meet.
        let input = sine(440.0, 48000.0, 96000, 1);
        for tempo in [0.75f32, 1.5] {
            let output = run(&input, 48000, 1, tempo);
            let biggest = output.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
            // The sine's own steepest slope is about 576 a sample.
            assert!(biggest < 900, "tempo {tempo}: a step of {biggest}");
        }
    }

    #[test]
    fn channels_stay_apart() {
        let mut input = Vec::new();
        for frame in 0..48000 {
            let phase = frame as f64 / 48000.0 * 2.0 * core::f64::consts::PI;
            input.push(((phase * 440.0).sin() * 8000.0) as i16);
            input.push(((phase * 660.0).sin() * 8000.0) as i16);
        }
        let output = run(&input, 48000, 2, 1.5);
        let left: Vec<i16> = output.iter().step_by(2).copied().collect();
        let right: Vec<i16> = output.iter().skip(1).step_by(2).copied().collect();
        let (l, r) = (frequency(&left, 48000.0, 1), frequency(&right, 48000.0, 1));
        assert!((l - 440.0).abs() < 8.0, "left {l}");
        assert!((r - 660.0).abs() < 25.0, "right {r}");
    }

    #[test]
    fn a_reset_starts_over() {
        let input = sine(440.0, 44100.0, 8000, 2);
        let mut wsola = Wsola::new(44100, 2, 1.5).unwrap();
        assert!(wsola.push(&input));
        wsola.reset();
        assert_eq!(wsola.available(), 0);
        assert!(wsola.push(&input));
        assert!(wsola.available() > 0);
    }
}
