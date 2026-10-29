//! Character console on top of the PL011 UART.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::pl011;

static LOCKED: AtomicBool = AtomicBool::new(false);

struct Stdout;

impl Write for Stdout {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                pl011::putc(b'\r');
            }
            pl011::putc(byte);
        }
        Ok(())
    }
}

/// Serialized console output used by `print!` / `println!`.
pub fn _print(args: fmt::Arguments) {
    while LOCKED.swap(true, Ordering::Acquire) {
        core::hint::spin_loop();
    }

    let _ = Stdout.write_fmt(args);

    LOCKED.store(false, Ordering::Release);
}

/// Lock-free output for panic and exception paths.
pub fn _print_unlocked(args: fmt::Arguments) {
    let mut out = Stdout;
    let _ = out.write_fmt(args);
}
