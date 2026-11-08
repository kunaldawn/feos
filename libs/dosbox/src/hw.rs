//! The motherboard's timing and interrupt chips: two 8259 PICs, an 8254 PIT,
//! the 8042 keyboard controller's status path and the 8237 DMA controller.

/// One 8259.
#[derive(Clone, Debug)]
pub struct Pic {
    pub irr: u8,
    pub isr: u8,
    pub imr: u8,
    pub base: u8,
    init_step: u8,
    needs_icw4: bool,
    single: bool,
    auto_eoi: bool,
    read_isr: bool,
}

impl Pic {
    fn new(base: u8, imr: u8) -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr,
            base,
            init_step: 0,
            needs_icw4: false,
            single: false,
            auto_eoi: false,
            read_isr: false,
        }
    }

    /// The highest-priority request that can interrupt what's in service.
    fn pending(&self) -> Option<u8> {
        let ready = self.irr & !self.imr;
        for line in 0..8 {
            if self.isr & (1 << line) != 0 {
                return None;
            }
            if ready & (1 << line) != 0 {
                return Some(line);
            }
        }
        None
    }

    fn command(&mut self, value: u8) {
        if value & 0x10 != 0 {
            // ICW1
            self.init_step = 1;
            self.needs_icw4 = value & 1 != 0;
            self.single = value & 2 != 0;
            self.imr = 0;
            self.isr = 0;
            self.irr = 0;
            self.auto_eoi = false;
            self.read_isr = false;
        } else if value & 0x08 != 0 {
            // OCW3
            if value & 2 != 0 {
                self.read_isr = value & 1 != 0;
            }
        } else {
            // OCW2
            match value >> 5 {
                1 | 5 => {
                    // Non-specific EOI clears the highest-priority in-service bit.
                    if self.isr != 0 {
                        self.isr &= self.isr - 1;
                    }
                }
                3 | 7 => self.isr &= !(1 << (value & 7)),
                _ => {}
            }
        }
    }

    fn data(&mut self, value: u8) {
        match self.init_step {
            1 => {
                self.base = value & 0xF8;
                self.init_step = if self.single {
                    if self.needs_icw4 {
                        3
                    } else {
                        0
                    }
                } else {
                    2
                };
            }
            2 => {
                self.init_step = if self.needs_icw4 { 3 } else { 0 };
            }
            3 => {
                self.auto_eoi = value & 2 != 0;
                self.init_step = 0;
            }
            _ => self.imr = value,
        }
    }

    fn read_command(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }
}

/// The master and slave 8259s.
#[derive(Clone, Debug)]
pub struct Pics {
    pub master: Pic,
    pub slave: Pic,
    /// Whether anything can interrupt the CPU right now.
    pending: bool,
}

impl Default for Pics {
    fn default() -> Self {
        Self::new()
    }
}

impl Pics {
    pub fn new() -> Self {
        Self {
            // The BIOS's masks: timer, keyboard, cascade and floppy enabled.
            master: Pic::new(0x08, 0xB8),
            slave: Pic::new(0x70, 0x00),
            pending: false,
        }
    }

    fn update(&mut self) {
        if self.slave.pending().is_some() {
            self.master.irr |= 1 << 2;
        } else {
            self.master.irr &= !(1 << 2);
        }
        self.pending = self.master.pending().is_some();
    }

