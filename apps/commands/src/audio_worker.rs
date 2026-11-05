//! Audio playback on its own task: files, or the audio track of a video.
//!
//! As with [`crate::decode_worker`], the task owns the file and the decoder,
//! and an [`AudioPipe`] carries requests and reports, with its lock held only to
//! copy them. The task feeds a [`feos_mixer::Stream`], which never blocks, so
//! when the mixer is full the task sleeps. Everything is checked between
//! packets, so stopping takes one packet at most.
//!
//! The position comes from the frames the mixer says have finished playing, and
//! not from a timer: it's what the player's video clock follows.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use feos_app::{App, Context, Platform};
use feos_audio::AudioFormat as MixFormat;
use feos_audio_codec::packet::grow_samples;
use feos_audio_codec::{AudioError, AudioFormat, AudioStream, PacketDecoder, PacketFormat};
use feos_fs::{FileSystem, NodeId};
use feos_mixer::{OpenError, Stream};
use feos_sync::Mutex;
use feos_video::{audio_track, AudioCodec, AudioTrack};

use crate::decode_worker::read_file;

/// How long the task sleeps with the mixer full, or with nothing to do.
const SLEEP_MS: u64 = 5;
/// Damaged packets in a row before giving up on a file.
const MAX_BAD_PACKETS: u32 = 64;
/// The largest packet's worth of samples worth having memory for.
const MAX_CHUNK_SAMPLES: usize = 1 << 20;
/// Output frames to have waiting before sound starts: a sixth of a second.
const PRIME_FRAMES: usize = 8_000;
/// The mixer's rate, which finished frames are counted in.
const OUTPUT_RATE: u64 = 48_000;

/// A short description of what's playing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrackInfo {
    /// A format or codec name.
    pub codec: &'static str,
    /// Sample rate of the source, in Hz.
    pub sample_rate: u32,
    /// Channels in the source.
    pub channels: u16,
    /// How long it lasts, or 0 if nobody knows.
    pub duration_ms: u64,
}

/// Why something can't be played.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AudioFailure {
    /// No device, or no stream for this format.
    Mixer(OpenError),
    /// The file can't be decoded.
    Codec(AudioError),
    /// The file couldn't be read.
    Unreadable,
    /// There's no audio track here that this can decode.
    NoTrack,
}

/// Which audio a request refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The file is an audio file.
    File,
    /// The file is a video, and this plays its audio track.
    Track,
}

struct Inner {
    owner: u32,
    media: u32,
    open: Option<(NodeId, Source)>,
    paused: bool,
    seek: Option<u64>,
    info: Option<TrackInfo>,
    failure: Option<AudioFailure>,
    ended: bool,
}

/// What the window learns from a report.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioReport {
    /// The file opened.
    pub info: Option<TrackInfo>,
    /// It can't play.
    pub failure: Option<AudioFailure>,
    /// It played to its end, and the device has finished too.
    pub ended: bool,
}

/// The connection between a client and an audio task.
pub struct AudioPipe {
    inner: Mutex<Inner>,
    position_ms: AtomicU64,
}

impl AudioPipe {
    /// Creates an unowned pipe.
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                owner: 0,
                media: 0,
                open: None,
                paused: false,
                seek: None,
                info: None,
                failure: None,
                ended: false,
            }),
            position_ms: AtomicU64::new(0),
        }
    }

    /// Hands the pipe to a new task, sending any previous one away. Returns the
    /// new task's identity.
    pub fn claim(&self) -> u32 {
        let mut inner = self.inner.lock();
        inner.owner = inner.owner.wrapping_add(1).max(1);
        inner.media = inner.media.wrapping_add(1);
        inner.open = None;
        inner.owner
    }

    /// Sends the task away. It stops within a packet and frees everything.
    pub fn shut_down(&self) {
        let _ = self.claim();
    }

    /// Asks for a file, abandoning the last one.
    pub fn open(&self, node: NodeId, source: Source, paused: bool) {
        let mut inner = self.inner.lock();
        inner.media = inner.media.wrapping_add(1);
        inner.open = Some((node, source));
        inner.paused = paused;
        inner.seek = None;
        inner.info = None;
        inner.failure = None;
        inner.ended = false;
        self.position_ms.store(0, Ordering::Relaxed);
    }

    /// The number the current request goes by.
    pub fn media(&self) -> u32 {
        self.inner.lock().media
    }

    /// Takes the waiting request, for a caller that is its own task.
    pub fn take_open(&self) -> Option<(NodeId, Source)> {
        self.inner.lock().open.take()
    }

    /// Abandons the file, so the task frees it.
    pub fn close_media(&self) {
        let mut inner = self.inner.lock();
        inner.media = inner.media.wrapping_add(1);
        inner.open = None;
    }

    /// Moves playback to a time.
    pub fn seek(&self, ms: u64) {
        let mut inner = self.inner.lock();
        inner.seek = Some(ms);
        inner.ended = false;
        self.position_ms.store(ms, Ordering::Relaxed);
    }

    /// Pauses or resumes.
    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().paused = paused;
    }

    /// Where playback is: the time of the sound leaving the device now.
    pub fn position_ms(&self) -> u64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    /// Sets the position, as a task would.
    #[cfg(test)]
    pub(crate) fn position_ms_for_test(&self, ms: u64) {
        self.position_ms.store(ms, Ordering::Relaxed);
    }

    /// Collects what the task has reported since the last call.
    pub fn report(&self) -> AudioReport {
        let mut inner = self.inner.lock();
        AudioReport {
            info: inner.info.take(),
            failure: inner.failure.take(),
            ended: core::mem::take(&mut inner.ended),
        }
    }
}

