//! A terminal's line discipline (stage 1c.11 of `docs/feos-abi-v2.md`): what typed bytes
//! do before a program reads them, and what a program's writes become on the way out.
//!
//! [`Tty`] holds what's been typed and not read, in one fixed ring of [`INPUT_MAX`] bytes
//! (Linux's `N_TTY_BUF_SIZE`), and the [`Termios`] that says how it's treated. Each byte
//! goes in through [`Tty::receive`], which edits the line being typed (canonical mode),
//! says what to echo, and names a signal for `VINTR`, `VQUIT` and `VSUSP` under `ISIG`;
//! the caller does the echoing and raises the signal, since this crate reaches neither
//! a pipe nor a process. Reads take whole lines in canonical mode ([`Tty::read`]), and
//! what's there otherwise, as [`noncanonical`] decides from `VMIN` and `VTIME`.
//!
//! [`background_read`] is POSIX's rule for a read by a process outside the foreground
//! group, and [`output`] is `OPOST | ONLCR`.
//!
//! Nothing allocates: the kernel keeps one of these per terminal in a static table.

#![cfg_attr(not(test), no_std)]

use feos_abi::{tc, Termios};

/// Bytes a terminal holds typed and not yet read, line being edited included.
pub const INPUT_MAX: usize = 4096;

const WORDS: usize = INPUT_MAX / 64;

/// The signal a typed character raises on the foreground group, under `ISIG`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    /// `VINTR`: `SIGINT`.
    Interrupt,
    /// `VQUIT`: `SIGQUIT`.
    Quit,
    /// `VSUSP`: `SIGTSTP`.
    Suspend,
}

impl Signal {
    /// Its number.
    pub const fn number(self) -> u32 {
        match self {
            Self::Interrupt => feos_abi::sig::INT,
            Self::Quit => feos_abi::sig::QUIT,
            Self::Suspend => feos_abi::sig::TSTP,
        }
    }
}

/// What one typed byte did: what to echo, and the signal to raise, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Received {
    echo: [u8; 8],
    echo_length: usize,
    /// Characters to rub out on the screen first (`"\x08 \x08"` each): what `VKILL`
    /// took back under `ECHOE`.
    pub rub_out: usize,
    pub signal: Option<Signal>,
}

impl Received {
    /// The bytes to write to the terminal for it.
    pub fn echo(&self) -> &[u8] {
        &self.echo[..self.echo_length]
    }

    fn say(&mut self, bytes: &[u8]) {
        let room = self.echo.len() - self.echo_length;
        let count = bytes.len().min(room);
        self.echo[self.echo_length..self.echo_length + count].copy_from_slice(&bytes[..count]);
        self.echo_length += count;
    }
}

/// A terminal's input and settings.
pub struct Tty {
    termios: Termios,
    bytes: [u8; INPUT_MAX],
    /// A `VEOF` in canonical mode: ends its line, and is never read itself.
    eof: [u64; WORDS],
    /// The last byte of a canonical line: a newline, `VEOL` or `VEOL2`.
    end: [u64; WORDS],
    head: usize,
    /// Bytes held, from `head`.
    length: usize,
    /// Of those, the ones a read may take: whole lines in canonical mode, all of them
    /// otherwise. The rest is the line being typed.
    ready: usize,
    /// When the last byte came, in the caller's nanoseconds, for `VTIME`.
    last_input: u64,
}

impl Default for Tty {
    fn default() -> Self {
        Self::new()
    }
}

impl Tty {
    /// A terminal with Linux's settings and nothing typed.
    pub fn new() -> Self {
        Self {
            termios: Termios::default(),
            bytes: [0; INPUT_MAX],
            eof: [0; WORDS],
            end: [0; WORDS],
            head: 0,
            length: 0,
            ready: 0,
            last_input: 0,
        }
    }

    pub const fn termios(&self) -> &Termios {
        &self.termios
    }

