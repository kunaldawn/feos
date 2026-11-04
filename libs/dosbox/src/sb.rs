//! A Sound Blaster 2.0 DSP: 8-bit single-cycle and auto-init DMA playback.
//!
//! The base port is `220h`, with `IRQ 7` and DMA channel 1, which is what the
//! `BLASTER` variable in the root environment says. FM goes to the OPL.

use alloc::collections::VecDeque;

use crate::hw::Dma;
use crate::memory::Memory;

/// The port block, relative to which registers are decoded.
pub const BASE: u16 = 0x220;
/// The IRQ line.
pub const IRQ: u8 = 7;
const DMA_CHANNEL: usize = 1;

/// DSP state.
pub struct SoundBlaster {
    reset_high: bool,
    read_queue: VecDeque<u8>,
    command: Option<u8>,
    args: [u8; 3],
    arg_count: usize,
    pub speaker_on: bool,
    /// Sample rate of the current transfer, in Hz.
    pub rate: u32,
    pub block: u32,
    /// Samples left until the next IRQ.
    remaining: u32,
    pub active: bool,
    auto_init: bool,
    paused: bool,
    /// The current output sample, signed.
    level: i32,
    /// A direct-DAC sample.
    direct: i32,
    /// Fractional position between output samples, in rate units.
    phase: u64,
    mixer_index: u8,
    mixer: [u8; 64],
    test: u8,
    /// A silence block's length (`DSP 80h`), which plays without DMA.
    silence: Option<u32>,
    /// A block ended; the machine raises the IRQ.
    raise: bool,
}

impl Default for SoundBlaster {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundBlaster {
    pub fn new() -> Self {
        Self {
            reset_high: false,
            read_queue: VecDeque::new(),
            command: None,
            args: [0; 3],
            arg_count: 0,
            speaker_on: false,
            rate: 11_025,
            block: 0,
            remaining: 0,
            active: false,
            auto_init: false,
            paused: false,
            level: 0,
            direct: 0,
            phase: 0,
            mixer_index: 0,
            mixer: [0; 64],
            test: 0,
            silence: None,
            raise: false,
        }
    }

    fn reset(&mut self) {
        self.command = None;
        self.arg_count = 0;
        self.active = false;
        self.auto_init = false;
        self.paused = false;
        self.speaker_on = false;
        self.level = 0;
        self.direct = 0;
        self.silence = None;
        self.raise = false;
        self.read_queue.clear();
        self.read_queue.push_back(0xAA);
    }

    fn args_needed(command: u8) -> usize {
        match command {
            0x10 | 0x40 | 0xE0 => 1,
            0x14 | 0x16 | 0x17 | 0x24 | 0x48 | 0x80 => 2,
            _ => 0,
        }
    }

    fn start(&mut self, length: u32, auto_init: bool) {
        self.block = length;
        self.remaining = length;
        self.active = true;
        self.auto_init = auto_init;
        self.paused = false;
    }

    fn execute(&mut self, command: u8) {
        match command {
            0x10 => {
                self.direct = (self.args[0] as i32 - 128) * 256;
            }
            0x14 | 0x16 | 0x17 | 0x24 => {
                let length = self.args[0] as u32 + ((self.args[1] as u32) << 8) + 1;
                self.start(length, false);
            }
            0x1C | 0x2C => self.start(self.block.max(1), true),
            0x40 => {
                let divisor = 256u32 - self.args[0] as u32;
                self.rate = (1_000_000 / divisor.max(1)).max(1);
            }
            0x48 => self.block = self.args[0] as u32 + ((self.args[1] as u32) << 8) + 1,
            0x91 => self.start(self.block.max(1), false),
            0x90 => self.start(self.block.max(1), true),
            0x80 => {
                let length = self.args[0] as u32 + ((self.args[1] as u32) << 8) + 1;
                self.remaining = length;
                self.block = length;
                self.active = false;
                self.silence = Some(length);
            }
            0xD0 => self.paused = true,
            0xD4 => self.paused = false,
            0xD1 => self.speaker_on = true,
            0xD3 => self.speaker_on = false,
            0xD8 => self.read_queue.push_back(if self.speaker_on { 0xFF } else { 0 }),
            0xDA => self.auto_init = false,
            0xE0 => self.read_queue.push_back(!self.args[0]),
            0xE1 => {
                self.read_queue.push_back(2);
                self.read_queue.push_back(1);
            }
            0xE4 => self.test = self.args[0],
            0xE8 => self.read_queue.push_back(self.test),
            0xF2 => self.raise = true,
            _ => {}
        }
    }