/// The name of a file format, for display.
fn format_name(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Wav => "WAV",
        AudioFormat::Aiff => "AIFF",
        AudioFormat::Au => "AU",
        AudioFormat::Mp3 => "MP3",
        AudioFormat::OggOpus => "Opus",
        AudioFormat::OggVorbis => "Vorbis",
        AudioFormat::OggFlac | AudioFormat::Flac => "FLAC",
        AudioFormat::Aac => "AAC",
    }
}

/// Audio out of a video container, decoded one packet at a time.
struct TrackSource<'a> {
    input: &'a [u8],
    track: AudioTrack,
    decoder: PacketDecoder<'a>,
    next: usize,
    /// Output to discard after a seek, in source frames.
    skip: u64,
}

impl<'a> TrackSource<'a> {
    fn open(input: &'a [u8]) -> Result<Self, AudioFailure> {
        let track = audio_track(input).ok_or(AudioFailure::NoTrack)?;
        let config = input.get(track.config.clone()).unwrap_or(&[]);
        let format = match track.codec {
            AudioCodec::Aac => PacketFormat::Aac(config),
            AudioCodec::Mp3 => PacketFormat::Mp3,
            AudioCodec::Vorbis => PacketFormat::Vorbis(config),
            AudioCodec::Flac => PacketFormat::Flac(config),
            AudioCodec::Pcm {
                bytes,
                big_endian,
                float,
            } => PacketFormat::Pcm {
                bytes,
                big_endian,
                float,
            },
            AudioCodec::MuLaw => PacketFormat::MuLaw,
            AudioCodec::ALaw => PacketFormat::ALaw,
            AudioCodec::Opus | AudioCodec::Other => return Err(AudioFailure::NoTrack),
        };
        let decoder = PacketDecoder::new(format, track.sample_rate, track.channels)
            .map_err(AudioFailure::Codec)?;
        Ok(Self {
            input,
            track,
            decoder,
            next: 0,
            skip: 0,
        })
    }

    fn duration_ms(&self) -> u64 {
        self.track.packets.last().map_or(0, |packet| packet.pts_ms)
    }

    fn seek(&mut self, ms: u64) -> Result<(), AudioError> {
        self.decoder.reset()?;
        self.next = self.track.packet_at(ms);
        let at = self.track.packets.get(self.next).map_or(0, |packet| packet.pts_ms);
        // The packet before it starts the overlap that the target packet needs.
        let rate = self.decoder.sample_rate() as u64;
        let before = self.next.min(1);
        let lead = at - self.track.packets[self.next - before].pts_ms;
        self.next -= before;
        self.skip = (lead + ms.saturating_sub(at)) * rate / 1000;
        Ok(())
    }

    fn next_chunk(&mut self, out: &mut [i16]) -> Result<Option<usize>, AudioError> {
        let Some(packet) = self.track.packet(self.input, self.next) else {
            return Ok(None);
        };
        let frames = self.decoder.decode(packet, out);
        // A short buffer leaves the packet to be tried again.
        if !matches!(frames, Err(AudioError::BufferTooSmall { .. })) {
            self.next += 1;
        }
        let frames = frames?;
        let channels = self.decoder.channels() as usize;
        let dropped = (frames as u64).min(self.skip) as usize;
        self.skip -= dropped as u64;
        if dropped > 0 {
            out.copy_within(dropped * channels..frames * channels, 0);
        }
        Ok(Some(frames - dropped))
    }
}