    fn canonical(&self) -> bool {
        self.termios.lflag & tc::ICANON != 0
    }

    /// New settings. Leaving canonical mode makes the line being typed readable as it
    /// is, as Linux does; entering it leaves what was readable readable.
    pub fn set_termios(&mut self, termios: Termios) {
        self.termios = termios;
        if !self.canonical() {
            self.ready = self.length;
        }
    }

    fn at(&self, index: usize) -> usize {
        (self.head + index) % INPUT_MAX
    }

    fn flag(map: &[u64; WORDS], at: usize) -> bool {
        map[at / 64] >> (at % 64) & 1 != 0
    }

    fn set_flag(map: &mut [u64; WORDS], at: usize, on: bool) {
        if on {
            map[at / 64] |= 1 << (at % 64);
        } else {
            map[at / 64] &= !(1 << (at % 64));
        }
    }

    /// Appends a byte to what's held, flagged as a line's `end` or a `VEOF`. `false` if
    /// there's no room.
    fn push(&mut self, byte: u8, end: bool, eof: bool) -> bool {
        if self.length == INPUT_MAX {
            return false;
        }
        let at = self.at(self.length);
        self.bytes[at] = byte;
        Self::set_flag(&mut self.end, at, end);
        Self::set_flag(&mut self.eof, at, eof);
        self.length += 1;
        true
    }

    /// Takes the last byte of the line being typed off, if it has one.
    fn pop(&mut self) -> Option<u8> {
        if self.length == self.ready {
            return None;
        }
        self.length -= 1;
        Some(self.bytes[self.at(self.length)])
    }

    /// Whether `byte` is one `ISIG` makes a signal of, now.
    pub fn signal_of(&self, byte: u8) -> Option<Signal> {
        if self.termios.lflag & tc::ISIG == 0 || byte == 0 {
            return None;
        }
        let cc = &self.termios.cc;
        if byte == cc[tc::VINTR] {
            Some(Signal::Interrupt)
        } else if byte == cc[tc::VQUIT] {
            Some(Signal::Quit)
        } else if byte == cc[tc::VSUSP] {
            Some(Signal::Suspend)
        } else {
            None
        }
    }

    /// One typed byte, at `now` (any monotonic nanoseconds; only `VTIME` looks).
    pub fn receive(&mut self, byte: u8, now: u64) -> Received {
        let mut out = Received::default();
        let echo = self.termios.lflag & tc::ECHO != 0;
        if let Some(signal) = self.signal_of(byte) {
            // What was typed goes, unless `NOFLSH`, as it does on Linux.
            if self.termios.lflag & tc::NOFLSH == 0 {
                self.flush_input();
            }
            if echo {
                out.say(&[b'^', byte ^ 0x40, b'\n']);
            }
            out.signal = Some(signal);
            return out;
        }
        let byte = if byte == b'\r' && self.termios.iflag & tc::ICRNL != 0 { b'\n' } else { byte };
        self.last_input = now;
        if !self.canonical() {
            if self.push(byte, false, false) {
                self.ready = self.length;
                if echo {
                    self.echo_char(&mut out, byte);
                }
            }
            return out;
        }
        let cc = self.termios.cc;
        let erase_echo = echo && self.termios.lflag & tc::ECHOE != 0;
        if byte == cc[tc::VERASE] || byte == 0x08 {
            // A whole UTF-8 sequence at a time.
            let mut removed = false;
            while let Some(last) = self.pop() {
                removed = true;
                if last & 0xc0 != 0x80 {
                    break;
                }
            }
            if removed && erase_echo {
                out.say(b"\x08 \x08");
            } else if removed && echo {
                self.echo_char(&mut out, byte);
            }
            return out;
        }
        if byte == cc[tc::VKILL] && byte != 0 {
            let mut characters = 0usize;
            while let Some(last) = self.pop() {
                characters += usize::from(last & 0xc0 != 0x80);
            }
            if erase_echo {
                out.rub_out = characters;
            }
            return out;
        }
        if byte == cc[tc::VEOF] && byte != 0 {
            // Ends the line as it is, and is never read: an empty line is end of file.
            if self.push(byte, false, true) {
                self.ready = self.length;
            }
            return out;
        }
        let ends = byte == b'\n' || (byte != 0 && (byte == cc[tc::VEOL] || byte == cc[tc::VEOL2]));
        // A line is kept short of the whole ring, so its end always fits.
        if !ends && self.length + 1 >= INPUT_MAX {
            return out;
        }
        if self.push(byte, ends, false) {
            if ends {
                self.ready = self.length;
            }
            if echo || (byte == b'\n' && self.termios.lflag & tc::ECHONL != 0) {
                self.echo_char(&mut out, byte);
            }
        }
        out
    }

