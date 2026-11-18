//! Calendar arithmetic, for clocks and filesystems that store a date.
//!
//! Everything is proleptic Gregorian in UTC. The conversions are Howard
//! Hinnant's `days_from_civil` and `civil_from_days`, which are exact for the
//! whole `u64` second range this kernel will ever see.

/// A broken-down date and time.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DateTime {
    /// Full year, such as 2026.
    pub year: u32,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 59.
    pub second: u8,
}

const SECONDS_PER_DAY: u64 = 86_400;

/// Days from 1970-01-01 to the given date.
pub const fn days_from_civil(year: u32, month: u8, day: u8) -> i64 {
    let month = month as i64;
    let year = year as i64 - (month <= 2) as i64;
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

impl DateTime {
    /// Breaks down seconds since the Unix epoch.
    pub const fn from_unix(seconds: u64) -> Self {
        let days = (seconds / SECONDS_PER_DAY) as i64;
        let rest = seconds % SECONDS_PER_DAY;
        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_index + 2) / 5 + 1;
        let month = if month_index < 10 {
            month_index + 3
        } else {
            month_index - 9
        };
        let year = year_of_era + era * 400 + (month <= 2) as i64;
        Self {
            year: year as u32,
            month: month as u8,
            day: day as u8,
            hour: (rest / 3600) as u8,
            minute: (rest / 60 % 60) as u8,
            second: (rest % 60) as u8,
        }
    }

    /// Seconds since the Unix epoch, or `None` for a date before it or with a
    /// field out of range.
    pub const fn to_unix(&self) -> Option<u64> {
        if self.month < 1
            || self.month > 12
            || self.day < 1
            || self.day > days_in_month(self.year, self.month)
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
        {
            return None;
        }
        let days = days_from_civil(self.year, self.month, self.day);
        if days < 0 {
            return None;
        }
        Some(
            days as u64 * SECONDS_PER_DAY
                + self.hour as u64 * 3600
                + self.minute as u64 * 60
                + self.second as u64,
        )
    }
}

