//! The registered console as a terminal: a small line discipline.
//!
//! A command reading standard input gets cooked lines: typing is echoed, Enter
//! ends a line, Backspace and Ctrl-U edit it, Ctrl-D is end of file and Ctrl-C
//! interrupts. The line editor bypasses all of this with raw reads. There is one
//! console, so its state is global too.

use core::sync::atomic::{AtomicBool, Ordering};

use feos_console::read_byte;
use feos_coreutils::{Input, Output};
use feos_sync::SpinLock;

const LINE_BYTES: usize = 1024;
const TYPE_AHEAD_BYTES: usize = 256;

/// How many timer ticks an escape sequence's next byte may take to arrive.
const PENDING_WAITS: usize = 3;

struct Tty {
    line: [u8; LINE_BYTES],
    length: usize,
    /// Bytes of a finished line already handed out.
    position: usize,
    /// True once `line` holds a finished line waiting to be read.
    ready: bool,
    type_ahead: [u8; TYPE_AHEAD_BYTES],
    head: usize,
    count: usize,
}

static TTY: SpinLock<Tty> = SpinLock::new(Tty {
    line: [0; LINE_BYTES],
    length: 0,
    position: 0,
    ready: false,
    type_ahead: [0; TYPE_AHEAD_BYTES],
    head: 0,
    count: 0,
});

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The next byte: first any that arrived while a command ran, then the console's.
///
/// The lock covers reading the console too. Several tasks can be looking at once, and
/// a byte one has read but not yet stored mustn't be overtaken by the next one.
fn next_raw() -> Option<u8> {
    let mut tty = TTY.lock();
    if tty.count == 0 {
        return read_byte();
    }
    let byte = tty.type_ahead[tty.head];
    tty.head = (tty.head + 1) % TYPE_AHEAD_BYTES;
    tty.count -= 1;
    Some(byte)
}

/// Looks for Ctrl-C without losing what else was typed. A host calls this from
/// [`crate::Host::poll_cancel`] while a command runs on the serial console.
pub fn poll_interrupt() -> bool {
    let mut tty = TTY.lock();
    while let Some(byte) = read_byte() {
        if byte == 0x03 {
            INTERRUPTED.store(true, Ordering::Relaxed);
            tty.count = 0;
            tty.length = 0;
            tty.position = 0;
            tty.ready = false;
            continue;
        }
        if tty.count < TYPE_AHEAD_BYTES {
            let tail = (tty.head + tty.count) % TYPE_AHEAD_BYTES;
            tty.type_ahead[tail] = byte;
            tty.count += 1;
        }
    }
    drop(tty);
    INTERRUPTED.swap(false, Ordering::Relaxed)
}

fn echo(bytes: &[u8]) {
    feos_console::write_bytes(bytes);
}

/// What one typed byte did to the line being read.
enum Cooked {
    /// Keep reading.
    Continue,
    /// The line is finished.
    Ready,
    /// Ctrl-D on an empty line.
    EndOfFile,
    /// Ctrl-C.
    Interrupted,
}

fn cook(byte: u8) -> Cooked {
    let mut tty = TTY.lock();
    match byte {
        b'\r' | b'\n' => {
            let length = tty.length;
            if length < LINE_BYTES {
                tty.line[length] = b'\n';
                tty.length += 1;
            }
            tty.ready = true;
            drop(tty);
            echo(b"\n");
            Cooked::Ready
        }
        0x7f | 0x08 => {
            // Remove a whole UTF-8 sequence.
            let mut removed = false;
            while tty.length > 0 {
                tty.length -= 1;
                removed = true;
                let byte = tty.line[tty.length];
                if byte & 0xc0 != 0x80 {
                    break;
                }
            }
            drop(tty);
            if removed {
                echo(b"\x08 \x08");
            }
            Cooked::Continue
        }
        0x15 => {
            let characters = tty.line[..tty.length]
                .iter()
                .filter(|byte| **byte & 0xc0 != 0x80)
                .count();
            tty.length = 0;
            drop(tty);
            for _ in 0..characters {
                echo(b"\x08 \x08");
            }
            Cooked::Continue
        }
        0x04 => {
            if tty.length == 0 {
                Cooked::EndOfFile
            } else {
                // Ctrl-D in mid-line hands over what is there.
                tty.ready = true;
                Cooked::Ready
            }
        }
        0x03 => {
            tty.length = 0;
            drop(tty);
            echo(b"^C\n");
            INTERRUPTED.store(true, Ordering::Relaxed);
            Cooked::Interrupted
        }
        byte if byte >= 0x20 || byte == b'\t' => {
            let length = tty.length;
            if length < LINE_BYTES {
                tty.line[length] = byte;
                tty.length += 1;
            }
            let full = tty.length == LINE_BYTES;
            if full {
                tty.ready = true;
            }
            drop(tty);
            echo(&[byte]);
            if full {
                Cooked::Ready
            } else {
                Cooked::Continue
            }
        }
        _ => Cooked::Continue,
    }
}

