//! A memory manager in the style of EMM386: programs run in virtual-8086 mode
//! under a monitor in the machine, which offers VCPI to DOS extenders.
//!
//! `EMM386 ON` at the prompt turns it on for the programs started after that. The
//! monitor is native: there are no guest tables at level 0, and interrupts go
//! straight to the guest's vector table, because the program runs with IOPL 3. A
//! `#GP` reaches [`crate::cpu::Cpu`]'s monitor, which emulates what is harmless and
//! ends the program otherwise: that's what happens to an extender that doesn't know
//! VCPI and tries to set `CR0` itself. A guest's own monitor, for instance a real
//! `EMM386.EXE`, uses the CPU's virtual-8086 mode with real tables instead.
//!
//! VCPI's `INT 67h AX=DE0Ch` hands a client protected mode at level 0, with its
//! own `CR3`, GDT, IDT, LDT and TSS; a far call to the entry point `DE01h` returned
//! brings it back to virtual-8086 mode. Paging is only on while a client runs.
//! Physical pages come out of XMS blocks: one set aside when a program starts,
//! because an extender may well take all the XMS there is first, and more as it runs
//! out.

use alloc::vec;
use alloc::vec::Vec;

use crate::bios::HOST_SEGMENT;
use crate::cpu::{self, Bus, Cpu, CpuRegs, TableReg};
use crate::dpmi::VCPI_ENTRY;
use crate::machine::SystemBus;
use crate::memory::Memory;
use crate::xms::{Ems, Xms};

const PAGE: u32 = 4096;
/// Page table entries `DE01h` fills in: the first megabyte and the HMA.
const IDENTITY_PTES: u32 = 0x110;
/// The pages set aside when a program starts, and the size of later additions.
const RESERVE_KB: u32 = 2048;
const GROW_KB: u32 = 256;

/// The manager's state.
pub struct Emm386 {
    /// Run programs under the monitor.
    pub enabled: bool,
    pools: Vec<Pool>,
    /// `DE0Ch` from virtual-8086 mode, waiting for the CPU: the structure's address.
    pending_switch: Option<u32>,
    pic: (u8, u8),
}

struct Pool {
    base: u32,
    used: Vec<bool>,
}

impl Default for Emm386 {
    fn default() -> Self {
        Self::new()
    }
}

impl Emm386 {
    pub fn new() -> Self {
        Self {
            enabled: false,
            pools: Vec::new(),
            pending_switch: None,
            pic: (0x08, 0x70),
        }
    }

    /// A program ended: everything it had goes back, and the switch stays.
    pub fn reset(&mut self) {
        self.pools.clear();
        self.pending_switch = None;
        self.pic = (0x08, 0x70);
    }

    /// Adds a block of pages.
    fn grow(&mut self, xms: &mut Xms, size_kb: u32) -> bool {
        let size_kb = size_kb.min(xms.largest_free_kb()) & !3;
        if size_kb == 0 {
            return false;
        }
        let Some((_, base)) = xms.alloc_block(size_kb) else {
            return false;
        };
        self.pools.push(Pool {
            base,
            used: vec![false; (size_kb / 4) as usize],
        });
        true
    }

    /// Sets pages aside, when a program starts under the monitor.
    pub fn reserve(&mut self, xms: &mut Xms) {
        if self.pools.is_empty() {
            self.grow(xms, RESERVE_KB);
        }
    }

    fn take_page(&mut self) -> Option<u32> {
        self.pools.iter_mut().find_map(|pool| {
            let index = pool.used.iter().position(|page| !*page)?;
            pool.used[index] = true;
            Some(pool.base + index as u32 * PAGE)
        })
    }

