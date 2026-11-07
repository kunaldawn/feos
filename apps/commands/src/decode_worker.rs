//! A video decoder on its own task, with a queue of finished frames.
//!
//! Opening a file and decoding its frames can take as long as the file likes,
//! so none of it runs on the compositor thread. The window asks for things
//! through a [`Pipe`], and a [`Worker`] answers in the queue:
//!
//! - The lock in a [`Pipe`] is only ever held to move a request or a buffer,
//!   never while reading a file or decoding. The window therefore can't stall
//!   on the worker, and a kill can always find a safe point: the kernel won't
//!   reap a task holding a lock, and this one holds it for a few instructions.
//! - The worker owns the file and the decoder, so nobody else needs a lock to
//!   reach them. It polls for shutdown and for kills between samples, returns,
//!   and frees them itself.
//! - Requests and frames carry a generation. A window that opens another file
//!   or closes doesn't wait for the worker; whatever it was working on is
//!   discarded when it next looks. A window can also reopen while an old worker
//!   is still on its way out: the old one no longer owns the pipe and leaves.

use alloc::boxed::Box;
use alloc::vec::Vec;

use feos_app::{App, Context};
use feos_fs::{FileSystem, NodeId};
use feos_sync::Mutex;
use feos_video::{
    info as video_info, DecodedFrame, DecoderLimits, Source, VideoDecoder, VideoError, VideoInfo,
};

/// Largest file the player reads into memory. One larger stays where it is, and is
/// read a sample at a time: see [`FileSource`].
pub const MAX_MEDIA_BYTES: usize = 16 * 1024 * 1024;
/// Largest decoded frame: 1920x1088 in RGBA.
pub const MAX_FRAME_BYTES: usize = 1920 * 1088 * 4;
/// Limits for the decoder, within the 64 MiB kernel heap that also holds the
/// file and frames above. Level-based reference counts for 1080p fit in this.
const DECODER_LIMITS: DecoderLimits = DecoderLimits {
    max_frames: 1 << 18,
    max_width: 4096,
    max_height: 4096,
    max_memory: 28 * 1024 * 1024,
};
/// Most frames waiting in the queue. Bigger frames get fewer of these.
const MAX_QUEUE: usize = 6;
/// Memory all queued frames may take together.
const QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// How long the task sleeps with nothing to do, in milliseconds.
const IDLE_SLEEP_MS: u64 = 4;

/// What a [`Pipe::seek`] lands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seek {
    /// The frame on screen at a time, in milliseconds.
    Time(u64),
    /// A frame, by display index.
    Frame(u32),
}

/// The properties of a file the worker has opened.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    /// The file's properties.
    pub info: VideoInfo,
    /// Frames in the sample table.
    pub frame_count: u32,
    /// Duration from the sample table.
    pub duration_ms: u64,
    /// Bytes in an RGBA frame.
    pub rgba_len: usize,
}

/// Why a file can't be played, for the window to report.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Failure {
    /// Reading or decoding failed.
    Video(VideoError),
    /// The file couldn't be read, or memory for it ran out.
    Unreadable,
    /// The file's first frame is larger than [`MAX_FRAME_BYTES`].
    FrameTooLarge,
    /// The codec is known, but no decoder exists for it.
    NoDecoder(VideoInfo),
}

struct Ready {
    frame: DecodedFrame,
    rgba: Vec<u8>,
    /// This frame was asked for, so it shows whatever the clock says.
    requested: bool,
}

/// Everything behind the lock. All of it is cheap to touch.
struct Inner {
    /// The worker that owns this pipe. Any other leaves when it next looks.
    owner: u32,
    /// Counts opened files; a new one discards all work on the last one.
    media: u32,
    open: Option<NodeId>,
    /// The next request to land on, and how many seeks there have been.
    seek: Option<Seek>,
    seek_id: u32,
    playing: bool,
    /// Where the presentation clock is, when frames may be skipped to keep up.
    skip_to: Option<u64>,