/// Copies from a finished line, returning zero when none is waiting.
fn take_cooked(out: &mut [u8]) -> usize {
    let mut tty = TTY.lock();
    if !tty.ready {
        return 0;
    }
    let position = tty.position;
    let count = (tty.length - position).min(out.len());
    out[..count].copy_from_slice(&tty.line[position..position + count]);
    tty.position += count;
    if tty.position >= tty.length {
        tty.position = 0;
        tty.length = 0;
        tty.ready = false;
    }
    count
}

/// Console input backed by the registered FEOS console.
pub struct ConsoleInput;

impl Input for ConsoleInput {
    fn read(&mut self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        loop {
            let taken = take_cooked(out);
            if taken > 0 {
                return taken;
            }
            let Some(byte) = next_raw() else {
                feos_console::wait_for_input();
                continue;
            };
            match cook(byte) {
                Cooked::Continue | Cooked::Ready => {}
                Cooked::EndOfFile | Cooked::Interrupted => return 0,
            }
        }
    }

    fn read_raw(&mut self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        loop {
            if let Some(byte) = next_raw() {
                out[0] = byte;
                return 1;
            }
            feos_console::wait_for_input();
        }
    }

    fn read_pending(&mut self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        for _ in 0..PENDING_WAITS {
            if let Some(byte) = next_raw() {
                out[0] = byte;
                return 1;
            }
            feos_console::wait_for_input();
        }
        0
    }

    fn is_terminal(&self) -> bool {
        true
    }
}

/// Console output backed by the registered FEOS console.
pub struct ConsoleOutput;

impl Output for ConsoleOutput {
    fn write(&mut self, bytes: &[u8]) {
        feos_console::write_bytes(bytes);
    }

    fn is_terminal(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Host, Io, Shell};
    use alloc::string::String;
    use feos_coreutils::NoSystem;

    static CONSOLE_INPUT: std::sync::Mutex<std::collections::VecDeque<u8>> =
        std::sync::Mutex::new(std::collections::VecDeque::new());
    static CONSOLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_console_reader() -> Option<u8> {
        CONSOLE_INPUT.lock().unwrap().pop_front()
    }

    fn feed(bytes: &[u8]) {
        let mut input = CONSOLE_INPUT.lock().unwrap();
        input.clear();
        input.extend(bytes);
        drop(input);
        feos_console::set_reader(test_console_reader);
    }

    struct NoHost;

    impl Host for NoHost {
        fn run_external(
            &mut self,
            _name: &str,
            _args: &[String],
            _stdin: &mut dyn Input,
            _stdout: &mut dyn Output,
            _stderr: &mut dyn Output,
            _background: bool,
        ) -> Option<i32> {
            None
        }
    }

    #[test]
    fn the_interactive_loop_reads_the_registered_console() {
        let _guard = CONSOLE_LOCK.lock().unwrap();
        feed(b"echo hi\r\x04");

        let mut shell = Shell::new(None, &NoSystem);
        let mut host = NoHost;
        let mut stdin = ConsoleInput;
        let mut stdout = ConsoleOutput;
        let mut stderr = ConsoleOutput;
        let mut io = Io {
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };

        assert_eq!(crate::interactive(&mut shell, &mut host, &mut io), 0);
        assert_eq!(shell.history, ["echo hi"]);
    }

    #[test]
    fn cooked_reads_edit_lines_and_stop_at_ctrl_d() {
        let _guard = CONSOLE_LOCK.lock().unwrap();
        feed(b"abx\x7fc\rkill me\x15kept\r\x04after");
        let mut input = ConsoleInput;
        assert_eq!(input.read_to_end(), b"abc\nkept\n");
        // What follows the end of file is still there for the next reader.
        let mut byte = [0u8; 1];
        assert_eq!(input.read_raw(&mut byte), 1);
        assert_eq!(&byte, b"a");
        feed(b"");
    }

    #[test]
    fn ctrl_c_ends_a_cooked_read_and_raises_the_flag() {
        let _guard = CONSOLE_LOCK.lock().unwrap();
        INTERRUPTED.store(false, Ordering::Relaxed);
        feed(b"partial\x03");
        let mut input = ConsoleInput;
        assert_eq!(input.read_to_end(), b"");
        assert!(poll_interrupt());
        assert!(!poll_interrupt());
    }

    #[test]
    fn polling_keeps_typed_bytes_for_later() {
        let _guard = CONSOLE_LOCK.lock().unwrap();
        INTERRUPTED.store(false, Ordering::Relaxed);
        feed(b"xy");
        assert!(!poll_interrupt());
        let mut input = ConsoleInput;
        let mut byte = [0u8; 1];
        assert_eq!(input.read_raw(&mut byte), 1);
        assert_eq!(&byte, b"x");
        assert_eq!(input.read_pending(&mut byte), 1);
        assert_eq!(&byte, b"y");
        assert_eq!(input.read_pending(&mut byte), 0);
    }

    #[test]
    fn a_cooked_line_longer_than_the_buffer_is_delivered_in_pieces() {
        let _guard = CONSOLE_LOCK.lock().unwrap();
        let mut bytes = alloc::vec![b'a'; LINE_BYTES + 10];
        bytes.extend_from_slice(b"\r\x04");
        feed(&bytes);
        let mut input = ConsoleInput;
        let all = input.read_to_end();
        assert_eq!(all.len(), LINE_BYTES + 11);
        assert_eq!(all.last(), Some(&b'\n'));
    }
}
