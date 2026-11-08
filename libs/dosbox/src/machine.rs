//! The FEOS-DOS machine: CPU, bus, devices, prompt and program loader.
//!
//! Guest time comes from the instructions the CPU retires: [`CYCLES_PER_MS`]
//! instruction units make one guest millisecond, and every timer, the PIT, the
//! OPL's timers, DMA and the retrace bits, reads the same clock. Idle guests skip
//! ahead to their next timer event. [`DosBox::frame`] runs a given budget and
//! [`DosBox::frame_paced`] works the budget out from elapsed real time.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::bios::{
    Bios, CALLBACK_MOUSE_DONE, CALLBACK_MOUSE_ENTER, CALLBACK_XMS, MOUSE_HANDLER_PTR, ROM_SEGMENT,
    XMS_ENTRY_OFFSET,
};
use crate::cpu::{self, Bus, Cpu, CpuRegs, InterruptAction, Step};
use crate::dos::{Dos, COMMAND_PSP};
use crate::dpmi::Dpmi;
use crate::emm386::Emm386;
use crate::hw::{Dma, Pics, Pit};
use crate::keyboard::Keyboard;
use crate::memory::{Memory, BDA_BASE, MONO_BASE, VIDEO_BASE};
use crate::mouse::Mouse;
use crate::sb::{self, SoundBlaster};
use crate::shell::{CommandResult, Shell};
use crate::vga::{Vga, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::xms::{Ems, Xms, EMS_FRAME};

/// The default speed: about a 386DX at 25 MHz.
pub const CYCLES_PER_MS: u32 = 3000;
/// The 8254's input clock.
const PIT_HZ: u64 = 1_193_182;
/// A paced frame never runs more guest time than this, so the desktop stays
/// responsive when the host can't keep up: the guest slows down instead.
const MAX_PACED_MS: u64 = 40;
/// The audio waiting to be taken is capped at half a second of stereo at 48 kHz.
const MAX_PENDING_SAMPLES: usize = 48_000;
/// The length of the BEL beep, in milliseconds.
const BELL_MS: usize = 50;
const BELL_HZ: u64 = 880;

/// An analogue joystick on the game port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Joystick {
    /// X and Y of up to two sticks, as a timing in microseconds (typically
    /// 10 to 1000); `None` means the axis isn't connected.
    pub axes: [Option<u16>; 4],
    /// Button state, with bit 0 being button A of stick one.
    pub buttons: u8,
}

/// Everything the CPU sees through the bus.
pub struct SystemBus {
    pub mem: Memory,
    pub vga: Vga,
    pub keyboard: Keyboard,
    pub mouse: Mouse,
    pub opl: Box<crate::opl::Opl>,
    pub bios: Bios,
    pub dos: Dos,
    pub pics: Pics,
    pub pit: Pit,
    pub dma: Dma,
    pub sb: SoundBlaster,
    pub xms: Xms,
    pub ems: Ems,
    pub dpmi: Dpmi,
    pub emm: Emm386,
    pub joystick: Option<Joystick>,
    /// When the game port's axis timers fired, in microseconds of guest time.
    joystick_fired: u64,
    pub exit_requested: bool,
    pub exit_code: u8,
    pub now_ms: u64,
    /// The instruction units retired, as of the last sync.
    cycles: u64,
    /// The CPU's speed.
    cycles_per_ms: u32,
    /// The time base for `cycles` when the speed last changed.
    anchor_cycles: u64,
    anchor_ticks: u64,
    anchor_us: u64,
    /// The service cost not yet reported to the CPU.
    cost: u32,
    port61: u8,

    audio_enabled: bool,
    audio_rate: u32,
    /// The microsecond the audio has been generated up to.
    audio_us: u64,
    /// Frames owed from rounding.
    audio_remainder: u64,
    /// Generated interleaved stereo waiting to be taken.
    pending_audio: Vec<i16>,
    speaker_phase: u64,
    speaker_level: bool,
    bell_phase: u64,
    bell_frames: usize,
    last_bell: u32,
    opl_us: u64,
}

impl SystemBus {
    pub fn new(fs: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        let mut dos = Dos::new();
        dos.seed(fs);
        let mut bus = Self {
            mem: Memory::new(),
            vga: Vga::new(),
            keyboard: Keyboard::new(),
            mouse: Mouse::new(),
            opl: Box::new(crate::opl::Opl::new(48_000)),
            bios: Bios::new(),
            dos,
            pics: Pics::new(),
            pit: Pit::new(),
            dma: Dma::new(),
            sb: SoundBlaster::new(),
            xms: Xms::new(),
            ems: Ems::new(),
            dpmi: Dpmi::new(),
            emm: Emm386::new(),
            joystick: None,
            joystick_fired: 0,
            exit_requested: false,
            exit_code: 0,
            now_ms: 0,
            cycles: 0,
            cycles_per_ms: CYCLES_PER_MS,
            anchor_cycles: 0,
            anchor_ticks: 0,
            anchor_us: 0,
            cost: 0,
            port61: 0,
            audio_enabled: false,
            audio_rate: 48_000,
            audio_us: 0,
            audio_remainder: 0,
            pending_audio: Vec::new(),
            speaker_phase: 0,
            speaker_level: false,
            bell_phase: 0,
            bell_frames: 0,
            last_bell: 0,
            opl_us: 0,
        };
        bus.vga.reset(&mut bus.mem);
        bus.bios.install(&mut bus.mem);
        bus.vga.install_rom(&mut bus.mem);
        bus.dos.install(&mut bus.mem);
        Dpmi::install_rom(&mut bus.mem);
        bus
    }

    /// The current guest time in 8254 ticks.
    pub fn ticks(&self) -> u64 {
        let elapsed = self.cycles.saturating_sub(self.anchor_cycles) as u128;
        self.anchor_ticks + (elapsed * PIT_HZ as u128 / (self.cycles_per_ms as u128 * 1000)) as u64
    }

    /// The current guest time in microseconds.
    pub fn time_us(&self) -> u64 {
        let elapsed = self.cycles.saturating_sub(self.anchor_cycles) as u128;
        self.anchor_us + (elapsed * 1000 / self.cycles_per_ms as u128) as u64
    }

    /// The instruction count at which a tick is reached.
    fn cycle_for_tick(&self, tick: u64) -> u64 {
        let delta = tick.saturating_sub(self.anchor_ticks) as u128;
        self.anchor_cycles
            + (delta * self.cycles_per_ms as u128 * 1000).div_ceil(PIT_HZ as u128) as u64
    }

    fn set_speed(&mut self, cycles_per_ms: u32) {
        self.anchor_ticks = self.ticks();
        self.anchor_us = self.time_us();
        self.anchor_cycles = self.cycles;
        self.cycles_per_ms = cycles_per_ms.clamp(100, 1_000_000);
    }

    /// Brings the OPL's timers up to the present.
    fn sync_opl(&mut self) {
        let now = self.time_us();
        if now > self.opl_us {
            self.opl.advance(now - self.opl_us);
            self.opl_us = now;
        }
    }

    /// Runs the devices whose work depends on time passing.
    fn sync_devices(&mut self) {
        let tick = self.ticks();
        if self.pit.advance(tick) {
            self.pics.raise(0);
        }
        if self.keyboard.pump(tick) {
            self.pics.raise(1);
        }
        if self.mouse.handler_due() {
            self.pics.raise(self.mouse.irq);
        }
        if self.sb.running() {
            self.generate_audio();
        }
        self.sb.service_dma_write(&mut self.dma, &mut self.mem);
        if self.sb.take_irq() {
            self.pics.raise(self.sb.irq_line());
        }
    }

