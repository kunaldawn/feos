//! Services the running desktop session offers to apps.
//!
//! Apps have no handle on the desktop: dependencies point one way, and a
//! windowed app only sees its [`WindowContext`](crate::WindowContext). This
//! module is the shared store between them. An app posts a request here, and
//! the session picks it up between frames. Everything is fixed capacity, and
//! every call is safe to make when no session is running: requests simply wait
//! in the queue, so check [`session_active`] before relying on one.

use core::sync::atomic::{AtomicBool, Ordering};

use feos_sync::SpinLock;
use feos_text::Text;

/// Capacity of the shared clipboard, in bytes.
pub const CLIPBOARD_MAX: usize = 4096;

/// Longest command line or path a request can carry.
pub const REQUEST_MAX: usize = 96;

/// Number of requests that can wait for the session.
const QUEUE: usize = 8;

/// How serious a notification is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Level {
    /// Something finished, or a status worth a glance.
    #[default]
    Info,
    /// An operation failed.
    Error,
}

/// One notification waiting to be shown.
#[derive(Clone, Copy, Debug)]
pub struct Notification {
    /// Short heading.
    pub title: Text<32>,
    /// One line of detail.
    pub body: Text<REQUEST_MAX>,
    /// Severity.
    pub level: Level,
}

impl Notification {
    const EMPTY: Self = Self {
        title: Text::EMPTY,
        body: Text::EMPTY,
        level: Level::Info,
    };
}

/// Something an app asked the desktop session to do.
#[derive(Clone, Copy, Debug)]
pub enum Request {
    /// Run a command line as the Run dialog would: a bundle id or shell
    /// command, with optional arguments. Windowed apps are preferred.
    Run(Text<REQUEST_MAX>),
    /// Open a bundle by id with arguments in place of its manifest's.
    Open {
        /// Bundle id.
        bundle: Text<32>,
        /// Arguments for this launch.
        args: Text<REQUEST_MAX>,
    },
    /// Start dragging a path; the desktop draws it and resolves the drop.
    BeginDrag(Text<REQUEST_MAX>),
    /// Bring this window forward, restoring it if it is minimized.
    Activate(u16),
    /// Close this window, as its title bar's button would.
    CloseWindow(u16),
}

/// Most windows [`publish_windows`] keeps.
pub const MAX_WINDOWS: usize = 16;

/// Hosted windows appear in process lists under `HOSTED_PID_BASE + window`. They
/// have no process of their own: the desktop draws them on its thread.
pub const HOSTED_PID_BASE: u32 = 1_000_000;

/// One open window, as the session last published it.
#[derive(Clone, Copy, Debug)]
pub struct WindowInfo {
    /// The window's number in [`Request::Activate`] and [`Request::CloseWindow`].
    pub window: u16,
    /// The window's process, when it has one.
    pub pid: Option<u32>,
    /// The desktop draws this window from an app on its own thread.
    pub hosted: bool,
    /// The title bar's text.
    pub title: Text<32>,
    /// Time the desktop has spent on this window's frames, in microseconds.
    pub cpu_us: u64,
    /// The window is minimized.
    pub minimized: bool,
    /// The window has the keyboard.
    pub focused: bool,
}

impl WindowInfo {
    /// The ID a process list shows for this window: its process, or a stand-in.
    pub fn list_pid(&self) -> u32 {
        self.pid
            .filter(|_| !self.hosted)
            .unwrap_or(HOSTED_PID_BASE + u32::from(self.window))
    }

    /// An entry for initializing a list.
    pub const fn empty() -> Self {
        Self {
            window: 0,
            pid: None,
            hosted: false,
            title: Text::EMPTY,
            cpu_us: 0,
            minimized: false,
            focused: false,
        }
    }
}

struct WindowList {
    items: [WindowInfo; MAX_WINDOWS],
    len: usize,
}

static WINDOWS: SpinLock<WindowList> = SpinLock::new(WindowList {
    items: [WindowInfo::empty(); MAX_WINDOWS],
    len: 0,
});