    /// Echoes `byte`: a control character as `^X` under `ECHOCTL`, the rest as it is.
    fn echo_char(&self, out: &mut Received, byte: u8) {
        let control = (byte < 0x20 && byte != b'\n' && byte != b'\t') || byte == 0x7f;
        if control && self.termios.lflag & tc::ECHOCTL != 0 {
            out.say(&[b'^', byte ^ 0x40]);
        } else {
            out.say(&[byte]);
        }
    }

    /// Whether a read would return at once: a whole line (or an end of file) in
    /// canonical mode, any byte otherwise.
    pub fn readable(&self) -> bool {
        self.ready > 0
    }

    /// Whether a reader has what it waits for: a whole line in canonical mode, `VMIN`
    /// bytes (at least one) otherwise. Past that, typed bytes can wait to be offered
    /// again, so what a program never asked for stays its shell's.
    pub fn satisfied(&self) -> bool {
        if self.canonical() {
            self.ready > 0
        } else {
            self.available() >= usize::from(self.termios.cc[tc::VMIN]).max(1)
        }
    }

    /// Takes back everything held and not read, the line being typed too, as it stands
    /// (erased bytes gone, carriage returns made newlines): what a terminal that's going
    /// leaves for its shell. As much as fits in `out`; the rest is dropped.
    pub fn take_unread(&mut self, out: &mut [u8]) -> usize {
        let mut count = 0;
        for index in 0..self.length {
            let at = self.at(index);
            if Self::flag(&self.eof, at) || count == out.len() {
                continue;
            }
            out[count] = self.bytes[at];
            count += 1;
        }
        self.flush_input();
        count
    }

    /// Bytes a read could return now: `FD_NREAD`. In canonical mode, every whole line's.
    pub fn available(&self) -> usize {
        (0..self.ready).filter(|&index| !Self::flag(&self.eof, self.at(index))).count()
    }

    /// When the last byte was typed.
    pub const fn last_input(&self) -> u64 {
        self.last_input
    }

    fn take_front(&mut self) {
        self.head = (self.head + 1) % INPUT_MAX;
        self.length -= 1;
        self.ready -= 1;
    }

    /// Takes what a read gets now into `out`: in canonical mode at most one line, `Some(0)`
    /// for an end of file; otherwise what's there, at most `out`'s length. `None` when
    /// there's nothing to take (in canonical mode, no whole line).
    pub fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.ready == 0 || out.is_empty() {
            return if self.ready == 0 { None } else { Some(0) };
        }
        let canonical = self.canonical();
        let mut count = 0;
        while self.ready > 0 && count < out.len() {
            let at = self.at(0);
            let (byte, eof, end) = (self.bytes[at], Self::flag(&self.eof, at), Self::flag(&self.end, at));
            self.take_front();
            if eof {
                // Ends the line, or is the end of file itself if nothing came before it.
                if canonical {
                    break;
                }
                continue;
            }
            out[count] = byte;
            count += 1;
            if canonical && end {
                break;
            }
        }
        Some(count)
    }

    /// Drops everything typed and not read: `TCIFLUSH`, and an `ISIG` character's.
    pub fn flush_input(&mut self) {
        self.head = 0;
        self.length = 0;
        self.ready = 0;
    }
}

