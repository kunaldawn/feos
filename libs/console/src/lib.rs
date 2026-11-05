//! Registered text console and the `print!` / `println!` macros.
//!
//! The kernel does not assume a particular UART. A board (or any other piece
//! of early boot code) installs a byte sink and an optional byte source with
//! [`set_writer`] and [`set_reader`]; everything else prints through the
//! macros and reads input with [`read_byte`]. A kernel can install an idle hook
//! with [`set_waiter`] so a shell waiting for input does not busy-spin.
//!
//! This keeps console plumbing out of drivers and lets the same kernel run on
//! a board with a PL011, an SBI console, or a memory-backed test console.
//!
//! # Locking
//!
//! [`_print`] formats into a small stack buffer first and only then takes
//! [`PRINT_LOCK`], an [`IrqSpinLock`], to write it out. A holder can't be
//! preempted, so other tasks never spin waiting for a descheduled printer, and
//! a killed task can't take the console with it. Lines longer than the buffer
//! are written in pieces and can interleave with other output.
//!
//! Panic and exception handlers use [`_print_unlocked`], which bypasses the
//! lock so a fault taken while the console is held can still be reported.

#![cfg_attr(not(test), no_std)]

use core::fmt::{self, Write};
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use feos_sync::IrqSpinLock;

/// Receives a chunk of text from the console.
pub type WriteFn = fn(&str);

/// Receives a chunk of raw bytes from the console.
pub type ByteWriteFn = fn(&[u8]);

/// Returns the next input byte, or `None` if the device has no data.
pub type ReadFn = fn() -> Option<u8>;

/// Waits for input or the next timer tick without busy-spinning.
pub type WaitFn = fn();

/// Offers printed text to the calling task's own console. Returns `false` when
/// the task has none, so the text goes to the registered console instead. The
/// flag says the text is error output.
pub type RouteFn = fn(bytes: &[u8], error: bool) -> bool;

static WRITER: AtomicUsize = AtomicUsize::new(0);
static BYTE_WRITER: AtomicUsize = AtomicUsize::new(0);
static READER: AtomicUsize = AtomicUsize::new(0);
static WAITER: AtomicUsize = AtomicUsize::new(0);
static ROUTER: AtomicUsize = AtomicUsize::new(0);
static PRINT_LOCK: IrqSpinLock<()> = IrqSpinLock::new(());

/// Bytes formatted before each write. This bounds how long one print keeps
/// interrupts masked: about 22 ms at a real 115200 baud, none under QEMU.
const PRINT_BUFFER: usize = 256;

/// Installs the text sink used by `print!` and `println!`.
///
/// The writer receives raw bytes as UTF-8 text and is responsible for any
/// newline translation the hardware needs (the QEMU board turns `\n` into
/// `\r\n` for the PL011).
pub fn set_writer(writer: WriteFn) {
    WRITER.store(writer as usize, Ordering::Release);
}

/// Installs a raw-byte sink used by [`write_raw`] and [`write_bytes`].
///
/// The sink must pass bytes through untouched, which keeps binary output
/// intact. Without one, those functions fall back to the text writer for valid
/// UTF-8.
pub fn set_byte_writer(writer: ByteWriteFn) {
    BYTE_WRITER.store(writer as usize, Ordering::Release);
}

/// Installs the byte source used by [`read_byte`].
pub fn set_reader(reader: ReadFn) {
    READER.store(reader as usize, Ordering::Release);
}

/// Installs the idle hook used by [`wait_for_input`].
///
/// The kernel sets this to an architecture idle wait (for example `WFI`), so a
/// shell blocked on the serial console does not consume a CPU. Without a hook,
/// waiting falls back to a spin loop for host tests and minimal ports.
pub fn set_waiter(waiter: WaitFn) {
    WAITER.store(waiter as usize, Ordering::Release);
}

/// Installs the per-task router consulted by the print macros.
pub fn set_router(router: RouteFn) {
    ROUTER.store(router as usize, Ordering::Release);
}

fn route(bytes: &[u8], error: bool) -> bool {
    let address = ROUTER.load(Ordering::Acquire);
    if address == 0 {
        return false;
    }
    // SAFETY: only `set_router` writes this atomic, and it stores a valid
    // `RouteFn` (a plain function pointer is `usize`-sized).
    let router: RouteFn = unsafe { transmute::<usize, RouteFn>(address) };
    router(bytes, error)
}

/// Reads one byte from the registered console, if any is available.
pub fn read_byte() -> Option<u8> {
    let address = READER.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }

    // SAFETY: only `set_reader` writes this atomic, and it stores a valid
    // `ReadFn` (a plain function pointer is `usize`-sized).
    let reader: ReadFn = unsafe { transmute::<usize, ReadFn>(address) };
    reader()
}

/// Waits until input may be available or the next timer tick fires.
///
/// The shell calls this between [`read_byte`] polls. The installed hook is
/// expected to enter an architectural idle state and mark the tick as idle for
/// scheduler accounting; it must eventually return so the caller can poll
/// again.
pub fn wait_for_input() {
    let address = WAITER.load(Ordering::Acquire);
    if address == 0 {
        core::hint::spin_loop();
        return;
    }

    // SAFETY: only `set_waiter` writes this atomic, and it stores a valid
    // `WaitFn` (a plain function pointer is `usize`-sized).
    let waiter: WaitFn = unsafe { transmute::<usize, WaitFn>(address) };
    waiter()
}

fn writer() -> Option<WriteFn> {
    let address = WRITER.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }

    // SAFETY: only `set_writer` writes this atomic, and it stores a valid
    // `WriteFn` (a plain function pointer is `usize`-sized).
    Some(unsafe { transmute::<usize, WriteFn>(address) })
}

