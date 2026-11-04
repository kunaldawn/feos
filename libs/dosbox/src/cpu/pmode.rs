//! Protected-mode segmentation, gates, privilege changes and task switches.

use super::*;

/// A raw descriptor, as the two dwords stored in a table.
#[derive(Clone, Copy)]
pub(crate) struct Desc {
    lo: u32,
    hi: u32,
}

impl Desc {
    fn present(self) -> bool {
        self.hi & 0x8000 != 0
    }
    fn dpl(self) -> u8 {
        ((self.hi >> 13) & 3) as u8
    }
    /// Code or data, rather than a system descriptor.
    fn is_segment(self) -> bool {
        self.hi & 0x1000 != 0
    }
    fn kind(self) -> u8 {
        ((self.hi >> 8) & 0xF) as u8
    }
    fn is_code(self) -> bool {
        self.is_segment() && self.kind() & 8 != 0
    }
    fn conforming(self) -> bool {
        self.is_code() && self.kind() & 4 != 0
    }
    fn base(self) -> u32 {
        (self.lo >> 16) | ((self.hi & 0xFF) << 16) | (self.hi & 0xFF00_0000)
    }
    fn limit(self) -> u32 {
        let raw = (self.lo & 0xFFFF) | (self.hi & 0xF_0000);
        if self.hi & 0x80_0000 != 0 {
            (raw << 12) | 0xFFF
        } else {
            raw
        }
    }
    fn to_seg(self) -> Seg {
        let kind = self.kind();
        let code = self.is_code();
        let expand_down = !code && kind & 4 != 0;
        let big = self.hi & 0x40_0000 != 0;
        let limit = self.limit();
        let (lo, hi) = if expand_down {
            (limit.wrapping_add(1), if big { u32::MAX } else { 0xFFFF })
        } else {
            (0, limit)
        };
        Seg {
            base: self.base(),
            lo,
            hi,
            big,
            readable: !code || kind & 2 != 0,
            writable: !code && kind & 2 != 0,
            code,
            conforming: code && kind & 4 != 0,
            dpl: self.dpl(),
        }
    }
}

const NULL_SEG: Seg = Seg {
    base: 0,
    lo: 1,
    hi: 0,
    big: false,
    readable: false,
    writable: false,
    code: false,
    conforming: false,
    dpl: 0,
};

fn gp(selector: u16) -> Trap {
    Trap::FaultCode(13, (selector & !3) as u32)
}