    /// Raises an IRQ line, 0 to 15.
    pub fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.master.irr |= 1 << irq;
        } else {
            self.slave.irr |= 1 << (irq & 7);
        }
        self.update();
    }

    /// Withdraws a request that hasn't been acknowledged.
    pub fn lower(&mut self, irq: u8) {
        if irq < 8 {
            self.master.irr &= !(1 << irq);
        } else {
            self.slave.irr &= !(1 << (irq & 7));
        }
        self.update();
    }

    /// Whether an unmasked interrupt is waiting.
    #[inline(always)]
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Acknowledges the waiting interrupt and returns its vector.
    pub fn acknowledge(&mut self) -> Option<u8> {
        let line = self.master.pending()?;
        
        let vector = if line == 2 {
            let Some(inner) = self.slave.pending() else {
                self.update();
                return None;
            };
            self.slave.irr &= !(1 << inner);
            if !self.slave.auto_eoi {
                self.slave.isr |= 1 << inner;
            }
            self.slave.base.wrapping_add(inner)
        } else {
            self.master.base.wrapping_add(line)
        };
        if line != 2 {
            self.master.irr &= !(1 << line);
        }
        if !self.master.auto_eoi {
            self.master.isr |= 1 << line;
        }
        self.update();
        Some(vector)
    }

    pub fn port_out(&mut self, port: u16, value: u8) {
        match port {
            0x20 => self.master.command(value),
            0x21 => self.master.data(value),
            0xA0 => self.slave.command(value),
            _ => self.slave.data(value),
        }
        self.update();
    }

    pub fn port_in(&mut self, port: u16) -> u8 {
        match port {
            0x20 => self.master.read_command(),
            0x21 => self.master.imr,
            0xA0 => self.slave.read_command(),
            _ => self.slave.imr,
        }
    }
}

/// One 8254 counter. Its count is derived from the tick it was loaded at.
#[derive(Clone, Debug)]
pub struct PitChannel {
    pub mode: u8,
    /// 1 low byte, 2 high byte, 3 low then high.
    pub access: u8,
    pub bcd: bool,
    /// The loaded count, with 0 meaning 65536.
    pub reload: u32,
    /// The tick the count was loaded at.
    pub start: u64,
    pub loaded: bool,
    write_low: u8,
    write_high_next: bool,
    read_high_next: bool,
    latch: Option<u16>,
    status_latch: Option<u8>,
    /// The gate, for channel 2.
    pub gate: bool,
}

impl PitChannel {
    fn new() -> Self {
        Self {
            mode: 3,
            access: 3,
            bcd: false,
            reload: 0x1_0000,
            start: 0,
            loaded: false,
            write_low: 0,
            write_high_next: false,
            read_high_next: false,
            latch: None,
            status_latch: None,
            gate: true,
        }
    }

    /// The counter's value at a tick.
    pub fn count(&self, tick: u64) -> u16 {
        if !self.loaded {
            return 0;
        }
        let elapsed = tick.saturating_sub(self.start);
        let reload = self.reload as u64;
        let value = match self.mode {
            2 => reload - (elapsed % reload),
            3 => {
                let half = (reload / 2).max(1);
                let phase = elapsed % half;
                reload.saturating_sub(phase * 2).max(2) & !1
            }
            _ => {
                if elapsed < reload {
                    reload - elapsed
                } else {
                    // After terminal count the counter keeps wrapping.
                    0x1_0000 - ((elapsed - reload) & 0xFFFF)
                }
            }
        };
        value as u16
    }

    /// The output pin's level at a tick, for channel 2's speaker read-back.
    pub fn output(&self, tick: u64) -> bool {
        if !self.loaded {
            return self.mode != 0;
        }
        let elapsed = tick.saturating_sub(self.start);
        let reload = self.reload as u64;
        match self.mode {
            0 | 1 => elapsed >= reload,
            2 => elapsed % reload != reload - 1,
            3 => elapsed % reload < reload.div_ceil(2),
            4 | 5 => elapsed != reload,
            _ => true,
        }
    }

    fn write(&mut self, value: u8, tick: u64) -> bool {
        let raw = match self.access {
            1 => value as u32,
            2 => (value as u32) << 8,
            _ => {
                if !self.write_high_next {
                    self.write_low = value;
                    self.write_high_next = true;
                    return false;
                }
                self.write_high_next = false;
                self.write_low as u32 | ((value as u32) << 8)
            }
        };
        self.reload = if raw == 0 { 0x1_0000 } else { raw };
        self.start = tick;
        self.loaded = true;
        true
    }

