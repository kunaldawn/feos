//! The mixer itself: fixed slots, rings and arithmetic, with no services.

use alloc::vec::Vec;

use feos_audio::{AudioFormat, Resampler, DRIVER_FORMAT};
use feos_hal::Audio;

use crate::OpenError;

/// How many streams can be open at once.
pub const MAX_STREAMS: usize = 8;
/// The loudest volume or gain.
pub const MAX_VOLUME: u8 = 100;
/// Output frames each stream's ring holds: a quarter of a second.
pub const RING_FRAMES: usize = 12_000;

/// Output channels: the driver's format is stereo.
const CHANNELS: usize = 2;
/// What the mixer tries to keep queued in the device, in frames. Long enough for
/// the task to miss a few turns on a busy machine, and short enough that volume
/// changes and new streams are heard promptly.
const TARGET_QUEUE_FRAMES: usize = 4_800;
/// Largest piece mixed at a time, in frames.
const CHUNK_FRAMES: usize = 512;
/// Input frames converted at a time.
const CONVERT_SAMPLES: usize = 512;

/// A handle to one open stream. Handles aren't reused, so a stale one is
/// harmless.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamId {
    slot: usize,
    serial: u32,
}

struct Slot {
    serial: u32,
    owner: u32,
    resampler: Resampler,
    /// Interleaved output samples. Its length is the capacity.
    ring: Vec<i16>,
    /// Oldest sample and the number waiting.
    head: usize,
    len: usize,
    paused: bool,
    gain: u8,
    /// Frames mixed since open or flush.
    mixed: u64,
}

/// Curved so that the bottom of the range isn't all near-silence: gain follows
/// the square of the level, in Q15.
fn curve(level: u8) -> i32 {
    let level = level.min(MAX_VOLUME) as i32;
    level * level * 32768 / (MAX_VOLUME as i32 * MAX_VOLUME as i32)
}

/// Mixes streams into one output. See the crate documentation.
pub struct Mixer {
    slots: [Option<Slot>; MAX_STREAMS],
    serial: u32,
    volume: u8,
    muted: bool,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    /// Creates a mixer at full volume with nothing open.
    pub const fn new() -> Self {
        const EMPTY: Option<Slot> = None;
        Self {
            slots: [EMPTY; MAX_STREAMS],
            serial: 0,
            volume: MAX_VOLUME,
            muted: false,
        }
    }

    /// Opens a stream in `format`, on behalf of process `owner`.
    pub fn open(&mut self, format: AudioFormat, owner: u32) -> Result<StreamId, OpenError> {
        let slot = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(OpenError::NoFreeStream)?;
        let resampler = Resampler::new(format, DRIVER_FORMAT).ok_or(OpenError::UnsupportedFormat)?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(RING_FRAMES * CHANNELS)
            .map_err(|_| OpenError::OutOfMemory)?;
        ring.resize(RING_FRAMES * CHANNELS, 0);
        self.serial = self.serial.wrapping_add(1);
        self.slots[slot] = Some(Slot {
            serial: self.serial,
            owner,
            resampler,
            ring,
            head: 0,
            len: 0,
            paused: false,
            gain: MAX_VOLUME,
            mixed: 0,
        });
        Ok(StreamId {
            slot,
            serial: self.serial,
        })
    }

    fn slot(&mut self, id: StreamId) -> Option<&mut Slot> {
        self.slots
            .get_mut(id.slot)?
            .as_mut()
            .filter(|slot| slot.serial == id.serial)
    }

    /// Closes a stream, discarding anything it holds.
    pub fn close(&mut self, id: StreamId) {
        if self.slot(id).is_some() {
            self.slots[id.slot] = None;
        }
    }

    /// Queues interleaved samples in the stream's format. Returns how many were
    /// taken, always whole frames; what doesn't fit is left for the next call.
    pub fn write(&mut self, id: StreamId, samples: &[i16]) -> usize {
        let Some(slot) = self.slot(id) else {
            return 0;
        };
        let mut taken = 0;
        while taken < samples.len() {
            let end = samples.len().min(taken + CONVERT_SAMPLES);
            let Slot {
                resampler,
                ring,
                head,
                len,
                ..
            } = slot;
            let capacity = ring.len();
            let tail = (*head + *len) % capacity;
            let run = (capacity - *len).min(capacity - tail);
            if run < CHANNELS {
                break;
            }
            let progress = resampler.process(&samples[taken..end], &mut ring[tail..tail + run]);
            *len += progress.produced;
            taken += progress.consumed;
            if progress.consumed == 0 && progress.produced == 0 {
                break;
            }
        }
        taken
    }