    /// The instruction count of the next timed event.
    fn next_event_cycle(&self) -> u64 {
        let mut next = u64::MAX;
        if let Some(tick) = self.pit.next_irq {
            next = next.min(self.cycle_for_tick(tick));
        }
        if let Some(tick) = self.keyboard.next_tick() {
            next = next.min(self.cycle_for_tick(tick));
        }
        if let Some(us) = self.sb.next_block_us() {
            let delta = us as u128 * self.cycles_per_ms as u128 / 1000;
            next = next.min(self.cycles + delta as u64);
        }
        if let Some(until) = self.bios.wait_until_us {
            let tick = (until as u128 * PIT_HZ as u128 / 1_000_000) as u64;
            next = next.min(self.cycle_for_tick(tick));
        }
        next
    }

    /// Generates audio from the last position up to the current guest time.
    fn generate_audio(&mut self) {
        let now = self.time_us();
        if now <= self.audio_us {
            return;
        }
        let delta = now - self.audio_us;
        self.audio_us = now;
        let rate = self.audio_rate as u64;
        let total = delta * rate + self.audio_remainder;
        let frames = (total / 1_000_000) as usize;
        self.audio_remainder = total % 1_000_000;
        if frames == 0 {
            return;
        }
        if !self.audio_enabled {
            // No sound to make, but a DMA transfer still takes its time.
            self.sb
                .generate(&mut self.dma, &self.mem, frames, self.audio_rate, None);
            return;
        }
        // A long gap is silence as far as real time is concerned.
        let frames = frames.min(self.audio_rate as usize);
        let mut samples = vec![0i16; frames * 2];
        self.opl.generate(&mut samples);
        self.sb.apply_fm_volume(&mut samples);
        self.sb.generate(
            &mut self.dma,
            &self.mem,
            frames,
            self.audio_rate,
            Some(&mut samples),
        );
        self.mix_speaker(&mut samples, frames);
        if samples.iter().all(|sample| *sample == 0) && self.pending_audio.is_empty() {
            return;
        }
        let room = MAX_PENDING_SAMPLES.saturating_sub(samples.len());
        if self.pending_audio.len() > room {
            let excess = self.pending_audio.len() - room;
            self.pending_audio
                .drain(..excess.min(self.pending_audio.len()));
        }
        self.pending_audio.extend_from_slice(&samples);
    }

    /// The PC speaker and the BEL beep, as square waves.
    fn mix_speaker(&mut self, samples: &mut [i16], frames: usize) {
        if self.vga.bell != self.last_bell {
            self.last_bell = self.vga.bell;
            self.bell_frames = self.audio_rate as usize * BELL_MS / 1000;
        }
        let rate = self.audio_rate as u64;
        let tone = match self.port61 & 3 {
            3 => {
                let reload = self.pit.channels[2].reload.max(1) as u64;
                Some(PIT_HZ / reload)
            }
            _ => None,
        };
        // Bit 1 alone drives the cone directly, which is how speakers get
        // bit-banged.
        let direct = self.port61 & 3 == 2;
        for frame in 0..frames {
            let mut value = 0i32;
            if let Some(frequency) = tone {
                // Two half-waves per period.
                self.speaker_phase += 2 * frequency;
                let toggles = self.speaker_phase / rate;
                self.speaker_phase %= rate;
                if toggles % 2 == 1 {
                    self.speaker_level = !self.speaker_level;
                }
                value += if self.speaker_level { 1200 } else { -1200 };
            } else if direct {
                value += 1200;
            }
            if self.bell_frames > 0 {
                self.bell_phase += 2 * BELL_HZ;
                if self.bell_phase >= rate {
                    self.bell_phase -= rate;
                    self.speaker_level = !self.speaker_level;
                }
                value += if self.speaker_level { 3000 } else { -3000 };
                self.bell_frames -= 1;
            }
            if value != 0 {
                for sample in &mut samples[frame * 2..frame * 2 + 2] {
                    *sample =
                        (*sample as i32 + value).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                }
            }
        }
    }

    fn set_a20(&mut self, on: bool) {
        self.mem.set_a20(on);
    }

    #[cold]
    fn read8_high(&mut self, addr: u32) -> u8 {
        if self.vga.is_vram(addr) {
            return self.vga.vram_read(addr);
        }
        if (EMS_FRAME..EMS_FRAME + 0x1_0000).contains(&addr) {
            return match self.ems.translate(addr) {
                Some(mapped) => self.mem.read8(mapped),
                None => 0xFF,
            };
        }
        self.mem.read8(addr)
    }

    #[cold]
    fn write8_high(&mut self, addr: u32, value: u8) {
        if self.vga.is_vram(addr) {
            self.vga.vram_write(addr, value);
            return;
        }
        if addr < 0xC_0000 {
            self.mem.write8(addr, value);
            if (VIDEO_BASE..VIDEO_BASE + 0x8000).contains(&addr)
                || (MONO_BASE..MONO_BASE + 0x8000).contains(&addr)
                || addr < 0xB_0000
            {
                self.vga.mark_dirty();
            }
            return;
        }
        if (EMS_FRAME..EMS_FRAME + 0x1_0000).contains(&addr) {
            if let Some(mapped) = self.ems.translate(addr) {
                self.mem.write8(mapped, value);
            }
        }
        // Everything else up there is ROM or nothing.
    }

    fn joystick_in(&self) -> u8 {
        let Some(stick) = self.joystick else {
            return 0xFF;
        };
        let elapsed = self.time_us().saturating_sub(self.joystick_fired);
        let mut value = (!stick.buttons & 0x0F) << 4;
        for (index, axis) in stick.axes.iter().enumerate() {
            match axis {
                Some(timing) if elapsed < *timing as u64 => value |= 1 << index,
                Some(_) => {}
                None => value |= 1 << index,
            }
        }
        value
    }

    /// `INT 15h`.
    fn int15(&mut self, regs: &mut CpuRegs<'_>) -> InterruptAction {
        match regs.ah() {
            0x4F | 0x90 | 0x91 => {
                regs.set_carry(regs.ah() == 0x4F);
                InterruptAction::Continue
            }
            0x86 => {
                let now = self.time_us();
                let until = *self.bios.wait_until_us.get_or_insert_with(|| {
                    now + (((regs.r[cpu::CX] & 0xFFFF) << 16) | (regs.r[cpu::DX] & 0xFFFF)) as u64
                });
                if now < until {
                    return InterruptAction::Blocked;
                }
                self.bios.wait_until_us = None;
                regs.set_ah(0);
                regs.set_carry(false);
                InterruptAction::Continue
            }
            0x87 => {
                let SystemBus { mem, vga, .. } = self;
                let moved = Xms::bios_move(regs, mem, vga);
                self.cost = self.cost.saturating_add(moved / 8);
                InterruptAction::Continue
            }
            _ => {
                let mut a20 = self.mem.a20();
                self.bios.int15(regs, true, &mut a20);
                self.set_a20(a20);
                InterruptAction::Continue
            }
        }
    }

