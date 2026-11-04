//! The two-byte `0F xx` opcode map: 286, 386 and 486 additions.

use super::*;

impl Cpu {
    pub(crate) fn op_0f<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let opcode = self.fetch8(bus)?;
        match opcode {
            0x00 => self.group6(bus),
            0x01 => self.group7(bus),
            0x02 | 0x03 => self.lar_lsl(bus, opcode),
            0x06 => {
                if self.pmode && self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                self.cr0 &= !8;
                Ok(())
            }
            0x08 | 0x09 => {
                if self.pmode && self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                Ok(())
            }
            0x20..=0x23 | 0x24 | 0x26 => self.mov_system(bus, opcode),
            0x80..=0x8F => {
                let w = self.vw();
                let rel = self.fetchw(bus, w)?;
                if self.condition(opcode & 0x0F) {
                    self.branch(self.ip.wrapping_add(rel))?;
                }
                Ok(())
            }
            0x90..=0x9F => {
                let m = self.modrm(bus)?;
                let value = u8::from(self.condition(opcode & 0x0F));
                self.wr_rm8(bus, &m, value)
            }
            0xA0 | 0xA8 => {
                let index = if opcode == 0xA0 { FS } else { GS };
                let value = self.sregs[index] as u32;
                let w = self.vw();
                self.pushv(bus, value, w)
            }
            0xA1 | 0xA9 => {
                let index = if opcode == 0xA1 { FS } else { GS };
                let w = self.vw();
                let off = self.sp_off();
                let value = self.rdv(bus, SS, off, w)?;
                self.load_seg(bus, index, value as u16)?;
                self.sp_add(w / 8);
                Ok(())
            }
            0xA3 | 0xAB | 0xB3 | 0xBB => {
                let m = self.modrm(bus)?;
                let kind = (opcode >> 3) & 3;
                let w = self.vw();
                let bit = self.regv(m.reg, w);
                self.bit_op(bus, &m, kind, bit, w, true)
            }
            0xBA => {
                let m = self.modrm(bus)?;
                if m.reg < 4 {
                    return Err(Trap::Fault(6));
                }
                let w = self.vw();
                let bit = self.fetch8(bus)? as u32;
                self.bit_op(bus, &m, m.reg & 3, bit, w, false)
            }
            0xA4 | 0xA5 | 0xAC | 0xAD => {
                let w = self.vw();
                let m = self.modrm(bus)?;
                let count = if opcode & 1 == 0 {
                    self.fetch8(bus)?
                } else {
                    self.r[CX] as u8
                } & 0x1F;
                let dst = self.rd_rmv(bus, &m, w)?;
                let src = self.regv(m.reg, w);
                if count == 0 || count as u32 > w {
                    return Ok(());
                }
                let n = count as u32;
                let sign = 1u32 << (w - 1);
                let (dst64, src64) = (dst as u64, src as u64);
                let (res, cf) = if opcode < 0xA8 {
                    (
                        ((dst64 << n) | (src64 >> (w - n))) as u32 & mask(w),
                        (dst64 >> (w - n)) & 1 != 0,
                    )
                } else {
                    (
                        ((dst64 >> n) | (src64 << (w - n))) as u32 & mask(w),
                        (dst64 >> (n - 1)) & 1 != 0,
                    )
                };
                self.materialize();
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_OF, (dst ^ res) & sign != 0);
                self.set_szp(res, w);
                self.wr_rmv(bus, &m, w, res)
            }
            0xA6 | 0xA7 => Err(Trap::Fault(6)),
            0xAF => {
                let w = self.vw();
                let m = self.modrm(bus)?;
                let value = self.rd_rmv(bus, &m, w)?;
                let res = self.imul(self.regv(m.reg, w), value, w);
                self.set_regv(m.reg, w, res);
                Ok(())
            }
            0xB0 | 0xB1 => {
                // CMPXCHG
                let w = if opcode == 0xB0 { 8 } else { self.vw() };
                let m = self.modrm(bus)?;
                let dst = self.rd_rmw(bus, &m, w)?;
                let acc = if w == 8 {
                    self.r[AX] & 0xFF
                } else {
                    self.regv(0, w)
                };
                self.alu(7, acc, dst, w);
                if acc == dst {
                    let src = if w == 8 {
                        self.reg8(m.reg) as u32
                    } else {
                        self.regv(m.reg, w)
                    };
                    self.wr_rmw(bus, &m, w, src)
                } else {
                    if w == 8 {
                        self.set_reg8(0, dst as u8);
                    } else {
                        self.set_regv(0, w, dst);
                    }
                    Ok(())
                }
            }
            0xB2 => self.load_far_pointer(bus, SS).map(|()| self.inhibit = true),
            0xB4 => self.load_far_pointer(bus, FS),
            0xB5 => self.load_far_pointer(bus, GS),
            0xB6 | 0xB7 | 0xBE | 0xBF => {
                let w = self.vw();
                let m = self.modrm(bus)?;
                let value = if opcode & 1 == 0 {
                    let byte = self.rd_rm8(bus, &m)?;
                    if opcode >= 0xBE {
                        byte as i8 as i32 as u32
                    } else {
                        byte as u32
                    }
                } else {
                    let word = self.rd_rmv(bus, &m, 16)? as u16;
                    if opcode >= 0xBE {
                        word as i16 as i32 as u32
                    } else {
                        word as u32
                    }
                };
                self.set_regv(m.reg, w, value);
                Ok(())
            }
            0xBC | 0xBD => {
                let w = self.vw();
                let m = self.modrm(bus)?;
                let value = self.rd_rmv(bus, &m, w)?;
                self.materialize();
                if value == 0 {
                    self.set_flag(FLAG_ZF, true);
                } else {
                    self.set_flag(FLAG_ZF, false);
                    let index = if opcode == 0xBC {
                        value.trailing_zeros()
                    } else {
                        31 - value.leading_zeros()
                    };
                    self.set_regv(m.reg, w, index);
                }
                Ok(())
            }
            0xC0 | 0xC1 => {
                // XADD
                let w = if opcode == 0xC0 { 8 } else { self.vw() };
                let m = self.modrm(bus)?;
                let dst = self.rd_rmw(bus, &m, w)?;
                let src = if w == 8 {
                    self.reg8(m.reg) as u32
                } else {
                    self.regv(m.reg, w)
                };
                let res = self.alu(0, dst, src, w);
                self.wr_rmw(bus, &m, w, res)?;
                if w == 8 {
                    self.set_reg8(m.reg, dst as u8);
                } else {
                    self.set_regv(m.reg, w, dst);
                }
                Ok(())
            }
            0xC8..=0xCF => {
                let index = (opcode & 7) as usize;
                self.r[index] = self.r[index].swap_bytes();
                Ok(())
            }
            _ => Err(Trap::Fault(6)),
        }
    }

    fn bit_op<B: Bus>(
        &mut self,
        bus: &mut B,
        m: &ModRm,
        kind: u8,
        bit: u32,
        w: u32,
        from_reg: bool,
    ) -> Exec {
        let index = bit & (w - 1);
        let (off, value) = if m.is_reg {
            (0, self.regv(m.rm, w))
        } else {
            let delta = if from_reg {
                let signed = if w == 32 {
                    bit as i32
                } else {
                    bit as u16 as i16 as i32
                };
                (signed >> if w == 32 { 5 } else { 4 }).wrapping_mul((w / 8) as i32) as u32
            } else {
                0
            };
            let off = self.wrap_addr(m.off.wrapping_add(delta));
            (off, self.rdv(bus, m.seg as usize, off, w)?)
        };
        self.set_flag(FLAG_CF, (value >> index) & 1 != 0);
        let new = match kind {
            0 => return Ok(()),
            1 => value | (1 << index),
            2 => value & !(1 << index),
            _ => value ^ (1 << index),
        };
        if m.is_reg {
            self.set_regv(m.rm, w, new);
            Ok(())
        } else {
            self.wrv(bus, m.seg as usize, off, w, new)
        }
    }

    fn group6<B: Bus>(&mut self, bus: &mut B) -> Exec {
        if !self.pmode {
            return Err(Trap::Fault(6));
        }
        let m = self.modrm(bus)?;
        match m.reg {
            0 | 1 => {
                let value = if m.reg == 0 { self.ldtr.sel } else { self.tr.sel };
                if m.is_reg {
                    let w = self.vw();
                    self.set_regv(m.rm, w, value as u32);
                    Ok(())
                } else {
                    self.wr16(bus, m.seg as usize, m.off, value)
                }
            }
            2 | 3 => {
                if self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                let selector = self.rd_rmv(bus, &m, 16)? as u16;
                if m.reg == 2 {
                    self.load_ldt(bus, selector)
                } else {
                    self.load_tr(bus, selector)
                }
            }
            4 | 5 => {
                let selector = self.rd_rmv(bus, &m, 16)? as u16;
                let ok = self.verify_seg(bus, selector, m.reg == 5);
                self.set_flag(FLAG_ZF, ok);
                Ok(())
            }
            _ => Err(Trap::Fault(6)),
        }
    }

    fn group7<B: Bus>(&mut self, bus: &mut B) -> Exec {
        let m = self.modrm(bus)?;
        let seg = m.seg as usize;
        match m.reg {
            0 | 1 => {
                if m.is_reg {
                    return Err(Trap::Fault(6));
                }
                let table = if m.reg == 0 { self.gdtr } else { self.idtr };
                self.wr16(bus, seg, m.off, table.limit as u16)?;
                let base = if self.op32 {
                    table.base
                } else {
                    table.base & 0x00FF_FFFF
                };
                self.wr32(bus, seg, self.wrap_addr(m.off.wrapping_add(2)), base)
            }
            2 | 3 => {
                if m.is_reg {
                    return Err(Trap::Fault(6));
                }
                if self.pmode && self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                let limit = self.rd16(bus, seg, m.off)? as u32;
                let mut base = self.rd32(bus, seg, self.wrap_addr(m.off.wrapping_add(2)))?;
                if !self.op32 {
                    base &= 0x00FF_FFFF;
                }
                let table = TableReg { base, limit };
                if m.reg == 2 {
                    self.gdtr = table;
                } else {
                    self.idtr = table;
                }
                Ok(())
            }
            4 => {
                let value = self.cr0;
                if m.is_reg {
                    let w = self.vw();
                    self.set_regv(m.rm, w, value);
                    Ok(())
                } else {
                    self.wr16(bus, seg, m.off, value as u16)
                }
            }
            6 => {
                if self.pmode && self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                let value = self.rd_rmv(bus, &m, 16)? & 0xF;
                // LMSW can set PE but not clear it.
                let keep = self.cr0 & 1;
                let new = (self.cr0 & !0xF) | value | keep;
                self.write_cr0(new)
            }
            7 => {
                if m.is_reg {
                    return Err(Trap::Fault(6));
                }
                if self.pmode && self.cpl != 0 {
                    return Err(Trap::FaultCode(13, 0));
                }
                self.flush_tlb();
                Ok(())
            }
            _ => Err(Trap::Fault(6)),
        }
    }

    fn lar_lsl<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        if !self.pmode {
            return Err(Trap::Fault(6));
        }
        let w = self.vw();
        let m = self.modrm(bus)?;
        let selector = self.rd_rmv(bus, &m, 16)? as u16;
        let result = self.lar_lsl_value(bus, selector, opcode == 0x03)?;
        match result {
            Some(value) => {
                self.set_regv(m.reg, w, value);
                self.set_flag(FLAG_ZF, true);
            }
            None => self.set_flag(FLAG_ZF, false),
        }
        Ok(())
    }

    fn mov_system<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        if self.pmode && self.cpl != 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        // These always act on registers, whatever the mode bits say.
        let byte = self.fetch8(bus)?;
        let reg = ((byte >> 3) & 7) as usize;
        let rm = (byte & 7) as usize;
        match opcode {
            0x20 => {
                self.r[rm] = match reg {
                    0 => self.cr0,
                    2 => self.cr2,
                    3 => self.cr3,
                    4 => self.cr4,
                    _ => return Err(Trap::Fault(6)),
                };
            }
            0x22 => {
                let value = self.r[rm];
                match reg {
                    0 => self.write_cr0(value)?,
                    2 => self.cr2 = value,
                    3 => {
                        self.cr3 = value;
                        self.flush_tlb();
                    }
                    4 => self.cr4 = value,
                    _ => return Err(Trap::Fault(6)),
                }
            }
            0x21 => self.r[rm] = self.dr[reg],
            0x23 => self.dr[reg] = self.r[rm],
            // Test registers read as zero and ignore writes.
            0x24 => self.r[rm] = 0,
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn write_cr0(&mut self, value: u32) -> Exec {
        // PG needs PE.
        if value & 0x8000_0000 != 0 && value & 1 == 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        let was_paging = self.paging;
        self.cr0 = value | 0x10;
        let pmode = value & 1 != 0;
        if !pmode && self.pmode {
            self.cpl = 0;
        }
        self.pmode = pmode;
        self.paging = value & 0x8000_0000 != 0;
        if self.paging != was_paging {
            self.flush_tlb();
        }
        Ok(())
    }
}