    opened: Option<Opened>,
    failure: Option<Failure>,
    /// One frame was damaged; playback carries on.
    damaged: Option<VideoError>,
    eos: bool,
    /// Frames waiting, oldest first.
    queue: [Option<Ready>; MAX_QUEUE],
    queued: usize,
    /// How many frames may wait, set by the worker from the frame size.
    depth: usize,
    /// Buffers ready for reuse, which keeps playback from reallocating.
    spare: [Vec<u8>; MAX_QUEUE + 1],
}

impl Inner {
    fn clear_queue(&mut self) {
        for slot in &mut self.queue {
            if let Some(ready) = slot.take() {
                give_back(&mut self.spare, ready.rgba);
            }
        }
        self.queued = 0;
    }

    /// Drops every buffer, because the next file has its own frame size.
    fn forget_media(&mut self) {
        self.clear_queue();
        for buffer in &mut self.spare {
            *buffer = Vec::new();
        }
        self.opened = None;
        self.failure = None;
        self.damaged = None;
        self.eos = false;
        self.skip_to = None;
        self.seek = None;
    }
}

fn give_back(spare: &mut [Vec<u8>], buffer: Vec<u8>) {
    if buffer.capacity() == 0 {
        return;
    }
    if let Some(slot) = spare.iter_mut().find(|slot| slot.capacity() == 0) {
        *slot = buffer;
    }
}

/// What a window's next tick learns from the worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    /// The file finished opening.
    pub opened: Option<Opened>,
    /// The file can't be played.
    pub failure: Option<Failure>,
    /// A frame was lost, but later ones may be fine.
    pub damaged: Option<VideoError>,
    /// The last frame has been decoded.
    pub eos: bool,
    /// Frames waiting in the queue.
    pub queued: usize,
}

/// The connection between a window and its worker.
pub struct Pipe {
    inner: Mutex<Inner>,
}

impl Pipe {
    /// Creates an unowned pipe.
    pub const fn new() -> Self {
        const NO_FRAME: Option<Ready> = None;
        const NO_BUFFER: Vec<u8> = Vec::new();
        Self {
            inner: Mutex::new(Inner {
                owner: 0,
                media: 0,
                open: None,
                seek: None,
                seek_id: 0,
                playing: false,
                skip_to: None,
                opened: None,
                failure: None,
                damaged: None,
                eos: false,
                queue: [NO_FRAME; MAX_QUEUE],
                queued: 0,
                depth: 2,
                spare: [NO_BUFFER; MAX_QUEUE + 1],
            }),
        }
    }

    /// Hands the pipe to a new worker, sending any previous one away. Returns
    /// the identity the new worker runs with.
    pub fn claim(&self) -> u32 {
        let mut inner = self.inner.lock();
        inner.owner = inner.owner.wrapping_add(1).max(1);
        inner.forget_media();
        inner.open = None;
        inner.playing = false;
        inner.media = inner.media.wrapping_add(1);
        inner.owner
    }

    /// Sends the worker away and drops all queued frames. It frees the file and
    /// decoder itself, in its own time; this returns at once.
    pub fn shut_down(&self) {
        let _ = self.claim();
    }

    /// Asks for a new file, abandoning the last one.
    pub fn open(&self, node: NodeId, playing: bool) {
        let mut inner = self.inner.lock();
        inner.forget_media();
        inner.media = inner.media.wrapping_add(1);
        inner.open = Some(node);
        inner.playing = playing;
        // The first frame of a file is a request, playing or not.
        inner.seek = Some(Seek::Frame(0));
        inner.seek_id = inner.seek_id.wrapping_add(1);
    }

    /// Abandons the open file, so the worker frees it.
    pub fn close_media(&self) {
        let mut inner = self.inner.lock();
        inner.forget_media();
        inner.media = inner.media.wrapping_add(1);
        inner.open = None;
        inner.playing = false;
    }