    /// The VCPI functions that need only the registers.
    pub fn service(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, xms: &mut Xms, ems: &Ems, cr0: u32) {
        let ok = |regs: &mut CpuRegs<'_>| regs.set_ah(0);
        match regs.al() {
            0x00 => {
                regs.set_bx(0x0100);
                ok(regs);
            }
            0x01 => {
                // The client's page table 0 at ES:DI and its descriptors at DS:SI.
                // Either can be in the EMS frame, so they go through its mapping.
                let store = |mem: &mut Memory, address: u32, value: u32| {
                    mem.write32(ems.translate(address).unwrap_or(address), value);
                };
                let table = ((regs.es() as u32) << 4).wrapping_add(regs.di() as u32);
                for page in 0..IDENTITY_PTES {
                    let target = page * PAGE;
                    let mapped = ems.translate(target).unwrap_or(target);
                    store(mem, table + page * 4, mapped | 7);
                }
                let descriptors = ((regs.ds() as u32) << 4).wrapping_add(regs.si() as u32);
                // Code, as a 32-bit segment over the ROM; two data segments.
                store(mem, descriptors, 0x0000_FFFF);
                store(mem, descriptors + 4, 0x0040_9A0F);
                store(mem, descriptors + 8, 0x0000_FFFF);
                store(mem, descriptors + 12, 0x0040_920F);
                store(mem, descriptors + 16, 0x0000_FFFF);
                store(mem, descriptors + 20, 0x00CF_9200);
                regs.set_di(regs.di().wrapping_add((IDENTITY_PTES * 4) as u16));
                let entry = ((HOST_SEGMENT as u32) << 4) - 0xF_0000 + VCPI_ENTRY as u32;
                regs.r[cpu::BX] = entry;
                ok(regs);
            }
            0x02 => {
                regs.r[cpu::DX] = crate::memory::EXTENDED_BASE + crate::memory::EXTENDED_KB * 1024 - PAGE;
                ok(regs);
            }
            0x03 => {
                let free: usize = self
                    .pools
                    .iter()
                    .map(|pool| pool.used.iter().filter(|page| !**page).count())
                    .sum();
                regs.r[cpu::DX] = free as u32 + xms.free_kb().1 / 4;
                ok(regs);
            }
            0x04 => {
                let mut found = self.take_page();
                if found.is_none() && self.grow(xms, GROW_KB) {
                    found = self.take_page();
                }
                match found {
                    Some(address) => {
                        // A page's worth of guest work, which this call is cheap for.
                        mem.fill(address, PAGE as usize, 0);
                        regs.r[cpu::DX] = address;
                        ok(regs);
                    }
                    None => regs.set_ah(0x88),
                }
            }
            0x05 => {
                let address = regs.r[cpu::DX];
                let freed = self.pools.iter_mut().any(|pool| {
                    let index = (address.wrapping_sub(pool.base) / PAGE) as usize;
                    match pool.used.get_mut(index) {
                        Some(page) if address >= pool.base => {
                            *page = false;
                            true
                        }
                        _ => false,
                    }
                });
                regs.set_ah(if freed { 0 } else { 0x8A });
            }
            0x06 => {
                // A page of the EMS frame is wherever the frame maps it now.
                let page = regs.cx() as u32;
                if page < 256 {
                    let address = page * PAGE;
                    regs.r[cpu::DX] = ems.translate(address).unwrap_or(address);
                    ok(regs);
                } else {
                    regs.set_ah(0x8B);
                }
            }
            0x07 => {
                regs.r[cpu::BX] = cr0;
                ok(regs);
            }
            0x08 | 0x09 => ok(regs),
            0x0A => {
                regs.set_bx(self.pic.0 as u16);
                regs.set_cx(self.pic.1 as u16);
                ok(regs);
            }
            0x0B => {
                self.pic = (regs.bx() as u8, regs.cx() as u8);
                ok(regs);
            }
            0x0C => {
                self.pending_switch = Some(regs.r[cpu::SI]);
                ok(regs);
            }
            _ => regs.set_ah(0x8F),
        }
    }