    fn read(&mut self, tick: u64) -> u8 {
        if let Some(status) = self.status_latch.take() {
            return status;
        }
        let count = self.latch.unwrap_or_else(|| self.count(tick));
        match self.access {
            1 => {
                self.latch = None;
                count as u8
            }
            2 => {
                self.latch = None;
                (count >> 8) as u8
            }
            _ => {
                if !self.read_high_next {
                    self.read_high_next = true;
                    if self.latch.is_none() {
                        self.latch = Some(count);
                    }
                    count as u8
                } else {
                    self.read_high_next = false;
                    self.latch = None;
                    (count >> 8) as u8
                }
            }
        }
    }

    fn control(&mut self, value: u8, tick: u64) {
        let access = (value >> 4) & 3;
        if access == 0 {
            if self.latch.is_none() {
                self.latch = Some(self.count(tick));
            }
            return;
        }
        self.access = access;
        self.mode = match (value >> 1) & 7 {
            6 => 2,
            7 => 3,
            mode => mode,
        };
        self.bcd = value & 1 != 0;
        self.write_high_next = false;
        self.read_high_next = false;
        self.latch = None;
        self.status_latch = None;
        self.loaded = false;
    }

    fn read_back(&mut self, value: u8, tick: u64) {
        if value & 0x20 == 0 && self.latch.is_none() {
            self.latch = Some(self.count(tick));
        }
        if value & 0x10 == 0 {
            let output = u8::from(self.output(tick)) << 7;
            let null = u8::from(!self.loaded) << 6;
            self.status_latch =
                Some(output | null | (self.access << 4) | (self.mode << 1) | u8::from(self.bcd));
        }
    }
}

/// The 8254.
#[derive(Clone, Debug)]
pub struct Pit {
    pub channels: [PitChannel; 3],
    /// The tick channel 0 next raises IRQ 0 at.
    pub next_irq: Option<u64>,
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl Pit {
    pub fn new() -> Self {
        let mut pit = Self {
            channels: [PitChannel::new(), PitChannel::new(), PitChannel::new()],
            next_irq: None,
        };
        // The BIOS programs channel 0 for 18.2 Hz.
        pit.channels[0].loaded = true;
        pit.next_irq = Some(0x1_0000);
        pit
    }

    pub fn port_out(&mut self, port: u16, value: u8, tick: u64) {
        if port == 0x43 {
            let select = value >> 6;
            if select == 3 {
                for index in 0..3 {
                    if value & (2 << index) != 0 {
                        self.channels[index].read_back(value, tick);
                    }
                }
                return;
            }
            self.channels[select as usize].control(value, tick);
            if select == 0 && value & 0x30 != 0 {
                self.next_irq = None;
            }
            return;
        }
        let index = (port & 3) as usize;
        if index > 2 {
            return;
        }
        let loaded = self.channels[index].write(value, tick);
        if loaded && index == 0 {
            self.next_irq = Some(tick + self.channels[0].reload as u64);
        }
    }

    pub fn port_in(&mut self, port: u16, tick: u64) -> u8 {
        let index = (port & 3) as usize;
        if index > 2 {
            return 0xFF;
        }
        self.channels[index].read(tick)
    }

    /// Fires channel 0 if it's due at `tick`. Returns whether IRQ 0 fired.
    pub fn advance(&mut self, tick: u64) -> bool {
        let Some(due) = self.next_irq else {
            return false;
        };
        if tick < due {
            return false;
        }
        let channel = &self.channels[0];
        self.next_irq = match channel.mode {
            2 | 3 => {
                let reload = channel.reload as u64;
                let missed = (tick - due) / reload;
                Some(due + (missed + 1) * reload)
            }
            _ => None,
        };
        true
    }
}

/// One 8237 channel.
#[derive(Clone, Debug, Default)]
pub struct DmaChannel {
    pub address: u16,
    pub count: u16,
    pub base_address: u16,
    pub base_count: u16,
    pub page: u8,
    pub mode: u8,
    pub masked: bool,
    /// Whether the last transfer reached terminal count, until status is read.
    pub terminal: bool,
}

/// The two 8237s: channels 0 to 3 move bytes, 4 to 7 move words.
#[derive(Clone, Debug)]
pub struct Dma {
    pub channels: [DmaChannel; 8],
    flip: [bool; 2],
}

impl Default for Dma {
    fn default() -> Self {
        Self::new()
    }
}

impl Dma {
    pub fn new() -> Self {
        let mut channels: [DmaChannel; 8] = Default::default();
        for channel in channels.iter_mut() {
            channel.masked = true;
        }
        Self {
            channels,
            flip: [false; 2],
        }
    }