    /// Frames waiting right now.
    pub fn queued(&self) -> usize {
        self.inner.lock().queued
    }

    /// Drops what's queued and asks for one frame to land on. The answer is
    /// handed out by [`take_requested`](Self::take_requested).
    pub fn seek(&self, seek: Seek) {
        let mut inner = self.inner.lock();
        inner.clear_queue();
        inner.seek = Some(seek);
        inner.seek_id = inner.seek_id.wrapping_add(1);
        inner.eos = false;
    }

    /// Lets the worker decode ahead, or stops it after this frame.
    pub fn set_playing(&self, playing: bool) {
        self.inner.lock().playing = playing;
    }

    /// Tells the worker where an external clock (the audio) is, so it can skip
    /// frames it would show late. `None` means every frame gets shown.
    pub fn set_skip_to(&self, clock_ms: Option<u64>) {
        self.inner.lock().skip_to = clock_ms;
    }

    /// Collects what the worker has reported since the last call.
    pub fn report(&self) -> Report {
        let mut inner = self.inner.lock();
        Report {
            opened: inner.opened.take(),
            failure: inner.failure.take(),
            damaged: inner.damaged.take(),
            eos: inner.eos,
            queued: inner.queued,
        }
    }

    /// Hands over the frame a seek was waiting for, if it's ready. `display`
    /// takes its pixels, and its old buffer goes back in the pool.
    pub fn take_requested(&self, display: &mut Vec<u8>) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        if !inner.queue[0].as_ref().is_some_and(|ready| ready.requested) {
            return None;
        }
        Some(pop_into(inner, display))
    }

    /// Hands over the latest frame due at `clock_ms`, and recycles any earlier
    /// ones. `display` takes its pixels; its old buffer goes back in the pool.
    pub fn take_due(&self, clock_ms: u64, display: &mut Vec<u8>) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let mut shown = None;
        while inner.queue[0]
            .as_ref()
            .is_some_and(|ready| ready.frame.pts_ms <= clock_ms)
        {
            shown = Some(pop_into(inner, display));
        }
        shown
    }
}

/// Takes the oldest frame. The caller has checked that there is one.
fn pop_into(inner: &mut Inner, display: &mut Vec<u8>) -> DecodedFrame {
    let ready = inner.queue[0].take().expect("the caller checked");
    inner.queue.rotate_left(1);
    inner.queued -= 1;
    let old = core::mem::replace(display, ready.rgba);
    give_back(&mut inner.spare, old);
    ready.frame
}

/// What one [`Worker::step`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// There's more to do right now.
    Busy,
    /// Nothing to do until the window asks for something or takes a frame.
    Idle,
    /// The pipe belongs to someone else. Stop stepping.
    Exit,
}

/// The state a worker owns: the file, the decoder and a buffer in progress.
pub struct Worker {
    identity: u32,
    media: u32,
    decoder: Option<VideoDecoder<Vec<u8>>>,
    /// The next frame to decode.
    next: u32,
    seek_id: u32,
    /// `next` was asked for: it gets decoded even when paused.
    requested: bool,
    buffer: Vec<u8>,
    rgba_len: usize,
    /// A frame of this file has been decoded, which the boot test looks for.
    first_shown: bool,
}

impl Worker {
    /// A worker that owns the pipe under `identity`, from [`Pipe::claim`].
    pub fn new(identity: u32) -> Self {
        Self {
            identity,
            media: u32::MAX,
            decoder: None,
            next: 0,
            seek_id: 0,
            requested: false,
            buffer: Vec::new(),
            rgba_len: 0,
            first_shown: false,
        }
    }

