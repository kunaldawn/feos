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
//! [`_print`] serializes whole formatting operations with a [`SpinLock`].
//! Panic and exception handlers use [`_print_unlocked`], which bypasses the
//! lock so a fault taken while the console is held can still be reported.

#![no_std]

use core::fmt::{self, Write};
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use feos_sync::SpinLock;

/// Receives a chunk of text from the console.
pub type WriteFn = fn(&str);

/// Receives a chunk of raw bytes from the console.
pub type ByteWriteFn = fn(&[u8]);

/// Returns the next input byte, or `None` if the device has no data.
pub type ReadFn = fn() -> Option<u8>;

/// Waits for input or the next timer tick without busy-spinning.
pub type WaitFn = fn();

static WRITER: AtomicUsize = AtomicUsize::new(0);
static BYTE_WRITER: AtomicUsize = AtomicUsize::new(0);
static READER: AtomicUsize = AtomicUsize::new(0);
static WAITER: AtomicUsize = AtomicUsize::new(0);
static PRINT_LOCK: SpinLock<()> = SpinLock::new(());

/// Installs the text sink used by `print!` and `println!`.
///
/// The writer receives raw bytes as UTF-8 text and is responsible for any
/// newline translation the hardware needs (the QEMU board turns `\n` into
/// `\r\n` for the PL011).
pub fn set_writer(writer: WriteFn) {
    WRITER.store(writer as usize, Ordering::Release);
}

/// Installs a raw-byte sink used by [`write_bytes`].
///
/// A board that installs this keeps binary output intact; otherwise
/// [`write_bytes`] falls back to the text writer for valid UTF-8.
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

/// Writes raw bytes to the registered console.
///
/// This is the byte-level counterpart to [`print!`]. It is used by the shell
/// so utilities such as `cat` and `hexdump` can emit arbitrary data. When the
/// board did not install a byte writer, valid UTF-8 is forwarded to the text
/// writer and invalid bytes are replaced with `?`.
pub fn write_bytes(bytes: &[u8]) {
    let _guard = PRINT_LOCK.lock();
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

/// Serialized console output used by the print macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // A panic while the lock is held is handled by `_print_unlocked`, so this
    // guard cannot deadlock the panic path.
    let _guard = PRINT_LOCK.lock();
    let _ = Sink.write_fmt(args);
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