    /// A write in the DSP's port block.
    pub fn port_out(&mut self, port: u16, value: u8) {
        match port - BASE {
            0x04 => self.mixer_index = value,
            0x05 => {
                if let Some(slot) = self.mixer.get_mut(self.mixer_index as usize) {
                    *slot = value;
                }
            }
            0x06 => {
                let high = value & 1 != 0;
                if self.reset_high && !high {
                    self.reset();
                }
                self.reset_high = high;
            }
            0x0C => {
                if let Some(command) = self.command {
                    if self.arg_count < self.args.len() {
                        self.args[self.arg_count] = value;
                        self.arg_count += 1;
                    }
                    if self.arg_count >= Self::args_needed(command) {
                        self.command = None;
                        self.execute(command);
                    }
                } else {
                    self.arg_count = 0;
                    if Self::args_needed(value) == 0 {
                        self.execute(value);
                    } else {
                        self.command = Some(value);
                    }
                }
            }
            _ => {}
        }
    }

    /// A read in the DSP's port block.
    pub fn port_in(&mut self, port: u16) -> u8 {
        match port - BASE {
            0x05 => self
                .mixer
                .get(self.mixer_index as usize)
                .copied()
                .unwrap_or(0xFF),
            0x0A => self.read_queue.pop_front().unwrap_or(0xFF),
            0x0C => 0x7F,
            0x0E => {
                if self.read_queue.is_empty() {
                    0x7F
                } else {
                    0xFF
                }
            }
            _ => 0xFF,
        }
    }

    /// Whether the DSP wants to raise its IRQ. Taking this clears the request.
    pub fn take_irq(&mut self) -> bool {
        core::mem::take(&mut self.raise)
    }

    /// The microseconds until the current block ends, if a transfer is running.
    pub fn next_block_us(&self) -> Option<u64> {
        if !(self.active || self.silence.is_some()) || self.paused {
            return None;
        }
        Some((self.remaining as u64 * 1_000_000 / self.rate.max(1) as u64) + 1)
    }

    fn next_sample(&mut self, dma: &mut Dma, mem: &Memory) -> bool {
        let channel = &mut dma.channels[DMA_CHANNEL];
        if self.silence.is_none() {
            if channel.masked {
                // The transfer waits for the guest to unmask the channel.
                return false;
            }
            let address = ((channel.page as u32) << 16) | channel.address as u32;
            let byte = mem.read8(address);
            channel.address = channel.address.wrapping_add(1);
            let (count, wrapped) = channel.count.overflowing_sub(1);
            channel.count = count;
            if wrapped {
                channel.terminal = true;
                if channel.mode & 0x10 != 0 {
                    channel.address = channel.base_address;
                    channel.count = channel.base_count;
                } else {
                    channel.masked = true;
                }
            }
            self.level = (byte as i32 - 128) * 256;
        } else {
            self.level = 0;
        }
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 {
            self.raise = true;
            if self.silence.take().is_some() {
                // A silence block is one-shot.
            } else if self.auto_init {
                self.remaining = self.block.max(1);
            } else {
                self.active = false;
            }
        }
        true
    }

