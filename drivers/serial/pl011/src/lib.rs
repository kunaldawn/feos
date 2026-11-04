//! ARM PL011 UART driver (polled, no interrupts).
//!
//! The driver is a plain struct over a base address, so a board can instantiate
//! one UART (or several) without any global state:
//!
//! ```ignore
//! static UART0: Pl011 = Pl011::new(0x0900_0000);
//! UART0.init();
//! ```
//!
//! See the ARM PrimeCell UART (PL011) TRM for register details.

#![no_std]

use core::ptr::{read_volatile, write_volatile};

use feos_hal::Serial;

const DR: usize = 0x00;
const RSRECR: usize = 0x04;
const FR: usize = 0x18;
const IBRD: usize = 0x24;
const FBRD: usize = 0x28;
const LCR_H: usize = 0x2c;
const CR: usize = 0x30;
const IFLS: usize = 0x34;
const IMSC: usize = 0x38;
const ICR: usize = 0x44;

const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full
const DR_ERRORS: u32 = 0xf00; // overrun, break, parity and framing

/// Transmit polls before giving up on a stuck UART.
const TX_POLLS: usize = 100_000;

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

/// Reference clock QEMU gives the PL011 on the `virt` machine.
const UART_CLK_HZ: u32 = 24_000_000;
const BAUD: u32 = 115_200;

/// A PL011 UART instance.
pub struct Pl011 {
    base: usize,
}

impl Pl011 {
    /// Creates a driver for the UART mapped at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    /// Base address of this instance.
    pub const fn base(&self) -> usize {
        self.base
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: `base` is an MMIO address supplied by the board.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: `base` is an MMIO address supplied by the board.
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Writes `text`, translating `\n` to `\r\n` for a terminal.
    ///
    /// This is convenient for boards that register a console writer.
    pub fn write_terminal(&self, text: &str) {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }
}

impl Serial for Pl011 {
    fn init(&self) {
        // Disable the UART while reconfiguring it.
        self.write(CR, 0);

        // Clear any pending interrupts.
        self.write(ICR, 0x7ff);

        // 115200 baud, 8N1. IBRD/FBRD are derived from the reference clock:
        // divisor = 24_000_000 / (16 * 115200) = 13.0208... -> 13 + 1/64.
        let divisor = (UART_CLK_HZ * 4) / BAUD;
        self.write(IBRD, divisor >> 6);
        self.write(FBRD, divisor & 0x3f);

        self.write(LCR_H, 0x70); // 8 bits, FIFOs enabled, no parity, 1 stop bit
        self.write(IFLS, 0); // RX/TX interrupt FIFO level: 1/8
        self.write(IMSC, 0); // polled mode: mask all interrupts
        self.write(CR, CR_UARTEN | CR_TXE | CR_RXE);
    }

    fn write_byte(&self, byte: u8) {
        // Bounded so a stuck UART costs characters, not the whole system.
        for _ in 0..TX_POLLS {
            if self.read(FR) & FR_TXFF == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write(DR, u32::from(byte));
    }

    fn read_byte(&self) -> Option<u8> {
        // Reading DR pops the byte, so bad ones are consumed. The bound keeps
        // this a poll even if the FIFO never drains.
        for _ in 0..32 {
            if self.read(FR) & FR_RXFE != 0 {
                return None;
            }
            let data = self.read(DR);
            if data & DR_ERRORS == 0 {
                return Some((data & 0xff) as u8);
            }
            self.write(RSRECR, 0);
        }
        None
    }
}