    /// Does one short piece of work: at most one file open or one sample.
    pub fn step(&mut self, pipe: &Pipe, fs: &'static dyn FileSystem) -> Step {
        let mut to_open = None;
        let mut stale = None;
        let mut skip_to = None;
        let decode = {
            let mut inner = pipe.inner.lock();
            let inner = &mut *inner;
            if inner.owner != self.identity {
                return Step::Exit;
            }
            if inner.media != self.media {
                self.media = inner.media;
                stale = self.decoder.take();
                self.requested = false;
                self.next = 0;
                to_open = inner.open.take();
            }
            if let Some(decoder) = self.decoder.as_ref() {
                if let Some(seek) = inner.seek.take() {
                    self.next = match seek {
                        Seek::Time(ms) => decoder.frame_at_time(ms),
                        Seek::Frame(index) => index,
                    };
                    self.seek_id = inner.seek_id;
                    self.requested = true;
                }
                skip_to = inner.skip_to;
                let wanted = self.requested
                    || (inner.playing && !inner.eos && inner.queued < inner.depth);
                if wanted && self.buffer.capacity() == 0 {
                    if let Some(slot) = inner.spare.iter_mut().find(|slot| slot.capacity() != 0) {
                        self.buffer = core::mem::take(slot);
                    }
                }
                wanted
            } else {
                false
            }
        };
        // Freed out here: a big file takes a moment to give back.
        if stale.is_some() {
            self.buffer = Vec::new();
        }
        drop(stale);

        if let Some(node) = to_open {
            self.buffer = Vec::new();
            self.open(pipe, fs, node);
            return Step::Busy;
        }
        if !decode {
            return Step::Idle;
        }
        self.decode_next(pipe, skip_to)
    }

    fn open(&mut self, pipe: &Pipe, fs: &'static dyn FileSystem, node: NodeId) {
        match open_file(fs, node) {
            Ok((decoder, opened)) => {
                feos_console::println!(
                    "[player] opened {} {}x{}, {} frames, {} ms",
                    opened.info.codec.name(),
                    opened.info.width,
                    opened.info.height,
                    opened.frame_count,
                    opened.duration_ms
                );
                self.first_shown = false;
                self.rgba_len = opened.rgba_len;
                self.decoder = Some(decoder);
                let mut inner = pipe.inner.lock();
                if inner.media == self.media {
                    inner.opened = Some(opened);
                    inner.depth = (QUEUE_BYTES / opened.rgba_len.max(1)).clamp(2, MAX_QUEUE);
                }
            }
            Err(failure) => self.fail(pipe, failure),
        }
    }

    fn fail(&mut self, pipe: &Pipe, failure: Failure) {
        self.decoder = None;
        let mut inner = pipe.inner.lock();
        if inner.media == self.media {
            inner.failure = Some(failure);
        }
    }