/// Anything the playback loop can pull audio from.
enum Puller<'a> {
    File(AudioStream<'a>),
    Track(TrackSource<'a>),
}

impl Puller<'_> {
    fn format(&self) -> Option<MixFormat> {
        match self {
            Self::File(stream) => {
                let info = stream.info();
                MixFormat::new(info.sample_rate, info.channels)
            }
            Self::Track(track) => {
                MixFormat::new(track.decoder.sample_rate(), track.decoder.channels())
            }
        }
    }

    fn channels(&self) -> usize {
        match self {
            Self::File(stream) => stream.info().channels as usize,
            Self::Track(track) => track.decoder.channels() as usize,
        }
    }

    fn chunk_samples(&self) -> usize {
        match self {
            Self::File(stream) => stream.max_chunk_samples(),
            Self::Track(track) => track.decoder.typical_samples(),
        }
    }

    fn info(&self) -> TrackInfo {
        match self {
            Self::File(stream) => {
                let info = stream.info();
                TrackInfo {
                    codec: format_name(info.format),
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                    duration_ms: info.duration_ms(),
                }
            }
            Self::Track(track) => TrackInfo {
                codec: track.track.codec.name(),
                sample_rate: track.decoder.sample_rate(),
                channels: track.decoder.channels(),
                duration_ms: track.duration_ms(),
            },
        }
    }

    fn seek(&mut self, ms: u64) -> Result<(), AudioError> {
        match self {
            Self::File(stream) => {
                let rate = stream.info().sample_rate as u64;
                stream.seek(ms.saturating_mul(rate) / 1000)
            }
            Self::Track(track) => track.seek(ms),
        }
    }

    fn next_chunk(&mut self, out: &mut [i16]) -> Result<Option<usize>, AudioError> {
        match self {
            Self::File(stream) => stream.next_chunk(out),
            Self::Track(track) => track.next_chunk(out),
        }
    }
}

/// Why [`play_media`] came back.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Finished {
    /// It played to its end.
    Ended,
    /// The pipe asked for something else, or the process was told to stop.
    Interrupted,
    /// It couldn't play. The pipe has been told too.
    Failed(AudioFailure),
}

