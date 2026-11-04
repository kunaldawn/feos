//! The AdLib/Sound Blaster FM ports (`388h`/`389h`, `38Ah`/`38Bh`).

use crate::opl3::{Opl3Device, OplRegisterFile};

/// The OPL2/OPL3 device and its latched address registers.
pub struct Opl {
    device: Opl3Device,
    rate: u32,
}

impl Opl {
    pub fn new(rate: u32) -> Self {
        Self {
            device: Opl3Device::new(rate),
            rate,
        }
    }

    pub fn reset(&mut self, rate: u32) {
        self.rate = rate;
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

    /// Fills an interleaved stereo buffer.
    pub fn generate(&mut self, buffer: &mut [i16]) {
        let _ = self.device.generate_samples(buffer);
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