/// What a non-canonical read does now, from `VMIN` and `VTIME`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// Returns, taking this many bytes (0 is a read of nothing, not end of file).
    Now(usize),
    /// Waits for input, until this deadline if there is one.
    Until(Option<u64>),
}

/// POSIX's four cases of `VMIN` (`min`) and `VTIME` (`time`, tenths of a second), for a
/// read of `wanted` bytes with `available` there, at `now`, the read having begun at
/// `started` and the last byte having come at `last_input` (all in nanoseconds):
///
/// - `MIN` 0, `TIME` 0: what's there, at once;
/// - `MIN` > 0, `TIME` 0: waits for `MIN` bytes (or `wanted`, if fewer);
/// - `MIN` 0, `TIME` > 0: waits for a byte, for `TIME` from the read's start;
/// - `MIN` > 0, `TIME` > 0: waits for a first byte, then for `MIN`, or for `TIME` with no
///   byte coming, counted from the last.
pub fn noncanonical(min: u8, time: u8, available: usize, wanted: usize, now: u64, started: u64, last_input: u64) -> Wait {
    let period = u64::from(time) * 100_000_000;
    let enough = (min as usize).min(wanted).max(1);
    let take = available.min(wanted);
    match (min, time) {
        (0, 0) => Wait::Now(take),
        (_, 0) if available >= enough => Wait::Now(take),
        (_, 0) => Wait::Until(None),
        (0, _) if available > 0 => Wait::Now(take),
        (0, _) => {
            let deadline = started.saturating_add(period);
            if now >= deadline {
                Wait::Now(0)
            } else {
                Wait::Until(Some(deadline))
            }
        }
        _ if available >= enough => Wait::Now(take),
        _ if available == 0 => Wait::Until(None),
        _ => {
            let deadline = last_input.saturating_add(period);
            if now >= deadline {
                Wait::Now(take)
            } else {
                Wait::Until(Some(deadline))
            }
        }
    }
}

/// What a read of the terminal by a process that may not be in its foreground group
/// does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Background {
    /// Reads, as any reader in the foreground does.
    Read,
    /// Raises `SIGTTIN` on the reader's group, and fails with `EINTR`.
    Signal,
    /// Fails with `EIO`: the reader ignores or blocks `SIGTTIN`, or its group is
    /// orphaned, so a stop would never be undone.
    Fail,
}

/// POSIX's rule for a read of a terminal of session `session`, with `foreground` its
/// foreground group (0 for none), by a process in group `pgid` of session `sid`. A
/// reader in another session isn't under the terminal's job control at all.
pub fn background_read(pgid: u32, sid: u32, foreground: u32, session: u32, ignores_ttin: bool, orphaned: bool) -> Background {
    if sid != session || foreground == 0 || pgid == foreground {
        Background::Read
    } else if ignores_ttin || orphaned {
        Background::Fail
    } else {
        Background::Signal
    }
}

