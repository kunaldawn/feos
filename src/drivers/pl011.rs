//! ARM PL011 UART driver (polled, no interrupts).
//!
//! The QEMU `virt` machine maps UART0 at 0x0900_0000 and wires it to the
//! serial console. See the ARM PrimeCell UART (PL011) TRM for register details.

use core::ptr::{read_volatile, write_volatile};

const UART0_BASE: usize = 0x0900_0000;

const DR: usize = 0x00;
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

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

/// QEMU gives the PL011 a 24 MHz reference clock.
const UART_CLK_HZ: u32 = 24_000_000;
const BAUD: u32 = 115_200;

struct Uart {
    base: usize,
}

impl Uart {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

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

    fn putc(&self, byte: u8) {
        while self.read(FR) & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        self.write(DR, u32::from(byte));
    }

    fn getc(&self) -> Option<u8> {
        if self.read(FR) & FR_RXFE != 0 {
            None
        } else {
            Some((self.read(DR) & 0xff) as u8)
        }
    }
}

static UART0: Uart = Uart::new(UART0_BASE);

pub fn init() {
    UART0.init();
}

pub fn putc(byte: u8) {
    UART0.putc(byte);
}

pub fn getc() -> Option<u8> {
    UART0.getc()
}
