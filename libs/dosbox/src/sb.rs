//! Sound Blaster digital audio: the SB 2.0 (DSP 2.01), SB Pro 2 (DSP 3.02, stereo,
//! CT1345 mixer) and SB16 (DSP 4.05, 16-bit DMA, CT1745 mixer).
//!
//! The card sits at `220h`, `IRQ 7`, DMA 1 and, on the SB16, DMA 5, which is what
//! the `BLASTER` variable in the root environment says. FM goes to the OPL. An
//! SB16 driver can move the IRQ and DMA channels through mixer registers `80h` and
//! `81h`, so the machine asks this module where they are.
//!
//! Behaviours taken from `rust-dos`'s `crates/soundblaster` (GPL-2.0-or-later):
//! the `E2h` DMA identification table, the mixer's register shadowing between the
//! SB Pro and SB16 volumes, and what registers the older cards don't have read as.
//! See `LICENSING.md`.

use alloc::collections::VecDeque;

use crate::hw::Dma;
use crate::memory::Memory;

/// The port block, relative to which registers are decoded.
pub const BASE: u16 = 0x220;
/// The IRQ line after a reset.
pub const IRQ: u8 = 7;
const DMA8_CHANNEL: u8 = 1;
const DMA16_CHANNEL: u8 = 5;

/// Which card is emulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    /// Sound Blaster 2.0: 8-bit mono, no mixer.
    Sb2,
    /// Sound Blaster Pro 2: 8-bit stereo, CT1345 mixer.
    SbPro,
    /// Sound Blaster 16: 8- and 16-bit stereo, CT1745 mixer.
    Sb16,
}

impl Model {
    /// The major and minor DSP version.
    pub fn dsp_version(self) -> (u8, u8) {
        match self {
            Model::Sb2 => (2, 1),
            Model::SbPro => (3, 2),
            Model::Sb16 => (4, 5),
        }
    }

    /// The `BLASTER` variable describing this card.
    pub fn blaster(self) -> &'static str {
        match self {
            Model::Sb2 => "A220 I7 D1 T3",
            Model::SbPro => "A220 I7 D1 T4",
            Model::Sb16 => "A220 I7 D1 H5 T6",
        }
    }

    /// The name `SBTYPE` shows.
    pub fn name(self) -> &'static str {
        match self {
            Model::Sb2 => "Sound Blaster 2.0",
            Model::SbPro => "Sound Blaster Pro",
            Model::Sb16 => "Sound Blaster 16",
        }
    }
}

/// A running DMA transfer.
#[derive(Clone, Copy, Debug)]
struct Transfer {
    bits16: bool,
    stereo: bool,
    signed: bool,
    auto_init: bool,
    /// Recording: DMA advances and nothing plays. The microphone is silent.
    input: bool,
    /// DMA units per block: bytes, or words for 16-bit.
    block: u32,
    /// Units left in this block.
    remaining: u32,
    /// DMA units per second.
    rate: u32,
    paused: bool,
    /// Stop when this block ends.
    last_block: bool,
    /// Started by `90h`/`91h` on an SB 2.0 or Pro: no commands until a reset.
    high_speed: bool,
}

/// What each parameter bit of `E2h` adds to its running value, for the four calls
/// in turn, and what each call adds besides, in the last column.
const E2_ADD: [[i16; 9]; 4] = [
    [0x01, -0x02, -0x04, 0x08, -0x10, 0x20, 0x40, -0x80, -106],
    [-0x01, 0x02, -0x04, 0x08, 0x10, -0x20, 0x40, -0x80, 165],
    [-0x01, 0x02, 0x04, -0x08, 0x10, -0x20, -0x40, 0x80, -151],
    [0x01, -0x02, 0x04, -0x08, -0x10, 0x20, -0x40, 0x80, 90],
];

const MASTER_L: u8 = 0x30;
const MASTER_R: u8 = 0x31;
const VOICE_L: u8 = 0x32;
const VOICE_R: u8 = 0x33;
const FM_L: u8 = 0x34;
const FM_R: u8 = 0x35;
const CD_L: u8 = 0x36;
const CD_R: u8 = 0x37;
/// The registers an SB Pro's mixer has.
const PRO_REGS: [u8; 9] = [0x00, 0x04, 0x0A, 0x0C, 0x0E, 0x22, 0x26, 0x28, 0x2E];

/// The SB16 registers that an SB Pro volume register reads and writes.
fn pro_volume_regs(reg: u8) -> (u8, u8) {
    match reg {
        0x04 => (VOICE_L, VOICE_R),
        0x22 => (MASTER_L, MASTER_R),
        0x26 => (FM_L, FM_R),
        _ => (CD_L, CD_R),
    }
}

