//! `feos-mixer` puts several audio streams on the one sound device.
//!
//! The device takes one format, [`feos_audio::DRIVER_FORMAT`] (48 kHz stereo),
//! and has one ring. Each client opens a [`Stream`] in its own format, and the
//! samples it writes are converted and held in that stream's ring. A task then
//! sums whatever the streams have and hands it to the device a little at a time,
//! so the device's queue stays short, a new stream is heard quickly and volume
//! changes take effect at once.
//!
//! [`core::Mixer`] is the part with no services, which the host tests drive.
//! The rest of this crate is one shared mixer and the task that runs it:
//!
//! - Nothing here blocks. [`Stream::write`] takes what fits, like
//!   [`Audio::write`](feos_hal::Audio::write).
//! - The lock is held for a moment at a time, so a kill finds a safe point.
//! - A task killed while holding a [`Stream`] never runs its `Drop`. The mixer
//!   records who opened each stream, and closes those whose owner has gone.
//! - The task exits when nothing has been open for a while, and any stream's
//!   writes keep the device fed if someone kills it.
//! - [`Capture`] is the other direction: one recorder at a time, in the device's format.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod core_mixer;
pub mod wsola;

use core::sync::atomic::{AtomicU32, Ordering};

use feos_app::{App, Context, Platform};
use feos_audio::AudioFormat;
use feos_hal::Audio;
use feos_sync::Mutex;

pub use core_mixer::{Mixer, StreamId, MAX_STREAMS, MAX_VOLUME, RING_FRAMES, SCOPE_FRAMES};

/// How long the task sleeps between refills, in milliseconds.
const PERIOD_MS: u64 = 5;
/// How often the task looks for streams whose owner has gone.
const REAP_PERIOD_MS: u64 = 500;
/// How long the task stays with nothing open, in milliseconds.
const IDLE_EXIT_MS: u64 = 3000;

static MIXER: Mutex<Mixer> = Mutex::new(Mixer::new());
/// PID of the task, or 0.
static TASK: AtomicU32 = AtomicU32::new(0);

/// Why a stream could not be opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenError {
    /// There's no working sound device.
    NoDevice,
    /// Every stream is in use.
    NoFreeStream,
    /// This format can't be converted to the device's.
    UnsupportedFormat,
    /// No memory for the stream's ring.
    OutOfMemory,
}

/// One client's connection to the mixer. Dropping it closes the stream, and
/// anything it still holds is discarded.
pub struct Stream {
    id: StreamId,
    device: &'static dyn Audio,
}

impl Stream {
    /// Opens a stream in `format`, owned by the calling process.
    pub fn open(platform: &Platform, format: AudioFormat) -> Result<Self, OpenError> {
        let device = platform
            .audio
            .filter(|device| device.is_ready())
            .ok_or(OpenError::NoDevice)?;
        let owner = platform.processes.current();
        let id = MIXER.lock().open(format, owner)?;
        ensure_task(platform);
        Ok(Self { id, device })
    }

    /// Queues interleaved samples in the stream's format, without blocking.
    /// Returns how many were taken, always whole frames.
    pub fn write(&self, samples: &[i16]) -> usize {
        let taken = {
            let mut mixer = MIXER.lock();
            let taken = mixer.write(self.id, samples);
            // In case the task is gone or starved.
            mixer.pump(self.device);
            taken
        };
        taken
    }

    /// Frames waiting in this stream that haven't reached the device.
    pub fn pending_frames(&self) -> usize {
        MIXER.lock().pending_frames(self.id)
    }

    /// Frames that this stream has fed to the device since it opened or was
    /// flushed, and that have also finished playing.
    pub fn played_frames(&self) -> u64 {
        let queued = self.device.queued() as u64 / 2;
        MIXER.lock().mixed_frames(self.id).saturating_sub(queued)
    }

    /// Drops everything waiting, and starts the [`played_frames`] count again.
    ///
    /// [`played_frames`]: Self::played_frames
    pub fn flush(&self) {
        MIXER.lock().flush(self.id);
    }

    /// Stops or resumes taking samples from this stream. The position holds.
    pub fn set_paused(&self, paused: bool) {
        MIXER.lock().set_paused(self.id, paused);
    }

    /// Sets this stream's own level, 0 to [`MAX_VOLUME`]. It applies on top of
    /// the master volume.
    pub fn set_gain(&self, level: u8) {
        MIXER.lock().set_gain(self.id, level);
    }

    /// Output frames the rings can take right now, for this stream.
    pub fn writable_frames(&self) -> usize {
        MIXER.lock().free_frames(self.id)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        MIXER.lock().close(self.id);
    }
}

/// The task recording, or 0. There's one input, and one reader of it.
static CAPTURE_OWNER: AtomicU32 = AtomicU32::new(0);