    fn decode_next(&mut self, pipe: &Pipe, skip_to: Option<u64>) -> Step {
        if self.buffer.capacity() == 0 {
            match zeroed(self.rgba_len) {
                Some(buffer) => self.buffer = buffer,
                None => {
                    // Frames in the queue will come back. With none, it's over.
                    if pipe.inner.lock().queued > 0 {
                        return Step::Idle;
                    }
                    self.fail(pipe, Failure::Video(VideoError::OutOfMemory));
                    return Step::Busy;
                }
            }
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return Step::Idle;
        };
        let count = decoder.frame_count();
        let target = match skip_to {
            Some(clock) if !self.requested => self.next.max(decoder.frame_at_time(clock)),
            _ => self.next,
        };
        match decoder.advance(target, &mut self.buffer, 1) {
            Ok(Some(frame)) => {
                if !self.first_shown {
                    self.first_shown = true;
                    feos_console::println!("[player] first frame decoded");
                }
                self.next = frame.index.saturating_add(1);
                let requested = core::mem::take(&mut self.requested);
                let rgba = core::mem::take(&mut self.buffer);
                let mut inner = pipe.inner.lock();
                let inner = &mut *inner;
                if inner.owner != self.identity
                    || inner.media != self.media
                    || inner.seek_id != self.seek_id
                    || inner.seek.is_some()
                    || inner.queued >= MAX_QUEUE
                {
                    give_back(&mut inner.spare, rgba);
                    return Step::Busy;
                }
                inner.queue[inner.queued] = Some(Ready {
                    frame,
                    rgba,
                    requested,
                });
                inner.queued += 1;
                if self.next >= count {
                    inner.eos = true;
                }
                Step::Busy
            }
            Ok(None) => Step::Busy,
            Err(VideoError::FrameOutOfRange { .. }) => {
                self.requested = false;
                let mut inner = pipe.inner.lock();
                if inner.media == self.media {
                    inner.eos = true;
                }
                Step::Busy
            }
            // A frame can be larger than the file's first one said. The decoder
            // holds it, so ask again once the buffer fits.
            Err(VideoError::BufferTooSmall { required, .. })
                if required <= MAX_FRAME_BYTES && required > self.buffer.len() =>
            {
                self.rgba_len = required;
                self.buffer = Vec::new();
                match zeroed(required) {
                    Some(buffer) => self.buffer = buffer,
                    None => self.fail(pipe, Failure::Video(VideoError::OutOfMemory)),
                }
                Step::Busy
            }
            Err(error) if error.is_fatal() || matches!(error, VideoError::BufferTooSmall { .. }) => {
                self.fail(pipe, Failure::Video(error));
                Step::Busy
            }
            // One damaged frame: keep what's on screen and carry on.
            Err(error) => {
                self.next = target.saturating_add(1);
                self.requested = false;
                let mut inner = pipe.inner.lock();
                if inner.media == self.media {
                    inner.damaged = Some(error);
                    if self.next >= count {
                        inner.eos = true;
                    }
                }
                Step::Busy
            }
        }
    }
}

fn open_file(
    fs: &'static dyn FileSystem,
    node: NodeId,
) -> Result<(VideoDecoder<Vec<u8>>, Opened), Failure> {
    let size = fs.size(node);
    let decoder = if size > MAX_MEDIA_BYTES {
        let file = Box::new(FileSource::new(fs, node));
        VideoDecoder::from_source(file, DECODER_LIMITS).map_err(Failure::Video)?
    } else {
        let data = read_file(fs, node, size).ok_or(Failure::Unreadable)?;
        // Checked before the index is built, which is all that's needed to refuse it.
        let info = video_info(&data).map_err(Failure::Video)?;
        if !info.can_decode() {
            return Err(Failure::NoDecoder(info));
        }
        if info.rgba_len().map_err(Failure::Video)? > MAX_FRAME_BYTES {
            return Err(Failure::FrameTooLarge);
        }
        VideoDecoder::with_limits(data, DECODER_LIMITS).map_err(Failure::Video)?
    };
    let info = *decoder.info();
    if !info.can_decode() {
        return Err(Failure::NoDecoder(info));
    }
    let rgba_len = info.rgba_len().map_err(Failure::Video)?;
    if rgba_len > MAX_FRAME_BYTES {
        return Err(Failure::FrameTooLarge);
    }
    let opened = Opened {
        info,
        frame_count: decoder.frame_count(),
        duration_ms: decoder.duration_ms(),
        rgba_len,
    };
    Ok((decoder, opened))
}

/// Reads a file where it lies, for one too large to hold.
pub struct FileSource {
    fs: &'static dyn FileSystem,
    node: NodeId,
    len: u64,
}

impl FileSource {
    /// How much a read asks for at once. Filesystem locks are held for a read, so
    /// this also bounds what anything else on the disk waits for.
    const MAX_READ: usize = 256 * 1024;

    /// The file at `node`.
    pub fn new(fs: &'static dyn FileSystem, node: NodeId) -> Self {
        Self {
            fs,
            node,
            len: fs.size(node) as u64,
        }
    }
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        let Ok(offset) = usize::try_from(offset) else {
            return 0;
        };
        let count = out.len().min(Self::MAX_READ);
        self.fs.read(self.node, offset, &mut out[..count])
    }
}