    /// Output frames the stream's ring has room for.
    pub fn free_frames(&mut self, id: StreamId) -> usize {
        self.slot(id)
            .map_or(0, |slot| (slot.ring.len() - slot.len) / CHANNELS)
    }

    /// Output frames waiting in the stream's ring.
    pub fn pending_frames(&mut self, id: StreamId) -> usize {
        self.slot(id).map_or(0, |slot| slot.len / CHANNELS)
    }

    /// Frames this stream has fed to the output since it opened or was flushed.
    pub fn mixed_frames(&mut self, id: StreamId) -> u64 {
        self.slot(id).map_or(0, |slot| slot.mixed)
    }

    /// Drops what the stream holds and restarts its frame count.
    pub fn flush(&mut self, id: StreamId) {
        if let Some(slot) = self.slot(id) {
            slot.head = 0;
            slot.len = 0;
            slot.mixed = 0;
            slot.resampler.reset();
        }
    }

    /// Stops or resumes taking samples from a stream.
    pub fn set_paused(&mut self, id: StreamId, paused: bool) {
        if let Some(slot) = self.slot(id) {
            slot.paused = paused;
        }
    }

    /// Sets a stream's own level, 0 to [`MAX_VOLUME`].
    pub fn set_gain(&mut self, id: StreamId, level: u8) {
        if let Some(slot) = self.slot(id) {
            slot.gain = level.min(MAX_VOLUME);
        }
    }

    /// The master volume.
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Sets the master volume.
    pub fn set_volume(&mut self, level: u8) {
        self.volume = level.min(MAX_VOLUME);
    }

    /// Whether the output is muted.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Mutes or unmutes the output. Streams keep moving, so clocks stay right.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// How many streams are open.
    pub fn active(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Every open stream and who owns it.
    pub fn owners(&self) -> [Option<(StreamId, u32)>; MAX_STREAMS] {
        let mut owners = [None; MAX_STREAMS];
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(slot) = slot {
                owners[index] = Some((
                    StreamId {
                        slot: index,
                        serial: slot.serial,
                    },
                    slot.owner,
                ));
            }
        }
        owners
    }

    /// Mixes into `out`, a whole number of frames. Returns the frames made: the
    /// most any unpaused stream had, up to the size of `out`. A stream with less
    /// simply stops contributing, so one slow client can't hold up the others.
    pub fn mix(&mut self, out: &mut [i16]) -> usize {
        let frames = out.len() / CHANNELS;
        let longest = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| !slot.paused)
            .map(|slot| slot.len / CHANNELS)
            .max()
            .unwrap_or(0)
            .min(frames);
        let samples = longest * CHANNELS;
        out[..samples].fill(0);
        if longest == 0 {
            return 0;
        }
        let master = if self.muted { 0 } else { curve(self.volume) };
        // Summed wide, then limited once, so that loud streams clip only as much
        // as their sum does.
        let mut sum = [0i32; CHUNK_FRAMES * CHANNELS];
        let mut done = 0;
        while done < samples {
            let chunk = (samples - done).min(sum.len());
            sum[..chunk].fill(0);
            for slot in self.slots.iter_mut().flatten() {
                if slot.paused {
                    continue;
                }
                let gain = (curve(slot.gain) as i64 * master as i64) >> 15;
                let count = chunk.min(slot.len);
                let capacity = slot.ring.len();
                for (index, total) in sum[..count].iter_mut().enumerate() {
                    let sample = slot.ring[(slot.head + index) % capacity] as i64;
                    *total = total.saturating_add(((sample * gain) >> 15) as i32);
                }
                slot.head = (slot.head + count) % capacity;
                slot.len -= count;
                slot.mixed += (count / CHANNELS) as u64;
            }
            for (target, total) in out[done..done + chunk].iter_mut().zip(&sum) {
                *target = (*total).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            done += chunk;
        }
        longest
    }

