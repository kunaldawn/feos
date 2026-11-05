//! The one-byte opcode map: a table of handlers indexed by opcode.

use core::marker::PhantomData;

use super::*;

pub(crate) type Handler<B> = fn(&mut Cpu, &mut B, u8) -> Exec;

pub(crate) struct Ops<B>(PhantomData<B>);

impl<B: Bus> Ops<B> {
    /// Opcode to handler. Prefix bytes never reach this table.
    pub(crate) const TABLE: &'static [Handler<B>; 256] = &{
        let mut t: [Handler<B>; 256] = [Cpu::op_ud::<B> as Handler<B>; 256];
        let mut i = 0;
        while i < 0x40 {
            if i & 7 <= 5 {
                t[i] = Cpu::op_alu::<B>;
            }
            i += 1;
        }
        t[0x06] = Cpu::op_push_seg::<B>;
        t[0x07] = Cpu::op_pop_seg::<B>;
        t[0x0E] = Cpu::op_push_seg::<B>;
        t[0x0F] = Cpu::op_0f::<B>;
        t[0x16] = Cpu::op_push_seg::<B>;
        t[0x17] = Cpu::op_pop_seg::<B>;
        t[0x1E] = Cpu::op_push_seg::<B>;
        t[0x1F] = Cpu::op_pop_seg::<B>;
        t[0x27] = Cpu::op_bcd::<B>;
        t[0x2F] = Cpu::op_bcd::<B>;
        t[0x37] = Cpu::op_bcd::<B>;
        t[0x3F] = Cpu::op_bcd::<B>;
        let mut i = 0x40;
        while i < 0x50 {
            t[i] = Cpu::op_inc_dec_reg::<B>;
            i += 1;
        }
        let mut i = 0x50;
        while i < 0x58 {
            t[i] = Cpu::op_push_reg::<B>;
            t[i + 8] = Cpu::op_pop_reg::<B>;
            i += 1;
        }
        t[0x60] = Cpu::op_pusha::<B>;
        t[0x61] = Cpu::op_popa::<B>;
        t[0x62] = Cpu::op_bound::<B>;
        t[0x63] = Cpu::op_arpl::<B>;
        t[0x68] = Cpu::op_push_imm::<B>;
        t[0x69] = Cpu::op_imul_imm::<B>;
        t[0x6A] = Cpu::op_push_imm::<B>;
        t[0x6B] = Cpu::op_imul_imm::<B>;
        t[0x6C] = Cpu::op_ins_outs::<B>;
        t[0x6D] = Cpu::op_ins_outs::<B>;
        t[0x6E] = Cpu::op_ins_outs::<B>;
        t[0x6F] = Cpu::op_ins_outs::<B>;
        let mut i = 0x70;
        while i < 0x80 {
            t[i] = Cpu::op_jcc::<B>;
            i += 1;
        }
        t[0x80] = Cpu::op_group1::<B>;
        t[0x81] = Cpu::op_group1::<B>;
        t[0x82] = Cpu::op_group1::<B>;
        t[0x83] = Cpu::op_group1::<B>;
        t[0x84] = Cpu::op_test_rm::<B>;
        t[0x85] = Cpu::op_test_rm::<B>;
        t[0x86] = Cpu::op_xchg_rm::<B>;
        t[0x87] = Cpu::op_xchg_rm::<B>;
        t[0x88] = Cpu::op_mov_rm_r8::<B>;
        t[0x89] = Cpu::op_mov_rm_rv::<B>;
        t[0x8A] = Cpu::op_mov_r8_rm::<B>;
        t[0x8B] = Cpu::op_mov_rv_rm::<B>;
        t[0x8C] = Cpu::op_mov_rm_sreg::<B>;
        t[0x8D] = Cpu::op_lea::<B>;
        t[0x8E] = Cpu::op_mov_sreg_rm::<B>;
        t[0x8F] = Cpu::op_pop_rm::<B>;
        let mut i = 0x90;
        while i < 0x98 {
            t[i] = Cpu::op_xchg_ax::<B>;
            i += 1;
        }
        t[0x98] = Cpu::op_cbw::<B>;
        t[0x99] = Cpu::op_cwd::<B>;
        t[0x9A] = Cpu::op_call_far_imm::<B>;
        t[0x9B] = Cpu::op_wait::<B>;
        t[0x9C] = Cpu::op_pushf::<B>;
        t[0x9D] = Cpu::op_popf::<B>;
        t[0x9E] = Cpu::op_sahf::<B>;
        t[0x9F] = Cpu::op_lahf::<B>;
        t[0xA0] = Cpu::op_mov_moffs::<B>;
        t[0xA1] = Cpu::op_mov_moffs::<B>;
        t[0xA2] = Cpu::op_mov_moffs::<B>;
        t[0xA3] = Cpu::op_mov_moffs::<B>;
        let mut i = 0xA4;
        while i < 0xB0 {
            t[i] = Cpu::op_string::<B>;
            i += 1;
        }
        t[0xA8] = Cpu::op_test_imm::<B>;
        t[0xA9] = Cpu::op_test_imm::<B>;
        let mut i = 0xB0;
        while i < 0xB8 {
            t[i] = Cpu::op_mov_r8_imm::<B>;
            t[i + 8] = Cpu::op_mov_rv_imm::<B>;
            i += 1;
        }
        t[0xC0] = Cpu::op_shift::<B>;
        t[0xC1] = Cpu::op_shift::<B>;
        t[0xC2] = Cpu::op_ret::<B>;
        t[0xC3] = Cpu::op_ret::<B>;
        t[0xC4] = Cpu::op_load_far::<B>;
        t[0xC5] = Cpu::op_load_far::<B>;
        t[0xC6] = Cpu::op_mov_rm_imm::<B>;
        t[0xC7] = Cpu::op_mov_rm_imm::<B>;
        t[0xC8] = Cpu::op_enter::<B>;
        t[0xC9] = Cpu::op_leave::<B>;
        t[0xCA] = Cpu::op_retf::<B>;
        t[0xCB] = Cpu::op_retf::<B>;
        t[0xCC] = Cpu::op_int::<B>;
        t[0xCD] = Cpu::op_int::<B>;
        t[0xCE] = Cpu::op_int::<B>;
        t[0xCF] = Cpu::op_iret::<B>;
        t[0xD0] = Cpu::op_shift::<B>;
        t[0xD1] = Cpu::op_shift::<B>;
        t[0xD2] = Cpu::op_shift::<B>;
        t[0xD3] = Cpu::op_shift::<B>;
        t[0xD4] = Cpu::op_aam::<B>;
        t[0xD5] = Cpu::op_aad::<B>;
        t[0xD6] = Cpu::op_salc::<B>;
        t[0xD7] = Cpu::op_xlat::<B>;
        let mut i = 0xD8;
        while i < 0xE0 {
            t[i] = Cpu::op_esc::<B>;
            i += 1;
        }
        t[0xE0] = Cpu::op_loop::<B>;
        t[0xE1] = Cpu::op_loop::<B>;
        t[0xE2] = Cpu::op_loop::<B>;
        t[0xE3] = Cpu::op_loop::<B>;
        t[0xE4] = Cpu::op_in_out::<B>;
        t[0xE5] = Cpu::op_in_out::<B>;
        t[0xE6] = Cpu::op_in_out::<B>;
        t[0xE7] = Cpu::op_in_out::<B>;
        t[0xE8] = Cpu::op_call_near::<B>;
        t[0xE9] = Cpu::op_jmp_near::<B>;
        t[0xEA] = Cpu::op_jmp_far_imm::<B>;
        t[0xEB] = Cpu::op_jmp_short::<B>;
        t[0xEC] = Cpu::op_in_out::<B>;
        t[0xED] = Cpu::op_in_out::<B>;
        t[0xEE] = Cpu::op_in_out::<B>;
        t[0xEF] = Cpu::op_in_out::<B>;
        t[0xF1] = Cpu::op_int::<B>;
        t[0xF4] = Cpu::op_hlt::<B>;
        t[0xF5] = Cpu::op_flag::<B>;
        t[0xF6] = Cpu::op_group3::<B>;
        t[0xF7] = Cpu::op_group3::<B>;
        t[0xF8] = Cpu::op_flag::<B>;
        t[0xF9] = Cpu::op_flag::<B>;
        t[0xFA] = Cpu::op_flag::<B>;
        t[0xFB] = Cpu::op_flag::<B>;
        t[0xFC] = Cpu::op_flag::<B>;
        t[0xFD] = Cpu::op_flag::<B>;
        t[0xFE] = Cpu::op_group4::<B>;
        t[0xFF] = Cpu::op_group5::<B>;
        t
    };
}

