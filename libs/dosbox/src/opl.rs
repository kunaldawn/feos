//! The AdLib/Sound Blaster FM ports (`388h`/`389h`, `38Ah`/`38Bh`).

use crate::opl3::{Opl3Device, OplRegisterFile};

/// The largest sample that still counts as silence.
const QUIET_LEVEL: i16 = 16;

/// The OPL2/OPL3 device and its latched address registers.
pub struct Opl {
    device: Opl3Device,
    rate: u32,
    /// A program has written to the chip, so generating samples is worthwhile.
    used: bool,
    /// Every voice has died away and the output has reached zero, so nothing
    /// changes until the next register write.
    quiet: bool,
}

impl Opl {
    pub fn new(rate: u32) -> Self {
        Self {
            device: Opl3Device::new(rate),
            rate,
            used: false,
            quiet: false,
        }
    }

    pub fn reset(&mut self, rate: u32) {
        self.rate = rate;
        self.used = false;
        self.quiet = false;
        let _ = self.device.reset(Some(rate));
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn write_address(&mut self, port: u16, value: u8) {
        let file = if port == 0x38A {
            OplRegisterFile::Secondary
        } else {
            OplRegisterFile::Primary
        };
        let _ = self.device.write_address(value, file);
    }

    pub fn write_data(&mut self, port: u16, value: u8) {
        let file = if port == 0x38B {
            OplRegisterFile::Secondary
        } else {
            OplRegisterFile::Primary
        };
        self.used = true;
        self.quiet = false;
        let _ = self.device.write_data(value, file, false);
    }

    pub fn read_status(&mut self) -> u8 {
        self.device.read_status()
    }

    /// Runs the chip's timers for `usec` microseconds of guest time.
    pub fn advance(&mut self, usec: u64) {
        // Both timers overflow within a fifth of a second at their slowest, and
        // their flags stay set until cleared, so longer gaps needn't be counted.
        self.device.run(usec.min(1_000_000) as f64);
    }

    /// Whether generation is being skipped because nothing is sounding.
    pub fn is_quiet(&self) -> bool {
        !self.used || self.quiet
    }

    /// Fills an interleaved stereo buffer.
    pub fn generate(&mut self, buffer: &mut [i16]) {
        if !self.used || self.quiet {
            buffer.fill(0);
            return;
        }
        let _ = self.device.generate_samples(buffer);
        // A silent chip's output can sit a step or two off zero.
        self.quiet =
            self.device.active_voice_count() == 0 && buffer.iter().all(|&s| s.abs() <= QUIET_LEVEL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(opl: &mut Opl, register: u8, value: u8) {
        opl.write_address(0x388, register);
        opl.write_data(0x389, value);
    }

    /// The standard AdLib detection sequence.
    #[test]
    fn adlib_detection_sees_the_timer_expire() {
        let mut opl = Opl::new(48_000);
        write(&mut opl, 4, 0x60);
        write(&mut opl, 4, 0x80);
        assert_eq!(opl.read_status() & 0xE0, 0);
        write(&mut opl, 2, 0xFF);
        write(&mut opl, 4, 0x21);
        assert_eq!(opl.read_status() & 0xE0, 0, "just started");
        opl.advance(100);
        assert_eq!(opl.read_status() & 0xE0, 0xC0, "timer 1 expired");
        write(&mut opl, 4, 0x60);
        write(&mut opl, 4, 0x80);
        assert_eq!(opl.read_status() & 0xE0, 0);
    }

    fn play_a_note(opl: &mut Opl) {
        for (register, value) in [
            (0x20u8, 0x01u8),
            (0x23, 0x01),
            (0x40, 0x10),
            (0x43, 0x00),
            (0x60, 0xF0),
            (0x63, 0xF0),
            (0x80, 0x0F),
            (0x83, 0x0F),
            (0xA0, 0x98),
            (0xB0, 0x31),
            (0xC0, 0x30),
        ] {
            write(opl, register, value);
        }
    }

    #[test]
    fn generation_stops_when_the_voices_have_died_and_restarts_on_a_write() {
        let mut opl = Opl::new(48_000);
        assert!(opl.is_quiet(), "nothing written yet");
        write(&mut opl, 0xBD, 0);
        let mut buffer = [0i16; 512];
        opl.generate(&mut buffer);
        assert!(opl.is_quiet(), "a register write with no voices stays silent");

        play_a_note(&mut opl);
        assert!(!opl.is_quiet());
        opl.generate(&mut buffer);
        assert!(buffer.iter().any(|sample| *sample != 0));
        assert!(!opl.is_quiet(), "a held note keeps it going");

        // Key off, with the fastest release: it fades, then generation stops.
        write(&mut opl, 0xB0, 0x11);
        for _ in 0..2000 {
            opl.generate(&mut buffer);
            if opl.is_quiet() {
                break;
            }
        }
        assert!(opl.is_quiet(), "a released note ends generation");

        // A skipped chip keeps its registers: the next key-on still sounds.
        write(&mut opl, 0xB0, 0x31);
        assert!(!opl.is_quiet());
        opl.generate(&mut buffer);
        assert!(buffer.iter().any(|sample| *sample != 0));
    }

    #[test]
    fn timers_run_while_generation_is_skipped() {
        let mut opl = Opl::new(48_000);
        write(&mut opl, 0xBD, 0);
        let mut buffer = [0i16; 512];
        opl.generate(&mut buffer);
        assert!(opl.is_quiet());
        write(&mut opl, 2, 0xFF);
        write(&mut opl, 4, 0x21);
        opl.generate(&mut buffer);
        assert!(opl.is_quiet(), "a timer doesn't need samples");
        opl.advance(100);
        assert_eq!(opl.read_status() & 0xE0, 0xC0);
    }

    #[test]
    fn opl_produces_samples_after_a_key_on() {
        let mut opl = Opl::new(48_000);
        // A simple two-operator AdLib patch.
        for (register, value) in [
            (0x20u8, 0x01u8),
            (0x23, 0x01),
            (0x40, 0x10),
            (0x43, 0x00),
            (0x60, 0xF0),
            (0x63, 0xF0),
            (0x80, 0x77),
            (0x83, 0x77),
            (0xA0, 0x98),
            (0xB0, 0x31),
            (0xC0, 0x30),
        ] {
            opl.write_address(0x388, register);
            opl.write_data(0x389, value);
        }
        let mut buffer = [0i16; 2048];
        opl.generate(&mut buffer);
        assert!(buffer.iter().any(|sample| *sample != 0));
    }
}