fn np(selector: u16) -> Trap {
    Trap::FaultCode(11, (selector & !3) as u32)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Switch {
    Jump,
    Call,
    Iret,
}

impl Cpu {
    fn desc_addr(&self, selector: u16) -> Exec<u32> {
        let index = (selector & !7) as u32;
        let (base, limit) = if selector & 4 != 0 {
            if self.ldtr.sel == 0 {
                return Err(gp(selector));
            }
            (self.ldtr.base, self.ldtr.limit)
        } else {
            (self.gdtr.base, self.gdtr.limit)
        };
        if index + 7 > limit {
            return Err(gp(selector));
        }
        Ok(base.wrapping_add(index))
    }

    fn read_desc<B: Bus>(&mut self, bus: &mut B, selector: u16) -> Exec<Desc> {
        let at = self.desc_addr(selector)?;
        Ok(Desc {
            lo: self.sys_rd32(bus, at)?,
            hi: self.sys_rd32(bus, at.wrapping_add(4))?,
        })
    }

    fn set_accessed<B: Bus>(&mut self, bus: &mut B, selector: u16, desc: Desc) -> Exec {
        if desc.is_segment() && desc.hi & 0x100 == 0 {
            let at = self.desc_addr(selector)?;
            self.sys_wr32(bus, at.wrapping_add(4), desc.hi | 0x100)?;
        }
        Ok(())
    }

    /// Loads a data or stack segment register.
    pub(crate) fn load_seg<B: Bus>(&mut self, bus: &mut B, index: usize, selector: u16) -> Exec {
        if !self.pmode {
            self.set_real_segment(index, selector);
            return Ok(());
        }
        if selector & !3 == 0 {
            if index == SS {
                return Err(Trap::FaultCode(13, 0));
            }
            self.sregs[index] = selector;
            self.seg[index] = NULL_SEG;
            return Ok(());
        }
        let desc = self.read_desc(bus, selector)?;
        let rpl = (selector & 3) as u8;
        if index == SS {
            if !desc.is_segment()
                || desc.is_code()
                || desc.kind() & 2 == 0
                || rpl != self.cpl
                || desc.dpl() != self.cpl
            {
                return Err(gp(selector));
            }
            if !desc.present() {
                return Err(Trap::FaultCode(12, (selector & !3) as u32));
            }
        } else {
            if !desc.is_segment() || (desc.is_code() && desc.kind() & 2 == 0) {
                return Err(gp(selector));
            }
            if (!desc.is_code() || !desc.conforming()) && desc.dpl() < rpl.max(self.cpl) {
                return Err(gp(selector));
            }
            if !desc.present() {
                return Err(np(selector));
            }
        }
        self.set_accessed(bus, selector, desc)?;
        self.sregs[index] = selector;
        self.seg[index] = desc.to_seg();
        Ok(())
    }

    fn set_cs(&mut self, selector: u16, desc: Desc, cpl: u8) {
        self.sregs[CS] = (selector & !3) | cpl as u16;
        self.seg[CS] = desc.to_seg();
        self.cpl = cpl;
    }

    /// The code-segment checks shared by far `JMP` and `CALL`.
    fn check_direct_code(&self, selector: u16, desc: Desc) -> Exec {
        let rpl = (selector & 3) as u8;
        if desc.conforming() {
            if desc.dpl() > self.cpl {
                return Err(gp(selector));
            }
        } else if rpl > self.cpl || desc.dpl() != self.cpl {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        Ok(())
    }

    /// The gate's target: its code selector, offset and descriptor.
    fn gate_target<B: Bus>(&mut self, bus: &mut B, gate: Desc) -> Exec<(u16, u32, Desc)> {
        let selector = (gate.lo >> 16) as u16;
        let mut offset = gate.lo & 0xFFFF;
        if gate.kind() & 8 != 0 {
            offset |= gate.hi & 0xFFFF_0000;
        }
        if selector & !3 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let desc = self.read_desc(bus, selector)?;
        if !desc.is_code() || desc.dpl() > self.cpl {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        Ok((selector, offset, desc))
    }

    pub(crate) fn jmp_far<B: Bus>(&mut self, bus: &mut B, selector: u16, offset: u32) -> Exec {
        if !self.pmode {
            self.set_real_segment(CS, selector);
            self.seg[CS].code = true;
            self.ip = if self.op32 { offset } else { offset & 0xFFFF };
            return Ok(());
        }
        if selector & !3 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let desc = self.read_desc(bus, selector)?;
        if desc.is_segment() {
            if !desc.is_code() {
                return Err(gp(selector));
            }
            self.check_direct_code(selector, desc)?;
            let offset = if self.op32 { offset } else { offset & 0xFFFF };
            if offset > desc.limit() {
                return Err(Trap::FaultCode(13, 0));
            }
            self.set_accessed(bus, selector, desc)?;
            let cpl = self.cpl;
            self.set_cs(selector, desc, cpl);
            self.ip = offset;
            return Ok(());
        }
        let rpl = (selector & 3) as u8;
        if desc.dpl() < self.cpl.max(rpl) {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        match desc.kind() {
            1 | 9 => self.task_switch(bus, selector, desc, Switch::Jump),
            5 => self.task_gate(bus, desc, Switch::Jump),
            4 | 12 => {
                let (target, off, code) = self.gate_target(bus, desc)?;
                if !code.conforming() && code.dpl() != self.cpl {
                    return Err(gp(target));
                }
                if off > code.limit() {
                    return Err(Trap::FaultCode(13, 0));
                }
                let cpl = self.cpl;
                self.set_cs(target, code, cpl);
                self.ip = off;
                Ok(())
            }
            _ => Err(gp(selector)),
        }
    }

    pub(crate) fn call_far<B: Bus>(
        &mut self,
        bus: &mut B,
        selector: u16,
        offset: u32,
        w: u32,
    ) -> Exec {
        let return_cs = self.sregs[CS] as u32;
        let return_ip = self.ip;
        if !self.pmode {
            self.pushv(bus, return_cs, w)?;
            self.pushv(bus, return_ip, w)?;
            self.set_real_segment(CS, selector);
            self.seg[CS].code = true;
            self.ip = if w == 32 { offset } else { offset & 0xFFFF };
            return Ok(());
        }
        if selector & !3 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let desc = self.read_desc(bus, selector)?;
        if desc.is_segment() {
            if !desc.is_code() {
                return Err(gp(selector));
            }
            self.check_direct_code(selector, desc)?;
            let offset = if w == 32 { offset } else { offset & 0xFFFF };
            if offset > desc.limit() {
                return Err(Trap::FaultCode(13, 0));
            }
            self.pushv(bus, return_cs, w)?;
            self.pushv(bus, return_ip, w)?;
            self.set_accessed(bus, selector, desc)?;
            let cpl = self.cpl;
            self.set_cs(selector, desc, cpl);
            self.ip = offset;
            return Ok(());
        }
        let rpl = (selector & 3) as u8;
        if desc.dpl() < self.cpl.max(rpl) {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        match desc.kind() {
            1 | 9 => self.task_switch(bus, selector, desc, Switch::Call),
            5 => self.task_gate(bus, desc, Switch::Call),
            4 | 12 => self.call_gate(bus, desc),
            _ => Err(gp(selector)),
        }
    }

    fn call_gate<B: Bus>(&mut self, bus: &mut B, gate: Desc) -> Exec {
        let (target, offset, code) = self.gate_target(bus, gate)?;
        if offset > code.limit() {
            return Err(Trap::FaultCode(13, 0));
        }
        let gate32 = gate.kind() & 8 != 0;
        let w = if gate32 { 32 } else { 16 };
        let return_cs = self.sregs[CS] as u32;
        let return_ip = self.ip;
        let new_cpl = if code.conforming() { self.cpl } else { code.dpl() };
        if new_cpl < self.cpl {
            let old_ss = self.sregs[SS] as u32;
            let old_sp = self.r[SP];
            let old_stack = self.seg[SS];
            let (new_ss, new_sp) = self.tss_stack(bus, new_cpl)?;
            // Copy the parameters off the old stack before switching.
            let count = (gate.hi & 0x1F) as usize;
            let mut params = [0u32; 32];
            let base_off = self.sp_off();
            for (index, slot) in params.iter_mut().enumerate().take(count) {
                let at = base_off.wrapping_add((index as u32) * (w / 8));
                *slot = self.rdv(bus, SS, at, w)?;
            }
            let saved_cpl = self.cpl;
            self.cpl = new_cpl;
            if let Err(trap) = self.load_stack_for(bus, new_ss, new_sp) {
                self.cpl = saved_cpl;
                self.seg[SS] = old_stack;
                self.sregs[SS] = old_ss as u16;
                self.r[SP] = old_sp;
                return Err(trap);
            }
            self.pushv(bus, old_ss, w)?;
            self.pushv(bus, old_sp, w)?;
            for index in (0..count).rev() {
                self.pushv(bus, params[index], w)?;
            }
        }
        self.pushv(bus, return_cs, w)?;
        self.pushv(bus, return_ip, w)?;
        self.set_cs(target, code, new_cpl);
        self.ip = offset;
        Ok(())
    }

    fn load_stack_for<B: Bus>(&mut self, bus: &mut B, selector: u16, sp: u32) -> Exec {
        self.load_seg(bus, SS, selector)?;
        if self.seg[SS].big {
            self.r[SP] = sp;
        } else {
            self.set_reg16(SP as u8, sp as u16);
        }
        Ok(())
    }

    /// The inner stack for a privilege level, from the current TSS.
    fn tss_stack<B: Bus>(&mut self, bus: &mut B, level: u8) -> Exec<(u16, u32)> {
        let base = self.tr.base;
        let level = level as u32;
        if self.tr.kind & 8 != 0 {
            let at = 4 + level * 8;
            if at + 5 > self.tr.limit {
                return Err(Trap::FaultCode(10, (self.tr.sel & !3) as u32));
            }
            let sp = self.sys_rd32(bus, base.wrapping_add(at))?;
            let ss = self.sys_rd16(bus, base.wrapping_add(at + 4))?;
            Ok((ss, sp))
        } else {
            let at = 2 + level * 4;
            if at + 3 > self.tr.limit {
                return Err(Trap::FaultCode(10, (self.tr.sel & !3) as u32));
            }
            let sp = self.sys_rd16(bus, base.wrapping_add(at))? as u32;
            let ss = self.sys_rd16(bus, base.wrapping_add(at + 2))?;
            Ok((ss, sp))
        }
    }

    /// After a return to an outer level, data segments the outer level can't
    /// use become null.
    fn clear_inner_segments(&mut self) {
        for index in [ES, DS, FS, GS] {
            let seg = self.seg[index];
            let null = !seg.readable && !seg.writable && seg.hi == 0;
            if !null && seg.dpl < self.cpl && (!seg.code || !seg.conforming) {
                self.sregs[index] = 0;
                self.seg[index] = NULL_SEG;
            }
        }
    }

    pub(crate) fn ret_far<B: Bus>(&mut self, bus: &mut B, extra: u32, w: u32) -> Exec {
        if !self.pmode {
            let ip = self.popv(bus, w)?;
            let cs = self.popv(bus, w)? as u16;
            self.set_real_segment(CS, cs);
            self.seg[CS].code = true;
            self.ip = if w == 32 { ip } else { ip & 0xFFFF };
            self.sp_add(extra);
            return Ok(());
        }
        let size = w / 8;
        let base = self.sp_off();
        let ip = self.rdv(bus, SS, base, w)?;
        let selector = self.rdv(bus, SS, base.wrapping_add(size), w)? as u16;
        if selector & !3 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let desc = self.read_desc(bus, selector)?;
        let rpl = (selector & 3) as u8;
        if !desc.is_code() || rpl < self.cpl {
            return Err(gp(selector));
        }
        if desc.conforming() {
            if desc.dpl() > rpl {
                return Err(gp(selector));
            }
        } else if desc.dpl() != rpl {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        let ip = if w == 32 { ip } else { ip & 0xFFFF };
        if ip > desc.limit() {
            return Err(Trap::FaultCode(13, 0));
        }
        if rpl > self.cpl {
            let at = base.wrapping_add(2 * size + extra);
            let new_sp = self.rdv(bus, SS, at, w)?;
            let new_ss = self.rdv(bus, SS, at.wrapping_add(size), w)? as u16;
            self.set_accessed(bus, selector, desc)?;
            self.set_cs(selector, desc, rpl);
            self.load_stack_for(bus, new_ss, new_sp)?;
            self.sp_add(extra);
            self.clear_inner_segments();
        } else {
            self.set_accessed(bus, selector, desc)?;
            let cpl = self.cpl;
            self.set_cs(selector, desc, cpl);
            self.sp_add(2 * size + extra);
        }
        self.ip = ip;
        Ok(())
    }

    /// Enters a handler through the IDT.
    pub(crate) fn interrupt_protected<B: Bus>(
        &mut self,
        bus: &mut B,
        vector: u8,
        code: Option<u32>,
        software: bool,
    ) -> Exec {
        let entry = (vector as u32) * 8;
        let idt_code = entry + 2;
        if entry + 7 > self.idtr.limit {
            return Err(Trap::FaultCode(13, idt_code));
        }
        let at = self.idtr.base.wrapping_add(entry);
        let gate = Desc {
            lo: self.sys_rd32(bus, at)?,
            hi: self.sys_rd32(bus, at.wrapping_add(4))?,
        };
        let kind = gate.kind();
        if gate.is_segment() || !matches!(kind, 5 | 6 | 7 | 14 | 15) {
            return Err(Trap::FaultCode(13, idt_code));
        }
        if software && gate.dpl() < self.cpl {
            return Err(Trap::FaultCode(13, idt_code));
        }
        if !gate.present() {
            return Err(Trap::FaultCode(11, idt_code));
        }
        if kind == 5 {
            self.task_gate(bus, gate, Switch::Call)?;
            if let Some(code) = code {
                if self.tr.kind & 8 != 0 {
                    self.push32(bus, code)?;
                } else {
                    self.push16(bus, code as u16)?;
                }
            }
            return Ok(());
        }
        let (target, offset, desc) = self.gate_target(bus, gate)?;
        let w = if kind & 8 != 0 { 32 } else { 16 };
        let new_cpl = if desc.conforming() { self.cpl } else { desc.dpl() };
        if offset > desc.limit() {
            return Err(Trap::FaultCode(13, 0));
        }
        let flags = self.flags();
        let (old_ss, old_sp, old_stack, old_cpl) =
            (self.sregs[SS], self.r[SP], self.seg[SS], self.cpl);
        let pushes = |cpu: &mut Self, bus: &mut B| -> Exec {
            if new_cpl < old_cpl {
                let (new_ss, new_sp) = cpu.tss_stack(bus, new_cpl)?;
                cpu.cpl = new_cpl;
                if cpu.load_stack_for(bus, new_ss, new_sp).is_err() {
                    return Err(Trap::FaultCode(10, (new_ss & !3) as u32));
                }
                cpu.pushv(bus, old_ss as u32, w)?;
                cpu.pushv(bus, old_sp, w)?;
            }
            cpu.pushv(bus, flags, w)?;
            let return_cs = cpu.sregs[CS] as u32;
            cpu.pushv(bus, return_cs, w)?;
            let return_ip = cpu.ip;
            cpu.pushv(bus, return_ip, w)?;
            if let Some(code) = code {
                cpu.pushv(bus, code, w)?;
            }
            Ok(())
        };
        if let Err(trap) = pushes(self, bus) {
            self.sregs[SS] = old_ss;
            self.r[SP] = old_sp;
            self.seg[SS] = old_stack;
            self.cpl = old_cpl;
            return Err(trap);
        }
        self.set_cs(target, desc, new_cpl);
        self.ip = offset;
        self.materialize();
        self.eflags &= !(FLAG_TF | FLAG_NT | FLAG_RF);
        if kind & 1 == 0 {
            self.eflags &= !FLAG_IF;
        }
        Ok(())
    }

    pub(crate) fn iret_protected<B: Bus>(&mut self, bus: &mut B, w: u32) -> Exec {
        if self.eflags & FLAG_NT != 0 {
            return self.task_return(bus);
        }
        let size = w / 8;
        let base = self.sp_off();
        let ip = self.rdv(bus, SS, base, w)?;
        let selector = self.rdv(bus, SS, base.wrapping_add(size), w)? as u16;
        let flags = self.rdv(bus, SS, base.wrapping_add(2 * size), w)?;
        if selector & !3 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let desc = self.read_desc(bus, selector)?;
        let rpl = (selector & 3) as u8;
        if !desc.is_code() || rpl < self.cpl {
            return Err(gp(selector));
        }
        if desc.conforming() {
            if desc.dpl() > rpl {
                return Err(gp(selector));
            }
        } else if desc.dpl() != rpl {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        let ip = if w == 32 { ip } else { ip & 0xFFFF };
        if ip > desc.limit() {
            return Err(Trap::FaultCode(13, 0));
        }
        if rpl > self.cpl {
            let new_sp = self.rdv(bus, SS, base.wrapping_add(3 * size), w)?;
            let new_ss = self.rdv(bus, SS, base.wrapping_add(4 * size), w)? as u16;
            // The flags come first: they're judged at the handler's privilege.
            self.load_flags(flags, w);
            self.set_accessed(bus, selector, desc)?;
            self.set_cs(selector, desc, rpl);
            self.load_stack_for(bus, new_ss, new_sp)?;
            self.clear_inner_segments();
        } else {
            self.load_flags(flags, w);
            self.set_accessed(bus, selector, desc)?;
            let cpl = self.cpl;
            self.set_cs(selector, desc, cpl);
            self.sp_add(3 * size);
        }
        self.ip = ip;
        Ok(())
    }

    fn task_gate<B: Bus>(&mut self, bus: &mut B, gate: Desc, kind: Switch) -> Exec {
        let selector = (gate.lo >> 16) as u16;
        if selector & 4 != 0 {
            return Err(gp(selector));
        }
        let desc = self.read_desc(bus, selector)?;
        if !matches!(desc.kind(), 1 | 9) || desc.is_segment() {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        self.task_switch(bus, selector, desc, kind)
    }

    fn task_return<B: Bus>(&mut self, bus: &mut B) -> Exec {
        let selector = self.sys_rd16(bus, self.tr.base)?;
        if selector & 4 != 0 || selector & !3 == 0 {
            return Err(Trap::FaultCode(10, (selector & !3) as u32));
        }
        let desc = self.read_desc(bus, selector)?;
        if desc.is_segment() || !matches!(desc.kind(), 3 | 11) {
            return Err(Trap::FaultCode(10, (selector & !3) as u32));
        }
        self.task_switch(bus, selector, desc, Switch::Iret)
    }

    fn task_switch<B: Bus>(
        &mut self,
        bus: &mut B,
        selector: u16,
        desc: Desc,
        kind: Switch,
    ) -> Exec {
        let new_base = desc.base();
        let new_limit = desc.limit();
        let new32 = desc.kind() & 8 != 0;
        if new_limit < if new32 { 103 } else { 43 } {
            return Err(Trap::FaultCode(10, (selector & !3) as u32));
        }
        if kind != Switch::Iret && desc.kind() & 2 != 0 {
            return Err(gp(selector));
        }
        let old_base = self.tr.base;
        let mut flags = self.flags();
        if kind == Switch::Iret {
            flags &= !FLAG_NT;
        }
        // Save the outgoing task.
        if self.tr.kind & 8 != 0 {
            self.sys_wr32(bus, old_base.wrapping_add(0x20), self.ip)?;
            self.sys_wr32(bus, old_base.wrapping_add(0x24), flags)?;
            for index in 0..8 {
                self.sys_wr32(bus, old_base.wrapping_add(0x28 + index * 4), self.r[index as usize])?;
            }
            for index in 0..6 {
                self.sys_wr16(
                    bus,
                    old_base.wrapping_add(0x48 + index * 4),
                    self.sregs[index as usize],
                )?;
            }
        } else if self.tr.sel != 0 {
            self.sys_wr16(bus, old_base.wrapping_add(0x0E), self.ip as u16)?;
            self.sys_wr16(bus, old_base.wrapping_add(0x10), flags as u16)?;
            for index in 0..8 {
                self.sys_wr16(
                    bus,
                    old_base.wrapping_add(0x12 + index * 2),
                    self.r[index as usize] as u16,
                )?;
            }
            for index in 0..4 {
                self.sys_wr16(
                    bus,
                    old_base.wrapping_add(0x22 + index * 2),
                    self.sregs[index as usize],
                )?;
            }
        }
        // Busy bits: the outgoing task stays busy only when it's being nested.
        if matches!(kind, Switch::Jump | Switch::Iret) && self.tr.sel & !3 != 0 {
            let at = self.desc_addr(self.tr.sel)?.wrapping_add(4);
            let hi = self.sys_rd32(bus, at)?;
            self.sys_wr32(bus, at, hi & !0x200)?;
        }
        if kind != Switch::Iret {
            let at = self.desc_addr(selector)?.wrapping_add(4);
            self.sys_wr32(bus, at, desc.hi | 0x200)?;
        }
        if kind == Switch::Call {
            self.sys_wr16(bus, new_base, self.tr.sel)?;
        }
        // Load the incoming task.
        let mut new_flags;
        let mut new_sregs = [0u16; 6];
        let new_ldt;
        if new32 {
            let cr3 = self.sys_rd32(bus, new_base.wrapping_add(0x1C))?;
            if self.paging && cr3 != self.cr3 {
                self.cr3 = cr3;
                self.flush_tlb();
            }
            self.ip = self.sys_rd32(bus, new_base.wrapping_add(0x20))?;
            new_flags = self.sys_rd32(bus, new_base.wrapping_add(0x24))?;
            for index in 0..8 {
                self.r[index as usize] =
                    self.sys_rd32(bus, new_base.wrapping_add(0x28 + index * 4))?;
            }
            for index in 0..6 {
                new_sregs[index as usize] =
                    self.sys_rd16(bus, new_base.wrapping_add(0x48 + index * 4))?;
            }
            new_ldt = self.sys_rd16(bus, new_base.wrapping_add(0x60))?;
        } else {
            self.ip = self.sys_rd16(bus, new_base.wrapping_add(0x0E))? as u32;
            new_flags = self.sys_rd16(bus, new_base.wrapping_add(0x10))? as u32;
            for index in 0..8 {
                self.r[index as usize] =
                    self.sys_rd16(bus, new_base.wrapping_add(0x12 + index * 2))? as u32;
            }
            for index in 0..4 {
                new_sregs[index as usize] =
                    self.sys_rd16(bus, new_base.wrapping_add(0x22 + index * 2))?;
            }
            new_ldt = self.sys_rd16(bus, new_base.wrapping_add(0x2A))?;
        }
        if kind == Switch::Call {
            new_flags |= FLAG_NT;
        }
        self.tr = SysSeg {
            sel: selector,
            base: new_base,
            limit: new_limit,
            kind: desc.kind() | 2,
        };
        self.cr0 |= 8;
        self.lazy = 0;
        self.eflags = (new_flags | 2) & !FLAG_VM;
        // Selectors are loaded now that the new context is in place.
        self.cpl = (new_sregs[CS] & 3) as u8;
        self.load_ldt_raw(bus, new_ldt)?;
        let code = self.read_desc(bus, new_sregs[CS])?;
        if !code.is_code() {
            return Err(Trap::FaultCode(10, (new_sregs[CS] & !3) as u32));
        }
        let cpl = self.cpl;
        self.set_cs(new_sregs[CS], code, cpl);
        for index in [SS, ES, DS, FS, GS] {
            self.load_seg(bus, index, new_sregs[index])?;
        }
        Ok(())
    }

    fn load_ldt_raw<B: Bus>(&mut self, bus: &mut B, selector: u16) -> Exec {
        if selector & !3 == 0 {
            self.ldtr = SysSeg::default();
            return Ok(());
        }
        if selector & 4 != 0 {
            return Err(gp(selector));
        }
        let desc = self.read_desc(bus, selector)?;
        if desc.is_segment() || desc.kind() != 2 {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        self.ldtr = SysSeg {
            sel: selector,
            base: desc.base(),
            limit: desc.limit(),
            kind: 2,
        };
        Ok(())
    }

    pub(crate) fn load_ldt<B: Bus>(&mut self, bus: &mut B, selector: u16) -> Exec {
        self.load_ldt_raw(bus, selector)
    }

    pub(crate) fn load_tr<B: Bus>(&mut self, bus: &mut B, selector: u16) -> Exec {
        if selector & !3 == 0 || selector & 4 != 0 {
            return Err(gp(selector));
        }
        let desc = self.read_desc(bus, selector)?;
        if desc.is_segment() || !matches!(desc.kind(), 1 | 9) {
            return Err(gp(selector));
        }
        if !desc.present() {
            return Err(np(selector));
        }
        let at = self.desc_addr(selector)?.wrapping_add(4);
        self.sys_wr32(bus, at, desc.hi | 0x200)?;
        self.tr = SysSeg {
            sel: selector,
            base: desc.base(),
            limit: desc.limit(),
            kind: desc.kind() | 2,
        };
        Ok(())
    }

    /// `VERR` and `VERW`: whether the segment can be read or written from here.
    pub(crate) fn verify_seg<B: Bus>(&mut self, bus: &mut B, selector: u16, write: bool) -> bool {
        if selector & !3 == 0 {
            return false;
        }
        let Ok(desc) = self.read_desc(bus, selector) else {
            return false;
        };
        if !desc.is_segment() {
            return false;
        }
        let rpl = (selector & 3) as u8;
        if !desc.conforming() && desc.dpl() < self.cpl.max(rpl) {
            return false;
        }
        if write {
            !desc.is_code() && desc.kind() & 2 != 0
        } else {
            !desc.is_code() || desc.kind() & 2 != 0
        }
    }

    /// `LAR` or `LSL`: the value to load, or `None` to clear ZF.
    pub(crate) fn lar_lsl_value<B: Bus>(
        &mut self,
        bus: &mut B,
        selector: u16,
        limit: bool,
    ) -> Exec<Option<u32>> {
        if selector & !3 == 0 {
            return Ok(None);
        }
        let Ok(desc) = self.read_desc(bus, selector) else {
            return Ok(None);
        };
        if !desc.is_segment() {
            let valid = if limit {
                matches!(desc.kind(), 1 | 2 | 3 | 9 | 11)
            } else {
                matches!(desc.kind(), 1 | 2 | 3 | 4 | 5 | 9 | 11 | 12)
            };
            if !valid {
                return Ok(None);
            }
        }
        let rpl = (selector & 3) as u8;
        if !desc.conforming() && desc.dpl() < self.cpl.max(rpl) {
            return Ok(None);
        }
        Ok(Some(if limit {
            desc.limit()
        } else {
            desc.hi & 0x00FF_FF00
        }))
    }
}