    /// Whether a channel can transfer now.
    #[inline]
    pub fn ready(&self, channel: usize) -> bool {
        self.channels.get(channel).is_some_and(|c| !c.masked)
    }

    /// Moves a channel on by one unit: it returns the unit's physical address and
    /// whether that reached terminal count.
    pub fn step(&mut self, index: usize) -> (u32, bool) {
        let channel = &mut self.channels[index];
        let address = if index >= 4 {
            // The word address shifts up, and the page's lowest bit drops out.
            (((channel.page & 0xFE) as u32) << 16) | ((channel.address as u32) << 1)
        } else {
            ((channel.page as u32) << 16) | channel.address as u32
        };
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
        (address, wrapped)
    }

    /// The channel that page register `port` belongs to.
    fn page_channel(port: u16) -> Option<usize> {
        Some(match port {
            0x87 => 0,
            0x83 => 1,
            0x81 => 2,
            0x82 => 3,
            0x8F => 4,
            0x8B => 5,
            0x89 => 6,
            0x8A => 7,
            _ => return None,
        })
    }

    fn write_register(&mut self, controller: usize, reg: u16, value: u8) {
        let first = controller * 4;
        match reg {
            0..=7 => {
                let channel = &mut self.channels[first + (reg / 2) as usize];
                let high = self.flip[controller];
                self.flip[controller] = !high;
                let merge = |old: u16| {
                    if high {
                        (old & 0x00FF) | ((value as u16) << 8)
                    } else {
                        (old & 0xFF00) | value as u16
                    }
                };
                if reg & 1 == 0 {
                    channel.base_address = merge(channel.base_address);
                    channel.address = channel.base_address;
                } else {
                    channel.base_count = merge(channel.base_count);
                    channel.count = channel.base_count;
                }
            }
            0x0A => self.channels[first + (value & 3) as usize].masked = value & 4 != 0,
            0x0B => self.channels[first + (value & 3) as usize].mode = value,
            0x0C => self.flip[controller] = false,
            0x0D => {
                self.flip[controller] = false;
                for channel in &mut self.channels[first..first + 4] {
                    channel.masked = true;
                }
            }
            0x0E => {
                for channel in &mut self.channels[first..first + 4] {
                    channel.masked = false;
                }
            }
            0x0F => {
                for (index, channel) in self.channels[first..first + 4].iter_mut().enumerate() {
                    channel.masked = value & (1 << index) != 0;
                }
            }
            _ => {}
        }
    }

    fn read_register(&mut self, controller: usize, reg: u16) -> u8 {
        let first = controller * 4;
        match reg {
            0..=7 => {
                let channel = &self.channels[first + (reg / 2) as usize];
                let value = if reg & 1 == 0 {
                    channel.address
                } else {
                    channel.count
                };
                let high = self.flip[controller];
                self.flip[controller] = !high;
                if high {
                    (value >> 8) as u8
                } else {
                    value as u8
                }
            }
            0x08 => {
                let mut status = 0;
                for (index, channel) in self.channels[first..first + 4].iter_mut().enumerate() {
                    if channel.terminal {
                        status |= 1 << index;
                    }
                    channel.terminal = false;
                }
                status
            }
            _ => 0xFF,
        }
    }