/// Replaces the list of open windows. Anything past [`MAX_WINDOWS`] is ignored.
pub fn publish_windows(windows: &[WindowInfo]) {
    let mut list = WINDOWS.lock();
    let len = windows.len().min(MAX_WINDOWS);
    list.items[..len].copy_from_slice(&windows[..len]);
    list.len = len;
}

/// Copies the open windows into `out` and returns how many there are.
pub fn windows(out: &mut [WindowInfo]) -> usize {
    let list = WINDOWS.lock();
    let len = list.len.min(out.len());
    out[..len].copy_from_slice(&list.items[..len]);
    len
}

/// Asks the session to bring a window forward.
pub fn request_activate(window: u16) -> bool {
    REQUESTS.lock().push(Request::Activate(window))
}

/// Asks the session to close a window.
pub fn request_close(window: u16) -> bool {
    REQUESTS.lock().push(Request::CloseWindow(window))
}

struct Queue<T: Copy, const N: usize> {
    items: [T; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Queue<T, N> {
    const fn new(empty: T) -> Self {
        Self {
            items: [empty; N],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, item: T) -> bool {
        if self.len == N {
            return false;
        }
        self.items[(self.head + self.len) % N] = item;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let item = self.items[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(item)
    }
}

struct Clipboard {
    bytes: [u8; CLIPBOARD_MAX],
    len: usize,
}

static CLIPBOARD: SpinLock<Clipboard> = SpinLock::new(Clipboard {
    bytes: [0; CLIPBOARD_MAX],
    len: 0,
});
static REQUESTS: SpinLock<Queue<Request, QUEUE>> =
    SpinLock::new(Queue::new(Request::Run(Text::EMPTY)));
static NOTIFICATIONS: SpinLock<Queue<Notification, QUEUE>> =
    SpinLock::new(Queue::new(Notification::EMPTY));
static SESSION: AtomicBool = AtomicBool::new(false);

/// Records whether a desktop session is draining the queues.
pub fn set_session_active(active: bool) {
    SESSION.store(active, Ordering::Release);
}

/// Returns `true` while a desktop session will act on requests.
pub fn session_active() -> bool {
    SESSION.load(Ordering::Acquire)
}

/// Replaces the clipboard text. Returns `false` when `text` had to be cut
/// short to fit; the cut lands on a character boundary.
pub fn clipboard_set(text: &str) -> bool {
    let mut end = text.len().min(CLIPBOARD_MAX);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipboard = CLIPBOARD.lock();
    clipboard.bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
    clipboard.len = end;
    end == text.len()
}

/// Copies the clipboard text into `out` and returns the part that fit.
///
/// The result is cut on a character boundary, so it is always valid UTF-8.
pub fn clipboard_get(out: &mut [u8]) -> &str {
    let clipboard = CLIPBOARD.lock();
    let mut end = clipboard.len.min(out.len());
    while end > 0
        && end < clipboard.len
        && (clipboard.bytes[end] & 0xc0) == 0x80
    {
        end -= 1;
    }
    out[..end].copy_from_slice(&clipboard.bytes[..end]);
    drop(clipboard);
    core::str::from_utf8(&out[..end]).unwrap_or("")
}

/// Number of bytes on the clipboard.
pub fn clipboard_len() -> usize {
    CLIPBOARD.lock().len
}

/// Asks the session to run `command`. Returns `false` when it did not fit.
pub fn request_run(command: &str) -> bool {
    if command.len() > REQUEST_MAX {
        return false;
    }
    REQUESTS.lock().push(Request::Run(Text::from_str(command)))
}

/// Asks the session to open bundle `bundle` with `args`.
pub fn request_open(bundle: &str, args: &str) -> bool {
    if args.len() > REQUEST_MAX {
        return false;
    }
    REQUESTS.lock().push(Request::Open {
        bundle: Text::from_str(bundle),
        args: Text::from_str(args),
    })
}

/// Asks the session to open `path` with the app associated with its extension.
/// Returns `false` when no app is associated, or the path does not fit.
pub fn request_open_file(path: &str) -> bool {
    match association_for(path) {
        Some(bundle) => request_open(bundle, path),
        None => false,
    }
}

/// Starts dragging `payload`, normally a file path.
pub fn begin_drag(payload: &str) -> bool {
    if payload.len() > REQUEST_MAX {
        return false;
    }
    REQUESTS
        .lock()
        .push(Request::BeginDrag(Text::from_str(payload)))
}

/// The next waiting request. The desktop session calls this between frames.
pub fn take_request() -> Option<Request> {
    REQUESTS.lock().pop()
}

/// Shows a notification, or returns `false` when the queue is full.
pub fn notify(title: &str, body: &str, level: Level) -> bool {
    NOTIFICATIONS.lock().push(Notification {
        title: Text::from_str(title),
        body: Text::from_str(body),
        level,
    })
}

/// The next waiting notification. The desktop session calls this between frames.
pub fn take_notification() -> Option<Notification> {
    NOTIFICATIONS.lock().pop()
}

/// File extensions the shipped bundles open, as `(extension, bundle id)`.
const ASSOCIATIONS: &[(&str, &str)] = &[
    ("mp4", "media-player"),
    ("mpg", "media-player"),
    ("mpeg", "media-player"),
    ("m1v", "media-player"),
    ("m4v", "media-player"),
    ("mov", "media-player"),
    ("mkv", "media-player"),
    ("webm", "media-player"),
    ("avi", "media-player"),
    ("flv", "media-player"),
    ("ivf", "media-player"),
    ("h264", "media-player"),
    ("264", "media-player"),
    ("gif", "media-player"),
    ("png", "image-viewer"),
    ("jpg", "image-viewer"),
    ("jpeg", "image-viewer"),
    ("bmp", "image-viewer"),
    ("ppm", "image-viewer"),
    ("pgm", "image-viewer"),
    ("pbm", "image-viewer"),
    ("pnm", "image-viewer"),
    ("qoi", "image-viewer"),
    ("ico", "image-viewer"),
    ("mp3", "music"),
    ("mp2", "music"),
    ("mp1", "music"),
    ("wav", "music"),
    ("wave", "music"),
    ("flac", "music"),
    ("ogg", "music"),
    ("oga", "music"),
    ("aac", "music"),
    ("m4a", "music"),
    ("aif", "music"),
    ("aiff", "music"),
    ("aifc", "music"),
    ("au", "music"),
    ("snd", "music"),
];

/// The bundle that opens `path`, judged by its extension.
pub fn association_for(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    ASSOCIATIONS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(extension))
        .map(|(_, bundle)| *bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associations_follow_the_extension_case_insensitively() {
        assert_eq!(association_for("/media/a.MP4"), Some("media-player"));
        assert_eq!(association_for("clip.mkv"), Some("media-player"));
        assert_eq!(association_for("/etc/motd"), None);
        assert_eq!(association_for("/dir.mp4/readme"), None);
        assert_eq!(association_for("/home/.mp4"), None);
    }

    #[test]
    fn queues_are_first_in_first_out_and_bounded() {
        let mut queue = Queue::<u32, 3>::new(0);
        assert!(queue.push(1));
        assert!(queue.push(2));
        assert!(queue.push(3));
        assert!(!queue.push(4));
        assert_eq!(queue.pop(), Some(1));
        assert!(queue.push(5));
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), Some(5));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn clipboard_round_trips_and_cuts_on_character_boundaries() {
        // One test owns the shared clipboard, so tests cannot race on it.
        assert!(clipboard_set("h\u{e9}llo"));
        assert_eq!(clipboard_len(), 6);
        let mut out = [0u8; 16];
        assert_eq!(clipboard_get(&mut out), "h\u{e9}llo");
        let mut short = [0u8; 2];
        assert_eq!(clipboard_get(&mut short), "h");

        let long = std::string::String::from("\u{20ac}").repeat(CLIPBOARD_MAX);
        assert!(!clipboard_set(&long));
        assert_eq!(clipboard_len() % 3, 0);
        assert!(clipboard_len() <= CLIPBOARD_MAX);
    }
}