/// Reads a whole file, or returns `None` if it's too large or memory is short.
pub fn read_file(fs: &dyn FileSystem, node: NodeId, size: usize) -> Option<Vec<u8>> {
    if size == 0 || size > MAX_MEDIA_BYTES {
        return None;
    }
    let mut data = zeroed(size)?;
    let mut offset = 0usize;
    while offset < data.len() {
        let read = fs.read(node, offset, &mut data[offset..]);
        if read == 0 {
            break;
        }
        offset += read;
    }
    data.truncate(offset);
    Some(data)
}

/// A zeroed buffer, or `None` when memory is short.
pub fn zeroed(len: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).ok()?;
    bytes.resize(len, 0);
    Some(bytes)
}

/// The pipe of the player window. There's one window, so there's one pipe.
pub static PIPE: Pipe = Pipe::new();

/// The task itself. Its argument is the identity from [`Pipe::claim`].
pub struct DecodeTask;

impl App for DecodeTask {
    fn name(&self) -> &'static str {
        "video-decoder"
    }

    fn usage(&self) -> &'static str {
        "(started by the media player)"
    }

    fn description(&self) -> &'static str {
        "decode video for the media player"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(fs) = context.platform.filesystem else {
            return 1;
        };
        let Ok(identity) = context.args.trim().parse() else {
            return 2;
        };
        let processes = context.platform.processes;
        let mut worker = Worker::new(identity);
        while !processes.kill_requested() {
            match worker.step(&PIPE, fs) {
                Step::Busy => processes.yield_now(),
                Step::Idle => processes.sleep_ms(IDLE_SLEEP_MS),
                Step::Exit => break,
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use std::boxed::Box;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use feos_fs::EntryKind;

    const GIF: &[u8] = include_bytes!("../../../libs/video/tests/data/anim.gif");

    /// A root directory holding one file.
    struct OneFile(&'static [u8]);

    impl FileSystem for OneFile {
        fn node_count(&self) -> usize {
            2
        }
        fn kind(&self, node: NodeId) -> Option<EntryKind> {
            Some(if node.0 == 0 { EntryKind::Directory } else { EntryKind::File })
        }
        fn name(&self, node: NodeId) -> Option<&str> {
            Some(if node.0 == 0 { "" } else { "anim.gif" })
        }
        fn parent(&self, _node: NodeId) -> Option<NodeId> {
            Some(NodeId::ROOT)
        }
        fn size(&self, node: NodeId) -> usize {
            if node.0 == 0 { 0 } else { self.0.len() }
        }
        fn read(&self, _node: NodeId, offset: usize, out: &mut [u8]) -> usize {
            let rest = self.0.get(offset..).unwrap_or(&[]);
            let count = rest.len().min(out.len());
            out[..count].copy_from_slice(&rest[..count]);
            count
        }
        fn child_count(&self, node: NodeId) -> usize {
            usize::from(node.0 == 0)
        }
        fn child_at(&self, _node: NodeId, _index: usize) -> Option<NodeId> {
            Some(NodeId(1))
        }
    }

    static FILE: OneFile = OneFile(GIF);
    const NODE: NodeId = NodeId(1);

    fn pipe() -> &'static Pipe {
        Box::leak(Box::new(Pipe::new()))
    }

    fn settle(worker: &mut Worker, pipe: &Pipe) {
        while worker.step(pipe, &FILE) == Step::Busy {}
    }

    #[test]
    fn a_paused_file_decodes_only_its_first_frame() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, false);
        settle(&mut worker, pipe);
        let report = pipe.report();
        assert!(report.opened.is_some());
        assert_eq!(report.queued, 1);
        let mut display = Vec::new();
        let frame = pipe.take_requested(&mut display).unwrap();
        assert_eq!(frame.index, 0);
        assert!(!display.is_empty());
    }

    #[test]
    fn playing_fills_the_queue_and_then_waits() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        let report = pipe.report();
        assert!(report.queued >= 2 && report.queued <= MAX_QUEUE);
        assert_eq!(worker.step(pipe, &FILE), Step::Idle);
    }

    #[test]
    fn frames_recycle_their_buffers() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        let mut display = Vec::new();
        let first = pipe.take_due(0, &mut display).unwrap();
        settle(&mut worker, pipe);
        let second = pipe.take_due(u64::MAX, &mut display).unwrap();
        assert!(second.index > first.index);
        // The queue drained, so it decodes more into the same buffers.
        settle(&mut worker, pipe);
        assert!(pipe.queued() > 0 || pipe.report().eos);
    }

    #[test]
    fn a_frame_is_not_handed_out_before_its_time() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        let mut display = Vec::new();
        assert!(pipe.take_due(0, &mut display).is_some());
        assert!(pipe.take_due(0, &mut display).is_none());
    }

    #[test]
    fn a_seek_discards_queued_frames_and_lands_on_its_target() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        pipe.seek(Seek::Frame(2));
        settle(&mut worker, pipe);
        let mut display = Vec::new();
        assert_eq!(pipe.take_requested(&mut display).unwrap().index, 2);
    }

    #[test]
    fn a_stale_worker_leaves_when_someone_else_claims_the_pipe() {
        let pipe = pipe();
        let mut old = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut old, pipe);
        let mut new = Worker::new(pipe.claim());
        assert_eq!(old.step(pipe, &FILE), Step::Exit);
        assert_eq!(pipe.queued(), 0, "claiming drops the old frames");
        pipe.open(NODE, true);
        settle(&mut new, pipe);
        assert!(pipe.queued() > 0);
    }

    #[test]
    fn closing_a_file_frees_it_at_the_next_step() {
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        pipe.close_media();
        settle(&mut worker, pipe);
        assert!(worker.decoder.is_none());
        assert_eq!(pipe.queued(), 0);
    }

    #[test]
    fn a_file_that_is_not_video_fails_without_stopping_the_worker() {
        static BROKEN: OneFile = OneFile(b"not a video at all");
        let pipe = pipe();
        let mut worker = Worker::new(pipe.claim());
        pipe.open(NODE, true);
        while worker.step(pipe, &BROKEN) == Step::Busy {}
        assert!(pipe.report().failure.is_some());
        pipe.open(NODE, true);
        settle(&mut worker, pipe);
        assert!(pipe.report().opened.is_some());
    }

    /// Runs a real worker on another thread, as the kernel would, and checks
    /// that the window never waits long and that shutting down always ends it.
    #[test]
    fn a_threaded_worker_stops_on_request_and_leaves_the_lock_free() {
        let pipe = pipe();
        let identity = pipe.claim();
        let killed = Arc::new(AtomicBool::new(false));
        let flag = killed.clone();
        let thread = std::thread::spawn(move || {
            let mut worker = Worker::new(identity);
            while !flag.load(Ordering::Relaxed) {
                match worker.step(pipe, &FILE) {
                    Step::Busy => std::thread::yield_now(),
                    Step::Idle => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Step::Exit => break,
                }
            }
        });
        pipe.open(NODE, true);
        let mut display = Vec::new();
        let mut shown = 0;
        for clock in 0..2000u64 {
            if pipe.take_due(clock, &mut display).is_some() {
                shown += 1;
            }
            if shown >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(shown >= 1);
        // A kill takes it down whatever it's doing, and nothing stays locked.
        killed.store(true, Ordering::Relaxed);
        thread.join().unwrap();
        assert!(pipe.inner.try_lock().is_some());
        // Nor does a window closing need anyone's help.
        pipe.shut_down();
        assert!(pipe.inner.try_lock().is_some());
    }
}