/// DSP and mixer state.
pub struct SoundBlaster {
    pub model: Model,
    reset_high: bool,
    read_queue: VecDeque<u8>,
    command: Option<u8>,
    args: [u8; 3],
    arg_count: usize,
    pub speaker_on: bool,
    /// Units per second from the last time constant.
    tc_rate: u32,
    /// Sample frames per second from `41h`/`42h`, on an SB16.
    sb16_rate: Option<u32>,
    /// The block size from `48h`, in units.
    block_size: u32,
    transfer: Option<Transfer>,
    /// Units of silence left from `80h`.
    silence: Option<u32>,
    /// The DAC's levels, signed: they hold until the next sample.
    level: [i32; 2],
    /// A left sample waiting for its right one.
    pending_left: Option<i32>,
    /// A direct-DAC sample.
    direct: i32,
    /// Fractional position between output samples, in rate units.
    phase: u64,
    mixer_index: u8,
    mixer: [u8; 256],
    test: u8,
    /// The current IRQ and DMA lines; an SB16 driver can move them.
    irq: u8,
    dma8: Option<u8>,
    dma16: Option<u8>,
    /// IRQ requests from 8-bit and 16-bit transfers, until acknowledged.
    irq8: bool,
    irq16: bool,
    /// A request the machine hasn't passed to the PIC yet.
    raise: bool,
    /// Counts write-status reads, to read busy now and then during a transfer.
    status_reads: u8,
    e2_value: u8,
    e2_count: u8,
    e2_pending: Option<u8>,
}

impl Default for SoundBlaster {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundBlaster {
    /// A Sound Blaster 16.
    pub fn new() -> Self {
        Self::with_model(Model::Sb16)
    }

    pub fn with_model(model: Model) -> Self {
        let mut card = Self {
            model,
            reset_high: false,
            read_queue: VecDeque::new(),
            command: None,
            args: [0; 3],
            arg_count: 0,
            speaker_on: model == Model::Sb16,
            tc_rate: 11_025,
            sb16_rate: None,
            block_size: 1,
            transfer: None,
            silence: None,
            level: [0; 2],
            pending_left: None,
            direct: 0,
            phase: 0,
            mixer_index: 0,
            mixer: [0; 256],
            test: 0,
            irq: IRQ,
            dma8: Some(DMA8_CHANNEL),
            dma16: (model == Model::Sb16).then_some(DMA16_CHANNEL),
            irq8: false,
            irq16: false,
            raise: false,
            status_reads: 0,
            e2_value: 0xAA,
            e2_count: 0,
            e2_pending: None,
        };
        card.reset_mixer();
        card
    }

    fn is_sb16(&self) -> bool {
        self.model == Model::Sb16
    }

    fn reset_mixer(&mut self) {
        self.mixer = [0; 256];
        for reg in [MASTER_L, MASTER_R, VOICE_L, VOICE_R, FM_L, FM_R, CD_L, CD_R] {
            self.mixer[reg as usize] = 0xC0;
        }
    }

    fn reset(&mut self) {
        // Reset is how a driver leaves high-speed mode, and the speaker keeps its
        // setting through that.
        if !self.high_speed() {
            self.speaker_on = self.is_sb16();
        }
        self.command = None;
        self.arg_count = 0;
        self.transfer = None;
        self.silence = None;
        self.level = [0; 2];
        self.pending_left = None;
        self.direct = 0;
        self.sb16_rate = None;
        self.irq8 = false;
        self.irq16 = false;
        self.raise = false;
        self.e2_pending = None;
        self.read_queue.clear();
        self.read_queue.push_back(0xAA);
    }

    fn high_speed(&self) -> bool {
        self.transfer.is_some_and(|t| t.high_speed)
    }

    /// Whether the DSP is moving data, or counting silence.
    pub fn running(&self) -> bool {
        self.transfer.is_some() || self.silence.is_some()
    }

    /// The IRQ line in use.
    pub fn irq_line(&self) -> u8 {
        self.irq
    }

    /// The units per second of the current transfer, if there is one.
    pub fn transfer_rate(&self) -> Option<u32> {
        self.transfer.map(|t| t.rate)
    }