    /// The `INT 2Fh` multiplex.
    fn int2f(&mut self, regs: &mut CpuRegs<'_>) {
        match regs.ax() {
            // Not running under Windows.
            0x1600 | 0x4680 => regs.set_al(0),
            0x1680 => regs.set_al(0),
            0x1687 => {
                let self_hosting = self.dos.self_hosting == Some(self.dos.psp);
                self.dpmi.installation_check(regs, self_hosting);
            }
            0x4300 => regs.set_al(0x80),
            0x4310 => {
                regs.set_es(ROM_SEGMENT);
                regs.set_bx(XMS_ENTRY_OFFSET);
            }
            _ => {}
        }
    }

    /// Charges guest work a service has done.
    pub(crate) fn charge(&mut self, units: u32) {
        self.cost = self.cost.saturating_add(units);
    }

    /// The default handlers for fatal exceptions.
    fn fatal_message(&mut self, regs: &mut CpuRegs<'_>, text: &str) {
        let frame = Cpu::physical(regs.ss(), regs.sp());
        let ip = self.mem.read16(frame);
        let cs = self.mem.read16(frame.wrapping_add(2));
        let line = format!("{text} at {cs:04X}:{ip:04X}");
        self.vga.write_line(&mut self.mem, line.as_bytes());
    }

    fn terminate_program(
        &mut self,
        regs: &mut CpuRegs<'_>,
        code: u8,
        kind: u8,
        keep: Option<u16>,
    ) -> InterruptAction {
        let SystemBus {
            dos,
            mem,
            exit_requested,
            exit_code,
            ..
        } = self;
        dos.terminate(regs, mem, code, kind, keep, exit_requested, exit_code)
    }

    /// Puts the machine's hardware state back for the prompt after a program:
    /// the interrupt chips and timer, A20 and the expanded memory mapping.
    fn reset_after_program(&mut self) {
        self.pics = Pics::new();
        let tick = self.ticks();
        self.pit = Pit::new();
        self.pit.next_irq = Some(tick + 0x1_0000);
        self.pit.channels[0].start = tick;
        self.dma = Dma::new();
        self.sb = SoundBlaster::with_model(self.sb.model);
        self.xms.reset();
        self.ems.reset();
        self.mem.set_ems_frame(self.ems.frame_map());
        self.dpmi.reset();
        self.emm.reset();
        self.mem.set_a20(false);
        self.port61 = 0;
        self.keyboard.output_full = false;
    }
}

impl Bus for SystemBus {
    #[inline(always)]
    fn read8(&mut self, addr: u32) -> u8 {
        if !(0xA_0000..0x10_0000).contains(&addr) {
            return self.mem.read8(addr);
        }
        self.read8_high(addr)
    }

    #[inline(always)]
    fn write8(&mut self, addr: u32, value: u8) {
        if !(0xA_0000..0x10_0000).contains(&addr) {
            self.mem.write8(addr, value);
            return;
        }
        self.write8_high(addr, value);
    }

    #[inline(always)]
    fn read16(&mut self, addr: u32) -> u16 {
        if !(0x9_FFFF..0x10_0000).contains(&addr) {
            return self.mem.read16(addr);
        }
        self.read8(addr) as u16 | ((self.read8(addr.wrapping_add(1)) as u16) << 8)
    }