fn byte_writer() -> Option<ByteWriteFn> {
    let address = BYTE_WRITER.load(Ordering::Acquire);
    if address == 0 {
        return None;
    }

    // SAFETY: only `set_byte_writer` writes this atomic, and it stores a valid
    // `ByteWriteFn` (a plain function pointer is `usize`-sized).
    Some(unsafe { transmute::<usize, ByteWriteFn>(address) })
}

/// Writes bytes to the registered console, unchanged.
///
/// Use this for binary data. When the board did not install a byte writer,
/// valid UTF-8 is forwarded to the text writer and invalid bytes are replaced
/// with `?`.
pub fn write_raw(bytes: &[u8]) {
    let _guard = PRINT_LOCK.lock();
    emit(bytes);
}

/// Writes terminal output: bytes are unchanged except that `\n` becomes
/// `\r\n`, the job a tty's line discipline would do.
///
/// This is the byte-level counterpart to [`print!`], used by the shell for
/// command output. Use [`write_raw`] for data that must arrive unchanged.
pub fn write_bytes(bytes: &[u8]) {
    let _guard = PRINT_LOCK.lock();
    for_each_terminal_chunk(bytes, emit);
}

/// Splits `bytes` around each `\n`, inserting the `\r` a terminal needs.
fn for_each_terminal_chunk(bytes: &[u8], mut chunk: impl FnMut(&[u8])) {
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        match line.split_last() {
            Some((b'\n', text)) => {
                chunk(text);
                chunk(b"\r\n");
            }
            _ => chunk(line),
        }
    }
}

fn emit(bytes: &[u8]) {
    if let Some(writer) = byte_writer() {
        writer(bytes);
        return;
    }
    let Some(writer) = writer() else {
        return;
    };

    let mut start = 0usize;
    while start < bytes.len() {
        match core::str::from_utf8(&bytes[start..]) {
            Ok(text) => {
                writer(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    if let Ok(text) = core::str::from_utf8(&bytes[start..start + valid]) {
                        writer(text);
                    }
                }
                writer("?");
                start += valid + 1;
            }
        }
    }
}

struct Sink;

impl Write for Sink {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if let Some(writer) = writer() {
            writer(text);
        }
        Ok(())
    }
}

/// Formats into a stack buffer, writing it out under the lock when it fills.
struct BufferedSink {
    buffer: [u8; PRINT_BUFFER],
    len: usize,
    error: bool,
}

impl BufferedSink {
    const fn new(error: bool) -> Self {
        Self {
            buffer: [0; PRINT_BUFFER],
            len: 0,
            error,
        }
    }

    fn flush(&mut self) {
        // The buffer only ever holds whole characters: see `write_str`.
        if let Ok(text) = core::str::from_utf8(&self.buffer[..self.len]) {
            if !text.is_empty() && !route(text.as_bytes(), self.error) {
                // A panic while the lock is held is handled by
                // `_print_unlocked`, so this cannot deadlock the panic path.
                let _guard = PRINT_LOCK.lock();
                if let Some(writer) = writer() {
                    writer(text);
                }
            }
        }
        self.len = 0;
    }
}

impl Write for BufferedSink {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            if self.len + character.len_utf8() > PRINT_BUFFER {
                self.flush();
            }
            let room = &mut self.buffer[self.len..];
            self.len += character.encode_utf8(room).len();
        }
        Ok(())
    }
}

/// Serialized console output used by the print macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut sink = BufferedSink::new(false);
    let _ = sink.write_fmt(args);
    sink.flush();
}

/// Like [`_print`], for error output.
#[doc(hidden)]
pub fn _eprint(args: fmt::Arguments) {
    let mut sink = BufferedSink::new(true);
    let _ = sink.write_fmt(args);
    sink.flush();
}

/// Lock-free console output for panic and exception handlers.
///
/// # Safety
/// This is safe to call from Rust, but it is only appropriate on paths that
/// must not block. It may interleave with normal output.
#[doc(hidden)]
pub fn _print_unlocked(args: fmt::Arguments) {
    let mut sink = Sink;
    let _ = sink.write_fmt(args);
}

/// Prints to the registered console without a trailing newline.
///
/// Output is discarded when no writer has been installed yet.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::_print(format_args!($($arg)*))
    };
}

/// Prints to the registered console with a trailing newline.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*))
    };
}

/// Prints to the task's error output without a trailing newline.
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::_eprint(format_args!($($arg)*))
    };
}

/// Prints to the task's error output with a trailing newline.
#[macro_export]
macro_rules! eprintln {
    () => {
        $crate::eprint!("\n")
    };
    ($($arg:tt)*) => {
        $crate::eprint!("{}\n", format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for_each_terminal_chunk(bytes, |chunk| out.extend_from_slice(chunk));
        out
    }

    #[test]
    fn terminal_output_translates_only_newlines() {
        assert_eq!(collect(b"a\nb"), b"a\r\nb");
        assert_eq!(collect(b"\n\n"), b"\r\n\r\n");
        assert_eq!(collect(b"\x00\xff\r"), b"\x00\xff\r");
        assert_eq!(collect(b""), b"");
    }

    #[test]
    fn long_text_stays_whole_across_buffer_flushes() {
        let mut sink = BufferedSink::new(false);
        // Multi-byte characters that straddle the buffer end must not split.
        for _ in 0..PRINT_BUFFER {
            sink.write_str("é").unwrap();
        }
        assert!(sink.len <= PRINT_BUFFER);
        assert!(core::str::from_utf8(&sink.buffer[..sink.len]).is_ok());
    }
}