    fn args_needed(&self, command: u8) -> usize {
        let sb16 = self.is_sb16();
        match command {
            0x04 | 0x08 | 0x0F | 0xF9 if sb16 => 1,
            0x05 | 0x0E | 0xFA if sb16 => 2,
            0x10 | 0x38 | 0x40 | 0xE0 | 0xE2 | 0xE4 => 1,
            0x14 | 0x16 | 0x17 | 0x24 | 0x48 | 0x74..=0x77 | 0x80 => 2,
            0x41 | 0x42 if sb16 => 2,
            0xB0..=0xCF if sb16 => 3,
            _ => 0,
        }
    }

    fn arg16(&self) -> u32 {
        self.args[0] as u32 | ((self.args[1] as u32) << 8)
    }

    /// An SB Pro's stereo switch: mixer `0Eh` bit 1.
    fn pro_stereo(&self) -> bool {
        self.model != Model::Sb2 && self.mixer[0x0E] & 0x02 != 0
    }

    /// Starts a transfer, at the time constant's rate unless `41h` set another.
    fn start(&mut self, bits16: bool, stereo: bool, signed: bool, auto_init: bool, input: bool, units: u32) {
        let rate = match self.sb16_rate {
            Some(rate) if self.is_sb16() => rate * if stereo { 2 } else { 1 },
            _ => self.tc_rate,
        };
        self.pending_left = None;
        self.silence = None;
        self.transfer = Some(Transfer {
            bits16,
            stereo,
            signed,
            auto_init,
            input,
            block: units.max(1),
            remaining: units.max(1),
            rate: rate.max(1),
            paused: false,
            last_block: false,
            high_speed: false,
        });
    }

    fn execute(&mut self, command: u8) {
        let stereo8 = self.pro_stereo();
        let sb16 = self.is_sb16();
        match command {
            0x10 => {
                self.direct = (self.args[0] as i32 - 128) * 256;
            }
            // 16h and 17h are ADPCM, played as plain 8-bit data.
            0x14 | 0x16 | 0x17 => {
                let length = self.arg16() + 1;
                self.start(false, stereo8, false, false, false, length);
            }
            0x1C | 0x1F => self.start(false, stereo8, false, true, false, self.block_size),
            0x20 => self.read_queue.push_back(0x80),
            0x24 => {
                let length = self.arg16() + 1;
                self.start(false, false, false, false, true, length);
            }
            0x2C => self.start(false, false, false, true, true, self.block_size),
            0x40 => {
                let divisor = 256u32 - self.args[0] as u32;
                self.tc_rate = (1_000_000 / divisor.max(1)).max(1);
                self.sb16_rate = None;
            }
            0x41 | 0x42 if sb16 => {
                self.sb16_rate = Some(((self.args[0] as u32) << 8) | self.args[1] as u32);
            }
            0x45 | 0x47 if sb16 => {
                if let Some(transfer) = &mut self.transfer {
                    if transfer.auto_init && transfer.bits16 == (command == 0x47) {
                        transfer.last_block = false;
                    }
                }
            }
            0x48 => self.block_size = self.arg16() + 1,
            0x74..=0x77 => {
                let length = self.arg16() + 1;
                self.start(false, false, false, false, false, length);
            }
            0x7D | 0x7F => self.start(false, false, false, true, false, self.block_size),
            0x80 => {
                self.transfer = None;
                self.silence = Some(self.arg16() + 1);
            }
            0x90 | 0x91 | 0x98 | 0x99 => {
                let input = command >= 0x98;
                self.start(false, stereo8 && !input, false, command & 1 == 0, input, self.block_size);
                if !sb16 {
                    if let Some(transfer) = &mut self.transfer {
                        transfer.high_speed = true;
                    }
                }
            }
            // Bit 3 input, bit 2 auto-init; the mode byte has bit 4 signed, bit 5 stereo.
            0xB0..=0xCF if sb16 => {
                let mode = self.args[0];
                let length = (self.args[1] as u32 | ((self.args[2] as u32) << 8)) + 1;
                self.start(
                    command < 0xC0,
                    mode & 0x20 != 0,
                    mode & 0x10 != 0,
                    command & 0x04 != 0,
                    command & 0x08 != 0,
                    length,
                );
            }
            0xD0 | 0xD5 => {
                if let Some(transfer) = &mut self.transfer {
                    transfer.paused = true;
                }
            }
            0xD4 | 0xD6 => {
                if let Some(transfer) = &mut self.transfer {
                    transfer.paused = false;
                }
            }
            0xD1 => self.speaker_on = true,
            0xD3 => self.speaker_on = false,
            0xD8 => self
                .read_queue
                .push_back(if self.speaker_on { 0xFF } else { 0 }),
            0xD9 | 0xDA => {
                if let Some(transfer) = &mut self.transfer {
                    transfer.last_block = true;
                }
            }
            0xE0 => self.read_queue.push_back(!self.args[0]),
            0xE1 => {
                let (major, minor) = self.model.dsp_version();
                self.read_queue.push_back(major);
                self.read_queue.push_back(minor);
            }
            0xE2 => {
                let add = &E2_ADD[(self.e2_count % 4) as usize];
                let mut value = self.e2_value as i16;
                for (bit, step) in add[..8].iter().enumerate() {
                    if (self.args[0] >> bit) & 1 != 0 {
                        value += step;
                    }
                }
                self.e2_value = (value + add[8]) as u8;
                self.e2_count = self.e2_count.wrapping_add(1);
                self.e2_pending = Some(self.e2_value);
            }
            0xE3 => self
                .read_queue
                .extend(b"COPYRIGHT (C) CREATIVE TECHNOLOGY LTD, 1992.\0"),
            0xE4 => self.test = self.args[0],
            0xE8 => self.read_queue.push_back(self.test),
            0xF2 => {
                self.irq8 = true;
                self.raise = true;
            }
            0xF3 if sb16 => {
                self.irq16 = true;
                self.raise = true;
            }
            0xF8 => self.read_queue.push_back(0),
            // Drivers probe the SB16's empty ASP socket.
            0x08 if sb16 && self.args[0] == 0x03 => self.read_queue.push_back(0xFF),
            0x0F if sb16 => self.read_queue.push_back(0),
            0xF9 if sb16 => self.read_queue.push_back(0),
            0x04 if !sb16 => self.read_queue.push_back(if self.model == Model::Sb2 {
                0x88
            } else {
                0x7B
            }),
            _ => {}
        }
    }

