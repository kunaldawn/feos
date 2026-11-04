//! 16550-compatible UART driver (polled, no interrupts).
//!
//! The driver is a plain struct over an I/O port base, so a board can
//! instantiate COM1 or any other 16550 at a different port without global
//! state. QEMU's `-serial` option maps COM1 to the first serial port at
//! `0x3f8`.

#![cfg_attr(not(test), no_std)]

use feos_hal::Serial;
use feos_port_io::{inb, outb};

/// Transmit holding register (write) / receive buffer (read).
const DATA: u16 = 0;
/// Interrupt enable register (with DLAB=0) / divisor latch low (DLAB=1).
const IER: u16 = 1;
/// FIFO control register (write) / interrupt identification (read).
const FCR: u16 = 2;
/// Line control register.
const LCR: u16 = 3;
/// Modem control register.
const MCR: u16 = 4;
/// Line status register.
const LSR: u16 = 5;

const LSR_DATA_READY: u8 = 1 << 0;
/// Overrun, parity, framing and break.
const LSR_ERRORS: u8 = 0x1e;
const LSR_THR_EMPTY: u8 = 1 << 5;
/// What an I/O port with no device behind it reads as.
const LSR_ABSENT: u8 = 0xff;

/// Transmit polls before giving up on a stuck UART. At 115200 baud a byte takes
/// under 100 us, far less than this many port reads.
const TX_POLLS: usize = 100_000;

/// Decodes a line status value into what [`Serial::read_byte`] should do.
fn receive_action(lsr: u8) -> ReceiveAction {
    if lsr == LSR_ABSENT || lsr & LSR_DATA_READY == 0 {
        ReceiveAction::Nothing
    } else if lsr & LSR_ERRORS != 0 {
        ReceiveAction::Discard
    } else {
        ReceiveAction::Deliver
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReceiveAction {
    /// No byte is waiting, or there's no UART at all.
    Nothing,
    /// A byte is waiting but it was received with an error.
    Discard,
    /// A good byte is waiting.
    Deliver,
}

/// A 16550-compatible UART instance.
pub struct Uart16550 {
    base: u16,
}

impl Uart16550 {
    /// Creates a driver for the UART whose first register is at `base`.
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Base I/O port of this instance.
    pub const fn base(&self) -> u16 {
        self.base
    }

    fn read(&self, offset: u16) -> u8 {
        // SAFETY: `base` is an I/O port supplied by the board.
        unsafe { inb(self.base + offset) }
    }

    fn write(&self, offset: u16, value: u8) {
        // SAFETY: `base` is an I/O port supplied by the board.
        unsafe { outb(self.base + offset, value) };
    }

    /// Writes `text`, translating `\n` to `\r\n` for a terminal.
    pub fn write_terminal(&self, text: &str) {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }
}

impl Serial for Uart16550 {
    fn init(&self) {
        // Mask all UART interrupts: the console is polled.
        self.write(IER, 0x00);

        // 115200 baud, 8 data bits, no parity, one stop bit.
        self.write(LCR, 0x80); // DLAB on
        self.write(DATA, 0x01); // divisor low: 115200 / 115200
        self.write(IER, 0x00); // divisor high
        self.write(LCR, 0x03); // 8N1, DLAB off

        // Enable and clear the FIFOs, 14-byte trigger level.
        self.write(FCR, 0xc7);

        // Data terminal ready and request-to-send.
        self.write(MCR, 0x03);
    }

    fn write_byte(&self, byte: u8) {
        // Bounded so a stuck UART costs characters, not the whole system.
        for _ in 0..TX_POLLS {
            if self.read(LSR) & LSR_THR_EMPTY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write(DATA, byte);
    }

    fn read_byte(&self) -> Option<u8> {
        // Reading DATA clears a per-byte error, so bad bytes are consumed. The
        // bound keeps this a poll even if the status never clears.
        for _ in 0..32 {
            match receive_action(self.read(LSR)) {
                ReceiveAction::Nothing => return None,
                ReceiveAction::Discard => {
                    let _ = self.read(DATA);
                }
                ReceiveAction::Deliver => return Some(self.read(DATA)),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_uart_reads_as_no_data() {
        // DRV-15: 0xff has the data-ready bit set, so a machine without COM1
        // used to deliver 0xff forever.
        assert_eq!(receive_action(LSR_ABSENT), ReceiveAction::Nothing);
    }

    #[test]
    fn line_status_decodes_data_and_errors() {
        assert_eq!(receive_action(0x60), ReceiveAction::Nothing);
        assert_eq!(receive_action(0x61), ReceiveAction::Deliver);
        assert_eq!(receive_action(0x61 | 0x02), ReceiveAction::Discard);
        assert_eq!(receive_action(0x61 | 0x08), ReceiveAction::Discard);
        // Errors with no byte waiting have nothing to discard.
        assert_eq!(receive_action(0x60 | 0x02), ReceiveAction::Nothing);
    }
}