/// Length of a month, or zero for a month that doesn't exist.
pub const fn days_in_month(year: u32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Nanoseconds in a second.
const NANOS: u128 = 1_000_000_000;

/// A counter value as nanoseconds, rounded down. The product needs 128 bits, as in
/// [`ticks_to_ms`](crate::ticks_to_ms). Zero for a zero frequency.
pub const fn ticks_to_ns(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    let ns = ticks as u128 * NANOS / frequency as u128;
    if ns > u64::MAX as u128 {
        u64::MAX
    } else {
        ns as u64
    }
}

/// The first counter value at which [`ticks_to_ns`] reads at least `ns`: a deadline in
/// nanoseconds as one the scheduler can wait for, never early. Saturates, so a deadline
/// past the counter's range is one it never reaches.
pub const fn ns_to_ticks(ns: u64, frequency: u64) -> u64 {
    let ticks = (ns as u128 * frequency as u128).div_ceil(NANOS);
    if ticks > u64::MAX as u128 {
        u64::MAX
    } else {
        ticks as u64
    }
}

/// One period of a counter, rounded up to a whole nanosecond: the resolution a clock read
/// from it has.
pub const fn period_ns(frequency: u64) -> u64 {
    if frequency == 0 {
        return 1;
    }
    let period = NANOS.div_ceil(frequency as u128);
    if period == 0 {
        1
    } else {
        period as u64
    }
}

/// The wall clock as an offset from monotonic time, given a reading of a clock that
/// counts whole seconds taken at monotonic time `now`: `wall = monotonic + offset`. The
/// reading is the second that began at or before `now`, so the offset this gives is at
/// most a second behind; taken just as the second changes, it's exact. A wall clock that
/// reads earlier than the monotonic time (no clock, or a broken one) is the epoch at boot.
pub const fn wall_offset(seconds: u64, now: u64) -> u64 {
    let wall = seconds as u128 * NANOS;
    if wall > now as u128 {
        let offset = wall - now as u128;
        if offset > u64::MAX as u128 {
            u64::MAX
        } else {
            offset as u64
        }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_values_become_nanoseconds_and_back() {
        // QEMU's AArch64 counter, 62.5 MHz: 16 ns a period.
        assert_eq!(ticks_to_ns(62_500_000, 62_500_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(1, 62_500_000), 16);
        assert_eq!(period_ns(62_500_000), 16);
        // A 3 GHz TSC: a period is a third of a nanosecond, reported as one.
        assert_eq!(period_ns(3_000_000_000), 1);
        assert_eq!(period_ns(1_000_000_007), 1);
        assert_eq!(period_ns(999_999_999), 2);
        assert_eq!(period_ns(0), 1);
        assert_eq!(ticks_to_ns(5, 0), 0);
        // Days of uptime at 3 GHz don't overflow the product.
        assert_eq!(ticks_to_ns(3_000_000_000 * 86_400 * 100, 3_000_000_000), 86_400 * 100 * 1_000_000_000);
    }

    #[test]
    fn a_deadline_is_never_reached_early() {
        for frequency in [62_500_000u64, 1_000_000_000, 2_899_999_999, 24_000_000, 7] {
            for ns in [0u64, 1, 15, 16, 17, 999_999_999, 1_000_000_001, 123_456_789_012] {
                let ticks = ns_to_ticks(ns, frequency);
                assert!(ticks_to_ns(ticks, frequency) >= ns, "{ns} at {frequency}");
                // And not late by more than a period.
                if ticks > 0 {
                    assert!(ticks_to_ns(ticks - 1, frequency) < ns, "{ns} at {frequency}");
                }
            }
        }
        assert_eq!(ns_to_ticks(u64::MAX, 3_000_000_000), u64::MAX);
    }

    #[test]
    fn the_wall_offset_is_the_reading_less_the_time_it_was_taken() {
        assert_eq!(wall_offset(1_800_000_000, 5_000_000_000), 1_800_000_000 * 1_000_000_000 - 5_000_000_000);
        assert_eq!(wall_offset(0, 5_000_000_000), 0);
        assert_eq!(wall_offset(3, 5_000_000_000), 0);
    }

    #[test]
    fn known_dates_convert_both_ways() {
        for (seconds, date) in [
            (0u64, (1970, 1, 1, 0, 0, 0)),
            (951_782_400, (2000, 2, 29, 0, 0, 0)),
            (1_735_689_599, (2024, 12, 31, 23, 59, 59)),
            (4_107_542_400, (2100, 3, 1, 0, 0, 0)),
        ] {
            let parsed = DateTime::from_unix(seconds);
            assert_eq!(
                (parsed.year, parsed.month, parsed.day, parsed.hour, parsed.minute, parsed.second),
                date
            );
            assert_eq!(parsed.to_unix(), Some(seconds));
        }
    }

    #[test]
    fn every_day_round_trips_across_centuries() {
        let mut seconds = 0u64;
        while seconds < 200 * 366 * SECONDS_PER_DAY {
            assert_eq!(DateTime::from_unix(seconds).to_unix(), Some(seconds));
            seconds += SECONDS_PER_DAY - 1;
        }
    }

    #[test]
    fn impossible_dates_are_refused() {
        let date = |year, month, day| DateTime {
            year,
            month,
            day,
            ..DateTime::default()
        };
        assert_eq!(date(2023, 2, 29).to_unix(), None);
        assert_eq!(date(2100, 2, 29).to_unix(), None);
        assert!(date(2000, 2, 29).to_unix().is_some());
        assert_eq!(date(2024, 13, 1).to_unix(), None);
        assert_eq!(date(2024, 0, 1).to_unix(), None);
        assert_eq!(date(1969, 12, 31).to_unix(), None);
        let late = DateTime { hour: 24, ..date(2024, 1, 1) };
        assert_eq!(late.to_unix(), None);
    }
}