    fn mixer_write(&mut self, value: u8) {
        let reg = self.mixer_index;
        if self.model == Model::Sb2 || (!self.is_sb16() && !PRO_REGS.contains(&reg)) {
            return;
        }
        match reg {
            0x00 => self.reset_mixer(),
            // SB Pro volumes: a nibble per side, kept in the SB16's registers.
            0x04 | 0x22 | 0x26 | 0x28 => {
                let (left, right) = pro_volume_regs(reg);
                self.mixer[left as usize] = (value & 0xF0) | 0x08;
                self.mixer[right as usize] = (value << 4) | 0x08;
            }
            0x80 => {
                // The bus's IRQ 2 is the AT's IRQ 9.
                if let Some(irq) = [(0x01, 9), (0x02, 5), (0x04, 7), (0x08, 10)]
                    .into_iter()
                    .find(|&(bit, _)| value & bit != 0)
                    .map(|(_, irq)| irq)
                {
                    self.irq = irq;
                }
            }
            0x81 => {
                self.dma8 = [(0x01, 0), (0x02, 1), (0x08, 3)]
                    .into_iter()
                    .find(|&(bit, _)| value & bit != 0)
                    .map(|(_, channel)| channel);
                self.dma16 = [(0x20, 5), (0x40, 6), (0x80, 7)]
                    .into_iter()
                    .find(|&(bit, _)| value & bit != 0)
                    .map(|(_, channel)| channel);
            }
            0x82 => {}
            _ => self.mixer[reg as usize] = value,
        }
    }

    fn mixer_read(&self) -> u8 {
        let reg = self.mixer_index;
        let sb16 = self.is_sb16();
        match reg {
            _ if self.model == Model::Sb2 => 0xFF,
            0x00 => 0,
            _ if !sb16 && !PRO_REGS.contains(&reg) => 0x0A,
            0x80 if sb16 => match self.irq {
                2 | 9 => 1,
                5 => 2,
                7 => 4,
                10 => 8,
                _ => 0,
            },
            0x81 if sb16 => {
                let low = match self.dma8 {
                    Some(0) => 1,
                    Some(1) => 2,
                    Some(3) => 8,
                    _ => 0,
                };
                let high = match self.dma16 {
                    Some(5) => 0x20,
                    Some(6) => 0x40,
                    Some(7) => 0x80,
                    _ => 0,
                };
                low | high
            }
            0x82 if sb16 => u8::from(self.irq8) | (u8::from(self.irq16) << 1) | 0x20,
            0x30..=0x47 => self.mixer[reg as usize],
            0x04 | 0x22 | 0x26 | 0x28 => {
                let (left, right) = pro_volume_regs(reg);
                (self.mixer[left as usize] & 0xF0) | (self.mixer[right as usize] >> 4)
            }
            _ if !PRO_REGS.contains(&reg) => 0x0A,
            _ => self.mixer[reg as usize],
        }
    }