    #[inline(always)]
    fn write16(&mut self, addr: u32, value: u16) {
        if !(0x9_FFFF..0x10_0000).contains(&addr) {
            self.mem.write16(addr, value);
            return;
        }
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    #[inline(always)]
    fn read32(&mut self, addr: u32) -> u32 {
        if !(0x9_FFFD..0x10_0000).contains(&addr) {
            return self.mem.read32(addr);
        }
        self.read16(addr) as u32 | ((self.read16(addr.wrapping_add(2)) as u32) << 16)
    }

    #[inline(always)]
    fn write32(&mut self, addr: u32, value: u32) {
        if !(0x9_FFFD..0x10_0000).contains(&addr) {
            self.mem.write32(addr, value);
            return;
        }
        self.write16(addr, value as u16);
        self.write16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    fn sync(&mut self, cycles: u64) {
        self.cycles = cycles;
    }

    fn host_call(&mut self, cpu: &mut Cpu, code: u8) -> InterruptAction {
        self.dpmi_host_call(cpu, code)
    }

    fn after_service(&mut self, cpu: &mut Cpu) -> bool {
        if core::mem::take(&mut self.dos.process_ended) && self.dpmi.active() {
            self.dpmi_check_clients();
        }
        if let Some(at) = self.emm.take_switch() {
            self.vcpi_enter_client(cpu, at);
            return true;
        }
        false
    }

    fn take_cost(&mut self) -> u32 {
        let dos = core::mem::take(&mut self.dos.cost);
        core::mem::take(&mut self.cost).saturating_add(dos)
    }

    fn fast_copy(&mut self, dst: u32, src: u32, len: u32) -> bool {
        self.mem.fast_copy(dst, src, len)
    }

    fn fast_fill(&mut self, dst: u32, pattern: u32, width: u32, len: u32) -> bool {
        self.mem.fast_fill(dst, pattern, width, len)
    }

    fn in8(&mut self, port: u16) -> u8 {
        match port {
            0x00..=0x0F | 0x81..=0x83 | 0x87 | 0x89..=0x8B | 0x8F | 0xC0..=0xDF => {
                self.dma.port_in(port)
            }
            0x20 | 0x21 | 0xA0 | 0xA1 => self.pics.port_in(port),
            0x40..=0x43 => {
                let tick = self.ticks();
                self.pit.port_in(port, tick)
            }
            0x60 => {
                let value = self.keyboard.read_data();
                self.pics.lower(1);
                value
            }
            0x61 => {
                let tick = self.ticks();
                // Bit 4 flips with each DRAM refresh, about every 15 us; bit 5
                // follows timer 2's output.
                let refresh = u8::from((tick / 18) & 1 != 0) << 4;
                let output = u8::from(self.pit.channels[2].output(tick)) << 5;
                (self.port61 & 0x0F) | refresh | output
            }
            0x64 => self.keyboard.read_status(),
            0x80 => 0,
            0x92 => u8::from(self.mem.a20()) << 1,
            0x201 => self.joystick_in(),
            0x388 | 0x228 => {
                self.sync_opl();
                self.opl.read_status()
            }
            // The SB Pro and SB16 show the FM chip at their base as well.
            0x220 | 0x222 if self.sb.model != sb::Model::Sb2 => {
                self.sync_opl();
                self.opl.read_status()
            }
            0x389 | 0x38A | 0x38B | 0x229 => 0xFF,
            0x3B4 | 0x3B5 | 0x3BA | 0x3C0..=0x3CF | 0x3D4 | 0x3D5 | 0x3D8 | 0x3D9 | 0x3DA => {
                let now = self.time_us();
                let SystemBus { vga, mem, .. } = self;
                vga.port_in(port, mem, now)
            }
            sb::BASE..=0x22F => {
                // Port reads see where the DSP has got to.
                self.generate_audio();
                let value = self.sb.port_in(port);
                if matches!(port, 0x22E | 0x22F) && self.sb.irq_cleared() {
                    self.pics.lower(self.sb.irq_line());
                }
                value
            }
            _ => 0xFF,
        }
    }

    fn out8(&mut self, port: u16, value: u8) {
        match port {
            0x00..=0x0F | 0x81..=0x83 | 0x87 | 0x89..=0x8B | 0x8F | 0xC0..=0xDF => {
                self.dma.port_out(port, value)
            }
            0x20 | 0x21 | 0xA0 | 0xA1 => self.pics.port_out(port, value),
            0x40..=0x43 => {
                let tick = self.ticks();
                // Keep the wave up to the moment its pitch changes.
                self.generate_audio();
                self.pit.port_out(port, value, tick);
            }
            0x60 => {
                if let Some(a20) = self.keyboard.write_data(value) {
                    self.set_a20(a20);
                }
            }
            0x61 => {
                self.generate_audio();
                self.port61 = value & 0x0F;
                self.pit.channels[2].gate = value & 1 != 0;
            }
            0x64 => self.keyboard.write_command(value),
            0x92 => self.set_a20(value & 2 != 0),
            0x201 => self.joystick_fired = self.time_us(),
            0x1CE..=0x1D3 => self.vga.vesa_pm_port(port, value),
            0x388 | 0x38A | 0x228 => self.opl.write_address(port, value),
            0x389 | 0x38B | 0x229 => {
                self.sync_opl();
                self.generate_audio();
                self.opl.write_data(port, value);
            }
            0x220..=0x223 if self.sb.model != sb::Model::Sb2 => {
                // 2x0h and 2x1h are the first register file, 2x2h and 2x3h the second.
                let mapped = 0x388 + port - 0x220;
                if port & 1 == 0 {
                    self.opl.write_address(mapped, value);
                } else {
                    self.sync_opl();
                    self.generate_audio();
                    self.opl.write_data(mapped, value);
                }
            }
            0x3B4 | 0x3B5 | 0x3BA | 0x3C0..=0x3CF | 0x3D4 | 0x3D5 | 0x3D8 | 0x3D9 | 0x3DA => {
                let SystemBus { vga, mem, .. } = self;
                vga.port_out(port, value, mem);
            }
            sb::BASE..=0x22F => {
                self.generate_audio();
                self.sb.port_out(port, value);
            }
            _ => {}
        }
    }

    fn interrupt(&mut self, vector: u8, regs: &mut CpuRegs<'_>) -> InterruptAction {
        match vector {
            0x00 => {
                self.vga.write_line(&mut self.mem, b"Divide overflow");
                self.terminate_program(regs, 0xFF, 0, None)
            }
            0x06 => {
                self.fatal_message(regs, "Invalid opcode");
                self.terminate_program(regs, 0xFF, 0, None)
            }
            0x07 => {
                self.fatal_message(regs, "No coprocessor");
                self.terminate_program(regs, 0xFF, 0, None)
            }
            0x08 => {
                let SystemBus { bios, mem, .. } = self;
                bios.int08(mem);
                InterruptAction::Continue
            }
            0x09 => {
                let SystemBus { keyboard, mem, .. } = self;
                keyboard.bios_int9(mem);
                InterruptAction::Continue
            }
            0x10 => {
                let SystemBus { mem, vga, .. } = self;
                let cost = vga.int10(regs, mem);
                self.cost = self.cost.saturating_add(cost);
                if core::mem::take(&mut self.vga.mode_changed) {
                    let (width, height) = mouse_screen(self.vga.mode);
                    self.mouse.set_range(width, height);
                }
                InterruptAction::Continue
            }
            0x11 => {
                self.bios.int11(regs);
                InterruptAction::Continue
            }
            0x12 => {
                self.bios.int12(regs);
                InterruptAction::Continue
            }
            0x13 => {
                self.bios.int13(regs);
                InterruptAction::Continue
            }
            0x14 => {
                self.bios.int14(regs);
                InterruptAction::Continue
            }
            0x15 => self.int15(regs),
            0x16 => {
                let SystemBus {
                    bios,
                    mem,
                    keyboard,
                    ..
                } = self;
                bios.int16(regs, mem, keyboard)
            }
            0x17 => {
                self.bios.int17(regs);
                InterruptAction::Continue
            }
            0x18 | 0x19 => self.terminate_program(regs, 0, 0, None),
            0x1A => {
                let SystemBus {
                    bios, mem, now_ms, ..
                } = self;
                bios.int1a(regs, mem, *now_ms);
                InterruptAction::Continue
            }
            0x20 => self.terminate_program(regs, 0, 0, None),
            0x21 => {
                let SystemBus {
                    dos,
                    mem,
                    vga,
                    now_ms,
                    exit_requested,
                    exit_code,
                    ..
                } = self;
                dos.int21(regs, mem, vga, *now_ms, exit_requested, exit_code)
            }
            0x22 => InterruptAction::Continue,
            0x23 => self.terminate_program(regs, 3, 1, None),
            0x24 => {
                regs.set_al(3);
                InterruptAction::Continue
            }
            0x25 | 0x26 => {
                regs.set_ax(0x8002);
                regs.set_carry(true);
                InterruptAction::Continue
            }
            0x27 => {
                let keep = regs.dx().wrapping_add(15) >> 4;
                self.terminate_program(regs, 0, 3, Some(keep))
            }
            0x28 => InterruptAction::Continue,
            0x29 => {
                let SystemBus { vga, mem, .. } = self;
                vga.teletype(mem, regs.al());
                InterruptAction::Continue
            }
            0x2F => {
                self.int2f(regs);
                InterruptAction::Continue
            }
            0x33 => {
                
                self.mouse.int33(regs)
            }
            0x67 if regs.ah() == 0xDE && self.emm.enabled => {
                let SystemBus {
                    emm, mem, xms, ems, ..
                } = self;
                emm.service(regs, mem, xms, ems, 0x11);
                InterruptAction::Continue
            }
            0x67 => {
                let SystemBus { ems, mem, vga, .. } = self;
                ems.int67(regs, mem, vga);
                self.mem.set_ems_frame(self.ems.frame_map());
                InterruptAction::Continue
            }
            CALLBACK_XMS => {
                let SystemBus { xms, mem, vga, .. } = self;
                let moved = xms.call(regs, mem, vga);
                self.cost = self.cost.saturating_add(moved / 8);
                InterruptAction::Continue
            }
            CALLBACK_MOUSE_ENTER => {
                let ptr = ((ROM_SEGMENT as u32) << 4) + MOUSE_HANDLER_PTR as u32;
                if self.mouse.enter_handler(regs) {
                    self.mem.write16(ptr, self.mouse.handler.0);
                    self.mem.write16(ptr + 2, self.mouse.handler.1);
                } else {
                    // Nothing to call: a far return in ROM.
                    self.mem.write16(ptr, XMS_ENTRY_OFFSET + 3);
                    self.mem.write16(ptr + 2, ROM_SEGMENT);
                }
                InterruptAction::Continue
            }
            CALLBACK_MOUSE_DONE => {
                self.mouse.in_handler = false;
                InterruptAction::Continue
            }
            _ => InterruptAction::Continue,
        }
    }
}

/// The mouse driver's virtual screen for a mode.
fn mouse_screen(mode: u8) -> (usize, usize) {
    match mode {
        0x0F | 0x10 => (640, 350),
        0x11 | 0x12 => (640, 480),
        _ => (640, 200),
    }
}

/// The emulated machine and its DOS prompt.
pub struct DosBox {
    pub cpu: Cpu,
    pub bus: SystemBus,
    pub shell: Shell,
    pub screen: Vec<u32>,
    pub exit: bool,
    running: bool,
    last_blink: bool,
    cursor_canvas: (i32, i32),
    cursor_seen: bool,
    /// Interrupt vectors as the prompt had them, restored after programs that
    /// didn't stay resident.
    prompt_vectors: Vec<u8>,
    /// The TSR count when the current program started.
    tsr_mark: u32,
    last_frame_ms: Option<u64>,
}

impl DosBox {
    pub fn new(fs: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        let mut machine = Self {
            cpu: Cpu::new(),
            bus: SystemBus::new(fs),
            shell: Shell::new(),
            screen: vec![0; SCREEN_WIDTH * SCREEN_HEIGHT],
            exit: false,
            running: false,
            last_blink: true,
            cursor_canvas: (0, 0),
            cursor_seen: false,
            prompt_vectors: Vec::new(),
            tsr_mark: 0,
            last_frame_ms: None,
        };
        machine.boot();
        machine
    }

    fn boot(&mut self) {
        let vectors: Vec<u8> = (0..0x400).map(|at| self.bus.mem.read8(at)).collect();
        self.prompt_vectors = vectors;
        let banner = [
            "",
            "FEOS-DOS 1.0  (FEOS Disk Operating System)",
            "Copyright (C) 2026 The FEOS Project",
            "C: is a RAM drive. Use MOUNT to bring games onto it.",
            "Type HELP for commands, ABOUT for details.",
            "",
        ];
        for line in banner {
            self.bus.vga.write_line(&mut self.bus.mem, line.as_bytes());
        }
        self.shell
            .load_batch(&mut self.bus.dos, "AUTOEXEC.BAT", &[]);
        if !self.shell.in_batch() {
            self.shell
                .show_prompt(&self.bus.dos, &mut self.bus.vga, &mut self.bus.mem, 0);
        }
    }

    pub fn keyboard(&mut self) -> &mut Keyboard {
        &mut self.bus.keyboard
    }

    pub fn push_key(&mut self, ascii: u8, scan: u8) {
        self.bus.keyboard.push(ascii, scan);
    }

    pub fn text(&self) -> String {
        self.bus.vga.text(&self.bus.mem)
    }

    /// Whether a guest program is currently executing.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Instruction units retired by the guest CPU.
    pub fn cycles(&self) -> u64 {
        self.cpu.cycles
    }

    /// The current video mode byte.
    pub fn video_mode(&self) -> u8 {
        self.bus.vga.mode
    }

    /// Sets how many instruction units make a guest millisecond.
    pub fn set_cycles_per_ms(&mut self, cycles: u32) {
        self.bus.cycles = self.cpu.cycles;
        self.bus.set_speed(cycles);
    }

    /// How many instruction units make a guest millisecond.
    pub fn cycles_per_ms(&self) -> u32 {
        self.bus.cycles_per_ms
    }

    /// Enables audio generation at `rate` Hz.
    pub fn set_audio_rate(&mut self, rate: u32) {
        self.bus.audio_rate = rate.max(8_000);
        self.bus.audio_enabled = true;
        self.bus.opl.reset(self.bus.audio_rate);
        self.bus.cycles = self.cpu.cycles;
        self.bus.audio_us = self.bus.time_us();
        self.bus.audio_remainder = 0;
    }

    /// Takes the interleaved stereo samples generated since the last call.
    pub fn take_audio(&mut self) -> Vec<i16> {
        core::mem::take(&mut self.bus.pending_audio)
    }

    /// The interleaved stereo samples waiting to be played. Use with
    /// [`consume_audio`](Self::consume_audio) for a non-blocking device.
    pub fn pending_audio(&self) -> &[i16] {
        &self.bus.pending_audio
    }

    /// Drops the first `count` waiting samples, which the device took.
    pub fn consume_audio(&mut self, count: usize) {
        let count = count.min(self.bus.pending_audio.len());
        self.bus.pending_audio.drain(..count);
    }

    /// Feeds an absolute pointer position and button state from the desktop.
    #[allow(clippy::too_many_arguments)]
    pub fn pointer_canvas(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        left: Option<bool>,
        right: Option<bool>,
        middle: Option<bool>,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        let (display_width, display_height) = self.bus.vga.display_size();
        let display_width = (display_width as i32).max(1);
        let display_height = (display_height as i32).max(1);
        let scale = (width / display_width).min(height / display_height).max(1);
        let offset_x = (width - display_width * scale) / 2;
        let offset_y = (height - display_height * scale) / 2;
        let screen_x = ((x - offset_x) / scale).clamp(0, display_width - 1);
        let screen_y = ((y - offset_y) / scale).clamp(0, display_height - 1);
        self.cursor_canvas = (x.clamp(0, width - 1), y.clamp(0, height - 1));
        self.cursor_seen = true;
        self.bus
            .mouse
            .move_fraction(screen_x, display_width, screen_y, display_height);
        if let Some(down) = left {
            self.bus.mouse.set_button(0, down);
        }
        if let Some(down) = right {
            self.bus.mouse.set_button(1, down);
        }
        if let Some(down) = middle {
            self.bus.mouse.set_button(2, down);
        }
    }

    /// The PC speaker's current frequency, when the gate and speaker are on.
    pub fn speaker_frequency(&self) -> Option<u32> {
        if self.bus.port61 & 3 == 3 {
            Some((PIT_HZ / self.bus.pit.channels[2].reload.max(1) as u64) as u32)
        } else {
            None
        }
    }

    /// Runs one frame paced by real time: guest time keeps up with `now_ms`
    /// where the budget allows, and slows down where it doesn't.
    pub fn frame_paced(&mut self, now_ms: u64) {
        let elapsed = match self.last_frame_ms {
            Some(last) => now_ms.saturating_sub(last),
            None => 16,
        };
        self.last_frame_ms = Some(now_ms);
        let budget = elapsed.clamp(1, MAX_PACED_MS) * self.bus.cycles_per_ms as u64;
        self.frame(now_ms, budget.min(u32::MAX as u64) as u32);
    }

    /// Runs one frame: execute up to `budget` instruction units of guest code, or
    /// edit the prompt.
    pub fn frame(&mut self, now_ms: u64, budget: u32) {
        let last = self.bus.now_ms;
        self.bus.now_ms = now_ms;
        if self.running {
            self.run_cpu(budget);
            if self.bus.exit_requested {
                self.finish_program();
            }
            self.bus.generate_audio();
            return;
        }

        // At the prompt no code is running, so guest time simply follows the host.
        let idle = now_ms.saturating_sub(last) * self.bus.cycles_per_ms as u64;
        self.cpu.cycles = self.cpu.cycles.wrapping_add(idle);
        self.bus.cycles = self.cpu.cycles;

        if let Some(line) = self.shell.next_batch() {
            self.execute_line(&line, now_ms);
            self.bus.generate_audio();
            return;
        }

        while let Some(key) = self.bus.keyboard.pop() {
            if let Some(line) = self.shell.feed_key(
                key,
                &self.bus.dos,
                &mut self.bus.vga,
                &mut self.bus.mem,
                now_ms,
            ) {
                self.execute_line(&line, now_ms);
                if self.running {
                    break;
                }
            }
        }
        self.bus.generate_audio();
    }

    fn execute_line(&mut self, line: &str, now_ms: u64) {
        let result = self.shell.execute(
            line,
            &mut self.bus.dos,
            &mut self.bus.vga,
            &mut self.bus.mem,
            now_ms,
        );
        match result {
            CommandResult::None => {}
            CommandResult::Run(node, tail) => {
                self.run_program(node, &tail);
                return;
            }
            CommandResult::Screenshot => {
                self.save_screenshot();
            }
            CommandResult::SaveState => {
                let text: &[u8] = if self.save_state_file() {
                    b"Saved C:\\STATE.BIN"
                } else {
                    b"Cannot write C:\\STATE.BIN"
                };
                self.bus.vga.write_line(&mut self.bus.mem, text);
            }
            CommandResult::LoadState => {
                let text: &[u8] = if self.load_state_file() {
                    b"Restored C:\\STATE.BIN"
                } else {
                    b"Cannot read C:\\STATE.BIN"
                };
                self.bus.vga.write_line(&mut self.bus.mem, text);
            }
            CommandResult::SetCycles(cycles) => self.set_cycles_per_ms(cycles),
            CommandResult::SetSoundBlaster(model) => {
                self.bus.sb = SoundBlaster::with_model(model);
            }
            CommandResult::SetMemoryManager(on) => self.bus.emm.enabled = on,
            CommandResult::SetDpmi(mode) => self.bus.dpmi.mode = mode,
            CommandResult::Exit => {
                self.exit = true;
                return;
            }
        }
        self.shell.after_command(
            &mut self.bus.dos,
            &mut self.bus.vga,
            &mut self.bus.mem,
            now_ms,
        );
    }

    /// Renders the current screen into a 24-bit BMP on `C:\SCREEN.BMP`.
    pub fn save_screenshot(&mut self) -> bool {
        self.bus.vga.render(&self.bus.mem, &mut self.screen, false);
        let (width, height) = self.bus.vga.display_size();
        let row_bytes = width * 3;
        let image_size = row_bytes * height;
        let file_size = 54 + image_size;
        let mut bmp = Vec::new();
        if bmp.try_reserve_exact(file_size).is_err() {
            return false;
        }
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(width as i32).to_le_bytes());
        bmp.extend_from_slice(&(height as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&(image_size as u32).to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..height).rev() {
            for x in 0..width {
                let pixel = self.screen.get(y * SCREEN_WIDTH + x).copied().unwrap_or(0);
                bmp.push(pixel as u8);
                bmp.push((pixel >> 8) as u8);
                bmp.push((pixel >> 16) as u8);
            }
        }
        let root = self.bus.dos.disk.root;
        if self
            .bus
            .dos
            .disk
            .create_file(root, "SCREEN.BMP", bmp)
            .is_some()
        {
            self.bus
                .vga
                .write_line(&mut self.bus.mem, b"Wrote C:\\SCREEN.BMP");
            true
        } else {
            false
        }
    }

    /// Runs a program by DOS path. Returns false when the file is missing.
    pub fn run(&mut self, name: &str) -> bool {
        self.run_with_args(name, "")
    }

    /// Runs a program with a command tail.
    pub fn run_with_args(&mut self, name: &str, args: &str) -> bool {
        let Some(node) = self.bus.dos.resolve(name) else {
            return false;
        };
        if !self.bus.dos.disk.is_file(node) {
            return false;
        }
        self.run_program(node, args);
        self.running
    }

    fn run_program(&mut self, node: usize, tail: &str) {
        let mut bytes = Vec::with_capacity(tail.len() + 1);
        if !tail.is_empty() {
            if !tail.starts_with(' ') {
                bytes.push(b' ');
            }
            bytes.extend_from_slice(tail.as_bytes());
        }
        self.bus.dos.psp = COMMAND_PSP;
        let name = self
            .bus
            .dos
            .disk
            .node(node)
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        let SystemBus { dos, mem, .. } = &mut self.bus;
        let loaded = dos.load_program(mem, &name, node, &bytes, 0);
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(code) => {
                let text: &[u8] = match code {
                    8 => b"Program too big to fit in memory",
                    11 => b"Cannot load program: invalid executable",
                    _ => b"Cannot load program",
                };
                self.bus.vga.write_line(&mut self.bus.mem, text);
                self.shell.program_finished(
                    0xFF,
                    &mut self.bus.dos,
                    &mut self.bus.vga,
                    &mut self.bus.mem,
                    self.bus.now_ms,
                );
                return;
            }
        };
        self.cpu.reset();
        self.cpu.r = [0; 8];
        for (index, selector) in [
            (cpu::ES, loaded.psp),
            (cpu::DS, loaded.psp),
            (cpu::SS, loaded.ss),
            (cpu::CS, loaded.cs),
        ] {
            self.cpu.set_real_segment(index, selector);
        }
        self.cpu.ip = loaded.ip as u32;
        self.cpu.r[cpu::SP] = loaded.sp as u32;
        self.cpu.r[cpu::CX] = 0xFF;
        self.cpu.r[cpu::DX] = loaded.psp as u32;
        self.cpu.r[cpu::SI] = loaded.ip as u32;
        self.cpu.r[cpu::DI] = loaded.sp as u32;
        self.cpu.r[cpu::BP] = 0x091C;
        self.cpu.set_flags(0x0202);
        if self.bus.emm.enabled {
            self.bus.emm.reserve(&mut self.bus.xms);
            self.bus.mem.set_a20(true);
            self.cpu.set_v86_monitor(true);
        }
        self.running = true;
        self.shell.running = true;
        self.bus.exit_requested = false;
        self.bus.exit_code = 0;
        self.bus.bios.reset();
        self.tsr_mark = self.bus.dos.tsr_count;
        self.bus.mem.write8(BDA_BASE + 0x71, 0);
    }

    fn run_cpu(&mut self, budget: u32) {
        let end = self.cpu.cycles.wrapping_add(budget as u64);
        loop {
            self.bus.cycles = self.cpu.cycles;
            self.bus.sync_devices();
            if self.bus.pics.pending() && self.cpu.interrupts_enabled() {
                if let Some(vector) = self.bus.pics.acknowledge() {
                    let step = self.cpu.hardware_interrupt(&mut self.bus, vector);
                    if let Step::Fault(vector) = step {
                        self.cpu_shutdown(vector);
                        return;
                    }
                }
            }
            if self.bus.exit_requested || self.cpu.cycles >= end {
                return;
            }
            let slice_end = end
                .min(self.bus.next_event_cycle())
                .max(self.cpu.cycles + 1);
            while self.cpu.cycles < slice_end {
                match self.cpu.step(&mut self.bus) {
                    Step::Ok => {
                        if self.bus.pics.pending() && self.cpu.interrupts_enabled() {
                            break;
                        }
                    }
                    Step::Blocked | Step::Halted => {
                        if self.bus.exit_requested {
                            return;
                        }
                        // Idle: nothing happens until the next event.
                        if self.cpu.cycles < slice_end {
                            self.cpu.cycles = slice_end;
                        }
                        break;
                    }
                    Step::Fault(vector) => {
                        self.cpu_shutdown(vector);
                        return;
                    }
                }
            }
        }
    }

    /// A triple fault shuts the CPU down, which on a PC would be a reset.
    fn cpu_shutdown(&mut self, vector: u8) {
        let line = format!(
            "CPU shutdown after exception {:02X}h at {:04X}:{:08X}",
            vector,
            self.cpu.sregs[cpu::CS],
            self.cpu.ip
        );
        self.bus.vga.write_line(&mut self.bus.mem, line.as_bytes());
        self.bus.exit_requested = true;
        self.bus.exit_code = 0xFF;
    }

    fn finish_program(&mut self) {
        let code = self.bus.exit_code;
        self.running = false;
        self.shell.running = false;
        self.bus.exit_requested = false;
        self.bus.exit_code = 0;
        {
            let SystemBus { dos, mem, .. } = &mut self.bus;
            dos.unwind_to_root(mem);
        }
        self.cpu.reset();
        // Protected mode and interrupt hooks don't outlive the program unless it
        // stayed resident.
        if self.bus.dos.tsr_count == self.tsr_mark {
            for (at, byte) in self.prompt_vectors.iter().enumerate() {
                self.bus.mem.write8(at as u32, *byte);
            }
        }
        self.bus.reset_after_program();
        if !self.bus.vga.is_text() {
            let SystemBus { vga, mem, .. } = &mut self.bus;
            vga.set_mode(mem, 3);
            vga.mode_changed = false;
        }
        crate::keyboard::bda_flush(&mut self.bus.mem);
        self.bus.vga.teletype(&mut self.bus.mem, b'\r');
        self.bus.vga.teletype(&mut self.bus.mem, b'\n');
        if code != 0 {
            let text = format!("Program exited with code {}.", code);
            self.bus.vga.write_line(&mut self.bus.mem, text.as_bytes());
        }
        self.shell.program_finished(
            code,
            &mut self.bus.dos,
            &mut self.bus.vga,
            &mut self.bus.mem,
            self.bus.now_ms,
        );
    }

    /// Draws the screen into a desktop canvas.
    pub fn render(&mut self, canvas: &mut feos_gfx::Canvas<'_>) {
        self.refresh_screen();
        self.bus.vga.blit(canvas, &self.screen);
        if let Some((x, y)) = self.software_cursor() {
            draw_software_cursor(canvas, x, y);
        }
    }

    /// Brings [`screen`](Self::screen) up to date with video memory, and returns
    /// whether that changed anything. [`render`](Self::render) does this too; a
    /// caller that draws on another thread calls this and copies the buffer.
    pub fn refresh_screen(&mut self) -> bool {
        let blink = (self.bus.now_ms / 500).is_multiple_of(2);
        if blink != self.last_blink {
            self.bus.vga.dirty = true;
            self.last_blink = blink;
        }
        if self.bus.vga.dirty {
            self.bus.vga.render(&self.bus.mem, &mut self.screen, blink);
            return true;
        }
        false
    }

    /// The size of the picture in [`screen`](Self::screen).
    pub fn display_size(&self) -> (usize, usize) {
        self.bus.vga.display_size()
    }

    /// Where the guest's mouse cursor belongs on the canvas, if it's showing.
    pub fn software_cursor(&self) -> Option<(i32, i32)> {
        (self.cursor_seen && self.bus.mouse.visible()).then_some(self.cursor_canvas)
    }
}

/// Draws the guest's mouse cursor at a canvas position.
pub fn draw_software_cursor(canvas: &mut feos_gfx::Canvas<'_>, x: i32, y: i32) {
    let shape = [
        0b10000u8, 0b11000, 0b11100, 0b11110, 0b11111, 0b11100, 0b11000, 0b10010, 0b00010,
    ];
    for (row, bits) in shape.iter().enumerate() {
        for column in 0..5 {
            if bits & (1 << (4 - column)) != 0 {
                canvas.put_pixel(x + column, y + row as i32, feos_gfx::Color::WHITE);
            }
        }
    }
}

const STATE_MAGIC: &[u8; 8] = b"FEOSDOS2";

impl DosBox {
    /// Serializes the machine state to a compact byte buffer.
    ///
    /// Files on `C:` are not part of the state, matching DOSBox's save
    /// states: what a program wrote stays written. A state is taken and resumed
    /// at the prompt, so extended memory and the hardware timers aren't saved.
    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(0x10_0000 + 0x4_0000 + 4096);
        out.extend_from_slice(STATE_MAGIC);
        for value in self.cpu.r {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in self.cpu.sregs {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.cpu.ip.to_le_bytes());
        out.extend_from_slice(&self.cpu.flags().to_le_bytes());
        out.push(u8::from(self.cpu.halted));
        out.extend_from_slice(&self.cpu.cycles.to_le_bytes());

        out.push(self.cpu.fpu.top);
        out.extend_from_slice(&self.cpu.fpu.control.to_le_bytes());
        out.extend_from_slice(&self.cpu.fpu.status.to_le_bytes());
        out.extend_from_slice(&self.cpu.fpu.tags.to_le_bytes());
        for index in 0..8 {
            out.extend_from_slice(&self.cpu.fpu.st[index].get_bytes());
        }

        out.extend_from_slice(self.bus.mem.as_slice());
        self.bus.vga.save_state(&mut out);

        let mouse = &self.bus.mouse;
        out.extend_from_slice(&mouse.x.to_le_bytes());
        out.extend_from_slice(&mouse.y.to_le_bytes());
        out.push(mouse.buttons);
        out.extend_from_slice(&mouse.hide_counter.to_le_bytes());
        out.extend_from_slice(&mouse.min_x.to_le_bytes());
        out.extend_from_slice(&mouse.max_x.to_le_bytes());
        out.extend_from_slice(&mouse.min_y.to_le_bytes());
        out.extend_from_slice(&mouse.max_y.to_le_bytes());
        out.extend_from_slice(&mouse.handler_mask.to_le_bytes());
        out.extend_from_slice(&mouse.handler.0.to_le_bytes());
        out.extend_from_slice(&mouse.handler.1.to_le_bytes());

        let dos = &self.bus.dos;
        out.extend_from_slice(&(dos.cwd as u32).to_le_bytes());
        out.extend_from_slice(&dos.dta.to_le_bytes());
        out.extend_from_slice(&dos.psp.to_le_bytes());
        out.extend_from_slice(&(dos.sft.len() as u32).to_le_bytes());
        for slot in &dos.sft {
            match slot {
                Some(file) => {
                    out.push(1);
                    out.extend_from_slice(&(file.node as u32).to_le_bytes());
                    out.extend_from_slice(&file.offset.to_le_bytes());
                    out.push(file.mode);
                    out.push(device_code(file.device));
                    out.extend_from_slice(&file.refs.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        out
    }

    /// Restores a state written by [`save_state`](Self::save_state). Every field
    /// is validated first, and malformed data leaves the machine as it was.
    pub fn load_state(&mut self, data: &[u8]) -> bool {
        match self.parse_state(data) {
            Some(apply) => {
                self.apply_state(apply);
                true
            }
            None => false,
        }
    }

    fn parse_state(&mut self, data: &[u8]) -> Option<ParsedState> {
        let mut at = 0usize;
        if take(data, &mut at, 8)? != STATE_MAGIC {
            return None;
        }
        let mut r = [0u32; 8];
        for value in r.iter_mut() {
            *value = read_u32(data, &mut at)?;
        }
        let mut sregs = [0u16; 6];
        for value in sregs.iter_mut() {
            *value = read_u16(data, &mut at)?;
        }
        let ip = read_u32(data, &mut at)?;
        let flags = read_u32(data, &mut at)?;
        let halted = read_u8(data, &mut at)? != 0;
        let cycles = read_u64(data, &mut at)?;

        let top = read_u8(data, &mut at)?;
        if top > 7 {
            return None;
        }
        let control = read_u16(data, &mut at)?;
        let status = read_u16(data, &mut at)?;
        let tags = read_u16(data, &mut at)?;
        let mut st = [[0u8; 10]; 8];
        for value in st.iter_mut() {
            value.copy_from_slice(take(data, &mut at, 10)?);
        }

        let memory_at = at;
        take(data, &mut at, 0x10_0000)?;

        // The video state validates itself against a scratch adapter first, so
        // that a bad state can't leave the real one half loaded.
        let vga_at = at;
        let mut scratch = Vga::new();
        if !scratch.load_state(data, &mut at) {
            return None;
        }

        let x = read_i32(data, &mut at)?;
        let y = read_i32(data, &mut at)?;
        let buttons = read_u8(data, &mut at)?;
        let hide_counter = read_i32(data, &mut at)?;
        let min_x = read_i32(data, &mut at)?;
        let max_x = read_i32(data, &mut at)?;
        let min_y = read_i32(data, &mut at)?;
        let max_y = read_i32(data, &mut at)?;
        let handler_mask = read_u16(data, &mut at)?;
        let handler0 = read_u16(data, &mut at)?;
        let handler1 = read_u16(data, &mut at)?;
        if min_x > max_x || min_y > max_y || hide_counter > 0 {
            return None;
        }

        let cwd = read_u32(data, &mut at)? as usize;
        if !self.bus.dos.disk.is_dir(cwd) {
            return None;
        }
        let dta = read_u32(data, &mut at)?;
        let _psp = read_u16(data, &mut at)?;
        let count = read_u32(data, &mut at)? as usize;
        if count > 256 {
            return None;
        }
        let mut sft = Vec::new();
        for _ in 0..count {
            if read_u8(data, &mut at)? == 0 {
                sft.push(None);
                continue;
            }
            let node = read_u32(data, &mut at)? as usize;
            let offset = read_u32(data, &mut at)?;
            let mode = read_u8(data, &mut at)?;
            let device = device_from_code(read_u8(data, &mut at)?)?;
            let refs = read_u16(data, &mut at)?;
            if device == crate::dos::Device::File && !self.bus.dos.disk.is_file(node) {
                return None;
            }
            sft.push(Some(crate::dos::Sft {
                node,
                offset,
                mode,
                device,
                refs,
            }));
        }
        if at != data.len() {
            return None;
        }
        Some(ParsedState {
            r,
            sregs,
            ip,
            flags,
            halted,
            cycles,
            top,
            control,
            status,
            tags,
            st,
            memory: data[memory_at..memory_at + 0x10_0000].to_vec(),
            vga_data: data[vga_at..].to_vec(),
            mouse: (x, y, buttons, hide_counter, min_x, max_x, min_y, max_y),
            handler: (handler_mask, handler0, handler1),
            cwd,
            dta,
            sft,
        })
    }

    fn apply_state(&mut self, state: ParsedState) {
        self.cpu.r = state.r;
        self.cpu.sregs = state.sregs;
        for index in 0..6 {
            let selector = state.sregs[index];
            self.cpu.set_real_segment(index, selector);
        }
        self.cpu.ip = state.ip;
        self.cpu.set_flags(state.flags);
        self.cpu.halted = state.halted;
        self.cpu.cycles = state.cycles.max(self.cpu.cycles);
        self.cpu.fpu.top = state.top;
        self.cpu.fpu.control = state.control;
        self.cpu.fpu.status = state.status;
        self.cpu.fpu.tags = state.tags;
        for index in 0..8 {
            self.cpu.fpu.st[index].set_bytes(&state.st[index]);
        }
        self.bus.mem.as_mut_slice().copy_from_slice(&state.memory);
        let mut vga_at = 0;
        self.bus.vga.load_state(&state.vga_data, &mut vga_at);

        let mouse = &mut self.bus.mouse;
        (
            mouse.x,
            mouse.y,
            mouse.buttons,
            mouse.hide_counter,
            mouse.min_x,
            mouse.max_x,
            mouse.min_y,
            mouse.max_y,
        ) = state.mouse;
        mouse.x = mouse.x.clamp(mouse.min_x, mouse.max_x);
        mouse.y = mouse.y.clamp(mouse.min_y, mouse.max_y);
        mouse.handler_mask = state.handler.0;
        mouse.handler = (state.handler.1, state.handler.2);

        let dos = &mut self.bus.dos;
        for slot in dos.sft.iter().flatten() {
            if slot.device == crate::dos::Device::File {
                dos.disk.close(slot.node);
            }
        }
        for slot in state.sft.iter().flatten() {
            if slot.device == crate::dos::Device::File {
                dos.disk.open(slot.node);
            }
        }
        dos.sft = state.sft;
        dos.cwd = state.cwd;
        dos.dta = state.dta;
        dos.psp = COMMAND_PSP;

        self.bus.keyboard.clear();
        let rate = self.bus.audio_rate;
        self.bus.opl.reset(rate);
        self.bus.vga.dirty = true;
        self.running = false;
        self.shell.running = false;
    }

    fn save_state_file(&mut self) -> bool {
        let data = self.save_state();
        let root = self.bus.dos.disk.root;
        self.bus
            .dos
            .disk
            .create_file(root, "STATE.BIN", data)
            .is_some()
    }

    fn load_state_file(&mut self) -> bool {
        let root = self.bus.dos.disk.root;
        let Some(node) = self.bus.dos.disk.lookup(root, "STATE.BIN") else {
            return false;
        };
        let Some(data) = self.bus.dos.disk.node(node).map(|node| node.data.clone()) else {
            return false;
        };
        self.load_state(&data)
    }
}

/// A validated state, ready to apply.
struct ParsedState {
    r: [u32; 8],
    sregs: [u16; 6],
    ip: u32,
    flags: u32,
    halted: bool,
    cycles: u64,
    top: u8,
    control: u16,
    status: u16,
    tags: u16,
    st: [[u8; 10]; 8],
    memory: Vec<u8>,
    vga_data: Vec<u8>,
    #[allow(clippy::type_complexity)]
    mouse: (i32, i32, u8, i32, i32, i32, i32, i32),
    handler: (u16, u16, u16),
    cwd: usize,
    dta: u32,
    sft: Vec<Option<crate::dos::Sft>>,
}

fn device_code(device: crate::dos::Device) -> u8 {
    use crate::dos::Device;
    match device {
        Device::File => 0,
        Device::Con => 1,
        Device::Nul => 2,
        Device::Aux => 3,
        Device::Prn => 4,
        Device::Clock => 5,
        Device::Driver => 6,
    }
}

fn device_from_code(code: u8) -> Option<crate::dos::Device> {
    use crate::dos::Device;
    Some(match code {
        0 => Device::File,
        1 => Device::Con,
        2 => Device::Nul,
        3 => Device::Aux,
        4 => Device::Prn,
        5 => Device::Clock,
        6 => Device::Driver,
        _ => return None,
    })
}

fn take<'a>(data: &'a [u8], at: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(len)?;
    let slice = data.get(*at..end)?;
    *at = end;
    Some(slice)
}

fn read_u8(data: &[u8], at: &mut usize) -> Option<u8> {
    Some(*take(data, at, 1)?.first()?)
}

fn read_u16(data: &[u8], at: &mut usize) -> Option<u16> {
    let bytes = take(data, at, 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: &mut usize) -> Option<u32> {
    let bytes = take(data, at, 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_i32(data: &[u8], at: &mut usize) -> Option<i32> {
    Some(read_u32(data, at)? as i32)
}

fn read_u64(data: &[u8], at: &mut usize) -> Option<u64> {
    let bytes = take(data, at, 8)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

#[cfg(test)]
mod tests;