/// A program's write on its way to the terminal, under `oflag`: with `OPOST | ONLCR`
/// each newline goes out as a carriage return and a newline. Translates as much of
/// `bytes` as fits in `out`, and says how much of each it took and filled.
pub fn output(oflag: u32, bytes: &[u8], out: &mut [u8]) -> (usize, usize) {
    let crlf = oflag & tc::OPOST != 0 && oflag & tc::ONLCR != 0;
    let (mut taken, mut filled) = (0, 0);
    for &byte in bytes {
        let need = if crlf && byte == b'\n' { 2 } else { 1 };
        if filled + need > out.len() {
            break;
        }
        if need == 2 {
            out[filled] = b'\r';
            filled += 1;
        }
        out[filled] = byte;
        filled += 1;
        taken += 1;
    }
    (taken, filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(tty: &mut Tty, bytes: &[u8]) -> std::vec::Vec<u8> {
        let mut echoed = std::vec::Vec::new();
        for &byte in bytes {
            echoed.extend_from_slice(tty.receive(byte, 0).echo());
        }
        echoed
    }

    fn read_all(tty: &mut Tty) -> std::vec::Vec<std::vec::Vec<u8>> {
        let mut reads = std::vec::Vec::new();
        let mut buffer = [0u8; 64];
        while let Some(count) = tty.read(&mut buffer) {
            reads.push(buffer[..count].to_vec());
        }
        reads
    }

    #[test]
    fn canonical_lines() {
        let mut tty = Tty::new();
        let echoed = typed(&mut tty, b"ab");
        assert_eq!(echoed, b"ab");
        // Not a line yet: nothing to read.
        assert!(!tty.readable());
        assert_eq!(tty.read(&mut [0u8; 8]), None);
        // A carriage return is a newline (`ICRNL`), and each read is one line.
        typed(&mut tty, b"c\rsecond\n");
        assert!(tty.readable() && tty.satisfied());
        assert_eq!(tty.available(), 11);
        assert_eq!(read_all(&mut tty), [b"abc\n".to_vec(), b"second\n".to_vec()]);
        // `VEOF` on an empty line is end of file; after text, it ends the line unread.
        typed(&mut tty, b"\x04part\x04");
        assert_eq!(read_all(&mut tty), [b"".to_vec(), b"part".to_vec()]);
        // A short buffer takes a line in pieces.
        typed(&mut tty, b"long line\n");
        let mut small = [0u8; 4];
        assert_eq!(tty.read(&mut small), Some(4));
        assert_eq!(&small, b"long");
        assert_eq!(read_all(&mut tty), [b" line\n".to_vec()]);
        // Without `ECHO`, nothing comes back.
        let mut quiet = Termios::default();
        quiet.lflag &= !tc::ECHO;
        tty.set_termios(quiet);
        assert_eq!(typed(&mut tty, b"secret\n"), b"");
        assert_eq!(read_all(&mut tty), [b"secret\n".to_vec()]);
    }

    #[test]
    fn erase() {
        let mut tty = Tty::new();
        // Backspace (`VERASE`, 0x7f, or 0x08) takes back a character, a whole UTF-8 one.
        let echoed = typed(&mut tty, "abx\x7fc é\x7f\x08!\n".as_bytes());
        assert_eq!(read_all(&mut tty), [b"abc!\n".to_vec()]);
        assert_eq!(echoed.windows(3).filter(|window| window == b"\x08 \x08").count(), 3);
        // It never reaches into a line already ended.
        typed(&mut tty, b"done\n\x7f\x7f");
        assert_eq!(read_all(&mut tty), [b"done\n".to_vec()]);
        // `VKILL` takes the whole line, rubbed out a character at a time.
        typed(&mut tty, "wrong é".as_bytes());
        assert_eq!(tty.receive(0x15, 0).rub_out, 7);
        typed(&mut tty, b"right\n");
        assert_eq!(read_all(&mut tty), [b"right\n".to_vec()]);
        // Without `ECHOE`, the erase character itself is echoed.
        let mut plain = Termios::default();
        plain.lflag &= !tc::ECHOE;
        tty.set_termios(plain);
        assert_eq!(typed(&mut tty, b"a\x7f"), b"a^?");
        // Not in non-canonical mode: there, it's just a byte.
        let mut raw = Termios::default();
        raw.lflag &= !(tc::ICANON | tc::ECHO);
        tty.set_termios(raw);
        tty.flush_input();
        typed(&mut tty, b"a\x7f");
        assert_eq!(read_all(&mut tty), [b"a\x7f".to_vec()]);
    }

    #[test]
    fn isig_raises() {
        let mut tty = Tty::new();
        typed(&mut tty, b"half a li");
        let got = tty.receive(0x03, 0);
        assert_eq!(got.signal, Some(Signal::Interrupt));
        assert_eq!(got.echo(), b"^C\n");
        // What was typed went with it.
        typed(&mut tty, b"\n");
        assert_eq!(read_all(&mut tty), [b"\n".to_vec()]);
        assert_eq!(tty.receive(0x1c, 0).signal, Some(Signal::Quit));
        assert_eq!(tty.receive(0x1a, 0).signal, Some(Signal::Suspend));
        assert_eq!(Signal::Interrupt.number(), 2);
        assert_eq!((Signal::Quit.number(), Signal::Suspend.number()), (3, 20));
        // With `NOFLSH`, the line stays.
        let mut keep = Termios::default();
        keep.lflag |= tc::NOFLSH;
        tty.set_termios(keep);
        typed(&mut tty, b"kept");
        assert_eq!(tty.receive(0x03, 0).signal, Some(Signal::Interrupt));
        typed(&mut tty, b"\n");
        assert_eq!(read_all(&mut tty), [b"kept\n".to_vec()]);
        // Without `ISIG`, Ctrl-C is a byte like any other, in raw mode too.
        let mut raw = Termios::default();
        raw.lflag &= !(tc::ISIG | tc::ICANON);
        tty.set_termios(raw);
        assert_eq!(tty.receive(0x03, 0).signal, None);
        assert_eq!(tty.signal_of(0x03), None);
        assert_eq!(read_all(&mut tty), [b"\x03".to_vec()]);
        // Another character can be made the interrupt, and a zero one is none.
        let mut moved = Termios::default();
        moved.cc[tc::VINTR] = b'q';
        moved.cc[tc::VQUIT] = 0;
        tty.set_termios(moved);
        assert_eq!(tty.signal_of(b'q'), Some(Signal::Interrupt));
        assert_eq!(tty.signal_of(0), None);
        assert_eq!(tty.signal_of(0x03), None);
    }

    #[test]
    fn vmin_vtime() {
        const TENTH: u64 = 100_000_000;
        // 0, 0: what's there, nothing included.
        assert_eq!(noncanonical(0, 0, 0, 10, 5, 0, 0), Wait::Now(0));
        assert_eq!(noncanonical(0, 0, 3, 2, 5, 0, 0), Wait::Now(2));
        // MIN only: waits for MIN, or the request if it's smaller.
        assert_eq!(noncanonical(4, 0, 3, 10, 5, 0, 0), Wait::Until(None));
        assert_eq!(noncanonical(4, 0, 4, 10, 5, 0, 0), Wait::Now(4));
        assert_eq!(noncanonical(4, 0, 2, 2, 5, 0, 0), Wait::Now(2));
        assert_eq!(noncanonical(1, 0, 1, 10, 5, 0, 0), Wait::Now(1));
        // TIME only: a byte, or the time from the read's start.
        assert_eq!(noncanonical(0, 5, 0, 10, TENTH, 0, 0), Wait::Until(Some(5 * TENTH)));
        assert_eq!(noncanonical(0, 5, 0, 10, 5 * TENTH, 0, 0), Wait::Now(0));
        assert_eq!(noncanonical(0, 5, 1, 10, TENTH, 0, 0), Wait::Now(1));
        // Both: no timer until the first byte, then MIN or TIME since the last byte.
        assert_eq!(noncanonical(3, 2, 0, 10, 9 * TENTH, 0, 0), Wait::Until(None));
        assert_eq!(noncanonical(3, 2, 1, 10, 9 * TENTH, 0, 8 * TENTH), Wait::Until(Some(10 * TENTH)));
        assert_eq!(noncanonical(3, 2, 1, 10, 10 * TENTH, 0, 8 * TENTH), Wait::Now(1));
        assert_eq!(noncanonical(3, 2, 3, 10, 8 * TENTH, 0, 8 * TENTH), Wait::Now(3));
        // The tty keeps when the last byte came, for that.
        let mut tty = Tty::new();
        tty.receive(b'x', 1234);
        assert_eq!(tty.last_input(), 1234);
        // A reader is satisfied with `VMIN` bytes, or one with `VMIN` 0.
        let mut raw = Termios::default();
        raw.lflag &= !tc::ICANON;
        raw.cc[tc::VMIN] = 3;
        tty.set_termios(raw);
        tty.flush_input();
        typed(&mut tty, b"ab");
        assert!(tty.readable() && !tty.satisfied());
        typed(&mut tty, b"c");
        assert!(tty.satisfied());
        raw.cc[tc::VMIN] = 0;
        tty.set_termios(raw);
        tty.flush_input();
        assert!(!tty.satisfied());
        typed(&mut tty, b"d");
        assert!(tty.satisfied());
    }

    #[test]
    fn flush_drops_input() {
        let mut tty = Tty::new();
        typed(&mut tty, b"one\ntwo\nthr");
        assert_eq!(tty.available(), 8);
        tty.flush_input();
        assert!(!tty.readable());
        assert_eq!(tty.available(), 0);
        typed(&mut tty, b"ee\n");
        assert_eq!(read_all(&mut tty), [b"ee\n".to_vec()]);
        // A full ring keeps room for a line's end, so a long line still ends.
        let long = std::vec![b'a'; INPUT_MAX + 100];
        typed(&mut tty, &long);
        typed(&mut tty, b"\n");
        assert!(tty.readable());
        assert_eq!(tty.available(), INPUT_MAX);
        tty.flush_input();
        // What's left goes back whole, a partial line too, end-of-file markers dropped.
        typed(&mut tty, b"read\nleft\x04over\rpart");
        assert_eq!(tty.read(&mut [0u8; 16]), Some(5));
        let mut back = [0u8; 32];
        let count = tty.take_unread(&mut back);
        assert_eq!(&back[..count], b"leftover\npart");
        assert!(!tty.readable());
        // Leaving canonical mode makes the line being typed readable.
        typed(&mut tty, b"typed");
        let mut raw = Termios::default();
        raw.lflag &= !tc::ICANON;
        tty.set_termios(raw);
        assert_eq!(read_all(&mut tty), [b"typed".to_vec()]);
    }

    #[test]
    fn background_read_policy() {
        use Background::{Fail, Read, Signal};
        // The foreground group reads; so does anyone when there's none, or from another
        // session.
        assert_eq!(background_read(5, 1, 5, 1, false, false), Read);
        assert_eq!(background_read(5, 1, 0, 1, false, false), Read);
        assert_eq!(background_read(5, 9, 7, 1, false, false), Read);
        // A background group is stopped by `SIGTTIN`...
        assert_eq!(background_read(5, 1, 7, 1, false, false), Signal);
        // ...unless it ignores or blocks it, or is orphaned: then `EIO`.
        assert_eq!(background_read(5, 1, 7, 1, true, false), Fail);
        assert_eq!(background_read(5, 1, 7, 1, false, true), Fail);
    }

    #[test]
    fn output_adds_carriage_returns() {
        let mut out = [0u8; 16];
        assert_eq!(output(tc::OPOST | tc::ONLCR, b"a\nb\n", &mut out), (4, 6));
        assert_eq!(&out[..6], b"a\r\nb\r\n");
        assert_eq!(output(tc::ONLCR, b"a\n", &mut out), (2, 2));
        assert_eq!(&out[..2], b"a\n");
        // A newline that doesn't fit whole waits for the next piece.
        let mut tight = [0u8; 2];
        assert_eq!(output(tc::OPOST | tc::ONLCR, b"a\n", &mut tight), (1, 1));
    }
}