    /// Hands the device as much as it wants right now. Returns the frames given.
    pub fn pump(&mut self, device: &dyn Audio) -> usize {
        let mut given = 0;
        let mut chunk = [0i16; CHUNK_FRAMES * CHANNELS];
        loop {
            let queued = device.queued() / CHANNELS;
            let room = TARGET_QUEUE_FRAMES.saturating_sub(queued);
            let room = room.min(device.available() / CHANNELS).min(CHUNK_FRAMES);
            if room == 0 {
                return given;
            }
            let made = self.mix(&mut chunk[..room * CHANNELS]);
            if made == 0 {
                return given;
            }
            // `available` said this fits, and only the mixer writes.
            let taken = device.write(&chunk[..made * CHANNELS]) / CHANNELS;
            given += taken;
            if taken < made {
                return given;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use feos_hal::AudioError;

    extern crate std;
    use std::vec;

    const STEREO_48K: AudioFormat = DRIVER_FORMAT;

    fn format(rate: u32, channels: u16) -> AudioFormat {
        AudioFormat::new(rate, channels).unwrap()
    }

    /// Everything the device was given, and a size for its queue.
    struct Device {
        taken: RefCell<std::vec::Vec<i16>>,
        queued: core::cell::Cell<usize>,
    }

    // SAFETY: the tests that use this stay on one thread.
    unsafe impl Sync for Device {}

    impl Device {
        fn new() -> Self {
            Self {
                taken: RefCell::new(vec![]),
                queued: core::cell::Cell::new(0),
            }
        }
    }

    impl Audio for Device {
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            2
        }
        fn is_ready(&self) -> bool {
            true
        }
        fn play(&self, _samples: &[i16]) -> Result<(), AudioError> {
            Ok(())
        }
        fn stop(&self) {}
        fn max_samples(&self) -> usize {
            usize::MAX
        }
        fn write(&self, samples: &[i16]) -> usize {
            self.taken.borrow_mut().extend_from_slice(samples);
            self.queued.set(self.queued.get() + samples.len());
            samples.len()
        }
        fn available(&self) -> usize {
            98304 - self.queued.get()
        }
        fn queued(&self) -> usize {
            self.queued.get()
        }
    }

    #[test]
    fn one_stream_at_the_device_rate_passes_unchanged() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        let input: std::vec::Vec<i16> = (0..200).map(|n| n * 7).collect();
        assert_eq!(mixer.write(id, &input), input.len());
        let mut out = [0i16; 200];
        assert_eq!(mixer.mix(&mut out), 100);
        assert_eq!(&out[..], &input[..]);
        assert_eq!(mixer.pending_frames(id), 0);
    }

    #[test]
    fn two_streams_add_and_saturate_instead_of_wrapping() {
        let mut mixer = Mixer::new();
        let a = mixer.open(STEREO_48K, 1).unwrap();
        let b = mixer.open(STEREO_48K, 2).unwrap();
        mixer.write(a, &[1000, -1000, 30000, -30000]);
        mixer.write(b, &[500, -500, 30000, -30000]);
        let mut out = [0i16; 4];
        assert_eq!(mixer.mix(&mut out), 2);
        assert_eq!(out, [1500, -1500, i16::MAX, i16::MIN]);
    }

    #[test]
    fn a_shorter_stream_does_not_hold_up_a_longer_one() {
        let mut mixer = Mixer::new();
        let long = mixer.open(STEREO_48K, 1).unwrap();
        let short = mixer.open(STEREO_48K, 2).unwrap();
        mixer.write(long, &[100; 8]);
        mixer.write(short, &[50; 2]);
        let mut out = [0i16; 8];
        assert_eq!(mixer.mix(&mut out), 4);
        assert_eq!(out, [150, 150, 100, 100, 100, 100, 100, 100]);
    }

    #[test]
    fn mono_at_44100_becomes_48k_stereo_at_the_right_length_and_level() {
        let mut mixer = Mixer::new();
        let id = mixer.open(format(44_100, 1), 1).unwrap();
        // A second of DC, in chunks small enough to pass through.
        let block = [8000i16; 441];
        let mut made = 0usize;
        let mut out = vec![0i16; 4096];
        let mut last = 0;
        for _ in 0..100 {
            let taken = mixer.write(id, &block);
            assert_eq!(taken, block.len());
            let frames = mixer.mix(&mut out);
            if frames > 0 {
                last = out[frames * 2 - 2];
                assert_eq!(out[0], out[1], "both channels carry the mono signal");
            }
            made += frames;
        }
        // The filter delays about 16 frames.
        assert!((47_900..=48_000).contains(&made), "{made}");
        assert!((7990..=8010).contains(&last), "{last}");
    }

    #[test]
    fn a_sine_survives_resampling_at_its_own_frequency() {
        // One kHz at 22050 Hz mono, measured by zero crossings at 48 kHz.
        let mut mixer = Mixer::new();
        let id = mixer.open(format(22_050, 1), 1).unwrap();
        let input: std::vec::Vec<i16> = (0..22_050)
            .map(|n| {
                let phase = n as f64 * 1000.0 / 22_050.0 * core::f64::consts::TAU;
                (phase.sin() * 16000.0) as i16
            })
            .collect();
        let mut out = vec![0i16; 8192];
        let mut crossings = 0;
        let mut previous = 0i16;
        let mut peak = 0i16;
        let mut taken = 0;
        while taken < input.len() || mixer.pending_frames(id) > 0 {
            taken += mixer.write(id, &input[taken..]);
            let frames = mixer.mix(&mut out);
            for frame in out[..frames * 2].chunks_exact(2) {
                if previous <= 0 && frame[0] > 0 {
                    crossings += 1;
                }
                previous = frame[0];
                peak = peak.max(frame[0]);
            }
        }
        assert!((990..=1001).contains(&crossings), "{crossings}");
        assert!((15000..=16500).contains(&peak), "{peak}");
    }

    #[test]
    fn a_full_ring_takes_no_more_and_keeps_what_it_has() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        let input = vec![5i16; (RING_FRAMES + 100) * 2];
        let taken = mixer.write(id, &input);
        assert_eq!(taken, RING_FRAMES * 2);
        assert_eq!(mixer.write(id, &input), 0);
        assert_eq!(mixer.free_frames(id), 0);
        let mut out = [0i16; 200];
        mixer.mix(&mut out);
        assert_eq!(mixer.free_frames(id), 100);
        assert_eq!(mixer.write(id, &input), 200);
    }