    /// Takes a switch to protected mode asked for by `DE0Ch`.
    pub(crate) fn take_switch(&mut self) -> Option<u32> {
        self.pending_switch.take()
    }
}

impl SystemBus {
    /// `DE0Ch` from virtual-8086 mode: the structure at the linear address holds
    /// `CR3`, pointers to the GDT and IDT images, LDTR, TR and `EIP`, `CS`.
    pub(crate) fn vcpi_enter_client(&mut self, cpu: &mut Cpu, at: u32) {
        // All of it can be in the EMS frame, so it's read through the bus.
        let table = |bus: &mut Self, pointer: u32| TableReg {
            base: Bus::read32(bus, pointer.wrapping_add(2)),
            limit: Bus::read16(bus, pointer) as u32,
        };
        let cr3 = Bus::read32(self, at);
        let pointer = Bus::read32(self, at.wrapping_add(4));
        let gdtr = table(self, pointer);
        let pointer = Bus::read32(self, at.wrapping_add(8));
        let idtr = table(self, pointer);
        let ldt = Bus::read16(self, at.wrapping_add(0x0C));
        let tr = Bus::read16(self, at.wrapping_add(0x0E));
        let eip = Bus::read32(self, at.wrapping_add(0x10));
        let cs = Bus::read16(self, at.wrapping_add(0x14));
        self.mem.set_a20(true);
        if let Err(trap) = cpu.enter_vcpi_client(self, cr3, gdtr, idtr, ldt, tr, cs, eip) {
            let line = alloc::format!(
                "VCPI: {trap:?}: CR2 {:08X} CR3 {cr3:08X} GDT {:08X}/{:X} TR {tr:04X} {cs:04X}:{eip:08X}",
                cpu.cr2, gdtr.base, gdtr.limit
            );
            self.stop_program(&line);
        }
    }

    /// The far call a client in protected mode makes to the entry point.
    pub(crate) fn vcpi_entry(&mut self, cpu: &mut Cpu) {
        if cpu.r[cpu::AX] & 0xFFFF != 0xDE0C {
            let cr0 = cpu.cr0;
            let mut flags = cpu.flags();
            {
                let Self {
                    emm, mem, xms, ems, ..
                } = self;
                let mut regs = CpuRegs {
                    r: &mut cpu.r,
                    sregs: &mut cpu.sregs,
                    flags: &mut flags,
                };
                emm.service(&mut regs, mem, xms, ems, cr0);
            }
            return;
        }
        // The far return address, then EIP, CS, EFLAGS, ESP, SS, ES, DS, FS, GS.
        let mut words = [0u32; 11];
        for (index, word) in words.iter_mut().enumerate() {
            let off = cpu.r[cpu::SP].wrapping_add(index as u32 * 4);
            match cpu.host_read(self, cpu::SS, off, 32) {
                Ok(value) => *word = value,
                Err(_) => {
                    self.stop_program("VCPI: the client's stack is not usable.");
                    return;
                }
            }
        }
        let mut context = cpu.context();
        context.ip = words[2];
        context.sregs[cpu::CS] = words[3] as u16;
        context.flags = words[4];
        context.r[cpu::SP] = words[5] & 0xFFFF;
        context.sregs[cpu::SS] = words[6] as u16;
        context.sregs[cpu::ES] = words[7] as u16;
        context.sregs[cpu::DS] = words[8] as u16;
        context.sregs[cpu::FS] = words[9] as u16;
        context.sregs[cpu::GS] = words[10] as u16;
        cpu.enter_real_context(&context);
    }

    /// The monitor found something it can't let a program do.
    pub(crate) fn monitor_fault(&mut self, cpu: &Cpu) {
        let line = alloc::format!(
            "EMM386: privileged operation at {:04X}:{:04X}. Try EMM386 OFF.",
            cpu.sregs[cpu::CS],
            cpu.ip
        );
        self.stop_program(&line);
    }
}