    /// The gains out of 256 for a pair of SB16 volume registers, with the master
    /// volume applied.
    fn gains(&self, left: u8, right: u8) -> [i32; 2] {
        if self.model == Model::Sb2 {
            return [256, 256];
        }
        let level = |reg: u8| (self.mixer[reg as usize] >> 3) as i32;
        // Five-bit levels, so 31 * 31 is full scale.
        let scale = |reg: u8, master: u8| level(reg) * level(master) * 256 / (31 * 31);
        [scale(left, MASTER_L), scale(right, MASTER_R)]
    }

    /// Scales FM through the mixer. The levels after a mixer reset leave it where
    /// a card's power-on volume does.
    pub fn apply_fm_volume(&self, samples: &mut [i16]) {
        let gains = self.gains(FM_L, FM_R);
        if gains == [256, 256] {
            return;
        }
        for pair in samples.as_chunks_mut::<2>().0 {
            pair[0] = ((pair[0] as i32 * gains[0]) >> 8) as i16;
            pair[1] = ((pair[1] as i32 * gains[1]) >> 8) as i16;
        }
    }

    /// A write in the DSP's port block.
    pub fn port_out(&mut self, port: u16, value: u8) {
        match port - BASE {
            0x04 => self.mixer_index = value,
            0x05 => self.mixer_write(value),
            0x06 => {
                let high = value & 1 != 0;
                if self.reset_high && !high {
                    self.reset();
                }
                self.reset_high = high;
            }
            0x0C => {
                if self.high_speed() {
                    return;
                }
                if let Some(command) = self.command {
                    if self.arg_count < self.args.len() {
                        self.args[self.arg_count] = value;
                        self.arg_count += 1;
                    }
                    if self.arg_count >= self.args_needed(command) {
                        self.command = None;
                        self.execute(command);
                    }
                } else {
                    self.arg_count = 0;
                    if self.args_needed(value) == 0 {
                        self.execute(value);
                    } else {
                        self.command = Some(value);
                    }
                }
            }
            _ => {}
        }
    }

    /// A read in the DSP's port block. Reading `2xEh` or `2xFh` acknowledges an
    /// IRQ; [`Self::irq_cleared`] says whether the line went low.
    pub fn port_in(&mut self, port: u16) -> u8 {
        match port - BASE {
            0x05 => self.mixer_read(),
            0x0A => self.read_queue.pop_front().unwrap_or(0xFF),
            // Busy now and then while a transfer runs: Miles's drivers poll for
            // the bit to tell the DSP has seen a block.
            0x0C => {
                if self.high_speed() {
                    return 0xFF;
                }
                self.status_reads = self.status_reads.wrapping_add(1);
                let running = self.transfer.is_some_and(|t| !t.paused);
                if running && self.status_reads & 7 == 7 {
                    0xFF
                } else {
                    0x7F
                }
            }
            0x0E => {
                self.irq8 = false;
                if self.read_queue.is_empty() {
                    0x7F
                } else {
                    0xFF
                }
            }
            0x0F if self.is_sb16() => {
                self.irq16 = false;
                0xFF
            }
            _ => 0xFF,
        }
    }

    /// Whether no IRQ is being requested any more.
    pub fn irq_cleared(&self) -> bool {
        !self.irq8 && !self.irq16
    }

    /// Whether the DSP wants to raise its IRQ. Taking this clears the request.
    pub fn take_irq(&mut self) -> bool {
        core::mem::take(&mut self.raise)
    }

    /// The microseconds until the current block ends, if a transfer is running.
    pub fn next_block_us(&self) -> Option<u64> {
        let (remaining, rate) = match self.transfer {
            Some(transfer) if transfer.paused => return None,
            Some(transfer) => (transfer.remaining, transfer.rate),
            None => (self.silence?, self.tc_rate),
        };
        Some((remaining as u64 * 1_000_000 / rate.max(1) as u64) + 1)
    }

    /// `E2h`'s answer goes to memory over the 8-bit channel, once that's unmasked.
    pub fn service_dma_write(&mut self, dma: &mut Dma, mem: &mut Memory) {
        let Some(value) = self.e2_pending else {
            return;
        };
        let Some(channel) = self.dma8 else {
            return;
        };
        if !dma.ready(channel as usize) {
            return;
        }
        self.e2_pending = None;
        // Only a channel set to write to memory stores it.
        let writes = dma.channels[channel as usize].mode & 0x0C == 0x04;
        let (address, _) = dma.step(channel as usize);
        if writes {
            mem.write8(address, value);
        }
    }