    pub fn port_out(&mut self, port: u16, value: u8) {
        match port {
            0x00..=0x0F => self.write_register(0, port, value),
            // The second controller's registers sit on even ports.
            0xC0..=0xDF => self.write_register(1, (port - 0xC0) / 2, value),
            _ => {
                if let Some(channel) = Self::page_channel(port) {
                    self.channels[channel].page = value;
                }
            }
        }
    }

    pub fn port_in(&mut self, port: u16) -> u8 {
        match port {
            0x00..=0x0F => self.read_register(0, port),
            0xC0..=0xDF => self.read_register(1, (port - 0xC0) / 2),
            _ => match Self::page_channel(port) {
                Some(channel) => self.channels[channel].page,
                None => 0xFF,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pic_delivers_in_priority_order_until_eoi() {
        let mut pics = Pics::new();
        pics.port_out(0x21, 0);
        pics.raise(1);
        pics.raise(0);
        assert_eq!(pics.acknowledge(), Some(8));
        assert!(!pics.pending(), "IRQ 1 waits for the EOI of IRQ 0");
        pics.port_out(0x20, 0x20);
        assert_eq!(pics.acknowledge(), Some(9));
        assert_eq!(pics.acknowledge(), None);
    }

    #[test]
    fn pic_masks_and_cascades_to_the_slave() {
        let mut pics = Pics::new();
        pics.port_out(0x21, 0xFF);
        pics.raise(0);
        assert!(!pics.pending());
        pics.port_out(0x21, 0xFB);
        pics.port_out(0xA1, 0);
        pics.raise(12);
        assert_eq!(pics.acknowledge(), Some(0x74));
        assert_eq!(pics.slave.isr, 1 << 4);
    }

    #[test]
    fn pic_reinitialises_with_a_new_base() {
        let mut pics = Pics::new();
        for (port, value) in [(0x20, 0x11), (0x21, 0x50), (0x21, 0x04), (0x21, 0x01)] {
            pics.port_out(port, value);
        }
        pics.port_out(0x21, 0);
        pics.raise(1);
        assert_eq!(pics.acknowledge(), Some(0x51));
    }

    #[test]
    fn pit_counts_down_and_latches() {
        let mut pit = Pit::new();
        pit.port_out(0x43, 0x34, 0);
        pit.port_out(0x40, 0x00, 100);
        pit.port_out(0x40, 0x10, 100);
        assert_eq!(pit.channels[0].reload, 0x1000);
        pit.port_out(0x43, 0x00, 150);
        assert_eq!(pit.port_in(0x40, 500), (0x1000 - 50) as u8);
        assert_eq!(pit.port_in(0x40, 500), ((0x1000 - 50) >> 8) as u8);
        assert_eq!(pit.next_irq, Some(100 + 0x1000));
        assert!(!pit.advance(100 + 0xFFF));
        assert!(pit.advance(100 + 0x1000));
        assert_eq!(pit.next_irq, Some(100 + 0x2000));
    }

    #[test]
    fn pit_one_shot_mode_fires_once() {
        let mut pit = Pit::new();
        pit.port_out(0x43, 0x30, 0);
        pit.port_out(0x40, 10, 0);
        pit.port_out(0x40, 0, 0);
        assert!(pit.advance(10));
        assert_eq!(pit.next_irq, None);
    }

    #[test]
    fn dma_reads_back_through_the_flip_flop() {
        let mut dma = Dma::new();
        dma.port_out(0x0C, 0);
        dma.port_out(2, 0x34);
        dma.port_out(2, 0x12);
        dma.port_out(3, 0xFF);
        dma.port_out(3, 0x00);
        dma.port_out(0x83, 0x07);
        assert_eq!(dma.channels[1].address, 0x1234);
        assert_eq!(dma.channels[1].count, 0xFF);
        assert_eq!(dma.channels[1].page, 7);
        dma.port_out(0x0A, 1);
        assert!(!dma.channels[1].masked);
    }
}
