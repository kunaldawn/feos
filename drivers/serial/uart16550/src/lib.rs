//! 16550-compatible UART driver (polled, no interrupts).
//!
//! The driver is a plain struct over an I/O port base, so a board can
//! instantiate COM1 or any other 16550 at a different port without global
//! state. QEMU's `-serial` option maps COM1 to the first serial port at
//! `0x3f8`.

#![no_std]

use core::arch::asm;

use feos_hal::Serial;

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
const LSR_THR_EMPTY: u8 = 1 << 5;

#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: callers pass the UART's valid I/O port range.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: callers pass the UART's valid I/O port range.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack)) };
    value
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
        while self.read(LSR) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        self.write(DATA, byte);
    }

    fn read_byte(&self) -> Option<u8> {
        if self.read(LSR) & LSR_DATA_READY != 0 {
            Some(self.read(DATA))
        } else {
            None
        }
    }
}
