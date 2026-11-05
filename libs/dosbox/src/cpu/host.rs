//! What the machine's own services need from the CPU: host calls, the monitor for
//! virtual-8086 mode, and setting the whole mode at once, the way a mode switch in
//! a host's ring 0 code would. See [`crate::dpmi`] and [`crate::emm386`].

use super::*;

/// Everything a host saves about a client in protected mode, or real mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Context {
    pub r: [u32; 8],
    pub sregs: [u16; 6],
    pub ip: u32,
    pub flags: u32,
}

impl Cpu {
    pub(crate) fn host_trap<B: Bus>(&mut self, bus: &mut B, code: u8) -> Exec {
        self.materialize();
        bus.sync(self.cycles);
        let action = bus.host_call(self, code);
        self.cycles = self.cycles.wrapping_add(bus.take_cost() as u64);
        match action {
            InterruptAction::Blocked => Err(Trap::Blocked),
            InterruptAction::Halt => Err(Trap::Terminated),
            InterruptAction::Continue => Ok(()),
        }
    }

    /// The monitor's view of a `#GP` in virtual-8086 mode. It has IOPL 3 and no
    /// I/O bitmap, so only the privileged instructions get here.
    pub(super) fn monitor_gp<B: Bus>(&mut self, bus: &mut B) -> Step {
        let base = self.seg[CS].base.wrapping_add(self.ip);
        let opcode = bus.read8(base);
        if opcode == 0xF4 {
            self.ip = (self.ip + 1) & 0xFFFF;
            self.halted = true;
            return Step::Halted;
        }
        if opcode == 0x0F {
            let second = bus.read8(base.wrapping_add(1));
            let modrm = bus.read8(base.wrapping_add(2));
            let (reg, rm) = (((modrm >> 3) & 7) as usize, (modrm & 7) as usize);
            let known = match second {
                0x20 if matches!(reg, 0 | 2 | 3 | 4) => {
                    self.r[rm] = match reg {
                        0 => self.cr0,
                        2 => self.cr2,
                        3 => self.cr3,
                        _ => self.cr4,
                    };
                    true
                }
                0x21 => {
                    self.r[rm] = self.dr[reg];
                    true
                }
                0x23 => {
                    self.dr[reg] = self.r[rm];
                    true
                }
                // Clearing the task-switched bit or caches changes nothing here.
                0x06 | 0x08 | 0x09 => {
                    self.ip = (self.ip + 2) & 0xFFFF;
                    return Step::Ok;
                }
                _ => false,
            };
            if known {
                self.ip = (self.ip + 3) & 0xFFFF;
                return Step::Ok;
            }
        }
        // Anything else would be switching modes behind the monitor's back.
        bus.host_call(self, HOST_MONITOR_FAULT);
        Step::Halted
    }

    /// Whether the machine's monitor runs virtual-8086 mode.
    pub fn v86_monitor(&self) -> bool {
        self.v86_monitor
    }

    /// Turns the machine's monitor on or off, and enters or leaves virtual-8086
    /// mode with the current registers. It's only for the moment a program starts.
    pub(crate) fn set_v86_monitor(&mut self, on: bool) {
        self.v86_monitor = on;
        if on {
            self.cr0 |= 1;
            self.eflags |= FLAG_IOPL;
            let sregs = self.sregs;
            self.enter_v86_segments(sregs);
        }
    }

    /// The registers as they are.
    pub(crate) fn context(&self) -> Context {
        Context {
            r: self.r,
            sregs: self.sregs,
            ip: self.ip,
            flags: self.flags(),
        }
    }

    /// Sets real mode, or virtual-8086 mode under the monitor, with 64 KiB segments.
    pub(crate) fn enter_real_context(&mut self, context: &Context) {
        self.r = context.r;
        self.ip = context.ip & 0xFFFF;
        self.lazy = 0;
        self.eflags = (context.flags | 2) & 0x0000_7FD7;
        self.idtr = TableReg {
            base: 0,
            limit: 0x3FF,
        };
        self.halted = false;
        self.inhibit = false;
        self.paging = false;
        if self.v86_monitor {
            self.eflags |= FLAG_IOPL;
            self.cr0 = (self.cr0 | 1 | 0x10) & 0x7FFF_FFFF;
            self.enter_v86_segments(context.sregs);
        } else {
            self.cr0 = (self.cr0 | 0x10) & 0x7FFF_FFFE;
            self.pmode = false;
            self.vm86 = false;
            self.cpl = 0;
            self.eflags &= !FLAG_IOPL;
            for index in 0..6 {
                self.sregs[index] = context.sregs[index];
                self.seg[index] = Seg::real(context.sregs[index]);
            }
            self.seg[CS].code = true;
        }
        self.flush_tlb();
    }

    /// Sets protected mode at the code selector's RPL, loading every segment as
    /// a mode switch from level 0 would. A data selector that has gone bad reads
    /// as null, and the client faults on it when it's used.
    pub(crate) fn enter_protected_context<B: Bus>(
        &mut self,
        bus: &mut B,
        context: &Context,
    ) -> Exec {
        self.vm86 = false;
        self.pmode = true;
        self.cr0 |= 1 | 0x10;
        self.halted = false;
        self.inhibit = false;
        self.r = context.r;
        self.ip = context.ip;
        self.lazy = 0;
        self.eflags = (context.flags | 2) & 0x0000_7FD7;
        self.cpl = (context.sregs[CS] & 3) as u8;
        let code = self.read_desc_for_host(bus, context.sregs[CS])?;
        self.set_cs_for_host(context.sregs[CS], code);
        self.load_seg(bus, SS, context.sregs[SS])?;
        for index in [ES, DS, FS, GS] {
            if self.load_seg(bus, index, context.sregs[index]).is_err() {
                self.sregs[index] = 0;
                self.seg[index] = Self::null_seg();
            }
        }
        Ok(())
    }

    /// Reads a value from a client's memory through its segments.
    pub(crate) fn host_read<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, w: u32) -> Exec<u32> {
        self.rdw(bus, seg, off, w)
    }

    /// Sets a VCPI client's system registers and enters protected mode at level 0.
    pub(crate) fn enter_vcpi_client<B: Bus>(
        &mut self,
        bus: &mut B,
        cr3: u32,
        gdtr: TableReg,
        idtr: TableReg,
        ldt: u16,
        tr: u16,
        cs: u16,
        eip: u32,
    ) -> Exec {
        self.vm86 = false;
        self.pmode = true;
        self.cpl = 0;
        self.gdtr = gdtr;
        self.idtr = idtr;
        self.cr3 = cr3;
        self.lazy = 0;
        self.eflags = (self.eflags | 2) & 0x0000_0FD7;
        self.cr0 = 0x8000_0011;
        self.paging = true;
        self.flush_tlb();
        self.load_ldt(bus, ldt)?;
        if tr & !3 != 0 {
            self.load_tr_clearing_busy(bus, tr)?;
        }
        let code = self.read_desc_for_host(bus, cs)?;
        self.set_cs_for_host(cs, code);
        self.ip = eip;
        Ok(())
    }
}