    /// Runs the transfer forward by `frames` output frames at `output_rate`,
    /// adding to `out` if it's given (interleaved stereo).
    pub fn generate(
        &mut self,
        dma: &mut Dma,
        mem: &Memory,
        frames: usize,
        output_rate: u32,
        mut out: Option<&mut [i16]>,
    ) {
        let output_rate = output_rate.max(1) as u64;
        for frame in 0..frames {
            if (self.active || self.silence.is_some()) && !self.paused {
                self.phase += self.rate as u64;
                while self.phase >= output_rate {
                    self.phase -= output_rate;
                    if !self.next_sample(dma, mem) {
                        self.phase = 0;
                        break;
                    }
                    if !self.active && self.silence.is_none() {
                        break;
                    }
                }
            }
            if let Some(buffer) = out.as_deref_mut() {
                let value = if self.speaker_on {
                    self.level + self.direct
                } else {
                    0
                };
                if value != 0 {
                    if let Some(cell) = buffer.get_mut(frame * 2..frame * 2 + 2) {
                        for sample in cell {
                            *sample = (*sample as i32 + value)
                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                }
            }
        }
    }

    /// Whether playback could produce sound.
    pub fn audible(&self) -> bool {
        self.speaker_on && (self.active || self.direct != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(sb: &mut SoundBlaster, value: u8) {
        sb.port_out(BASE + 0x0C, value);
    }

    fn setup_dma(dma: &mut Dma, length: u16, auto_init: bool) {
        dma.port_out(0x0C, 0);
        dma.port_out(2, 0x00);
        dma.port_out(2, 0x00);
        dma.port_out(3, (length - 1) as u8);
        dma.port_out(3, ((length - 1) >> 8) as u8);
        dma.port_out(0x83, 0x02);
        dma.port_out(0x0B, 0x49 | if auto_init { 0x10 } else { 0 });
        dma.port_out(0x0A, 1);
    }

    #[test]
    fn reset_sequence_returns_aa_and_version() {
        let mut sb = SoundBlaster::new();
        sb.port_out(BASE + 6, 1);
        sb.port_out(BASE + 6, 0);
        assert_eq!(sb.port_in(BASE + 0x0E) & 0x80, 0x80);
        assert_eq!(sb.port_in(BASE + 0x0A), 0xAA);
        write(&mut sb, 0xE1);
        assert_eq!(sb.port_in(BASE + 0x0A), 2);
        assert_eq!(sb.port_in(BASE + 0x0A), 1);
    }

    #[test]
    fn single_cycle_dma_plays_and_raises_the_irq() {
        let mut sb = SoundBlaster::new();
        let mut dma = Dma::new();
        let mut mem = Memory::new();
        for index in 0..16 {
            mem.write8(0x2_0000 + index, 200);
        }
        setup_dma(&mut dma, 16, false);
        write(&mut sb, 0xD1);
        write(&mut sb, 0x40);
        write(&mut sb, 256u32.wrapping_sub(1_000_000 / 11_025) as u8);
        write(&mut sb, 0x14);
        write(&mut sb, 15);
        write(&mut sb, 0);
        assert!(sb.active);
        let mut out = alloc::vec![0i16; 2 * 2048];
        sb.generate(&mut dma, &mem, 2048, 48_000, Some(&mut out));
        assert!(sb.take_irq());
        assert!(!sb.active);
        assert!(out.iter().any(|sample| *sample > 0));
        assert_eq!(dma.channels[1].address, 16);
    }

    #[test]
    fn auto_init_keeps_playing_until_told_to_exit() {
        let mut sb = SoundBlaster::new();
        let mut dma = Dma::new();
        let mem = Memory::new();
        setup_dma(&mut dma, 8, true);
        write(&mut sb, 0x48);
        write(&mut sb, 7);
        write(&mut sb, 0);
        write(&mut sb, 0x1C);
        sb.generate(&mut dma, &mem, 1024, 48_000, None);
        assert!(sb.take_irq());
        assert!(sb.active);
        write(&mut sb, 0xDA);
        sb.generate(&mut dma, &mem, 1024, 48_000, None);
        assert!(sb.take_irq());
        assert!(!sb.active);
    }
}