impl Cpu {
    pub(crate) fn op_ud<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        Err(Trap::Fault(6))
    }

    fn op_alu<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let op = opcode >> 3;
        let form = opcode & 7;
        match form {
            0 | 1 => {
                let w = if form == 0 { 8 } else { self.vw() };
                let m = self.modrm(bus)?;
                let src = if w == 8 {
                    self.reg8(m.reg) as u32
                } else {
                    self.regv(m.reg, w)
                };
                let dst = self.rd_rmw(bus, &m, w)?;
                let res = self.alu(op, dst, src, w);
                if op != 7 {
                    self.wr_rmw(bus, &m, w, res)?;
                }
            }
            2 => {
                let m = self.modrm(bus)?;
                let src = self.rd_rm8(bus, &m)? as u32;
                let dst = self.reg8(m.reg) as u32;
                let res = self.alu(op, dst, src, 8);
                if op != 7 {
                    self.set_reg8(m.reg, res as u8);
                }
            }
            3 => {
                let w = self.vw();
                let m = self.modrm(bus)?;
                let src = self.rd_rmv(bus, &m, w)?;
                let dst = self.regv(m.reg, w);
                let res = self.alu(op, dst, src, w);
                if op != 7 {
                    self.set_regv(m.reg, w, res);
                }
            }
            4 => {
                let imm = self.fetch8(bus)? as u32;
                let res = self.alu(op, self.r[AX] & 0xFF, imm, 8);
                if op != 7 {
                    self.set_reg8(0, res as u8);
                }
            }
            _ => {
                let w = self.vw();
                let imm = self.fetchw(bus, w)?;
                let res = self.alu(op, self.regv(0, w), imm, w);
                if op != 7 {
                    self.set_regv(0, w, res);
                }
            }
        }
        Ok(())
    }

    fn op_push_seg<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let index = match opcode {
            0x06 => ES,
            0x0E => CS,
            0x16 => SS,
            _ => DS,
        };
        let value = self.sregs[index] as u32;
        let w = self.vw();
        self.pushv(bus, value, w)
    }

    fn op_pop_seg<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let index = match opcode {
            0x07 => ES,
            0x17 => SS,
            _ => DS,
        };
        let w = self.vw();
        let off = self.sp_off();
        let value = self.rdv(bus, SS, off, w)?;
        self.load_seg(bus, index, value as u16)?;
        self.sp_add(w / 8);
        if index == SS {
            self.inhibit = true;
        }
        Ok(())
    }

    fn op_bcd<B: Bus>(&mut self, _bus: &mut B, opcode: u8) -> Exec {
        let old_cf = self.cf();
        let old_af = self.af();
        match opcode {
            0x27 | 0x2F => {
                let sub = opcode == 0x2F;
                let mut al = self.r[AX] as u8;
                let old_al = al;
                let mut cf = false;
                let mut af = false;
                if (al & 0x0F) > 9 || old_af {
                    // Subtracting 6 can itself borrow, and that counts for DAS.
                    cf = sub && al < 6;
                    al = if sub {
                        al.wrapping_sub(6)
                    } else {
                        al.wrapping_add(6)
                    };
                    af = true;
                }
                if old_al > 0x99 || old_cf {
                    al = if sub {
                        al.wrapping_sub(0x60)
                    } else {
                        al.wrapping_add(0x60)
                    };
                    cf = true;
                }
                self.set_reg8(0, al);
                self.set_szp(al as u32, 8);
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_AF, af);
            }
            _ => {
                let sub = opcode == 0x3F;
                let mut ax = self.r[AX] as u16;
                let adjust = (ax & 0x0F) > 9 || old_af;
                if adjust {
                    ax = if sub {
                        ax.wrapping_sub(0x0106)
                    } else {
                        ax.wrapping_add(0x0106)
                    };
                }
                ax &= 0xFF0F;
                self.set_reg16(AX as u8, ax);
                self.materialize();
                self.set_flag(FLAG_CF, adjust);
                self.set_flag(FLAG_AF, adjust);
            }
        }
        Ok(())
    }

    fn op_inc_dec_reg<B: Bus>(&mut self, _bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let index = opcode & 7;
        let value = self.regv(index, w);
        let res = self.inc_dec(value, opcode & 8 != 0, w);
        self.set_regv(index, w, res);
        Ok(())
    }

    fn op_push_reg<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let value = self.regv(opcode & 7, w);
        self.pushv(bus, value, w)
    }

    fn op_pop_reg<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let value = self.popv(bus, w)?;
        self.set_regv(opcode & 7, w, value);
        Ok(())
    }

    fn op_pusha<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let sp = self.regv(SP as u8, w);
        for index in [AX, CX, DX, BX] {
            let value = self.regv(index as u8, w);
            self.pushv(bus, value, w)?;
        }
        self.pushv(bus, sp, w)?;
        for index in [BP, SI, DI] {
            let value = self.regv(index as u8, w);
            self.pushv(bus, value, w)?;
        }
        Ok(())
    }

    fn op_popa<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        for index in [DI, SI, BP] {
            let value = self.popv(bus, w)?;
            self.set_regv(index as u8, w, value);
        }
        self.popv(bus, w)?;
        for index in [BX, DX, CX, AX] {
            let value = self.popv(bus, w)?;
            self.set_regv(index as u8, w, value);
        }
        Ok(())
    }

    fn op_bound<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        if m.is_reg {
            return Err(Trap::Fault(6));
        }
        let seg = m.seg as usize;
        let (index, low, high) = if w == 32 {
            (
                self.r[m.reg as usize] as i32,
                self.rd32(bus, seg, m.off)? as i32,
                self.rd32(bus, seg, self.wrap_addr(m.off.wrapping_add(4)))? as i32,
            )
        } else {
            (
                self.reg16(m.reg) as i16 as i32,
                self.rd16(bus, seg, m.off)? as i16 as i32,
                self.rd16(bus, seg, self.wrap_addr(m.off.wrapping_add(2)))? as i16 as i32,
            )
        };
        if index < low || index > high {
            return Err(Trap::Fault(5));
        }
        Ok(())
    }

    fn op_arpl<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        if !self.pmode {
            return Err(Trap::Fault(6));
        }
        let m = self.modrm(bus)?;
        let dst = self.rd_rmv(bus, &m, 16)? as u16;
        let src = self.reg16(m.reg);
        if dst & 3 < src & 3 {
            self.wr_rmv(bus, &m, 16, ((dst & !3) | (src & 3)) as u32)?;
            self.set_flag(FLAG_ZF, true);
        } else {
            self.set_flag(FLAG_ZF, false);
        }
        Ok(())
    }

    fn op_push_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let value = if opcode == 0x6A {
            self.fetch8(bus)? as i8 as i32 as u32
        } else {
            self.fetchw(bus, w)?
        };
        self.pushv(bus, value, w)
    }

    /// Multiplies signed values and sets CF and OF if the result doesn't fit.
    pub(crate) fn imul(&mut self, a: u32, b: u32, w: u32) -> u32 {
        let (res, over) = if w == 32 {
            let full = (a as i32 as i64) * (b as i32 as i64);
            (full as u32, full != full as i32 as i64)
        } else {
            let full = (a as i16 as i32) * (b as i16 as i32);
            (full as u16 as u32, full != full as i16 as i32)
        };
        self.materialize();
        self.set_flag(FLAG_CF, over);
        self.set_flag(FLAG_OF, over);
        res
    }

    fn op_imul_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        let value = self.rd_rmv(bus, &m, w)?;
        let imm = if opcode == 0x6B {
            self.fetch8(bus)? as i8 as i32 as u32
        } else if w == 32 {
            self.fetch32(bus)?
        } else {
            self.fetch16(bus)? as i16 as i32 as u32
        };
        let res = self.imul(value, imm, w);
        self.set_regv(m.reg, w, res);
        Ok(())
    }

    fn io_allowed<B: Bus>(&mut self, bus: &mut B, port: u16, bytes: u32) -> Exec {
        if self.vm86 {
            // IOPL doesn't matter here: the I/O bitmap always does.
            if self.v86_monitor {
                return Ok(());
            }
        } else if !self.pmode || self.cpl <= self.iopl() {
            return Ok(());
        }
        if self.tr.kind & 8 == 0 || self.tr.limit < 103 {
            return Err(Trap::FaultCode(13, 0));
        }
        let map = self.sys_rd16(bus, self.tr.base.wrapping_add(102))? as u32;
        for index in 0..bytes {
            let bit = port as u32 + index;
            let at = map + bit / 8;
            if at > self.tr.limit {
                return Err(Trap::FaultCode(13, 0));
            }
            let byte = self.sys_rd8(bus, self.tr.base.wrapping_add(at))?;
            if byte & (1 << (bit % 8)) != 0 {
                return Err(Trap::FaultCode(13, 0));
            }
        }
        Ok(())
    }

    fn op_ins_outs<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let port = self.r[DX] as u16;
        self.io_allowed(bus, port, w / 8)?;
        bus.sync(self.cycles);
        let step = if self.df() {
            (w / 8).wrapping_neg()
        } else {
            w / 8
        };
        let mut count = if self.rep != 0 { self.count_reg() } else { 1 };
        if count == 0 {
            return Ok(());
        }
        let mut chunk = count.min(REP_CHUNK);
        self.cycles = self.cycles.wrapping_add(chunk as u64);
        while chunk > 0 {
            if opcode < 0x6E {
                let di = self.addr_reg(DI);
                let value = match w {
                    8 => bus.in8(port) as u32,
                    16 => bus.in16(port) as u32,
                    _ => bus.in32(port),
                };
                self.wrw(bus, ES, di, w, value)?;
                self.addr_reg_add(DI, step);
            } else {
                let si = self.addr_reg(SI);
                let seg = self.seg_override.map_or(DS, |seg| seg as usize);
                let value = self.rdw(bus, seg, si, w)?;
                match w {
                    8 => bus.out8(port, value as u8),
                    16 => bus.out16(port, value as u16),
                    _ => bus.out32(port, value),
                }
                self.addr_reg_add(SI, step);
            }
            chunk -= 1;
            count -= 1;
            if self.rep != 0 {
                self.set_count_reg(count);
            }
        }
        if self.rep != 0 && count != 0 {
            self.ip = self.start_ip;
        }
        Ok(())
    }

    fn op_jcc<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let rel = self.fetch8(bus)? as i8 as i32 as u32;
        if self.condition(opcode & 0x0F) {
            self.branch(self.ip.wrapping_add(rel))?;
        }
        Ok(())
    }

    fn op_group1<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let imm = match opcode {
            0x80 | 0x82 => self.fetch8(bus)? as u32,
            0x83 => self.fetch8(bus)? as i8 as i32 as u32,
            _ => self.fetchw(bus, w)?,
        };
        let dst = self.rd_rmw(bus, &m, w)?;
        let res = self.alu(m.reg, dst, imm, w);
        if m.reg != 7 {
            self.wr_rmw(bus, &m, w, res)?;
        }
        Ok(())
    }

    fn op_test_rm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let dst = self.rd_rmw(bus, &m, w)?;
        let src = if w == 8 {
            self.reg8(m.reg) as u32
        } else {
            self.regv(m.reg, w)
        };
        self.alu(4, dst, src, w);
        Ok(())
    }

    fn op_xchg_rm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let rm = self.rd_rmw(bus, &m, w)?;
        if w == 8 {
            let reg = self.reg8(m.reg);
            self.wr_rm8(bus, &m, reg)?;
            self.set_reg8(m.reg, rm as u8);
        } else {
            let reg = self.regv(m.reg, w);
            self.wr_rmv(bus, &m, w, reg)?;
            self.set_regv(m.reg, w, rm);
        }
        Ok(())
    }

    fn op_mov_rm_r8<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let m = self.modrm(bus)?;
        let value = self.reg8(m.reg);
        self.wr_rm8(bus, &m, value)
    }

    fn op_mov_rm_rv<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        let value = self.regv(m.reg, w);
        self.wr_rmv(bus, &m, w, value)
    }

    fn op_mov_r8_rm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let m = self.modrm(bus)?;
        let value = self.rd_rm8(bus, &m)?;
        self.set_reg8(m.reg, value);
        Ok(())
    }

    fn op_mov_rv_rm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        let value = self.rd_rmv(bus, &m, w)?;
        self.set_regv(m.reg, w, value);
        Ok(())
    }

    fn op_mov_rm_sreg<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let m = self.modrm(bus)?;
        if m.reg > 5 {
            return Err(Trap::Fault(6));
        }
        let value = self.sregs[m.reg as usize];
        if m.is_reg {
            let w = self.vw();
            self.set_regv(m.rm, w, value as u32);
            Ok(())
        } else {
            self.wr16(bus, m.seg as usize, m.off, value)
        }
    }

    fn op_lea<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        if m.is_reg {
            return Err(Trap::Fault(6));
        }
        self.set_regv(m.reg, w, m.off);
        Ok(())
    }

    fn op_mov_sreg_rm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let m = self.modrm(bus)?;
        if m.reg > 5 || m.reg as usize == CS {
            return Err(Trap::Fault(6));
        }
        let value = self.rd_rmv(bus, &m, 16)? as u16;
        self.load_seg(bus, m.reg as usize, value)?;
        if m.reg as usize == SS {
            self.inhibit = true;
        }
        Ok(())
    }

    fn op_pop_rm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        // The address is computed after the pop, so an ESP base sees the new value.
        let value = self.popv(bus, w)?;
        let m = self.modrm(bus)?;
        self.wr_rmv(bus, &m, w, value)
    }

    fn op_xchg_ax<B: Bus>(&mut self, _bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let index = opcode & 7;
        let value = self.regv(0, w);
        let other = self.regv(index, w);
        self.set_regv(0, w, other);
        self.set_regv(index, w, value);
        Ok(())
    }

    fn op_cbw<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        if self.op32 {
            self.r[AX] = self.r[AX] as u16 as i16 as i32 as u32;
        } else {
            self.set_reg16(AX as u8, self.r[AX] as u8 as i8 as i16 as u16);
        }
        Ok(())
    }

    fn op_cwd<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        if self.op32 {
            self.r[DX] = ((self.r[AX] as i32) >> 31) as u32;
        } else {
            self.set_reg16(DX as u8, ((self.r[AX] as i16) >> 15) as u16);
        }
        Ok(())
    }

    fn op_call_far_imm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let offset = self.fetchw(bus, w)?;
        let selector = self.fetch16(bus)?;
        self.call_far(bus, selector, offset, w)
    }

    fn op_jmp_far_imm<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let offset = self.fetchw(bus, w)?;
        let selector = self.fetch16(bus)?;
        self.jmp_far(bus, selector, offset)
    }

    fn op_wait<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        if self.cr0 & 0xA == 0xA {
            return Err(Trap::Fault(7));
        }
        Ok(())
    }

    fn op_pushf<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        if self.vm86 && self.iopl() < 3 {
            return Err(Trap::FaultCode(13, 0));
        }
        let w = self.vw();
        // The image never shows VM or RF.
        let flags = (self.flags() | 2) & 0x0000_7FD7;
        self.pushv(bus, flags, w)
    }

    fn op_popf<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        if self.vm86 && self.iopl() < 3 {
            return Err(Trap::FaultCode(13, 0));
        }
        let w = self.vw();
        let value = self.popv(bus, w)?;
        self.load_flags(value, w);
        Ok(())
    }

    /// Writes FLAGS from guest data under the current privilege rules.
    pub(crate) fn load_flags(&mut self, value: u32, w: u32) {
        let mut writable = ARITH_FLAGS | FLAG_TF | FLAG_DF | FLAG_NT;
        if self.vm86 {
            // Only reached with IOPL 3, and IOPL itself stays out of reach.
            writable |= FLAG_IF;
        } else if !self.pmode || self.cpl == 0 {
            writable |= FLAG_IOPL | FLAG_IF;
        } else if self.cpl <= self.iopl() {
            writable |= FLAG_IF;
        }
        if w == 16 {
            writable &= 0xFFFF;
        }
        self.lazy = 0;
        self.eflags = (self.eflags & !writable) | (value & writable) | 2;
    }

    fn op_sahf<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        self.materialize();
        let ah = (self.r[AX] >> 8) & 0xD5;
        self.eflags = (self.eflags & !0xD5) | ah | 2;
        Ok(())
    }

    fn op_lahf<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        let low = (self.flags() & 0xD5) as u8 | 2;
        self.set_reg8(4, low);
        Ok(())
    }

    fn op_mov_moffs<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let off = if self.addr32 {
            self.fetch32(bus)?
        } else {
            self.fetch16(bus)? as u32
        };
        let seg = self.seg_override.map_or(DS, |seg| seg as usize);
        if opcode < 0xA2 {
            let value = self.rdw(bus, seg, off, w)?;
            if w == 8 {
                self.set_reg8(0, value as u8);
            } else {
                self.set_regv(0, w, value);
            }
            Ok(())
        } else {
            let value = self.regv(0, if w == 8 { 16 } else { w });
            self.wrw(bus, seg, off, w, value)
        }
    }

    fn op_test_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let imm = self.fetchw(bus, w)?;
        let acc = self.r[AX];
        self.alu(4, acc, imm, w);
        Ok(())
    }

    #[inline(always)]
    fn count_reg(&self) -> u32 {
        if self.addr32 {
            self.r[CX]
        } else {
            self.r[CX] & 0xFFFF
        }
    }

    #[inline(always)]
    fn set_count_reg(&mut self, value: u32) {
        if self.addr32 {
            self.r[CX] = value;
        } else {
            self.set_reg16(CX as u8, value as u16);
        }
    }

    #[inline(always)]
    fn addr_reg(&self, index: usize) -> u32 {
        if self.addr32 {
            self.r[index]
        } else {
            self.r[index] & 0xFFFF
        }
    }

    #[inline(always)]
    fn addr_reg_add(&mut self, index: usize, amount: u32) {
        if self.addr32 {
            self.r[index] = self.r[index].wrapping_add(amount);
        } else {
            let value = (self.r[index] as u16).wrapping_add(amount as u16);
            self.set_reg16(index as u8, value);
        }
    }

    fn op_string<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let step = if self.df() {
            (w / 8).wrapping_neg()
        } else {
            w / 8
        };
        if self.rep == 0 {
            return self.string_once(bus, opcode, w, step);
        }
        let mut count = self.count_reg();
        if count == 0 {
            return Ok(());
        }
        if !self.paging && !self.df() && matches!(opcode, 0xA4 | 0xA5 | 0xAA | 0xAB) {
            let done = self.string_fast(bus, opcode, w, count);
            if done != 0 {
                count -= done;
                self.set_count_reg(count);
                self.cycles = self.cycles.wrapping_add(done as u64);
                if count != 0 {
                    self.ip = self.start_ip;
                }
                return Ok(());
            }
        }
        let compare = matches!(opcode, 0xA6 | 0xA7 | 0xAE | 0xAF);
        let mut chunk = count.min(REP_CHUNK);
        let mut done = 0u64;
        while chunk > 0 {
            self.string_once(bus, opcode, w, step)?;
            count -= 1;
            chunk -= 1;
            done += 1;
            self.set_count_reg(count);
            if compare && self.zf() != (self.rep == 0xF3) {
                self.cycles = self.cycles.wrapping_add(done);
                return Ok(());
            }
        }
        self.cycles = self.cycles.wrapping_add(done);
        if count != 0 {
            self.ip = self.start_ip;
        }
        Ok(())
    }

    /// `REP MOVS` and `REP STOS` over plain RAM. Returns the iterations done.
    fn string_fast<B: Bus>(&mut self, bus: &mut B, opcode: u8, w: u32, count: u32) -> u32 {
        let size = w / 8;
        let iterations = count.min(REP_CHUNK_FAST);
        let bytes = iterations * size;
        let di = self.addr_reg(DI);
        let limit = if self.addr32 { u32::MAX } else { 0xFFFF };
        let end_di = match di.checked_add(bytes - 1) {
            Some(end) if end <= limit => end,
            _ => return 0,
        };
        let es = &self.seg[ES];
        if di < es.lo || end_di > es.hi || !es.writable {
            return 0;
        }
        let dst = es.base.wrapping_add(di);
        if opcode >= 0xAA {
            let value = self.r[AX];
            if !bus.fast_fill(dst, value, size, bytes) {
                return 0;
            }
        } else {
            let si = self.addr_reg(SI);
            let end_si = match si.checked_add(bytes - 1) {
                Some(end) if end <= limit => end,
                _ => return 0,
            };
            let seg = self.seg_override.map_or(DS, |seg| seg as usize);
            let s = &self.seg[seg];
            if si < s.lo || end_si > s.hi || !s.readable {
                return 0;
            }
            let src = s.base.wrapping_add(si);
            if !bus.fast_copy(dst, src, bytes) {
                return 0;
            }
            self.addr_reg_add(SI, bytes);
        }
        self.addr_reg_add(DI, bytes);
        iterations
    }

    fn string_once<B: Bus>(&mut self, bus: &mut B, opcode: u8, w: u32, step: u32) -> Exec {
        let seg = self.seg_override.map_or(DS, |seg| seg as usize);
        match opcode {
            0xA4 | 0xA5 => {
                let si = self.addr_reg(SI);
                let di = self.addr_reg(DI);
                let value = self.rdw(bus, seg, si, w)?;
                self.wrw(bus, ES, di, w, value)?;
                self.addr_reg_add(SI, step);
                self.addr_reg_add(DI, step);
            }
            0xA6 | 0xA7 => {
                let si = self.addr_reg(SI);
                let di = self.addr_reg(DI);
                let a = self.rdw(bus, seg, si, w)?;
                let b = self.rdw(bus, ES, di, w)?;
                self.alu(7, a, b, w);
                self.addr_reg_add(SI, step);
                self.addr_reg_add(DI, step);
            }
            0xAA | 0xAB => {
                let di = self.addr_reg(DI);
                let value = self.r[AX];
                self.wrw(bus, ES, di, w, value)?;
                self.addr_reg_add(DI, step);
            }
            0xAC | 0xAD => {
                let si = self.addr_reg(SI);
                let value = self.rdw(bus, seg, si, w)?;
                if w == 8 {
                    self.set_reg8(0, value as u8);
                } else {
                    self.set_regv(0, w, value);
                }
                self.addr_reg_add(SI, step);
            }
            _ => {
                let di = self.addr_reg(DI);
                let b = self.rdw(bus, ES, di, w)?;
                let a = self.r[AX];
                self.alu(7, a, b, w);
                self.addr_reg_add(DI, step);
            }
        }
        Ok(())
    }

    fn op_mov_r8_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let value = self.fetch8(bus)?;
        self.set_reg8(opcode & 7, value);
        Ok(())
    }

    fn op_mov_rv_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let value = self.fetchw(bus, w)?;
        self.set_regv(opcode & 7, w, value);
        Ok(())
    }

    /// The shift and rotate group. Rotates leave SF, ZF, PF and AF alone.
    pub(crate) fn shift(&mut self, kind: u8, value: u32, count: u8, w: u32) -> u32 {
        let count = (count & 0x1F) as u32;
        if count == 0 {
            return value;
        }
        let m = mask(w);
        let value = value & m;
        let sign = 1u32 << (w - 1);
        let cf_in = self.cf();
        self.materialize();
        match kind & 7 {
            0 | 1 => {
                let n = count % w;
                let res = if n == 0 {
                    value
                } else if kind & 7 == 0 {
                    ((value << n) | (value >> (w - n))) & m
                } else {
                    ((value >> n) | (value << (w - n))) & m
                };
                let (cf, of) = if kind & 7 == 0 {
                    let cf = res & 1 != 0;
                    (cf, (res & sign != 0) != cf)
                } else {
                    let top = res & sign != 0;
                    (top, top != (res & (sign >> 1) != 0))
                };
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_OF, of);
                res
            }
            2 | 3 => {
                let n = count % (w + 1);
                let mut res = value;
                let mut cf = cf_in;
                if kind & 7 == 2 {
                    for _ in 0..n {
                        let out = res & sign != 0;
                        res = ((res << 1) | u32::from(cf)) & m;
                        cf = out;
                    }
                    self.set_flag(FLAG_OF, (res & sign != 0) != cf);
                } else {
                    for _ in 0..n {
                        let out = res & 1 != 0;
                        res = (res >> 1) | (u32::from(cf) << (w - 1));
                        cf = out;
                    }
                    self.set_flag(FLAG_OF, (res & sign != 0) != (res & (sign >> 1) != 0));
                }
                self.set_flag(FLAG_CF, cf);
                res
            }
            4 | 6 => {
                let wide = (value as u64) << count;
                let res = (wide as u32) & m;
                let cf = count <= w && (wide >> w) & 1 != 0;
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_OF, (res & sign != 0) != cf);
                self.set_flag(FLAG_AF, false);
                self.set_szp(res, w);
                res
            }
            5 => {
                let res = if count >= w { 0 } else { value >> count };
                let cf = count <= w && (value >> (count - 1)) & 1 != 0;
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_OF, value & sign != 0);
                self.set_flag(FLAG_AF, false);
                self.set_szp(res, w);
                res
            }
            _ => {
                let signed = if w == 32 {
                    value as i32 as i64
                } else if w == 16 {
                    value as u16 as i16 as i64
                } else {
                    value as u8 as i8 as i64
                };
                let res = (signed >> count.min(63)) as u32 & m;
                let cf = (signed >> (count - 1).min(63)) & 1 != 0;
                self.set_flag(FLAG_CF, cf);
                self.set_flag(FLAG_OF, false);
                self.set_flag(FLAG_AF, false);
                self.set_szp(res, w);
                res
            }
        }
    }

    fn op_shift<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let count = match opcode {
            0xC0 | 0xC1 => self.fetch8(bus)?,
            0xD0 | 0xD1 => 1,
            _ => self.r[CX] as u8,
        };
        let value = self.rd_rmw(bus, &m, w)?;
        let res = self.shift(m.reg, value, count, w);
        self.wr_rmw(bus, &m, w, res)
    }

    fn op_ret<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let extra = if opcode == 0xC2 {
            self.fetch16(bus)? as u32
        } else {
            0
        };
        let target = self.popv(bus, w)?;
        self.branch(target)?;
        self.sp_add(extra);
        Ok(())
    }

    fn op_load_far<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let index = if opcode == 0xC4 { ES } else { DS };
        self.load_far_pointer(bus, index)
    }

    /// `LDS`, `LES`, `LFS`, `LGS` and `LSS`.
    pub(crate) fn load_far_pointer<B: Bus>(&mut self, bus: &mut B, index: usize) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        if m.is_reg {
            return Err(Trap::Fault(6));
        }
        let seg = m.seg as usize;
        let offset = self.rdv(bus, seg, m.off, w)?;
        let at = self.wrap_addr(m.off.wrapping_add(w / 8));
        let selector = self.rd16(bus, seg, at)?;
        self.load_seg(bus, index, selector)?;
        self.set_regv(m.reg, w, offset);
        Ok(())
    }

    fn op_mov_rm_imm<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let value = self.fetchw(bus, w)?;
        self.wr_rmw(bus, &m, w, value)
    }

    fn op_enter<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let size = self.fetch16(bus)? as u32;
        let nesting = self.fetch8(bus)? & 0x1F;
        let bp = self.regv(BP as u8, w);
        self.pushv(bus, bp, w)?;
        let frame = self.regv(SP as u8, w);
        if nesting > 0 {
            let step = w / 8;
            let big = self.sp_big();
            let mut walk = if big { self.r[BP] } else { self.r[BP] & 0xFFFF };
            for _ in 1..nesting {
                walk = walk.wrapping_sub(step);
                if !big {
                    walk &= 0xFFFF;
                }
                let value = self.rdv(bus, SS, walk, w)?;
                self.pushv(bus, value, w)?;
            }
            self.pushv(bus, frame, w)?;
        }
        self.set_regv(BP as u8, w, frame);
        self.sp_add(size.wrapping_neg());
        Ok(())
    }

    fn op_leave<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let bp = if self.sp_big() {
            self.r[BP]
        } else {
            self.r[BP] & 0xFFFF
        };
        let value = self.rdv(bus, SS, bp, w)?;
        if self.sp_big() {
            self.r[SP] = bp.wrapping_add(w / 8);
        } else {
            self.set_reg16(SP as u8, (bp as u16).wrapping_add((w / 8) as u16));
        }
        self.set_regv(BP as u8, w, value);
        Ok(())
    }

    fn op_retf<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = self.vw();
        let extra = if opcode == 0xCA {
            self.fetch16(bus)? as u32
        } else {
            0
        };
        self.ret_far(bus, extra, w)
    }

    fn op_int<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let vector = match opcode {
            0xCC => 3,
            0xCE => {
                if !self.of() {
                    return Ok(());
                }
                4
            }
            0xF1 => 1,
            _ => {
                // Only `INT n` goes by IOPL in virtual-8086 mode.
                if self.vm86 && self.iopl() < 3 {
                    return Err(Trap::FaultCode(13, 0));
                }
                self.fetch8(bus)?
            }
        };
        self.no_trap = true;
        self.int_source = IntSource::Software;
        self.interrupt(bus, vector, None, opcode != 0xF1)
    }

    fn op_iret<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        self.no_trap = true;
        if self.pmode {
            return self.iret_protected(bus, w);
        }
        if self.vm86 && self.iopl() < 3 {
            return Err(Trap::FaultCode(13, 0));
        }
        let ip = self.popv(bus, w)?;
        let cs = self.popv(bus, w)? as u16;
        let flags = self.popv(bus, w)?;
        self.set_real_segment(CS, cs);
        self.ip = if w == 32 { ip } else { ip & 0xFFFF };
        self.load_flags(flags, w);
        Ok(())
    }

    fn op_aam<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let base = self.fetch8(bus)?;
        if base == 0 {
            return Err(Trap::Fault(0));
        }
        let al = self.r[AX] as u8;
        self.set_reg8(4, al / base);
        self.set_reg8(0, al % base);
        self.set_szp(self.r[AX] & 0xFF, 8);
        Ok(())
    }

    fn op_aad<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let base = self.fetch8(bus)?;
        let al = (self.r[AX] as u8).wrapping_add(((self.r[AX] >> 8) as u8).wrapping_mul(base));
        self.set_reg8(0, al);
        self.set_reg8(4, 0);
        self.set_szp(al as u32, 8);
        Ok(())
    }

    fn op_salc<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        self.set_reg8(0, if self.cf() { 0xFF } else { 0 });
        Ok(())
    }

    fn op_xlat<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let seg = self.seg_override.map_or(DS, |seg| seg as usize);
        let off = self.wrap_addr(self.r[BX].wrapping_add(self.r[AX] & 0xFF));
        let value = self.rd8(bus, seg, off)?;
        self.set_reg8(0, value);
        Ok(())
    }

    fn op_esc<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        if self.cr0 & 0xC != 0 {
            return Err(Trap::Fault(7));
        }
        let m = self.modrm(bus)?;
        self.fpu_execute(bus, opcode, &m)
    }

    fn op_loop<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let rel = self.fetch8(bus)? as i8 as i32 as u32;
        let take = if opcode == 0xE3 {
            self.count_reg() == 0
        } else {
            let count = self.count_reg().wrapping_sub(1);
            self.set_count_reg(count);
            let count = self.count_reg();
            match opcode {
                0xE0 => count != 0 && !self.zf(),
                0xE1 => count != 0 && self.zf(),
                _ => count != 0,
            }
        };
        if take {
            self.branch(self.ip.wrapping_add(rel))?;
        }
        Ok(())
    }

    fn op_in_out<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let port = if opcode < 0xE8 {
            self.fetch8(bus)? as u16
        } else {
            self.r[DX] as u16
        };
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        self.io_allowed(bus, port, w / 8)?;
        bus.sync(self.cycles);
        if opcode & 2 == 0 {
            match w {
                8 => {
                    let value = bus.in8(port);
                    self.set_reg8(0, value);
                }
                16 => {
                    let value = bus.in16(port);
                    self.set_reg16(0, value);
                }
                _ => self.r[AX] = bus.in32(port),
            }
        } else {
            match w {
                8 => bus.out8(port, self.r[AX] as u8),
                16 => bus.out16(port, self.r[AX] as u16),
                _ => bus.out32(port, self.r[AX]),
            }
        }
        Ok(())
    }

    fn op_call_near<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let rel = self.fetchw(bus, w)?;
        let ip = self.ip;
        let target = ip.wrapping_add(rel);
        self.pushv(bus, ip, w)?;
        self.branch(target)
    }

    fn op_jmp_near<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let rel = self.fetchw(bus, w)?;
        self.branch(self.ip.wrapping_add(rel))
    }

    fn op_jmp_short<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let rel = self.fetch8(bus)? as i8 as i32 as u32;
        self.branch(self.ip.wrapping_add(rel))
    }

    fn op_hlt<B: Bus>(&mut self, _bus: &mut B, _opcode: u8) -> Exec {
        if self.cpl != 0 {
            return Err(Trap::FaultCode(13, 0));
        }
        Err(Trap::Hlt)
    }

    fn op_flag<B: Bus>(&mut self, _bus: &mut B, opcode: u8) -> Exec {
        match opcode {
            0xF5 => {
                let cf = self.cf();
                self.set_flag(FLAG_CF, !cf);
            }
            0xF8 => self.set_flag(FLAG_CF, false),
            0xF9 => self.set_flag(FLAG_CF, true),
            0xFA | 0xFB => {
                if self.cpl > self.iopl() {
                    return Err(Trap::FaultCode(13, 0));
                }
                if opcode == 0xFA {
                    self.eflags &= !FLAG_IF;
                } else {
                    if self.eflags & FLAG_IF == 0 {
                        self.inhibit = true;
                    }
                    self.eflags |= FLAG_IF;
                }
            }
            0xFC => self.eflags &= !FLAG_DF,
            _ => self.eflags |= FLAG_DF,
        }
        Ok(())
    }

    fn op_group3<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let w = if opcode & 1 == 0 { 8 } else { self.vw() };
        let m = self.modrm(bus)?;
        let value = self.rd_rmw(bus, &m, w)?;
        match m.reg {
            0 | 1 => {
                let imm = self.fetchw(bus, w)?;
                self.alu(4, value, imm, w);
            }
            2 => self.wr_rmw(bus, &m, w, !value & mask(w))?,
            3 => {
                let res = self.alu(5, 0, value, w);
                self.wr_rmw(bus, &m, w, res)?;
            }
            4 => {
                let over = match w {
                    8 => {
                        let res = (self.r[AX] & 0xFF) * value;
                        self.set_reg16(AX as u8, res as u16);
                        res >> 8 != 0
                    }
                    16 => {
                        let res = (self.r[AX] & 0xFFFF) * value;
                        self.set_reg16(AX as u8, res as u16);
                        self.set_reg16(DX as u8, (res >> 16) as u16);
                        res >> 16 != 0
                    }
                    _ => {
                        let res = self.r[AX] as u64 * value as u64;
                        self.r[AX] = res as u32;
                        self.r[DX] = (res >> 32) as u32;
                        res >> 32 != 0
                    }
                };
                self.materialize();
                self.set_flag(FLAG_CF, over);
                self.set_flag(FLAG_OF, over);
            }
            5 => {
                let over = match w {
                    8 => {
                        let res = (self.r[AX] as u8 as i8 as i16) * (value as u8 as i8 as i16);
                        self.set_reg16(AX as u8, res as u16);
                        res != res as i8 as i16
                    }
                    16 => {
                        let res = (self.r[AX] as u16 as i16 as i32) * (value as u16 as i16 as i32);
                        self.set_reg16(AX as u8, res as u16);
                        self.set_reg16(DX as u8, (res >> 16) as u16);
                        res != res as i16 as i32
                    }
                    _ => {
                        let res = (self.r[AX] as i32 as i64) * (value as i32 as i64);
                        self.r[AX] = res as u32;
                        self.r[DX] = (res >> 32) as u32;
                        res != res as i32 as i64
                    }
                };
                self.materialize();
                self.set_flag(FLAG_CF, over);
                self.set_flag(FLAG_OF, over);
            }
            6 => {
                if value == 0 {
                    return Err(Trap::Fault(0));
                }
                match w {
                    8 => {
                        let dividend = self.r[AX] & 0xFFFF;
                        let quotient = dividend / value;
                        if quotient > 0xFF {
                            return Err(Trap::Fault(0));
                        }
                        self.set_reg8(0, quotient as u8);
                        self.set_reg8(4, (dividend % value) as u8);
                    }
                    16 => {
                        let dividend = ((self.r[DX] & 0xFFFF) << 16) | (self.r[AX] & 0xFFFF);
                        let quotient = dividend / value;
                        if quotient > 0xFFFF {
                            return Err(Trap::Fault(0));
                        }
                        self.set_reg16(AX as u8, quotient as u16);
                        self.set_reg16(DX as u8, (dividend % value) as u16);
                    }
                    _ => {
                        let dividend = ((self.r[DX] as u64) << 32) | self.r[AX] as u64;
                        let quotient = dividend / value as u64;
                        if quotient > 0xFFFF_FFFF {
                            return Err(Trap::Fault(0));
                        }
                        self.r[AX] = quotient as u32;
                        self.r[DX] = (dividend % value as u64) as u32;
                    }
                }
            }
            _ => {
                if value == 0 {
                    return Err(Trap::Fault(0));
                }
                match w {
                    8 => {
                        let dividend = self.r[AX] as u16 as i16 as i32;
                        let divisor = value as u8 as i8 as i32;
                        let quotient = dividend / divisor;
                        if quotient != quotient as i8 as i32 {
                            return Err(Trap::Fault(0));
                        }
                        self.set_reg8(0, quotient as u8);
                        self.set_reg8(4, (dividend % divisor) as u8);
                    }
                    16 => {
                        let dividend =
                            (((self.r[DX] & 0xFFFF) << 16) | (self.r[AX] & 0xFFFF)) as i32 as i64;
                        let divisor = value as u16 as i16 as i64;
                        let quotient = dividend / divisor;
                        if quotient != quotient as i16 as i64 {
                            return Err(Trap::Fault(0));
                        }
                        self.set_reg16(AX as u8, quotient as u16);
                        self.set_reg16(DX as u8, (dividend % divisor) as u16);
                    }
                    _ => {
                        let dividend = (((self.r[DX] as u64) << 32) | self.r[AX] as u64) as i64;
                        let divisor = value as i32 as i64;
                        let (Some(quotient), Some(remainder)) =
                            (dividend.checked_div(divisor), dividend.checked_rem(divisor))
                        else {
                            return Err(Trap::Fault(0));
                        };
                        if quotient != quotient as i32 as i64 {
                            return Err(Trap::Fault(0));
                        }
                        self.r[AX] = quotient as u32;
                        self.r[DX] = remainder as u32;
                    }
                }
            }
        }
        Ok(())
    }

    fn op_group4<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Exec {
        let byte = self.fetch8(bus)?;
        if (byte == CALLBACK_MODRM || byte == CALLBACK_MODRM + 1) && !self.pmode {
            let linear = self.seg[CS].base.wrapping_add(self.start_ip);
            if (CALLBACK_ROM_BASE..=0xF_FFFF).contains(&linear) {
                let vector = self.fetch8(bus)?;
                return self.callback(bus, vector, byte == CALLBACK_MODRM);
            }
        }
        if byte == HOST_CALL_MODRM {
            let linear = self.seg[CS].base.wrapping_add(self.start_ip);
            if (CALLBACK_ROM_BASE..=0xF_FFFF).contains(&linear) {
                let code = self.fetch8(bus)?;
                return self.host_trap(bus, code);
            }
        }
        let m = self.modrm_byte(bus, byte)?;
        if m.reg > 1 {
            return self.op_ud(bus, opcode);
        }
        let value = self.rd_rm8(bus, &m)? as u32;
        let res = self.inc_dec(value, m.reg == 1, 8);
        self.wr_rm8(bus, &m, res as u8)
    }

    fn op_group5<B: Bus>(&mut self, bus: &mut B, _opcode: u8) -> Exec {
        let w = self.vw();
        let m = self.modrm(bus)?;
        match m.reg {
            0 | 1 => {
                let value = self.rd_rmv(bus, &m, w)?;
                let res = self.inc_dec(value, m.reg == 1, w);
                self.wr_rmv(bus, &m, w, res)
            }
            2 => {
                let target = self.rd_rmv(bus, &m, w)?;
                let ip = self.ip;
                self.pushv(bus, ip, w)?;
                self.branch(target)
            }
            3 | 5 => {
                if m.is_reg {
                    return Err(Trap::Fault(6));
                }
                let seg = m.seg as usize;
                let offset = self.rdv(bus, seg, m.off, w)?;
                let at = self.wrap_addr(m.off.wrapping_add(w / 8));
                let selector = self.rd16(bus, seg, at)?;
                if m.reg == 3 {
                    self.call_far(bus, selector, offset, w)
                } else {
                    self.jmp_far(bus, selector, offset)
                }
            }
            4 => {
                let target = self.rd_rmv(bus, &m, w)?;
                self.branch(target)
            }
            6 => {
                let value = self.rd_rmv(bus, &m, w)?;
                self.pushv(bus, value, w)
            }
            _ => Err(Trap::Fault(6)),
        }
    }
}