/// Plays one file until it ends or the pipe wants something else. `identity` and
/// `media` are what the pipe has to keep saying for this to carry on. With
/// `wait_at_end`, the end is reported and this waits for the next request.
pub fn play_media(
    pipe: &AudioPipe,
    platform: &Platform,
    fs: &dyn FileSystem,
    node: NodeId,
    source: Source,
    identity: u32,
    media: u32,
    wait_at_end: bool,
) -> Finished {
    let fail = |failure| {
        let mut inner = pipe.inner.lock();
        if inner.media == media {
            inner.failure = Some(failure);
        }
        Finished::Failed(failure)
    };
    let Some(data) = read_file(fs, node, fs.size(node)) else {
        return fail(AudioFailure::Unreadable);
    };
    // A sound-only MP4 is a video container with no picture in it.
    let source = match source {
        Source::File if feos_video::detect(&data).is_some() => Source::Track,
        other => other,
    };
    let puller = match source {
        Source::File => AudioStream::open(&data)
            .map(Puller::File)
            .map_err(AudioFailure::Codec),
        Source::Track => TrackSource::open(&data).map(Puller::Track),
    };
    let mut puller = match puller {
        Ok(puller) => puller,
        Err(failure) => return fail(failure),
    };
    let Some(format) = puller.format() else {
        return fail(AudioFailure::Mixer(OpenError::UnsupportedFormat));
    };
    let mut stream = match Stream::open(platform, format) {
        Ok(stream) => stream,
        Err(error) => return fail(AudioFailure::Mixer(error)),
    };
    let mut chunk = Vec::new();
    if !grow_samples(&mut chunk, puller.chunk_samples().clamp(4096, MAX_CHUNK_SAMPLES)) {
        return fail(AudioFailure::Codec(AudioError::OutOfMemory));
    }
    {
        let mut inner = pipe.inner.lock();
        if inner.media == media {
            inner.info = Some(puller.info());
        }
    }

    let processes = platform.processes;
    // A slow decoder gets a head start, so one late packet isn't a gap in the
    // sound. It's released as soon as there's enough, or nothing more to come.
    let mut priming = true;
    stream.set_paused(true);
    let (mut at, mut len) = (0usize, 0usize);
    let mut base_ms = 0u64;
    let mut paused = false;
    let mut finished = false;
    let mut reported_end = false;
    let mut bad_packets = 0u32;
    loop {
        if processes.kill_requested() {
            return Finished::Interrupted;
        }
        let (want_pause, seek) = {
            let mut inner = pipe.inner.lock();
            if inner.owner != identity || inner.media != media {
                return Finished::Interrupted;
            }
            (inner.paused, inner.seek.take())
        };
        if let Some(ms) = seek {
            stream.flush();
            priming = true;
            stream.set_paused(true);
            at = 0;
            len = 0;
            base_ms = ms;
            finished = false;
            reported_end = false;
            if puller.seek(ms).is_err() {
                finished = true;
            }
        }
        if priming && (finished || stream.pending_frames() >= PRIME_FRAMES) {
            priming = false;
            stream.set_paused(paused);
        }
        if want_pause != paused {
            paused = want_pause;
            if !priming {
                stream.set_paused(paused);
            }
        }
        let played = stream.played_frames();
        pipe.position_ms
            .store(base_ms + played * 1000 / OUTPUT_RATE, Ordering::Relaxed);

        if paused {
            processes.sleep_ms(SLEEP_MS);
            continue;
        }
        if at == len && !finished {
            match puller.next_chunk(&mut chunk) {
                Ok(Some(frames)) => {
                    bad_packets = 0;
                    at = 0;
                    len = frames * puller.channels();
                    // A file can change its layout partway. Follow it.
                    if let Some(now) = puller.format() {
                        if now != format && len > 0 {
                            if let Ok(reopened) = Stream::open(platform, now) {
                                stream = reopened;
                            }
                        }
                    }
                }
                Ok(None) => finished = true,
                Err(AudioError::BufferTooSmall { required, .. })
                    if required <= MAX_CHUNK_SAMPLES && grow_samples(&mut chunk, required) => {}
                Err(_) => {
                    bad_packets += 1;
                    if bad_packets >= MAX_BAD_PACKETS {
                        finished = true;
                    }
                }
            }
        }
        if at < len {
            let taken = stream.write(&chunk[at..len]);
            at += taken;
            if taken == 0 {
                processes.sleep_ms(SLEEP_MS);
            } else {
                processes.yield_now();
            }
            continue;
        }
        if finished {
            // Keep the device fed, and notice when it has played the last of it.
            let _ = stream.write(&[]);
            if stream.pending_frames() == 0 && platform.audio.is_none_or(|d| d.queued() == 0) {
                if !wait_at_end {
                    return Finished::Ended;
                }
                if !reported_end {
                    reported_end = true;
                    pipe.inner.lock().ended = true;
                }
            }
            processes.sleep_ms(SLEEP_MS);
            continue;
        }
        processes.yield_now();
    }
}

/// The audio task for a window. Its arguments say which pipe, and its identity.
pub struct AudioTask;

/// The media player's pipe.
pub static PLAYER_PIPE: AudioPipe = AudioPipe::new();
/// The Music window's pipe.
pub static MUSIC_PIPE: AudioPipe = AudioPipe::new();

impl App for AudioTask {
    fn name(&self) -> &'static str {
        "audio-decoder"
    }

    fn usage(&self) -> &'static str {
        "(started by the media apps)"
    }

    fn description(&self) -> &'static str {
        "decode audio for the media apps"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(fs) = context.platform.filesystem else {
            return 1;
        };
        let mut words = context.args.split_ascii_whitespace();
        let pipe = match words.next() {
            Some("player") => &PLAYER_PIPE,
            Some("music") => &MUSIC_PIPE,
            _ => return 2,
        };
        let Some(identity) = words.next().and_then(|word| word.parse().ok()) else {
            return 2;
        };
        let platform = context.platform;
        let processes = platform.processes;
        while !processes.kill_requested() {
            let request = {
                let mut inner = pipe.inner.lock();
                if inner.owner != identity {
                    return 0;
                }
                inner.open.take().map(|(node, source)| (node, source, inner.media))
            };
            let Some((node, source, media)) = request else {
                processes.sleep_ms(SLEEP_MS);
                continue;
            };
            play_media(pipe, platform, fs, node, source, identity, media, true);
        }
        0
    }
}