    #[test]
    fn volume_scales_and_mute_silences_while_time_moves_on() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        mixer.set_volume(50);
        mixer.write(id, &[10000, 10000]);
        let mut out = [0i16; 2];
        mixer.mix(&mut out);
        assert_eq!(out[0], 2500, "half the level is a quarter of the gain");

        mixer.set_muted(true);
        mixer.write(id, &[10000, 10000]);
        assert_eq!(mixer.mix(&mut out), 1);
        assert_eq!(out, [0, 0]);
        assert_eq!(mixer.mixed_frames(id), 2, "muted streams still advance");

        mixer.set_muted(false);
        mixer.set_volume(MAX_VOLUME);
        mixer.set_gain(id, 0);
        mixer.write(id, &[10000, 10000]);
        mixer.mix(&mut out);
        assert_eq!(out, [0, 0]);
    }

    #[test]
    fn paused_streams_keep_their_place() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        mixer.write(id, &[7, 7, 8, 8]);
        mixer.set_paused(id, true);
        let mut out = [0i16; 4];
        assert_eq!(mixer.mix(&mut out), 0);
        mixer.set_paused(id, false);
        assert_eq!(mixer.mix(&mut out), 2);
        assert_eq!(out, [7, 7, 8, 8]);
    }

    #[test]
    fn a_flush_restarts_the_position() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        mixer.write(id, &[1; 8]);
        let mut out = [0i16; 4];
        mixer.mix(&mut out);
        assert_eq!(mixer.mixed_frames(id), 2);
        mixer.flush(id);
        assert_eq!(mixer.mixed_frames(id), 0);
        assert_eq!(mixer.pending_frames(id), 0);
    }

    #[test]
    fn stale_handles_do_nothing() {
        let mut mixer = Mixer::new();
        let old = mixer.open(STEREO_48K, 1).unwrap();
        mixer.close(old);
        let new = mixer.open(STEREO_48K, 2).unwrap();
        assert_eq!(mixer.write(old, &[1, 1]), 0);
        mixer.close(old);
        assert_eq!(mixer.write(new, &[1, 1]), 2);
        assert_eq!(mixer.active(), 1);
    }

    #[test]
    fn slots_run_out_cleanly() {
        let mut mixer = Mixer::new();
        for owner in 0..MAX_STREAMS {
            mixer.open(STEREO_48K, owner as u32).unwrap();
        }
        assert_eq!(mixer.open(STEREO_48K, 99), Err(OpenError::NoFreeStream));
        assert_eq!(mixer.owners()[3].unwrap().1, 3);
    }

    #[test]
    fn formats_it_cannot_convert_are_refused() {
        let mut mixer = Mixer::new();
        assert_eq!(
            mixer.open(format(768_000, 2), 1),
            Err(OpenError::UnsupportedFormat)
        );
    }

    #[test]
    fn the_device_queue_stays_short() {
        let mut mixer = Mixer::new();
        let id = mixer.open(STEREO_48K, 1).unwrap();
        mixer.write(id, &vec![3i16; RING_FRAMES * 2]);
        let device = Device::new();
        let given = mixer.pump(&device);
        assert_eq!(given, TARGET_QUEUE_FRAMES);
        assert_eq!(mixer.pump(&device), 0);
        assert_eq!(mixer.pending_frames(id), RING_FRAMES - TARGET_QUEUE_FRAMES);
        device.queued.set(0);
        assert_eq!(mixer.pump(&device), TARGET_QUEUE_FRAMES);
        assert!(device.taken.borrow().iter().all(|&sample| sample == 3));
    }

    #[test]
    fn nothing_to_play_writes_nothing() {
        // The driver drains and stops rather than playing silence forever.
        let mut mixer = Mixer::new();
        let _id = mixer.open(STEREO_48K, 1).unwrap();
        let device = Device::new();
        assert_eq!(mixer.pump(&device), 0);
        assert!(device.taken.borrow().is_empty());
    }
}