    /// Moves one unit. Returns false if the channel can't transfer yet.
    fn next_unit(&mut self, dma: &mut Dma, mem: &Memory) -> bool {
        let Some(mut transfer) = self.transfer else {
            if let Some(left) = self.silence {
                self.level = [0; 2];
                if left <= 1 {
                    self.silence = None;
                    self.irq8 = true;
                    self.raise = true;
                } else {
                    self.silence = Some(left - 1);
                }
                return true;
            }
            return false;
        };
        let channel = if transfer.bits16 && self.is_sb16() {
            self.dma16.or(self.dma8)
        } else {
            self.dma8
        };
        let Some(channel) = channel.map(usize::from).filter(|&c| dma.ready(c)) else {
            // The transfer waits for the guest to unmask the channel.
            return false;
        };
        let (address, _) = dma.step(channel);
        if !transfer.input {
            let sample = if channel >= 4 {
                let value = mem.read8(address) as u16 | ((mem.read8(address + 1) as u16) << 8);
                if transfer.bits16 {
                    if transfer.signed {
                        value as i16 as i32
                    } else {
                        value as i32 - 0x8000
                    }
                } else {
                    (value as u8 as i32 - 128) * 256
                }
            } else {
                let byte = mem.read8(address);
                
                if transfer.bits16 {
                    // 16-bit data over the 8-bit channel: the high byte follows.
                    let (high, _) = dma.step(channel);
                    let value = byte as u16 | ((mem.read8(high) as u16) << 8);
                    if transfer.signed {
                        value as i16 as i32
                    } else {
                        value as i32 - 0x8000
                    }
                } else if transfer.signed {
                    byte as i8 as i32 * 256
                } else {
                    (byte as i32 - 128) * 256
                }
            };
            if transfer.stereo {
                match self.pending_left.take() {
                    None => self.pending_left = Some(sample),
                    Some(left) => self.level = [left, sample],
                }
            } else {
                self.level = [sample, sample];
            }
        }
        transfer.remaining = transfer.remaining.saturating_sub(1);
        self.transfer = Some(transfer);
        if transfer.remaining == 0 {
            if transfer.bits16 {
                self.irq16 = true;
            } else {
                self.irq8 = true;
            }
            self.raise = true;
            if transfer.auto_init && !transfer.last_block {
                transfer.remaining = transfer.block;
                self.transfer = Some(transfer);
            } else {
                self.transfer = None;
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
        let gains = self.gains(VOICE_L, VOICE_R);
        for frame in 0..frames {
            let rate = match self.transfer {
                Some(transfer) if !transfer.paused => transfer.rate,
                Some(_) => 0,
                None if self.silence.is_some() => self.tc_rate,
                None => 0,
            };
            if rate != 0 {
                self.phase += rate as u64;
                while self.phase >= output_rate {
                    self.phase -= output_rate;
                    if !self.next_unit(dma, mem) {
                        self.phase = 0;
                        break;
                    }
                    if !self.running() {
                        self.phase = 0;
                        break;
                    }
                }
            }
            if let Some(buffer) = out.as_deref_mut() {
                if !self.speaker_on {
                    continue;
                }
                let left = self.level[0] + self.direct;
                let right = self.level[1] + self.direct;
                if left != 0 || right != 0 {
                    if let Some(cell) = buffer.get_mut(frame * 2..frame * 2 + 2) {
                        let mix = |sample: &mut i16, value: i32, gain: i32| {
                            *sample = (*sample as i32 + ((value * gain) >> 8))
                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        };
                        mix(&mut cell[0], left, gains[0]);
                        mix(&mut cell[1], right, gains[1]);
                    }
                }
            }
        }
    }

    /// Whether playback could produce sound.
    pub fn audible(&self) -> bool {
        self.speaker_on && (self.transfer.is_some() || self.direct != 0)
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

    /// Channel 5 at word address `0x8000` of page 2: physical `3_0000h`.
    fn setup_dma16(dma: &mut Dma, words: u16, auto_init: bool) {
        dma.port_out(0xD8, 0);
        dma.port_out(0xC4, 0x00);
        dma.port_out(0xC4, 0x80);
        dma.port_out(0xC6, (words - 1) as u8);
        dma.port_out(0xC6, ((words - 1) >> 8) as u8);
        dma.port_out(0x8B, 0x02);
        dma.port_out(0xD6, 0x49 | if auto_init { 0x10 } else { 0 });
        dma.port_out(0xD4, 1);
    }

    #[test]
    fn each_model_reports_its_dsp_version() {
        for (model, major, minor) in [
            (Model::Sb2, 2, 1),
            (Model::SbPro, 3, 2),
            (Model::Sb16, 4, 5),
        ] {
            let mut sb = SoundBlaster::with_model(model);
            sb.port_out(BASE + 6, 1);
            sb.port_out(BASE + 6, 0);
            assert_eq!(sb.port_in(BASE + 0x0E) & 0x80, 0x80);
            assert_eq!(sb.port_in(BASE + 0x0A), 0xAA);
            write(&mut sb, 0xE1);
            assert_eq!(sb.port_in(BASE + 0x0A), major);
            assert_eq!(sb.port_in(BASE + 0x0A), minor);
        }
    }

    #[test]
    fn single_cycle_dma_plays_and_raises_the_irq() {
        let mut sb = SoundBlaster::with_model(Model::Sb2);
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
        assert!(sb.running());
        let mut out = alloc::vec![0i16; 2 * 2048];
        sb.generate(&mut dma, &mem, 2048, 48_000, Some(&mut out));
        assert!(sb.take_irq());
        assert!(!sb.running());
        assert!(out.iter().any(|sample| *sample > 0));
        assert_eq!(dma.channels[1].address, 16);
    }

    #[test]
    fn auto_init_keeps_playing_until_told_to_exit() {
        let mut sb = SoundBlaster::with_model(Model::Sb2);
        let mut dma = Dma::new();
        let mem = Memory::new();
        setup_dma(&mut dma, 8, true);
        write(&mut sb, 0x48);
        write(&mut sb, 7);
        write(&mut sb, 0);
        write(&mut sb, 0x1C);
        sb.generate(&mut dma, &mem, 1024, 48_000, None);
        assert!(sb.take_irq());
        assert!(sb.running());
        write(&mut sb, 0xDA);
        sb.generate(&mut dma, &mem, 1024, 48_000, None);
        assert!(sb.take_irq());
        assert!(!sb.running());
    }

    #[test]
    fn pro_stereo_alternates_channels_through_the_mixer_switch() {
        let mut sb = SoundBlaster::with_model(Model::SbPro);
        let mut dma = Dma::new();
        let mut mem = Memory::new();
        for index in 0..64u32 {
            mem.write8(0x2_0000 + index, if index % 2 == 0 { 0xF0 } else { 0x10 });
        }
        setup_dma(&mut dma, 64, false);
        sb.port_out(BASE + 4, 0x0E);
        sb.port_out(BASE + 5, 0x02);
        // Master and voice full, on both sides.
        for reg in [0x22, 0x04] {
            sb.port_out(BASE + 4, reg);
            sb.port_out(BASE + 5, 0xFF);
        }
        write(&mut sb, 0xD1);
        write(&mut sb, 0x40);
        write(&mut sb, 256u32.wrapping_sub(1_000_000 / 22_050) as u8);
        write(&mut sb, 0x14);
        write(&mut sb, 63);
        write(&mut sb, 0);
        let mut out = alloc::vec![0i16; 2 * 64];
        sb.generate(&mut dma, &mem, 64, 48_000, Some(&mut out));
        assert!(out.chunks(2).any(|pair| pair[0] > 0 && pair[1] < 0), "left high, right low");
    }

    #[test]
    fn pro_mixer_scales_voice_per_side_and_reads_back() {
        let mut sb = SoundBlaster::with_model(Model::SbPro);
        sb.port_out(BASE + 4, 0x22);
        sb.port_out(BASE + 5, 0xFF);
        sb.port_out(BASE + 4, 0x04);
        sb.port_out(BASE + 5, 0xF0);
        assert_eq!(sb.port_in(BASE + 5), 0xF0);
        sb.direct = 0x4000;
        sb.speaker_on = true;
        let mut dma = Dma::new();
        let mem = Memory::new();
        let mut out = [0i16; 2];
        sb.generate(&mut dma, &mem, 1, 48_000, Some(&mut out));
        assert!(out[0] > 0x3000);
        assert!(out[1] < out[0] / 8, "right voice is low: {out:?}");
        // Registers the card doesn't have.
        sb.port_out(BASE + 4, 0x80);
        assert_eq!(sb.port_in(BASE + 5), 0x0A);
    }

    #[test]
    fn sb16_plays_signed_16_bit_stereo_on_dma_5() {
        let mut sb = SoundBlaster::new();
        assert!(sb.speaker_on, "an SB16's speaker is always on");
        let mut dma = Dma::new();
        let mut mem = Memory::new();
        for frame in 0..32u32 {
            mem.write16(0x3_0000 + frame * 4, 0x4000);
            mem.write16(0x3_0000 + frame * 4 + 2, 0xC000);
        }
        setup_dma16(&mut dma, 64, false);
        // 44.1 kHz.
        write(&mut sb, 0x41);
        write(&mut sb, 0xAC);
        write(&mut sb, 0x44);
        // 16-bit single-cycle output, signed stereo, 64 samples.
        write(&mut sb, 0xB0);
        write(&mut sb, 0x30);
        write(&mut sb, 63);
        write(&mut sb, 0);
        assert_eq!(sb.transfer_rate(), Some(88_200));
        let mut out = alloc::vec![0i16; 2 * 64];
        sb.generate(&mut dma, &mem, 64, 48_000, Some(&mut out));
        assert!(sb.take_irq());
        assert!(!sb.running());
        let peak = out.chunks(2).find(|pair| pair[0] != 0).expect("sound");
        assert!(peak[0] > 0 && peak[1] < 0, "{peak:?}");
        assert_eq!(dma.channels[5].address, 0x8000 + 64);
        // The 16-bit interrupt shows in the mixer and is acknowledged at 2xFh.
        sb.port_out(BASE + 4, 0x82);
        assert_eq!(sb.port_in(BASE + 5) & 3, 2);
        sb.port_in(BASE + 0x0F);
        assert_eq!(sb.port_in(BASE + 5) & 3, 0);
    }

    #[test]
    fn sb16_auto_init_stops_with_d9_and_channels_follow_the_mixer() {
        let mut sb = SoundBlaster::new();
        let mut dma = Dma::new();
        let mem = Memory::new();
        setup_dma16(&mut dma, 8, true);
        write(&mut sb, 0x41);
        write(&mut sb, 0x5D);
        write(&mut sb, 0xC0);
        write(&mut sb, 0xB4);
        write(&mut sb, 0x10);
        write(&mut sb, 7);
        write(&mut sb, 0);
        sb.generate(&mut dma, &mem, 256, 48_000, None);
        assert!(sb.take_irq());
        assert!(sb.running());
        write(&mut sb, 0xD9);
        sb.generate(&mut dma, &mem, 256, 48_000, None);
        assert!(!sb.running());

        sb.port_out(BASE + 4, 0x80);
        assert_eq!(sb.port_in(BASE + 5), 4);
        sb.port_out(BASE + 5, 2);
        assert_eq!(sb.irq_line(), 5);
        sb.port_out(BASE + 4, 0x81);
        assert_eq!(sb.port_in(BASE + 5), 0x22);
    }

    #[test]
    fn a_masked_channel_holds_the_transfer_back() {
        let mut sb = SoundBlaster::new();
        let mut dma = Dma::new();
        let mem = Memory::new();
        write(&mut sb, 0x41);
        write(&mut sb, 0xAC);
        write(&mut sb, 0x44);
        write(&mut sb, 0xB0);
        write(&mut sb, 0x10);
        write(&mut sb, 7);
        write(&mut sb, 0);
        sb.generate(&mut dma, &mem, 256, 48_000, None);
        assert!(!sb.take_irq(), "no IRQ while DMA 5 is masked");
        setup_dma16(&mut dma, 8, false);
        sb.generate(&mut dma, &mem, 256, 48_000, None);
        assert!(sb.take_irq());
    }

    #[test]
    fn high_speed_mode_ignores_commands_until_a_reset() {
        let mut sb = SoundBlaster::with_model(Model::Sb2);
        let mut dma = Dma::new();
        let mem = Memory::new();
        setup_dma(&mut dma, 8, true);
        write(&mut sb, 0x48);
        write(&mut sb, 7);
        write(&mut sb, 0);
        write(&mut sb, 0x90);
        assert_eq!(sb.port_in(BASE + 0x0C), 0xFF);
        write(&mut sb, 0xD3);
        sb.generate(&mut dma, &mem, 16, 48_000, None);
        assert!(sb.running());
        sb.port_out(BASE + 6, 1);
        sb.port_out(BASE + 6, 0);
        assert!(!sb.running());
        assert_eq!(sb.port_in(BASE + 0x0A), 0xAA);
    }

    #[test]
    fn e2_answers_over_the_dma_channel() {
        let mut sb = SoundBlaster::new();
        let mut dma = Dma::new();
        let mut mem = Memory::new();
        setup_dma(&mut dma, 1, false);
        dma.port_out(0x0B, 0x45);
        write(&mut sb, 0xE2);
        write(&mut sb, 0xA5);
        sb.service_dma_write(&mut dma, &mut mem);
        assert_ne!(mem.read8(0x2_0000), 0);
    }
}
