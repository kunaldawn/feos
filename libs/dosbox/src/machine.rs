//! The FEOS-DOS machine: CPU, bus, devices, prompt and program loader.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::bios::Bios;
use crate::cpu::{Bus, Cpu, CpuRegs, InterruptAction, Step, FLAG_IF};
use crate::dos::Dos;
use crate::keyboard::Keyboard;
use crate::memory::{Memory, MONO_BASE, VIDEO_BASE};
use crate::shell::{CommandResult, Shell};
use crate::vga::{Vga, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Everything the CPU sees through the bus.
pub struct SystemBus {
    pub mem: Memory,
    pub vga: Vga,
    pub keyboard: Keyboard,
    pub mouse: crate::mouse::Mouse,
    pub opl: alloc::boxed::Box<crate::opl::Opl>,
    pub bios: Bios,
    pub dos: Dos,
    pub exit_requested: bool,
    pub exit_code: u8,
    pub now_ms: u64,
    status_toggle: bool,
    port61: u8,
    /// PIT channel 2 divisor, used by the PC speaker.
    pit2_divisor: u16,
    pit2_low: bool,
}

impl SystemBus {
    pub fn new(fs: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        let mut dos = Dos::new();
        dos.seed(fs);
        let mut bus = Self {
            mem: Memory::new(),
            vga: Vga::new(),
            keyboard: Keyboard::new(),
            mouse: crate::mouse::Mouse::new(),
            opl: alloc::boxed::Box::new(crate::opl::Opl::new(48_000)),
            bios: Bios::new(),
            dos,
            exit_requested: false,
            exit_code: 0,
            now_ms: 0,
            status_toggle: false,
            port61: 0,
            pit2_divisor: 0,
            pit2_low: true,
        };
        bus.vga.reset(&mut bus.mem);
        bus.mem.write32(0x46C, 0);
        bus
    }
}

impl Bus for SystemBus {
    fn read8(&mut self, addr: u32) -> u8 {
        if self.vga.is_planar() && (0xA0000..0xB0000).contains(&addr) {
            return self.vga.read_plane_byte((addr - 0xA0000) as usize);
        }
        self.mem.read8(addr)
    }

    fn write8(&mut self, addr: u32, value: u8) {
        if self.vga.is_planar() && (0xA0000..0xB0000).contains(&addr) {
            self.vga.write_plane_byte((addr - 0xA0000) as usize, value);
            return;
        }
        self.mem.write8(addr, value);
        if (0xA0000..0xB0000).contains(&addr)
            || (VIDEO_BASE..VIDEO_BASE + 0x8000).contains(&addr)
            || (MONO_BASE..MONO_BASE + 0x8000).contains(&addr)
        {
            self.vga.mark_dirty();
        }
    }

    fn in8(&mut self, port: u16) -> u8 {
        match port {
            0x3D4 | 0x3B4 => self.vga.crtc_index,
            0x3D5 | 0x3B5 => self.vga.crtc_read(),
            0x388 => self.opl.read_status(),
            0x3C8 => self.vga.dac_index,
            0x3C9 => self.vga.dac_read(),
            0x3DA => {
                self.status_toggle = !self.status_toggle;
                if self.status_toggle {
                    0x09
                } else {
                    0x00
                }
            }
            0x60 => self.keyboard.last_scan(),
            0x64 => 0x14,
            0x40..=0x43 => 0,
            0x61 => self.port61,
            0x20 | 0x21 => 0,
            0x3F8..=0x3FF => 0,
            0x201 => 0,
            0x80 => 0,
            _ => 0xFF,
        }
    }

    fn out8(&mut self, port: u16, value: u8) {
        match port {
            0x3D4 | 0x3B4 => self.vga.crtc_index(value),
            0x3D5 | 0x3B5 => self.vga.crtc_write(&mut self.mem, value),
            0x3C0 => self.vga.attribute_write(value),
            0x3C4 => self.vga.sequencer_index = value,
            0x3C5 => self.vga.sequencer_write(self.vga.sequencer_index, value),
            0x3CE => self.vga.graphics_index = value,
            0x3CF => self.vga.graphics_write(self.vga.graphics_index, value),
            0x388 | 0x38A => self.opl.write_address(port, value),
            0x389 | 0x38B => self.opl.write_data(port, value),
            0x3C8 => self.vga.dac_index(value),
            0x3C9 => self.vga.dac_write(value),
            0x3D9 => self.vga.set_cga_palette(value),
            0x43 => {
                if value >> 6 == 2 {
                    self.pit2_low = true;
                }
            }
            0x42 => {
                if self.pit2_low {
                    self.pit2_divisor = (self.pit2_divisor & 0xFF00) | value as u16;
                    self.pit2_low = false;
                } else {
                    self.pit2_divisor = (self.pit2_divisor & 0x00FF) | ((value as u16) << 8);
                    self.pit2_low = true;
                }
            }
            0x61 => self.port61 = value,
            0x20
            | 0x21
            | 0x40..=0x41
            | 0x60
            | 0x64
            | 0x3C1..=0x3C3
            | 0x3C6
            | 0x3C7
            | 0x3D8
            | 0x3DA..=0x3DF
            | 0x3F8..=0x3FF => {}
            _ => {}
        }
    }

    fn interrupt(&mut self, vector: u8, regs: &mut CpuRegs<'_>) -> InterruptAction {
        match vector {
            0x08 | 0x1C => InterruptAction::Continue,
            0x10 => {
                let SystemBus {
                    bios,
                    mem,
                    vga,
                    mouse,
                    ..
                } = self;
                bios.int10(regs, mem, vga);
                mouse.set_range(vga.width, vga.height);
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
            0x15 => {
                self.bios.int15(regs);
                InterruptAction::Continue
            }
            0x16 => {
                let SystemBus { bios, keyboard, .. } = self;
                bios.int16(regs, keyboard)
            }
            0x17 => {
                self.bios.int17(regs);
                InterruptAction::Continue
            }
            0x1A => {
                let SystemBus {
                    bios, mem, now_ms, ..
                } = self;
                bios.int1a(regs, mem, *now_ms);
                InterruptAction::Continue
            }
            0x20 | 0x27 => {
                self.exit_requested = true;
                self.exit_code = 0;
                InterruptAction::Halt
            }
            0x21 => {
                let SystemBus {
                    dos,
                    mem,
                    vga,
                    keyboard,
                    now_ms,
                    exit_requested,
                    exit_code,
                    ..
                } = self;
                dos.int21(regs, mem, vga, keyboard, *now_ms, exit_requested, exit_code)
            }
            0x28 => InterruptAction::Continue,
            0x2F => {
                regs.set_al(0);
                InterruptAction::Continue
            }
            0x33 => self.mouse.int33(regs),
            0x67 => {
                self.bios.int67(regs);
                InterruptAction::Continue
            }
            _ => InterruptAction::Continue,
        }
    }
}

struct Program {
    _name: String,
    _psp: u16,
}

/// The emulated machine and its DOS prompt.
pub struct DosBox {
    pub cpu: Cpu,
    pub bus: SystemBus,
    pub shell: Shell,
    pub screen: Vec<u32>,
    pub exit: bool,
    program: Option<Program>,
    last_blink: bool,
    last_bell: u32,
    cursor_canvas: (i32, i32),
    cursor_seen: bool,
    audio_rate: u32,
    audio_enabled: bool,
    last_audio_ms: u64,
    pending_audio: Vec<i16>,
    speaker_phase: u32,
    speaker_level: bool,
    bell_phase: u32,
    bell_frames: usize,
}

impl DosBox {
    pub fn new(fs: Option<&'static dyn feos_fs::FileSystem>) -> Self {
        let mut machine = Self {
            cpu: Cpu::new(),
            bus: SystemBus::new(fs),
            shell: Shell::new(),
            screen: vec![0; SCREEN_WIDTH * SCREEN_HEIGHT],
            exit: false,
            program: None,
            last_blink: true,
            last_bell: 0,
            cursor_canvas: (0, 0),
            cursor_seen: false,
            audio_rate: 48_000,
            audio_enabled: false,
            last_audio_ms: 0,
            pending_audio: Vec::new(),
            speaker_phase: 0,
            speaker_level: false,
            bell_phase: 0,
            bell_frames: 0,
        };
        machine.boot();
        machine
    }

    fn boot(&mut self) {
        self.bus.vga.reset(&mut self.bus.mem);
        let banner = [
            "",
            "FEOS-DOS 1.0  (FEOS Disk Operating System)",
            "Copyright (C) 2026 The FEOS Project",
            "C: is a 640K RAM drive seeded from the FEOS filesystem.",
            "Type HELP for commands, ABOUT for details.",
            "",
        ];
        for line in banner {
            self.bus.vga.write_line(&mut self.bus.mem, line.as_bytes());
        }
        self.shell.load_batch(&mut self.bus.dos, "AUTOEXEC.BAT");
        if self.shell.batch.is_empty() {
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
        self.program.is_some()
    }

    /// Instructions retired by the guest CPU.
    pub fn cycles(&self) -> u64 {
        self.cpu.cycles
    }

    /// The current video mode byte.
    pub fn video_mode(&self) -> u8 {
        self.bus.vga.mode
    }

    /// Enables audio generation at `rate` Hz.
    pub fn set_audio_rate(&mut self, rate: u32) {
        self.audio_rate = rate.max(8_000);
        self.audio_enabled = true;
        self.bus.opl.reset(self.audio_rate);
        self.last_audio_ms = 0;
    }

    /// Takes the interleaved stereo samples generated since the last call.
    pub fn take_audio(&mut self) -> Vec<i16> {
        core::mem::take(&mut self.pending_audio)
    }

    /// Feeds an absolute pointer position and button state from the desktop.
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
        let scale = (width / 640).min(height / 400).max(1);
        let offset_x = (width - 640 * scale) / 2;
        let offset_y = (height - 400 * scale) / 2;
        let screen_x = ((x - offset_x) / scale).clamp(0, 639);
        let screen_y = ((y - offset_y) / scale).clamp(0, 399);
        self.cursor_canvas = (x.clamp(0, width - 1), y.clamp(0, height - 1));
        self.cursor_seen = true;
        let video_x = screen_x * self.bus.vga.width as i32 / 640;
        let video_y = screen_y * self.bus.vga.height as i32 / 400;
        self.bus.mouse.move_to(video_x, video_y);
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
        if self.bus.port61 & 3 == 3 && self.bus.pit2_divisor != 0 {
            Some(1_193_182 / self.bus.pit2_divisor as u32)
        } else {
            None
        }
    }

    /// Returns true once after a BEL (`0x07`) was printed.
    pub fn take_bell(&mut self) -> bool {
        if self.bus.vga.bell != self.last_bell {
            self.last_bell = self.bus.vga.bell;
            true
        } else {
            false
        }
    }

    /// Runs one frame: execute a slice of guest code or edit the prompt.
    pub fn frame(&mut self, now_ms: u64, budget: u32) {
        self.bus.now_ms = now_ms;
        if self.program.is_some() {
            self.run_cpu(now_ms, budget);
            if self.bus.exit_requested {
                self.finish_program();
            }
            self.generate_audio(now_ms);
            return;
        }

        if let Some(line) = self.shell.next_batch() {
            self.execute_line(&line, now_ms);
            self.generate_audio(now_ms);
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
            }
        }
        self.generate_audio(now_ms);
    }

    fn generate_audio(&mut self, now_ms: u64) {
        if !self.audio_enabled {
            return;
        }
        let delta = now_ms.saturating_sub(self.last_audio_ms).min(100);
        self.last_audio_ms = now_ms;
        let frames = (delta as usize * self.audio_rate as usize / 1000).min(4096);
        if frames == 0 {
            return;
        }
        let mut samples = vec![0i16; frames * 2];
        self.bus.opl.generate(&mut samples);
        if self.take_bell() {
            self.bell_frames = self.audio_rate as usize * 50 / 1000;
        }
        let speaker = self.speaker_frequency();
        for frame in 0..frames {
            let mut value = 0i32;
            if let Some(frequency) = speaker {
                self.speaker_phase += frequency;
                if self.speaker_phase >= self.audio_rate {
                    self.speaker_phase -= self.audio_rate;
                    self.speaker_level = !self.speaker_level;
                }
                value += if self.speaker_level { 1200 } else { -1200 };
            }
            if self.bell_frames > 0 {
                self.bell_phase += 880;
                if self.bell_phase >= self.audio_rate {
                    self.bell_phase -= self.audio_rate;
                    value = if value >= 0 { 4000 } else { -4000 };
                }
                self.bell_frames -= 1;
            }
            if value != 0 {
                let left = samples[frame * 2] as i32 + value;
                let right = samples[frame * 2 + 1] as i32 + value;
                samples[frame * 2] = left.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                samples[frame * 2 + 1] = right.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
        if samples.iter().all(|sample| *sample == 0) {
            self.pending_audio.clear();
        } else {
            self.pending_audio = samples;
        }
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
            CommandResult::None => {
                if self.program.is_none() && self.shell.batch.is_empty() {
                    self.shell.show_prompt(
                        &self.bus.dos,
                        &mut self.bus.vga,
                        &mut self.bus.mem,
                        now_ms,
                    );
                }
            }
            CommandResult::Run(node, tail) => {
                self.run_program(node, &tail);
            }
            CommandResult::Screenshot => {
                self.save_screenshot();
                if self.shell.batch.is_empty() {
                    self.shell.show_prompt(
                        &self.bus.dos,
                        &mut self.bus.vga,
                        &mut self.bus.mem,
                        now_ms,
                    );
                }
            }
            CommandResult::SaveState => {
                if self.save_state_file() {
                    self.bus
                        .vga
                        .write_line(&mut self.bus.mem, b"Saved C:\\STATE.BIN");
                } else {
                    self.bus
                        .vga
                        .write_line(&mut self.bus.mem, b"Cannot write C:\\STATE.BIN");
                }
                if self.shell.batch.is_empty() {
                    self.shell.show_prompt(
                        &self.bus.dos,
                        &mut self.bus.vga,
                        &mut self.bus.mem,
                        now_ms,
                    );
                }
            }
            CommandResult::LoadState => {
                if self.load_state_file() {
                    self.bus
                        .vga
                        .write_line(&mut self.bus.mem, b"Restored C:\\STATE.BIN");
                } else {
                    self.bus
                        .vga
                        .write_line(&mut self.bus.mem, b"Cannot read C:\\STATE.BIN");
                }
                if self.shell.batch.is_empty() {
                    self.shell.show_prompt(
                        &self.bus.dos,
                        &mut self.bus.vga,
                        &mut self.bus.mem,
                        now_ms,
                    );
                }
            }
            CommandResult::Exit => self.exit = true,
        }
    }

    /// Renders the current screen into a 24-bit BMP on `C:\SCREEN.BMP`.
    pub fn save_screenshot(&mut self) -> bool {
        self.bus.vga.render(&self.bus.mem, &mut self.screen, false);
        let width = 640usize;
        let height = 400usize;
        let row_bytes = width * 3;
        let image_size = row_bytes * height;
        let file_size = 54 + image_size;
        let mut bmp = Vec::with_capacity(file_size);
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
                let pixel = self.screen[y * width + x];
                bmp.push((pixel >> 16) as u8);
                bmp.push((pixel >> 8) as u8);
                bmp.push(pixel as u8);
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
        let Some(node) = self.bus.dos.resolve(name) else {
            return false;
        };
        if !self.bus.disk_is_file(node) {
            return false;
        }
        self.run_program(node, "");
        true
    }

    fn run_program(&mut self, node: usize, tail: &str) {
        let Some(entry) = self.bus.dos.disk.node(node) else {
            return;
        };
        let name = entry.name.clone();
        let data = entry.data.clone();
        let is_exe = name.to_ascii_uppercase().ends_with(".EXE")
            || (data.len() >= 2 && &data[0..2] == b"MZ");
        let loaded = if is_exe {
            self.load_exe(&data, tail)
        } else {
            self.load_com(&data, tail)
        };
        if !loaded {
            self.bus.vga.write_line(
                &mut self.bus.mem,
                b"Cannot load program: invalid executable",
            );
            self.shell.show_prompt(
                &self.bus.dos,
                &mut self.bus.vga,
                &mut self.bus.mem,
                self.bus.now_ms,
            );
            return;
        }
        self.program = Some(Program {
            _name: name,
            _psp: crate::dos::PSP_SEGMENT,
        });
        self.shell.running = true;
        self.bus.exit_requested = false;
        self.bus.exit_code = 0;
        self.bus.dos.find = None;
        self.bus.dos.line.active = false;
        self.bus.dos.alloc.clear();
        self.bus.dos.psp = crate::dos::PSP_SEGMENT;
        self.bus.dos.dta = ((crate::dos::PSP_SEGMENT as u32) << 4) + 0x80;
    }

    fn write_psp(&mut self, psp: u16, tail: &str) {
        let base = (psp as u32) << 4;
        self.bus.mem.write16(base, 0x20CD);
        self.bus.mem.write16(base + 2, 0x9FFF);
        // A real environment block, so self-relocating EXEs that scan it
        // (LZEXE, PKLITE and friends) see a valid terminator.
        let env_segment = 0x0F00u16;
        self.bus.mem.write16(base + 0x2C, env_segment);
        let env = (env_segment as u32) << 4;
        self.bus.mem.fill(env, 0x100, 0);
        let mut cursor = env;
        for entry in [
            b"PATH=C:\\".as_slice(),
            b"COMSPEC=C:\\COMMAND.COM".as_slice(),
            b"PROMPT=$P$G".as_slice(),
        ] {
            self.bus.mem.copy_from(cursor, entry);
            cursor += entry.len() as u32;
            self.bus.mem.write8(cursor, 0);
            cursor += 1;
        }
        self.bus.mem.write16(cursor, 0);
        self.bus.mem.write8(base + 0x50, 0xCD);
        self.bus.mem.write8(base + 0x51, 0x21);
        self.bus.mem.write8(base + 0x52, 0xCB);
        let tail = tail.as_bytes();
        let len = tail.len().min(126);
        self.bus.mem.write8(base + 0x80, len as u8);
        self.bus.mem.write8(base + 0x81, 0x0D);
        for (index, byte) in tail.iter().take(len).enumerate() {
            self.bus.mem.write8(base + 0x82 + index as u32, *byte);
        }
        self.bus.mem.write8(base + 0x82 + len as u32, 0x0D);
    }

    fn load_com(&mut self, data: &[u8], tail: &str) -> bool {
        let psp = crate::dos::PSP_SEGMENT;
        let base = (psp as u32) << 4;
        self.bus.mem.fill(base, 0x1_0000, 0);
        self.write_psp(psp, tail);
        self.bus.mem.copy_from(base + 0x100, data);
        self.cpu.reset();
        self.cpu.r = [0; 8];
        self.cpu.r[crate::cpu::SP] = 0xFFFE;
        self.cpu.sregs = [psp, psp, psp, psp];
        self.cpu.ip = 0x100;
        self.cpu.flags = 0x0202;
        self.bus.mem.write16((psp as u32) * 16 + 0xFFFE, 0);
        true
    }

    fn load_exe(&mut self, data: &[u8], tail: &str) -> bool {
        if data.len() < 0x20 || &data[0..2] != b"MZ" {
            return false;
        }
        let word = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
        let last_page = word(2);
        let pages = word(4);
        let reloc_count = word(6);
        let header_paras = word(8);
        let ss = word(0x0E);
        let sp = word(0x10);
        let ip = word(0x14);
        let cs = word(0x16);
        let reloc_table = word(0x18);

        let image_offset = header_paras * 16;
        let mut module = if last_page == 0 {
            pages * 512
        } else {
            (pages.saturating_sub(1)) * 512 + last_page
        };
        module = module.saturating_sub(image_offset);
        module = module.min(data.len().saturating_sub(image_offset));
        if image_offset > data.len() {
            return false;
        }

        let psp = crate::dos::PSP_SEGMENT;
        let load_seg = psp.wrapping_add(0x10);
        let base = (psp as u32) << 4;
        self.bus.mem.fill(base, 0x1_0000, 0);
        self.write_psp(psp, tail);
        let load_phys = (load_seg as u32) << 4;
        self.bus
            .mem
            .copy_from(load_phys, &data[image_offset..image_offset + module]);

        for index in 0..reloc_count {
            let at = reloc_table + index * 4;
            if at + 4 > data.len() {
                break;
            }
            let offset = word(at);
            let segment = word(at + 2);
            let address =
                ((load_seg.wrapping_add(segment as u16) as u32) << 4).wrapping_add(offset as u32);
            let value = self.bus.mem.read16(address).wrapping_add(load_seg);
            self.bus.mem.write16(address, value);
        }

        self.cpu.reset();
        self.cpu.r = [0; 8];
        self.cpu.sregs[crate::cpu::ES] = psp;
        self.cpu.sregs[crate::cpu::DS] = psp;
        self.cpu.sregs[crate::cpu::CS] = load_seg.wrapping_add(cs as u16);
        self.cpu.sregs[crate::cpu::SS] = load_seg.wrapping_add(ss as u16);
        self.cpu.ip = ip as u16;
        self.cpu.r[crate::cpu::SP] = if sp == 0 { 0xFFFE } else { sp as u16 };
        self.cpu.flags = 0x0202;
        let stack = ((self.cpu.sregs[crate::cpu::SS] as u32) << 4)
            .wrapping_add(self.cpu.r[crate::cpu::SP] as u32);
        self.bus.mem.write16(stack, 0);
        true
    }

    fn run_cpu(&mut self, now_ms: u64, budget: u32) {
        for _ in 0..budget {
            match self.cpu.step(&mut self.bus) {
                Step::Ok => {}
                Step::Blocked => break,
                Step::Halted => break,
                Step::Fault(opcode) => {
                    let cs = self.cpu.sregs[crate::cpu::CS];
                    let ip = self.cpu.ip;
                    self.bus.vga.write_line(
                        &mut self.bus.mem,
                        alloc::format!(
                            "CPU fault: unsupported opcode {:02X}h at {:04X}:{:04X}",
                            opcode,
                            cs,
                            ip
                        )
                        .as_bytes(),
                    );
                    let start = self.cpu.trace_index.wrapping_sub(8);
                    for index in start..self.cpu.trace_index {
                        let entry = self.cpu.trace[index % crate::cpu::TRACE_LEN];
                        let line = alloc::format!(
                            " {:04X}:{:04X} {:02X} AX={:04X} BX={:04X} CX={:04X} DX={:04X}",
                            entry.cs,
                            entry.ip,
                            entry.opcode,
                            entry.ax,
                            entry.bx,
                            entry.cx,
                            entry.dx
                        );
                        self.bus.vga.write_line(&mut self.bus.mem, line.as_bytes());
                    }
                    self.bus.exit_requested = true;
                    self.bus.exit_code = 0xFF;
                    break;
                }
            }
        }

        if self.bus.bios.tick(&mut self.bus.mem, now_ms) {
            self.cpu.halted = false;
            if self.cpu.flags & FLAG_IF != 0 {
                let _ = self.cpu.inject_interrupt(&mut self.bus, 0x1C);
            }
        }
    }

    fn finish_program(&mut self) {
        let code = self.bus.exit_code;
        self.program = None;
        self.shell.running = false;
        self.bus.exit_requested = false;
        self.bus.exit_code = 0;
        self.bus.dos.find = None;
        self.bus.dos.line.active = false;
        self.cpu.halted = false;
        self.bus.vga.teletype(&mut self.bus.mem, b'\r');
        self.bus.vga.teletype(&mut self.bus.mem, b'\n');
        if code != 0 {
            let text = alloc::format!("Program exited with code {}.", code);
            self.bus.vga.write_line(&mut self.bus.mem, text.as_bytes());
        }
        if self.shell.batch.is_empty() {
            self.shell.show_prompt(
                &self.bus.dos,
                &mut self.bus.vga,
                &mut self.bus.mem,
                self.bus.now_ms,
            );
        }
    }

    /// Draws the text screen into a desktop canvas.
    pub fn render(&mut self, canvas: &mut feos_gfx::Canvas<'_>) {
        let blink = (self.bus.now_ms / 500) % 2 == 0;
        if blink != self.last_blink {
            self.bus.vga.dirty = true;
            self.last_blink = blink;
        }
        if self.bus.vga.dirty {
            self.bus.vga.render(&self.bus.mem, &mut self.screen, blink);
        }
        self.bus.vga.blit(canvas, &self.screen);
        if self.cursor_seen && self.bus.mouse.visible {
            let (x, y) = self.cursor_canvas;
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
    }
}

impl DosBox {
    /// Serializes the machine state to a compact byte buffer.
    ///
    /// Files on `C:` are not part of the state, matching DOSBox's save
    /// states: what a program wrote stays written.
    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1_500_000);
        out.extend_from_slice(b"FEOSDOS1");
        for value in self.cpu.r {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in self.cpu.sregs {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.cpu.ip.to_le_bytes());
        out.extend_from_slice(&self.cpu.flags.to_le_bytes());
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

        out.push(self.bus.vga.mode);
        out.push(match self.bus.vga.kind {
            crate::vga::GraphicsKind::Text => 0,
            crate::vga::GraphicsKind::Cga320 => 1,
            crate::vga::GraphicsKind::Cga640 => 2,
            crate::vga::GraphicsKind::Vga13 => 3,
            crate::vga::GraphicsKind::Planar => 4,
        });
        out.extend_from_slice(&(self.bus.vga.width as u32).to_le_bytes());
        out.extend_from_slice(&(self.bus.vga.height as u32).to_le_bytes());
        out.extend_from_slice(&(self.bus.vga.columns as u16).to_le_bytes());
        out.extend_from_slice(&(self.bus.vga.rows as u16).to_le_bytes());
        out.extend_from_slice(&(self.bus.vga.cursor.0 as u16).to_le_bytes());
        out.extend_from_slice(&(self.bus.vga.cursor.1 as u16).to_le_bytes());
        out.push(self.bus.vga.cursor_start);
        out.push(self.bus.vga.cursor_end);
        out.push(u8::from(self.bus.vga.cursor_enabled));
        out.extend_from_slice(&self.bus.vga.start_addr.to_le_bytes());
        for entry in self.bus.vga.palette {
            out.extend_from_slice(&entry);
        }
        out.extend_from_slice(&self.bus.vga.vram);
        out.push(self.bus.vga.plane_mask);
        out.push(self.bus.vga.read_plane);
        out.push(self.bus.vga.set_reset);
        out.push(self.bus.vga.enable_set_reset);
        out.push(self.bus.vga.bit_mask);
        out.push(self.bus.vga.logic_op);
        out.extend_from_slice(&self.bus.vga.crtc);

        out.extend_from_slice(&self.bus.mouse.x.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.y.to_le_bytes());
        out.push(self.bus.mouse.buttons);
        out.push(u8::from(self.bus.mouse.visible));
        out.extend_from_slice(&self.bus.mouse.min_x.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.max_x.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.min_y.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.max_y.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.handler_mask.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.handler.0.to_le_bytes());
        out.extend_from_slice(&self.bus.mouse.handler.1.to_le_bytes());

        out.extend_from_slice(&(self.bus.dos.cwd as u32).to_le_bytes());
        out.extend_from_slice(&self.bus.dos.dta.to_le_bytes());
        out.extend_from_slice(&self.bus.dos.psp.to_le_bytes());
        out.push(self.bus.dos.drive);
        for slot in &self.bus.dos.handles {
            match slot {
                Some(handle) => {
                    out.push(1);
                    out.extend_from_slice(&(handle.node as u32).to_le_bytes());
                    out.extend_from_slice(&(handle.offset as u32).to_le_bytes());
                    out.push(handle.mode);
                    out.push(handle.device);
                }
                None => out.push(0),
            }
        }
        out.extend_from_slice(&self.bus.bios.ticks.to_le_bytes());
        out
    }

    /// Restores a state written by [`save_state`](Self::save_state).
    pub fn load_state(&mut self, data: &[u8]) -> bool {
        let mut at = 0usize;
        let Some(header) = take(data, &mut at, 8) else {
            return false;
        };
        if header != b"FEOSDOS1" {
            return false;
        }
        let Some(values) = take(data, &mut at, 16) else {
            return false;
        };
        for index in 0..8 {
            self.cpu.r[index] = u16::from_le_bytes([values[index * 2], values[index * 2 + 1]]);
        }
        let Some(values) = take(data, &mut at, 8) else {
            return false;
        };
        for index in 0..4 {
            self.cpu.sregs[index] = u16::from_le_bytes([values[index * 2], values[index * 2 + 1]]);
        }
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.cpu.ip = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.cpu.flags = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.cpu.halted = value != 0;
        let Some(value) = read_u64(data, &mut at) else {
            return false;
        };
        self.cpu.cycles = value;

        let Some(top) = read_u8(data, &mut at) else {
            return false;
        };
        self.cpu.fpu.top = top & 7;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.cpu.fpu.control = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.cpu.fpu.status = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.cpu.fpu.tags = value;
        for index in 0..8 {
            let Some(bytes) = take(data, &mut at, 10) else {
                return false;
            };
            let mut value = [0u8; 10];
            value.copy_from_slice(bytes);
            self.cpu.fpu.st[index].set_bytes(&value);
        }

        let Some(memory) = take(data, &mut at, 0x10_0000) else {
            return false;
        };
        self.bus.mem.as_mut_slice().copy_from_slice(memory);

        let Some(mode) = read_u8(data, &mut at) else {
            return false;
        };
        let Some(kind) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.mode = mode;
        self.bus.vga.kind = match kind {
            1 => crate::vga::GraphicsKind::Cga320,
            2 => crate::vga::GraphicsKind::Cga640,
            3 => crate::vga::GraphicsKind::Vga13,
            4 => crate::vga::GraphicsKind::Planar,
            _ => crate::vga::GraphicsKind::Text,
        };
        let Some(value) = read_u32(data, &mut at) else {
            return false;
        };
        self.bus.vga.width = value as usize;
        let Some(value) = read_u32(data, &mut at) else {
            return false;
        };
        self.bus.vga.height = value as usize;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.vga.columns = value as usize;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.vga.rows = value as usize;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.vga.cursor.0 = value as usize;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.vga.cursor.1 = value as usize;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.cursor_start = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.cursor_end = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.cursor_enabled = value != 0;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.vga.start_addr = value;
        let Some(palette) = take(data, &mut at, 768) else {
            return false;
        };
        for index in 0..256 {
            self.bus.vga.palette[index].copy_from_slice(&palette[index * 3..index * 3 + 3]);
        }
        let Some(vram) = take(data, &mut at, 4 * 0x1_0000) else {
            return false;
        };
        self.bus.vga.vram.copy_from_slice(vram);
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.plane_mask = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.read_plane = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.set_reset = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.enable_set_reset = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.bit_mask = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.vga.logic_op = value;
        let Some(crtc) = take(data, &mut at, 32) else {
            return false;
        };
        self.bus.vga.crtc.copy_from_slice(crtc);

        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.x = value;
        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.y = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.mouse.buttons = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.mouse.visible = value != 0;
        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.min_x = value;
        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.max_x = value;
        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.min_y = value;
        let Some(value) = read_i32(data, &mut at) else {
            return false;
        };
        self.bus.mouse.max_y = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.mouse.handler_mask = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.mouse.handler.0 = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.mouse.handler.1 = value;

        let Some(value) = read_u32(data, &mut at) else {
            return false;
        };
        if (value as usize) < self.bus.dos.disk.nodes.len() {
            self.bus.dos.cwd = value as usize;
        }
        let Some(value) = read_u32(data, &mut at) else {
            return false;
        };
        self.bus.dos.dta = value;
        let Some(value) = read_u16(data, &mut at) else {
            return false;
        };
        self.bus.dos.psp = value;
        let Some(value) = read_u8(data, &mut at) else {
            return false;
        };
        self.bus.dos.drive = value;
        for slot in &mut self.bus.dos.handles {
            let Some(present) = read_u8(data, &mut at) else {
                return false;
            };
            if present == 0 {
                *slot = None;
                continue;
            }
            let Some(node) = read_u32(data, &mut at) else {
                return false;
            };
            let Some(offset) = read_u32(data, &mut at) else {
                return false;
            };
            let Some(mode) = read_u8(data, &mut at) else {
                return false;
            };
            let Some(device) = read_u8(data, &mut at) else {
                return false;
            };
            *slot = Some(crate::dos::OpenFile {
                node: node as usize,
                offset: offset as usize,
                mode,
                device,
            });
        }
        let Some(ticks) = read_u32(data, &mut at) else {
            return false;
        };
        self.bus.bios.ticks = ticks;
        self.bus.keyboard.clear();
        self.bus.opl.reset(self.audio_rate);
        self.bus.vga.dirty = true;
        self.bus.vga.sync_bios(&mut self.bus.mem);
        self.program = None;
        self.shell.running = false;
        true
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

impl SystemBus {
    fn disk_is_file(&self, node: usize) -> bool {
        self.dos.disk.is_file(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_batch(dos: &mut DosBox) {
        for _ in 0..16 {
            if dos.shell.batch.is_empty() {
                break;
            }
            dos.frame(0, 10_000);
        }
    }

    #[test]
    fn boots_and_shows_prompt() {
        let mut dos = DosBox::new(None);
        drain_batch(&mut dos);
        let text = dos.text();
        assert!(text.contains("FEOS-DOS"));
        assert!(text.contains("C:\\>"));
    }

    #[test]
    fn runs_a_real_com_program() {
        let mut dos = DosBox::new(None);
        drain_batch(&mut dos);
        assert!(dos.run("HELLO.COM"));
        for _ in 0..32 {
            dos.frame(0, 100_000);
            if dos.text().contains("Hello from FEOS-DOS!") {
                break;
            }
        }
        assert!(dos.text().contains("Hello from FEOS-DOS!"));
    }

    #[test]
    fn audio_mix_produces_stereo_samples() {
        let mut dos = DosBox::new(None);
        dos.set_audio_rate(48_000);
        // Key on a simple OPL patch so the buffer is not silence.
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
            dos.bus.opl.write_address(0x388, register);
            dos.bus.opl.write_data(0x389, value);
        }
        dos.frame(16, 10_000);
        let samples = dos.take_audio();
        assert_eq!(samples.len(), (48_000 * 16 / 1000) * 2);
        assert!(samples.iter().any(|sample| *sample != 0));
        assert!(dos.take_audio().is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut dos = DosBox::new(None);
        drain_batch(&mut dos);
        for byte in b"echo hello\r" {
            dos.push_key(*byte, 0);
        }
        dos.frame(0, 10_000);
        let before = dos.text();
        let state = dos.save_state();
        dos.bus.vga.clear(&mut dos.bus.mem, 0x07);
        assert_ne!(dos.text(), before);
        assert!(dos.load_state(&state));
        assert_eq!(dos.text(), before);
    }

    #[test]
    fn dir_command_lists_readme() {
        let mut dos = DosBox::new(None);
        drain_batch(&mut dos);
        for byte in b"dir\r" {
            dos.push_key(*byte, 0);
        }
        dos.frame(0, 10_000);
        assert!(dos.text().contains("README.TXT"));
    }
}
