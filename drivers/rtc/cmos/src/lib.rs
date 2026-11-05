//! The PC's CMOS real-time clock, on ports `0x70` and `0x71`.
//!
//! The clock stores broken-down time, in BCD unless a status bit says otherwise,
//! and may be mid-update when read. So: wait out any update, read everything, and
//! read again until two passes agree. The registers can't say which century it
//! is, so the century is assumed from the date this kernel was written.

#![cfg_attr(not(test), no_std)]

use feos_hal::time::DateTime;
use feos_hal::RealTimeClock;

#[cfg(target_arch = "x86_64")]
use feos_port_io::{inb, outb};

/// Selects a register. Bit 7 keeps NMIs masked as firmware left them.
#[cfg(target_arch = "x86_64")]
const INDEX_PORT: u16 = 0x70;
#[cfg(target_arch = "x86_64")]
const DATA_PORT: u16 = 0x71;

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;

/// Status A: an update is about to happen or in progress.
const UPDATE_IN_PROGRESS: u8 = 0x80;
/// Status B: registers are binary, not BCD.
const BINARY: u8 = 0x04;
/// Status B: hours run 0-23, not 1-12.
const HOURS_24: u8 = 0x02;
/// The hours register's PM flag, in 12-hour mode.
const HOUR_PM: u8 = 0x80;

/// Reads never go past this many attempts: a broken clock has no right to hang
/// the caller.
const MAX_ATTEMPTS: usize = 64;
/// Polls of the update flag in one attempt.
const UPDATE_POLLS: usize = 100_000;

/// The two-digit year is taken to be in this century's window.
const CENTURY: u32 = 2000;

/// Raw register values, before interpreting them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct RawTime {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    status_b: u8,
}

fn from_bcd(value: u8) -> Option<u8> {
    let (high, low) = (value >> 4, value & 0x0f);
    (high <= 9 && low <= 9).then_some(high * 10 + low)
}

/// Decodes register values into seconds since the epoch.
fn decode(raw: RawTime) -> Option<u64> {
    let binary = raw.status_b & BINARY != 0;
    let field = |value: u8| if binary { Some(value) } else { from_bcd(value) };

    let pm = raw.hour & HOUR_PM != 0 && raw.status_b & HOURS_24 == 0;
    let mut hour = field(raw.hour & !HOUR_PM)?;
    if raw.status_b & HOURS_24 == 0 {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    DateTime {
        year: CENTURY + field(raw.year)? as u32,
        month: field(raw.month)?,
        day: field(raw.day)?,
        hour,
        minute: field(raw.minute)?,
        second: field(raw.second)?,
    }
    .to_unix()
}

/// The clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct CmosRtc;

impl CmosRtc {
    /// The clock on this machine.
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(target_arch = "x86_64")]
fn read_register(register: u8) -> u8 {
    // SAFETY: the standard CMOS index and data ports. Selecting a register has
    // no effect other than choosing what the data port reads.
    unsafe {
        outb(INDEX_PORT, register);
        inb(DATA_PORT)
    }
}

#[cfg(target_arch = "x86_64")]
fn read_raw() -> Option<RawTime> {
    for _ in 0..UPDATE_POLLS {
        if read_register(STATUS_A) & UPDATE_IN_PROGRESS == 0 {
            return Some(RawTime {
                second: read_register(SECONDS),
                minute: read_register(MINUTES),
                hour: read_register(HOURS),
                day: read_register(DAY),
                month: read_register(MONTH),
                year: read_register(YEAR),
                status_b: read_register(STATUS_B),
            });
        }
        core::hint::spin_loop();
    }
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn read_raw() -> Option<RawTime> {
    let _ = (SECONDS, MINUTES, HOURS, DAY, MONTH, YEAR, STATUS_A, STATUS_B);
    let _ = (UPDATE_IN_PROGRESS, UPDATE_POLLS);
    None
}

impl RealTimeClock for CmosRtc {
    fn unix_time(&self) -> Option<u64> {
        let mut previous = read_raw()?;
        for _ in 0..MAX_ATTEMPTS {
            let current = read_raw()?;
            if current == previous {
                return decode(current);
            }
            previous = current;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status_b: u8) -> RawTime {
        RawTime {
            second: 0x30,
            minute: 0x15,
            hour: 0x09,
            day: 0x28,
            month: 0x02,
            year: 0x24,
            status_b,
        }
    }

    const EXPECTED: u64 = 1_709_111_730; // 2024-02-28 09:15:30.

    #[test]
    fn decodes_bcd_24_hour() {
        assert_eq!(decode(raw(HOURS_24)), Some(EXPECTED));
    }

    #[test]
    fn decodes_binary() {
        let time = RawTime {
            second: 30,
            minute: 15,
            hour: 9,
            day: 28,
            month: 2,
            year: 24,
            status_b: BINARY | HOURS_24,
        };
        assert_eq!(decode(time), Some(EXPECTED));
    }

    #[test]
    fn decodes_twelve_hour_noon_and_midnight() {
        let mut time = raw(0);
        assert_eq!(decode(time), Some(EXPECTED));
        time.hour = 0x12 | HOUR_PM;
        assert_eq!(decode(time), Some(EXPECTED - 9 * 3600 + 12 * 3600));
        time.hour = 0x12;
        assert_eq!(decode(time), Some(EXPECTED - 9 * 3600));
        time.hour = 0x09 | HOUR_PM;
        assert_eq!(decode(time), Some(EXPECTED + 12 * 3600));
    }

    #[test]
    fn garbage_is_no_time_rather_than_a_wrong_one() {
        let mut time = raw(HOURS_24);
        time.month = 0x13;
        assert_eq!(decode(time), None);
        time = raw(HOURS_24);
        time.second = 0xaa;
        assert_eq!(decode(time), None);
        assert_eq!(decode(RawTime::default()), None);
        time = raw(0);
        time.hour = 0x13;
        assert_eq!(decode(time), None);
    }
}