/// Sound coming in from the device, in its format, [`feos_audio::DRIVER_FORMAT`].
/// Dropping it stops recording. If its owner is killed, the mixer's task does that.
pub struct Capture {
    device: &'static dyn Audio,
}

impl Capture {
    /// Starts recording. There's only the one input, so this fails with
    /// [`OpenError::NoFreeStream`] while something else has it.
    pub fn open(platform: &Platform) -> Result<Self, OpenError> {
        let device = platform
            .audio
            .filter(|device| device.is_ready())
            .ok_or(OpenError::NoDevice)?;
        let owner = platform.processes.current();
        let holder = CAPTURE_OWNER.load(Ordering::Acquire);
        if holder != 0 && platform.processes.detail(holder).is_some() {
            return Err(OpenError::NoFreeStream);
        }
        CAPTURE_OWNER.store(owner.max(1), Ordering::Release);
        device.capture_stop();
        if !device.capture_start() {
            CAPTURE_OWNER.store(0, Ordering::Release);
            return Err(OpenError::NoDevice);
        }
        ensure_task(platform);
        Ok(Self { device })
    }

    /// Moves what's been recorded to `out` as whole frames, without blocking, and says how
    /// many samples. The device holds about a second, so call this more often than that.
    pub fn read(&self, out: &mut [i16]) -> usize {
        self.device.capture_read(out)
    }

    /// Frames recorded so far, read or not.
    pub fn position(&self) -> u64 {
        self.device.capture_position()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.device.capture_stop();
        CAPTURE_OWNER.store(0, Ordering::Release);
    }
}

/// The master volume, 0 to [`MAX_VOLUME`].
pub fn volume() -> u8 {
    MIXER.lock().volume()
}

/// Sets the master volume, 0 to [`MAX_VOLUME`].
pub fn set_volume(level: u8) {
    MIXER.lock().set_volume(level);
}

/// Whether the output is muted. Streams keep moving while it is.
pub fn muted() -> bool {
    MIXER.lock().muted()
}

/// Mutes or unmutes the output.
pub fn set_muted(muted: bool) {
    MIXER.lock().set_muted(muted);
}

/// The sound being heard now, for a visualiser: see [`Mixer::scope`]. Fills
/// `out` with interleaved stereo at the device's rate
/// ([`feos_audio::DRIVER_FORMAT`]) and returns the count of frames ever
/// mixed, which stops moving when nothing plays.
pub fn scope(out: &mut [i16]) -> u64 {
    MIXER.lock().scope(out)
}

/// How many streams are open.
pub fn active_streams() -> usize {
    MIXER.lock().active()
}

fn ensure_task(platform: &Platform) {
    let pid = TASK.load(Ordering::Acquire);
    if pid != 0 && platform.processes.detail(pid).is_some() {
        return;
    }
    if let Ok(pid) = platform.processes.spawn(&MixerTask, "") {
        TASK.store(pid, Ordering::Release);
    }
}

/// The task that keeps the device fed.
pub struct MixerTask;

impl App for MixerTask {
    fn name(&self) -> &'static str {
        "mixer"
    }

    fn usage(&self) -> &'static str {
        "(started with the first audio stream)"
    }

    fn description(&self) -> &'static str {
        "mix audio streams onto the sound device"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let platform = context.platform;
        let Some(device) = platform.audio else {
            return 1;
        };
        let processes = platform.processes;
        let clock = platform.clock;
        let mut next_reap = clock.uptime_ms() + REAP_PERIOD_MS;
        let mut idle_since = None;
        while !processes.kill_requested() {
            let now = clock.uptime_ms();
            let active = {
                let mut mixer = MIXER.lock();
                mixer.pump(device);
                mixer.active()
            };
            if now >= next_reap {
                next_reap = now + REAP_PERIOD_MS;
                let owners = MIXER.lock().owners();
                for (id, owner) in owners.into_iter().flatten() {
                    // This checks the process table, so not under the lock.
                    if processes.detail(owner).is_none() {
                        MIXER.lock().close(id);
                    }
                }
                let holder = CAPTURE_OWNER.load(Ordering::Acquire);
                if holder != 0 && processes.detail(holder).is_none() {
                    device.capture_stop();
                    CAPTURE_OWNER.store(0, Ordering::Release);
                }
            }
            if active > 0 || CAPTURE_OWNER.load(Ordering::Acquire) != 0 {
                idle_since = None;
            } else {
                let since = *idle_since.get_or_insert(now);
                if now.saturating_sub(since) >= IDLE_EXIT_MS {
                    break;
                }
            }
            processes.sleep_ms(PERIOD_MS);
        }
        TASK.store(0, Ordering::Release);
        0
    }
}
