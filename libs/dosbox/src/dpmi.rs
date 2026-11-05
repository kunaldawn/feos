//! A DPMI 0.9 host: what a DOS extender finds through `INT 2Fh AX=1687h` when it
//! doesn't bring one of its own, and what extenders prefer over taking the CPU
//! themselves.
//!
//! A client far-calls the entry point and comes back in protected mode at level 3
//! with IOPL 3, so `CLI`, `STI` and port I/O work as they do for a DOS program. It
//! gets descriptors, memory and real-mode services through `INT 31h`.
//!
//! The host's code is host calls ([`crate::cpu::HOST_CALL_MODRM`]) in the ROM, and
//! its tables are real ones the CPU uses, in extended memory taken from XMS: a GDT,
//! an IDT whose gates lead every vector to a host call at level 0, a TSS with the
//! level 0 stack, and each client's LDT and locked stack. Paging stays off, so
//! linear addresses are physical and memory blocks are plain XMS blocks.
//!
//! Interrupts in protected mode go to the client's handler (`INT 31h AX=0205h`) or
//! else down through the real-mode vector table, with the registers passed as they
//! are. Hardware interrupts that come in real mode reach a client's handler through
//! the stubs the host puts in the vector table. Clients nest, and one ends when DOS
//! ends its process.
//!
//! The 0.9 specification doesn't translate DOS calls, but `INT 21h` with selectors
//! is what Windows and Borland's servers give, and Borland's RTM relies on it, so
//! the common calls are translated ([`SystemBus::int21_translate`]): names and
//! buffers go through a transfer buffer in conventional memory and DOS runs on that.
//! The calls that need the real mode's world, `EXEC` and console input, still go
//! down. Other extenders do this themselves and never pass such calls down.
//!
//! The structure follows `rust-dos`'s `src/dpmi` (GPL-2.0-or-later), which was the
//! reference for how the mode switches and stacks fit together. See `LICENSING.md`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::bios::HOST_SEGMENT;
use crate::cpu::host::Context;
use crate::cpu::{
    self, Cpu, CpuRegs, InterruptAction, IntSource, SysSeg, TableReg, AX, BP, BX, CS, CX, DI, DS,
    DX, ES, FLAG_CF, FLAG_IF, FLAG_IOPL, FLAG_TF, FS, GS, HOST_MONITOR_FAULT, SI, SP, SS,
};
use crate::machine::SystemBus;

/// The ROM, at `HOST_SEGMENT`: real-mode entry points.
pub const ENTRY: u16 = 0x0000;
const RM_RETURN: u16 = 0x0004;
const RAW_TO_PM: u16 = 0x0008;
const SAVE_RM: u16 = 0x000C;
/// The VCPI entry point, in a client's code segment over the ROM.
pub const VCPI_ENTRY: u16 = 0x0010;
const TERMINATE: u16 = 0x0014;
/// Entry points at level 3, as offsets in `HOST_CODE3`.
const RET_HWINT: u16 = 0x0020;
const RET_CALLBACK: u16 = 0x0024;
const RET_EXCEPTION: u16 = 0x0028;
const RAW_TO_RM: u16 = 0x002C;
const SAVE_PM: u16 = 0x0030;
const MSDOS_API: u16 = 0x0034;
/// A real-mode stub per vector: the host call, then `IRET`.
const RM_INT: u16 = 0x0400;
/// A real-mode stub per callback.
const CALLBACKS: u16 = 0x0800;
const CALLBACK_SLOTS: usize = 16;
/// Where the IDT's gates lead, at level 0, one per vector.
const PM_IDT: u16 = 0x1000;
/// The default interrupt handlers `AX=0204h` hands out, one per vector.
const PM_DEFAULT_INT: u16 = 0x1400;
/// The default exception handlers `AX=0202h` hands out, one per exception.
const PM_DEFAULT_EXC: u16 = 0x1800;
const EXCEPTIONS: usize = 32;

const T_ENTRY: u8 = 0x01;
const T_RM_RETURN: u8 = 0x02;
const T_RAW_TO_PM: u8 = 0x03;
const T_NOTHING: u8 = 0x04;
const T_VCPI: u8 = 0x05;
const T_RM_INT: u8 = 0x06;
const T_CALLBACK: u8 = 0x07;
const T_IDT: u8 = 0x10;
const T_DEFAULT_INT: u8 = 0x11;
const T_DEFAULT_EXC: u8 = 0x12;
const T_RET_HWINT: u8 = 0x13;
const T_RET_CALLBACK: u8 = 0x14;
const T_RET_EXCEPTION: u8 = 0x15;
const T_RAW_TO_RM: u8 = 0x16;
const T_MSDOS_API: u8 = 0x17;

const HOST_CODE0: u16 = 0x08;
const HOST_DATA0: u16 = 0x10;
const HOST_TSS: u16 = 0x18;
const HOST_LDT: u16 = 0x20;
const HOST_CODE3: u16 = 0x2B;
/// The BIOS data area, which programs load as they would segment `0040h`.
const BIOS_DATA: u16 = 0x40;
const GDT_ENTRIES: u32 = 9;

const HOST_IDT: u32 = 0x0100;
const HOST_TSS_AT: u32 = 0x0900;
const HOST_STACK_TOP: u32 = 0x2000;
const HOST_KB: u32 = 8;

const LDT_ENTRIES: usize = 8192;
const LDT_SIZE: u32 = LDT_ENTRIES as u32 * 8;
const LPMS_SIZE: u32 = 0x4000;
const CLIENT_KB: u32 = (LDT_SIZE + LPMS_SIZE) / 1024;
/// `AX=000Dh` hands these out by number, so `AX=0000h` allocates above them.
const RESERVED_LDT: usize = 16;
/// The conventional memory a client gives the host, for its real-mode stack.
const PRIVATE_PARAS: u16 = 0x0300;
/// The start of that memory is the transfer buffer, and the stack is above it.
const TRANSFER_SIZE: u32 = 0x2000;
/// How much of a handler's stack below a nested one is left alone, because the
/// handler can have moved to its own stack and turned interrupts on.
const LPMS_GAP: u32 = 0x400;
const EXEC_BLOCK: u32 = 0x200;
const EXEC_TAIL: u32 = 0x220;
const EXEC_FCB1: u32 = 0x2A0;
const EXEC_FCB2: u32 = 0x2D0;
const FIND_DTA: u32 = TRANSFER_SIZE - 0x80;
/// The depth of mode switches a client can be in.
const MAX_FRAMES: usize = 64;

/// A saved context, to go back to when a return comes.
#[derive(Clone, Copy)]
struct Frame {
    kind: FrameKind,
    context: Context,
    rm_sp: u16,
    lpms_sp: u32,
    /// The registers structure of a real-mode call.
    registers: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// A software interrupt reflected to real mode: registers come back.
    Software,
    /// A hardware interrupt reflected to real mode: nothing changes.
    Hardware,
    /// `AX=0300h` to `0302h`: registers go to the structure.
    RealCall,
    /// A client's handler called from real mode.
    FromReal,
    /// A client's handler called on the locked stack from protected mode.
    FromProtected,
    /// A real-mode callback's procedure.
    Callback,
    /// A client's exception handler.
    Exception,
}

#[derive(Clone, Copy)]
struct Callback {
    selector: u16,
    offset: u32,
    /// The registers structure, as a selector and offset.
    registers: (u16, u32),
}

struct Client {
    psp: u16,
    bits32: bool,
    block: (u16, u32),
    ldt_used: Vec<bool>,
    /// The handlers set with `AX=0205h`.
    vectors: Vec<Option<(u16, u32)>>,
    /// The handlers set with `AX=0203h`.
    exceptions: Vec<Option<(u16, u32)>>,
    /// The original real-mode handlers of the vectors stubs are in.
    hooked: Vec<(u8, u32)>,
    callbacks: [Option<Callback>; CALLBACK_SLOTS],
    frames: Vec<Frame>,
    /// Memory blocks: XMS handle, address and size in bytes.
    memory: Vec<(u16, u32, u32)>,
    rm_seg: u16,
    rm_sp: u16,
    lpms_sel: u16,
    lpms_sp: u32,
    /// A selector whose base follows the real-mode stack, for callbacks.
    rm_stack_sel: u16,
    environment: u16,
    environment_sel: u16,
    psp_sel: u16,
    /// The DTA `INT 21h AH=1Ah` set, as a selector and offset.
    dta: (u16, u32),
}

impl Client {
    fn selector(index: usize) -> u16 {
        ((index as u16) << 3) | 7
    }

    fn index(&self, selector: u16) -> Option<usize> {
        if selector & 7 != 7 {
            return None;
        }
        let index = (selector >> 3) as usize;
        self.ldt_used.get(index).copied()?.then_some(index)
    }

    fn free_entries(&self, count: usize) -> Option<usize> {
        let mut run = 0;
        for index in RESERVED_LDT..LDT_ENTRIES {
            if self.ldt_used[index] {
                run = 0;
            } else {
                run += 1;
                if run == count {
                    return Some(index + 1 - count);
                }
            }
        }
        None
    }

    fn frame_bytes(&self) -> u32 {
        if self.bits32 {
            4
        } else {
            2
        }
    }
}

/// When the host answers `INT 2Fh AX=1687h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Always.
    On,
    /// Never.
    Off,
    /// Except to programs that bring an extender of their own and use it when
    /// there's no host: see [`brings_its_own_extender`].
    Auto,
}

/// Whether a program's image shows it brings an extender that works without a
/// host. DOS/4G and DOS/4GW then run on the bare CPU, which is the path they were
/// tuned for. Borland's RTM needs its own server, `DPMI32VM.OVL`, or Windows.
pub fn brings_its_own_extender(image: &[u8]) -> bool {
    const SIGNS: [&[u8]; 2] = [b"DOS/4G", b"32rtm error"];
    SIGNS
        .iter()
        .any(|sign| image.windows(sign.len()).any(|part| part == *sign))
}

/// The host's state.
pub struct Dpmi {
    pub mode: Mode,
    block: Option<(u16, u32)>,
    clients: Vec<Client>,
}

impl Default for Dpmi {
    fn default() -> Self {
        Self::new()
    }
}

/// A descriptor with a base, a limit in bytes and the access byte.
fn descriptor(base: u32, limit: u32, access: u8, big: bool) -> (u32, u32) {
    let (raw, granular) = if limit > 0xF_FFFF {
        (limit >> 12, true)
    } else {
        (limit, false)
    };
    let lo = (raw & 0xFFFF) | ((base & 0xFFFF) << 16);
    let hi = ((base >> 16) & 0xFF)
        | ((access as u32) << 8)
        | (raw & 0xF_0000)
        | (u32::from(big) << 22)
        | (u32::from(granular) << 23)
        | (base & 0xFF00_0000);
    (lo, hi)
}

fn desc_base(lo: u32, hi: u32) -> u32 {
    (lo >> 16) | ((hi & 0xFF) << 16) | (hi & 0xFF00_0000)
}

impl Dpmi {
    pub fn new() -> Self {
        Self {
            mode: Mode::Auto,
            block: None,
            clients: Vec::new(),
        }
    }

    /// Whether the host has a client now.
    pub fn active(&self) -> bool {
        !self.clients.is_empty()
    }

    /// A program ended and the machine is going back to the prompt.
    pub fn reset(&mut self) {
        self.block = None;
        self.clients.clear();
    }

    /// The ROM's code.
    pub fn install_rom(mem: &mut crate::memory::Memory) {
        let base = (HOST_SEGMENT as u32) << 4;
        let call = |mem: &mut crate::memory::Memory, offset: u16, code: u8, after: u8| {
            mem.copy_from(base + offset as u32, &[0xFE, cpu::HOST_CALL_MODRM, code, after]);
        };
        call(mem, ENTRY, T_ENTRY, 0xCB);
        call(mem, RM_RETURN, T_RM_RETURN, 0xCB);
        call(mem, RAW_TO_PM, T_RAW_TO_PM, 0xCB);
        // Saving and restoring state takes no room, so these just return.
        mem.write8(base + SAVE_RM as u32, 0xCB);
        call(mem, VCPI_ENTRY, T_VCPI, 0xCB);
        // MOV AX,4CFFh; INT 21h.
        mem.copy_from(base + TERMINATE as u32, &[0xB8, 0xFF, 0x4C, 0xCD, 0x21]);
        call(mem, RET_HWINT, T_RET_HWINT, 0xCB);
        call(mem, RET_CALLBACK, T_RET_CALLBACK, 0xCB);
        call(mem, RET_EXCEPTION, T_RET_EXCEPTION, 0xCB);
        call(mem, RAW_TO_RM, T_RAW_TO_RM, 0xCB);
        call(mem, SAVE_PM, T_NOTHING, 0xCB);
        call(mem, MSDOS_API, T_MSDOS_API, 0xCB);
        for vector in 0..256u16 {
            // The real-mode stub runs IRET when the handler has finished.
            call(mem, RM_INT + vector * 4, T_RM_INT, 0xCF);
            call(mem, PM_IDT + vector * 4, T_IDT, 0xCF);
            call(mem, PM_DEFAULT_INT + vector * 4, T_DEFAULT_INT, 0xCF);
        }
        for slot in 0..CALLBACK_SLOTS as u16 {
            call(mem, CALLBACKS + slot * 4, T_CALLBACK, 0xCF);
        }
        for exception in 0..EXCEPTIONS as u16 {
            call(mem, PM_DEFAULT_EXC + exception * 4, T_DEFAULT_EXC, 0xCB);
        }
    }

    /// `INT 2Fh AX=1687h`, from a program that brings an extender or not.
    pub fn installation_check(&self, regs: &mut CpuRegs<'_>, self_hosting: bool) {
        if self.mode == Mode::Off || (self.mode == Mode::Auto && self_hosting) {
            return;
        }
        regs.set_ax(0);
        regs.set_bx(1);
        regs.set_cl(3);
        regs.set_dx(0x005A);
        regs.set_si(PRIVATE_PARAS);
        regs.set_es(HOST_SEGMENT);
        regs.set_di(ENTRY);
    }

}

fn read_context_flags_for_client(flags: u32) -> u32 {
    (flags & !FLAG_TF) | FLAG_IOPL
}

/// The offsets in the real-mode registers structure of `AX=0300h`.
mod rm {
    pub const EDI: u32 = 0x00;
    pub const ESI: u32 = 0x04;
    pub const EBP: u32 = 0x08;
    pub const EBX: u32 = 0x10;
    pub const EDX: u32 = 0x14;
    pub const ECX: u32 = 0x18;
    pub const EAX: u32 = 0x1C;
    pub const FLAGS: u32 = 0x20;
    pub const ES: u32 = 0x22;
    pub const DS: u32 = 0x24;
    pub const FS: u32 = 0x26;
    pub const GS: u32 = 0x28;
    pub const IP: u32 = 0x2A;
    pub const CS: u32 = 0x2C;
    pub const SP: u32 = 0x2E;
    pub const SS: u32 = 0x30;
}

/// `INT 31h`'s error codes.
mod error {
    pub const UNSUPPORTED: u16 = 0x8001;
    pub const BAD_STATE: u16 = 0x8002;
    pub const NO_DESCRIPTORS: u16 = 0x8011;
    pub const NO_LINEAR: u16 = 0x8012;
    pub const NO_MEMORY: u16 = 0x8013;
    pub const NO_HANDLES: u16 = 0x8016;
    pub const BAD_VALUE: u16 = 0x8021;
    pub const BAD_SELECTOR: u16 = 0x8022;
    pub const BAD_HANDLE: u16 = 0x8023;
    pub const NO_CALLBACK: u16 = 0x8015;
    pub const DOS_MEMORY: u16 = 0x0008;
}

impl SystemBus {
    /// A host call from the ROM.
    pub(crate) fn dpmi_host_call(&mut self, cpu: &mut Cpu, code: u8) -> InterruptAction {
        let offset = cpu.instr_start();
        let table = |base: u16| ((offset as u16).wrapping_sub(base) / 4) as u8;
        match code {
            T_ENTRY => self.dpmi_enter(cpu),
            T_RM_RETURN => self.dpmi_rm_return(cpu),
            T_RAW_TO_PM => self.dpmi_raw_to_pm(cpu),
            T_RAW_TO_RM => self.dpmi_raw_to_rm(cpu),
            T_NOTHING => self.dpmi_far_return(cpu),
            T_MSDOS_API => self.dpmi_msdos_api(cpu),
            T_VCPI => self.vcpi_entry(cpu),
            T_RM_INT => self.dpmi_rm_interrupt(cpu, table(RM_INT)),
            T_CALLBACK => self.dpmi_callback(cpu, table(CALLBACKS) as usize),
            T_IDT => self.dpmi_gate(cpu, table(PM_IDT)),
            T_DEFAULT_INT => self.dpmi_default_interrupt(cpu, table(PM_DEFAULT_INT)),
            T_DEFAULT_EXC => self.dpmi_default_exception(cpu, table(PM_DEFAULT_EXC)),
            T_RET_HWINT => self.dpmi_handler_return(cpu),
            T_RET_CALLBACK => self.dpmi_callback_return(cpu),
            T_RET_EXCEPTION => self.dpmi_exception_return(cpu),
            HOST_MONITOR_FAULT => self.monitor_fault(cpu),
            _ => {}
        }
        InterruptAction::Continue
    }

    /// Ends the running program with a message.
    pub(crate) fn stop_program(&mut self, text: &str) {
        self.vga.write_line(&mut self.mem, text.as_bytes());
        self.exit_requested = true;
        self.exit_code = 0xFF;
    }

    fn write_desc(&mut self, at: u32, (lo, hi): (u32, u32)) {
        self.mem.write32(at, lo);
        self.mem.write32(at + 4, hi);
    }

    fn start_host(&mut self) -> Option<u32> {
        if let Some((_, base)) = self.dpmi.block {
            return Some(base);
        }
        let (handle, base) = self.xms.alloc_block(HOST_KB)?;
        self.dpmi.block = Some((handle, base));
        let rom = (HOST_SEGMENT as u32) << 4;
        let entries = [
            (0, descriptor(0, 0, 0, false)),
            (HOST_CODE0, descriptor(rom, 0xFFFF, 0x9A, false)),
            (HOST_DATA0, descriptor(0, 0xFFFF_FFFF, 0x92, true)),
            (
                HOST_TSS,
                descriptor(base + HOST_TSS_AT, 0x67, 0x8B, false),
            ),
            (HOST_LDT, descriptor(0, 0, 0, false)),
            (HOST_CODE3 & !3, descriptor(rom, 0xFFFF, 0xFA, false)),
            (BIOS_DATA, descriptor(0x400, 0xFFFF, 0xF2, false)),
        ];
        for (selector, entry) in entries {
            self.write_desc(base + selector as u32, entry);
        }
        for vector in 0..256u32 {
            let offset = PM_IDT as u32 + vector * 4;
            // An interrupt gate that level 3 can reach, to level 0.
            self.mem
                .write32(base + HOST_IDT + vector * 8, ((HOST_CODE0 as u32) << 16) | offset);
            self.mem
                .write32(base + HOST_IDT + vector * 8 + 4, 0x0000_EE00);
        }
        let tss = base + HOST_TSS_AT;
        self.mem.write32(tss + 4, base + HOST_STACK_TOP);
        self.mem.write32(tss + 8, HOST_DATA0 as u32);
        self.mem.write16(tss + 102, 104);
        Some(base)
    }

    /// Points the CPU at the host's tables and the top client's LDT.
    fn set_tables(&mut self, cpu: &mut Cpu) {
        let Some((_, base)) = self.dpmi.block else {
            return;
        };
        cpu.gdtr = TableReg {
            base,
            limit: GDT_ENTRIES * 8 - 1,
        };
        cpu.idtr = TableReg {
            base: base + HOST_IDT,
            limit: 256 * 8 - 1,
        };
        cpu.tr = SysSeg {
            sel: HOST_TSS,
            base: base + HOST_TSS_AT,
            limit: 0x67,
            kind: 0xB,
        };
        if let Some(client) = self.dpmi.clients.last() {
            cpu.ldtr = SysSeg {
                sel: HOST_LDT,
                base: client.block.1,
                limit: LDT_SIZE - 1,
                kind: 2,
            };
        }
    }

    /// Goes to a protected-mode context at level 3, or ends the client if that
    /// context can't be loaded.
    fn resume(&mut self, cpu: &mut Cpu, context: &Context) {
        self.set_tables(cpu);
        let mut context = *context;
        context.flags = read_context_flags_for_client(context.flags) | (context.flags & FLAG_TF);
        if cpu.enter_protected_context(self, &context).is_err() {
            self.abort_client(cpu, "DPMI: a client's state can't be restored.");
        }
    }

    fn enter_rm(&mut self, cpu: &mut Cpu, context: &Context) {
        cpu.enter_real_context(context);
    }

    fn alloc_entry(&mut self, value: (u32, u32)) -> Option<u16> {
        let client = self.dpmi.clients.last_mut()?;
        let index = client.free_entries(1)?;
        client.ldt_used[index] = true;
        let at = client.block.1 + index as u32 * 8;
        self.write_desc(at, value);
        Some(Client::selector(index))
    }

    /// The far call to the entry point: the program becomes a client.
    fn dpmi_enter(&mut self, cpu: &mut Cpu) {
        let mut context = cpu.context();
        let frame = Cpu::physical(context.sregs[SS], context.r[SP] as u16);
        let return_ip = self.mem.read16(frame);
        let return_cs = self.mem.read16(frame + 2);
        context.r[SP] = (context.r[SP] & 0xFFFF_0000) | (context.r[SP] as u16).wrapping_add(4) as u32;
        let fail = |bus: &mut Self, cpu: &mut Cpu, mut context: Context| {
            context.sregs[CS] = return_cs;
            context.ip = return_ip as u32;
            context.flags |= FLAG_CF;
            context.r[AX] = (context.r[AX] & 0xFFFF_0000) | error::NO_DESCRIPTORS as u32;
            bus.enter_rm(cpu, &context);
        };
        let bits32 = context.r[AX] & 1 != 0;
        // A client uses the high addresses from now on, and so do the tables.
        self.mem.set_a20(true);
        if self.start_host().is_none() {
            return fail(self, cpu, context);
        }
        let Some((handle, base)) = self.xms.alloc_block(CLIENT_KB) else {
            return fail(self, cpu, context);
        };
        for offset in 0..LDT_SIZE / 4 {
            self.mem.write32(base + offset * 4, 0);
        }
        let psp = self.dos.psp;
        let private_seg = context.sregs[ES];
        let mut ldt_used = vec![false; LDT_ENTRIES];
        ldt_used[0] = true;
        self.dpmi.clients.push(Client {
            psp,
            bits32,
            block: (handle, base),
            ldt_used,
            vectors: vec![None; 256],
            exceptions: vec![None; EXCEPTIONS],
            hooked: Vec::new(),
            callbacks: [None; CALLBACK_SLOTS],
            frames: Vec::new(),
            memory: Vec::new(),
            rm_seg: private_seg,
            rm_sp: PRIVATE_PARAS * 16 - 2,
            lpms_sel: 0,
            lpms_sp: LPMS_SIZE,
            rm_stack_sel: 0,
            environment: 0,
            environment_sel: 0,
            psp_sel: 0,
            dta: (0, 0x80),
        });
        let data = |seg: u16| descriptor((seg as u32) << 4, 0xFFFF, 0xF2, false);
        let code_sel = self.alloc_entry(descriptor((return_cs as u32) << 4, 0xFFFF, 0xFA, false));
        let data_sel = self.alloc_entry(data(context.sregs[DS]));
        let stack_sel = if context.sregs[SS] == context.sregs[DS] {
            data_sel
        } else {
            self.alloc_entry(data(context.sregs[SS]))
        };
        let psp_sel = self.alloc_entry(descriptor((psp as u32) << 4, 0xFF, 0xF2, false));
        let lpms = self.alloc_entry(descriptor(base + LDT_SIZE, LPMS_SIZE - 1, 0xF2, bits32));
        let rm_stack = self.alloc_entry(data(0));
        let environment = self.mem.read16(((psp as u32) << 4) + 0x2C);
        let env_sel = self.alloc_entry(data(environment));
        let (
            Some(code_sel),
            Some(data_sel),
            Some(stack_sel),
            Some(psp_sel),
            Some(lpms),
            Some(rm_stack),
            Some(env_sel),
        ) = (code_sel, data_sel, stack_sel, psp_sel, lpms, rm_stack, env_sel)
        else {
            self.dpmi.clients.pop();
            return fail(self, cpu, context);
        };
        self.mem.write16(((psp as u32) << 4) + 0x2C, env_sel);
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.lpms_sel = lpms;
            client.rm_stack_sel = rm_stack;
            client.environment = environment;
            client.environment_sel = env_sel;
            client.psp_sel = psp_sel;
            client.dta = (psp_sel, 0x80);
        }
        context.sregs[CS] = code_sel;
        context.ip = return_ip as u32;
        context.sregs[DS] = data_sel;
        context.sregs[ES] = psp_sel;
        context.sregs[SS] = stack_sel;
        context.sregs[FS] = 0;
        context.sregs[GS] = 0;
        context.r[SP] &= 0xFFFF;
        context.flags &= !FLAG_CF;
        self.resume(cpu, &context);
    }

    /// The registers at the moment a gate was taken, from the level 0 stack.
    fn gate_context(&mut self, cpu: &Cpu) -> Option<(Context, Option<u32>)> {
        let (_, base) = self.dpmi.block?;
        let sp = cpu.r[SP];
        let top = base + HOST_STACK_TOP;
        let (code, at) = match top.wrapping_sub(sp) {
            20 => (None, sp),
            24 => (Some(self.mem.read32(sp)), sp + 4),
            _ => return None,
        };
        let mut context = cpu.context();
        context.ip = self.mem.read32(at);
        context.sregs[CS] = self.mem.read16(at + 4);
        context.flags = self.mem.read32(at + 8);
        context.r[SP] = self.mem.read32(at + 12);
        context.sregs[SS] = self.mem.read16(at + 16);
        Some((context, code))
    }

    /// An interrupt or exception came through the IDT.
    fn dpmi_gate(&mut self, cpu: &mut Cpu, vector: u8) {
        let source = cpu.int_source;
        let Some((context, code)) = self.gate_context(cpu) else {
            self.abort_client(cpu, "DPMI: an interrupt came in an unknown state.");
            return;
        };
        if source == IntSource::Exception && (vector as usize) < EXCEPTIONS {
            if vector == 13 {
                let mut context = context;
                if self.emulate_privileged(cpu, &mut context) {
                    return self.resume(cpu, &context);
                }
            }
            let handler = self
                .dpmi
                .clients
                .last()
                .and_then(|client| client.exceptions[vector as usize]);
            match handler {
                Some(handler) => self.call_exception_handler(cpu, vector, code.unwrap_or(0), context, handler),
                None => self.default_exception(cpu, vector, code.unwrap_or(0), context),
            }
            return;
        }
        self.interrupt(cpu, vector, context, source == IntSource::Hardware);
    }

    /// What a client at level 3 can't do itself but expects to work: reading the
    /// control and debug registers, and instructions with nothing to do here.
    fn emulate_privileged(&mut self, cpu: &mut Cpu, context: &mut Context) -> bool {
        let Some((base, _)) = self.selector_info(context.sregs[CS]) else {
            return false;
        };
        let at = base.wrapping_add(context.ip);
        let first = self.mem.read8(at);
        if first == 0xF4 {
            context.ip = context.ip.wrapping_add(1);
            return true;
        }
        if first != 0x0F {
            return false;
        }
        let second = self.mem.read8(at.wrapping_add(1));
        let modrm = self.mem.read8(at.wrapping_add(2));
        let (reg, rm) = (((modrm >> 3) & 7) as usize, (modrm & 7) as usize);
        let length = match second {
            0x20 => {
                let value = match reg {
                    0 => cpu.cr0,
                    2 => cpu.cr2,
                    3 => cpu.cr3,
                    4 => cpu.cr4,
                    _ => return false,
                };
                if rm == SP {
                    return false;
                }
                context.r[rm] = value;
                3
            }
            0x21 => {
                if rm == SP {
                    return false;
                }
                context.r[rm] = cpu.dr[reg];
                3
            }
            // Writes to the registers change nothing a host relies on.
            0x22 => 3,
            0x23 => {
                cpu.dr[reg] = context.r[rm];
                3
            }
            0x06 | 0x08 | 0x09 => 2,
            _ => return false,
        };
        context.ip = context.ip.wrapping_add(length);
        true
    }

    /// An interrupt in protected mode: to the client's handler, or down.
    fn interrupt(&mut self, cpu: &mut Cpu, vector: u8, context: Context, hardware: bool) {
        let handler = self
            .dpmi
            .clients
            .last()
            .and_then(|client| client.vectors[vector as usize]);
        if let Some(handler) = handler {
            if hardware {
                self.call_on_locked_stack(cpu, context, handler);
            } else {
                self.call_on_client_stack(cpu, context, handler);
            }
            return;
        }
        self.interrupt_default(cpu, vector, context, hardware);
    }

    /// What happens to an interrupt no handler takes, or a handler passes on.
    fn interrupt_default(&mut self, cpu: &mut Cpu, vector: u8, context: Context, hardware: bool) {
        if !hardware {
            match vector {
                0x31 => return self.int31(cpu, context),
                0x21 => return self.int21_translate(cpu, context),
                // In protected mode now.
                0x2F if context.r[AX] as u16 == 0x1686 => {
                    let mut context = context;
                    context.r[AX] &= 0xFFFF_0000;
                    return self.resume(cpu, &context);
                }
                // The "MS-DOS" extensions, which Borland's RTM looks for.
                0x2F if context.r[AX] as u16 == 0x168A => {
                    let mut context = context;
                    let esi = self.client_offset(context.r[SI]);
                    let name = self.selector_info(context.sregs[DS]).map(|(base, _)| {
                        (0..7u32).map(|i| self.mem.read8(base.wrapping_add(esi).wrapping_add(i)))
                            .eq(b"MS-DOS\0".iter().copied())
                    });
                    if name == Some(true) {
                        context.r[AX] &= 0xFFFF_FF00;
                        context.sregs[ES] = HOST_CODE3;
                        context.r[DI] = MSDOS_API as u32;
                    }
                    return self.resume(cpu, &context);
                }
                _ => {}
            }
        }
        self.reflect(cpu, vector, context, hardware);
    }

    /// The base of a selector, and whether its stack is 32-bit.
    fn selector_info(&self, selector: u16) -> Option<(u32, bool)> {
        let at = if selector & 4 != 0 {
            self.dpmi.clients.last()?.block.1 + (selector & !7) as u32
        } else {
            self.dpmi.block?.1 + (selector & !7) as u32
        };
        let (lo, hi) = (self.mem.read32(at), self.mem.read32(at + 4));
        Some((desc_base(lo, hi), hi & 0x40_0000 != 0))
    }

    /// Pushes the given words on a client's stack, the first pushed first.
    fn push_client(&mut self, context: &mut Context, words: &[u32]) -> bool {
        let Some(bits32) = self.dpmi.clients.last().map(|client| client.bits32) else {
            return false;
        };
        let Some((base, big)) = self.selector_info(context.sregs[SS]) else {
            return false;
        };
        let size = if bits32 { 4 } else { 2 };
        let mut sp = if big {
            context.r[SP]
        } else {
            context.r[SP] & 0xFFFF
        };
        for &word in words {
            sp = sp.wrapping_sub(size);
            if !big {
                sp &= 0xFFFF;
            }
            let at = base.wrapping_add(sp);
            if bits32 {
                self.mem.write32(at, word);
            } else {
                self.mem.write16(at, word as u16);
            }
        }
        context.r[SP] = if big {
            sp
        } else {
            (context.r[SP] & 0xFFFF_0000) | sp
        };
        true
    }

    /// A software interrupt's handler: an `IRET` frame on the client's own stack.
    fn call_on_client_stack(&mut self, cpu: &mut Cpu, mut context: Context, handler: (u16, u32)) {
        let frame = [
            context.flags & 0xFFFF_FFFF,
            context.sregs[CS] as u32,
            context.ip,
        ];
        if !self.push_client(&mut context, &frame) {
            self.abort_client(cpu, "DPMI: a client's stack can't be used.");
            return;
        }
        context.sregs[CS] = handler.0;
        context.ip = handler.1;
        context.flags &= !(FLAG_IF | FLAG_TF);
        self.resume(cpu, &context);
    }

    /// A context on the locked stack, ready to have frames pushed.
    fn locked_context(&self, from: &Context) -> Option<Context> {
        let client = self.dpmi.clients.last()?;
        let mut context = *from;
        if context.sregs[SS] != client.lpms_sel {
            context.sregs[SS] = client.lpms_sel;
            // With no handler running, nothing's on the stack, however a handler
            // got out of it. With one running, it can be on a stack of its own with
            // interrupts on, and what it left behind must stay intact.
            context.r[SP] = if client.frames.iter().any(|frame| {
                matches!(
                    frame.kind,
                    FrameKind::FromProtected | FrameKind::FromReal | FrameKind::Callback
                )
            }) {
                client.lpms_sp.saturating_sub(LPMS_GAP).max(0x200)
            } else {
                LPMS_SIZE
            };
        }
        Some(context)
    }

    fn push_frame(&mut self, cpu: &mut Cpu, kind: FrameKind, context: Context, registers: u32) -> bool {
        let Some(client) = self.dpmi.clients.last_mut() else {
            return false;
        };
        // An exception handler can jump out instead of returning, and then what's
        // on the stack is forgotten, so the oldest of those give way.
        if client.frames.len() >= MAX_FRAMES {
            match client.frames.iter().position(|f| f.kind == FrameKind::Exception) {
                Some(index) => {
                    client.frames.remove(index);
                }
                None => {
                    self.abort_client(cpu, "DPMI: the client's mode switches are nested too deep.");
                    return false;
                }
            }
        }
        client.frames.push(Frame {
            kind,
            context,
            rm_sp: client.rm_sp,
            lpms_sp: client.lpms_sp,
            registers,
        });
        true
    }

    /// A hardware interrupt's handler, with protected mode already running.
    fn call_on_locked_stack(&mut self, cpu: &mut Cpu, context: Context, handler: (u16, u32)) {
        if !self.push_frame(cpu, FrameKind::FromProtected, context, 0) {
            return;
        }
        let Some(mut locked) = self.locked_context(&context) else {
            return;
        };
        let frame = [context.flags, HOST_CODE3 as u32, RET_HWINT as u32];
        if !self.push_client(&mut locked, &frame) {
            self.abort_client(cpu, "DPMI: the locked stack can't be used.");
            return;
        }
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.lpms_sp = locked.r[SP];
        }
        locked.sregs[CS] = handler.0;
        locked.ip = handler.1;
        locked.flags &= !(FLAG_IF | FLAG_TF);
        self.resume(cpu, &locked);
    }

    /// A handler returned, to the stub that was on its stack.
    fn dpmi_handler_return(&mut self, cpu: &mut Cpu) {
        let Some(frame) = self.pop_frame(&[FrameKind::FromReal, FrameKind::FromProtected]) else {
            self.abort_client(cpu, "DPMI: a handler returned with no call open.");
            return;
        };
        if frame.kind == FrameKind::FromReal {
            let context = frame.context;
            // Its instruction pointer is past the host call, on the stub's IRET.
            self.enter_rm(cpu, &context);
        } else {
            self.resume(cpu, &frame.context);
        }
    }

    /// Takes the top frame if it's of the given kinds, and restores its stacks.
    fn pop_frame(&mut self, kinds: &[FrameKind]) -> Option<Frame> {
        let client = self.dpmi.clients.last_mut()?;
        let frame = *client.frames.last()?;
        if !kinds.contains(&frame.kind) {
            return None;
        }
        client.frames.pop();
        client.rm_sp = frame.rm_sp;
        client.lpms_sp = frame.lpms_sp;
        Some(frame)
    }

    /// The real-mode handler behind a vector, past any stub of the host's.
    fn real_handler(&self, vector: u8) -> u32 {
        for client in self.dpmi.clients.iter().rev() {
            if let Some((_, original)) = client.hooked.iter().find(|(v, _)| *v == vector) {
                return *original;
            }
        }
        self.mem.read32(vector as u32 * 4)
    }

    /// Calls real-mode code on the host's stack, with an `IRET` frame back to the
    /// host. `words` go on the stack first, for a call with parameters.
    fn real_call_frame(&mut self, rm: &mut Context, iret: bool) {
        let mut sp = rm.r[SP] as u16;
        let mut push = |mem: &mut crate::memory::Memory, value: u16| {
            sp = sp.wrapping_sub(2);
            mem.write16(Cpu::physical(rm.sregs[SS], sp), value);
        };
        if iret {
            push(&mut self.mem, rm.flags as u16);
        }
        push(&mut self.mem, HOST_SEGMENT);
        push(&mut self.mem, RM_RETURN);
        rm.r[SP] = sp as u32;
    }

    /// Takes an interrupt down to real mode.
    fn reflect(&mut self, cpu: &mut Cpu, vector: u8, context: Context, hardware: bool) {
        let Some(client) = self.dpmi.clients.last() else {
            return;
        };
        let (rm_seg, rm_sp, psp, environment) =
            (client.rm_seg, client.rm_sp, client.psp, client.environment);
        // `EXEC` passes the environment to the child by segment.
        if vector == 0x21 && !hardware && (context.r[AX] >> 8) as u8 == 0x4B {
            self.mem.write16(((psp as u32) << 4) + 0x2C, environment);
        }
        let kind = if hardware {
            FrameKind::Hardware
        } else {
            FrameKind::Software
        };
        if !self.push_frame(cpu, kind, context, 0) {
            return;
        }
        let target = self.real_handler(vector);
        let mut rm = context;
        rm.sregs = [rm_seg, (target >> 16) as u16, rm_seg, rm_seg, 0, 0];
        rm.ip = target & 0xFFFF;
        rm.r[SP] = rm_sp as u32;
        rm.flags = context.flags & !(FLAG_IOPL | FLAG_TF);
        self.real_call_frame(&mut rm, true);
        rm.flags &= !FLAG_IF;
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.rm_sp = rm.r[SP] as u16;
        }
        self.enter_rm(cpu, &rm);
    }

    /// Real-mode code the host called has returned.
    fn dpmi_rm_return(&mut self, cpu: &mut Cpu) {
        let Some(frame) = self.pop_frame(&[
            FrameKind::Software,
            FrameKind::Hardware,
            FrameKind::RealCall,
        ]) else {
            self.abort_client(cpu, "DPMI: real-mode code returned with no call open.");
            return;
        };
        let rm = cpu.context();
        let mut context = frame.context;
        match frame.kind {
            FrameKind::Hardware => {}
            FrameKind::Software => {
                if let Some(client) = self.dpmi.clients.last() {
                    let (psp, selector) = (client.psp, client.environment_sel);
                    self.mem.write16(((psp as u32) << 4) + 0x2C, selector);
                }
                for index in [AX, CX, DX, BX, BP, SI, DI] {
                    context.r[index] = rm.r[index];
                }
                context.flags = (context.flags & !0x0CD5) | (rm.flags & 0x0CD5);
            }
            _ => {
                self.write_rm_registers(frame.registers, &rm);
                context.flags &= !FLAG_CF;
            }
        }
        self.resume(cpu, &context);
    }

    fn write_rm_registers(&mut self, at: u32, rm: &Context) {
        let mem = &mut self.mem;
        mem.write32(at.wrapping_add(rm::EDI), rm.r[DI]);
        mem.write32(at.wrapping_add(rm::ESI), rm.r[SI]);
        mem.write32(at.wrapping_add(rm::EBP), rm.r[BP]);
        mem.write32(at.wrapping_add(rm::EBX), rm.r[BX]);
        mem.write32(at.wrapping_add(rm::EDX), rm.r[DX]);
        mem.write32(at.wrapping_add(rm::ECX), rm.r[CX]);
        mem.write32(at.wrapping_add(rm::EAX), rm.r[AX]);
        mem.write16(at.wrapping_add(rm::FLAGS), rm.flags as u16);
        mem.write16(at.wrapping_add(rm::ES), rm.sregs[ES]);
        mem.write16(at.wrapping_add(rm::DS), rm.sregs[DS]);
        mem.write16(at.wrapping_add(rm::FS), rm.sregs[FS]);
        mem.write16(at.wrapping_add(rm::GS), rm.sregs[GS]);
        mem.write16(at.wrapping_add(rm::IP), rm.ip as u16);
        mem.write16(at.wrapping_add(rm::CS), rm.sregs[CS]);
        mem.write16(at.wrapping_add(rm::SP), rm.r[SP] as u16);
        mem.write16(at.wrapping_add(rm::SS), rm.sregs[SS]);
    }

    fn read_rm_registers(&self, at: u32, base: &Context) -> Context {
        let mem = &self.mem;
        let mut rm = *base;
        rm.r[DI] = mem.read32(at.wrapping_add(rm::EDI));
        rm.r[SI] = mem.read32(at.wrapping_add(rm::ESI));
        rm.r[BP] = mem.read32(at.wrapping_add(rm::EBP));
        rm.r[BX] = mem.read32(at.wrapping_add(rm::EBX));
        rm.r[DX] = mem.read32(at.wrapping_add(rm::EDX));
        rm.r[CX] = mem.read32(at.wrapping_add(rm::ECX));
        rm.r[AX] = mem.read32(at.wrapping_add(rm::EAX));
        rm.flags = mem.read16(at.wrapping_add(rm::FLAGS)) as u32;
        rm.sregs[ES] = mem.read16(at.wrapping_add(rm::ES));
        rm.sregs[DS] = mem.read16(at.wrapping_add(rm::DS));
        rm.sregs[FS] = mem.read16(at.wrapping_add(rm::FS));
        rm.sregs[GS] = mem.read16(at.wrapping_add(rm::GS));
        rm.ip = mem.read16(at.wrapping_add(rm::IP)) as u32;
        rm.sregs[CS] = mem.read16(at.wrapping_add(rm::CS));
        rm.r[SP] = mem.read16(at.wrapping_add(rm::SP)) as u32;
        rm.sregs[SS] = mem.read16(at.wrapping_add(rm::SS));
        rm
    }

    /// A vector whose real-mode stub the host owns was called in real mode.
    fn dpmi_rm_interrupt(&mut self, cpu: &mut Cpu, vector: u8) {
        let rm = cpu.context();
        let handler = self
            .dpmi
            .clients
            .last()
            .and_then(|client| client.vectors[vector as usize]);
        let Some(handler) = handler else {
            // Nobody in protected mode wants it now: on to the real handler, with
            // the frame the INT made still on the stack.
            let target = self.real_handler(vector);
            let mut rm = rm;
            rm.sregs[CS] = (target >> 16) as u16;
            rm.ip = target & 0xFFFF;
            self.enter_rm(cpu, &rm);
            return;
        };
        if !self.push_frame(cpu, FrameKind::FromReal, rm, 0) {
            return;
        }
        let Some(client) = self.dpmi.clients.last() else {
            return;
        };
        let (rm_seg, code_ds) = (client.rm_seg, client.lpms_sel);
        if rm.sregs[SS] == rm_seg {
            if let Some(client) = self.dpmi.clients.last_mut() {
                client.rm_sp = rm.r[SP] as u16;
            }
        }
        let mut pm = rm;
        pm.sregs = [0, HOST_CODE3, code_ds, code_ds, 0, 0];
        pm.sregs[SS] = code_ds;
        let lpms = self.dpmi.clients.last().map_or(LPMS_SIZE, |c| c.lpms_sp);
        pm.r[SP] = lpms;
        pm.flags = rm.flags & !(FLAG_IF | FLAG_TF);
        let frame = [pm.flags, HOST_CODE3 as u32, RET_HWINT as u32];
        if !self.push_client(&mut pm, &frame) {
            self.abort_client(cpu, "DPMI: the locked stack can't be used.");
            return;
        }
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.lpms_sp = pm.r[SP];
        }
        pm.sregs[CS] = handler.0;
        pm.ip = handler.1;
        self.resume(cpu, &pm);
    }

    /// A real-mode callback was called.
    fn dpmi_callback(&mut self, cpu: &mut Cpu, slot: usize) {
        let rm = cpu.context();
        let Some(client) = self.dpmi.clients.last() else {
            return;
        };
        let Some(callback) = client.callbacks.get(slot).copied().flatten() else {
            return;
        };
        let (rm_seg, lpms_sel, rm_stack_sel, lpms_sp) =
            (client.rm_seg, client.lpms_sel, client.rm_stack_sel, client.lpms_sp);
        let Some((struct_base, _)) = self.selector_info(callback.registers.0) else {
            return;
        };
        let registers = struct_base.wrapping_add(callback.registers.1);
        self.write_rm_registers(registers, &rm);
        if !self.push_frame(cpu, FrameKind::Callback, rm, registers) {
            return;
        }
        if rm.sregs[SS] == rm_seg {
            if let Some(client) = self.dpmi.clients.last_mut() {
                client.rm_sp = rm.r[SP] as u16;
            }
        }
        // DS:(E)SI is the real-mode stack, as the procedure expects.
        if let Some(index) = self.dpmi.clients.last().and_then(|c| c.index(rm_stack_sel)) {
            let at = self.dpmi.clients.last().map_or(0, |c| c.block.1) + index as u32 * 8;
            self.write_desc(at, descriptor((rm.sregs[SS] as u32) << 4, 0xFFFF, 0xF2, false));
        }
        let mut pm = rm;
        pm.sregs = [callback.registers.0, callback.selector, lpms_sel, rm_stack_sel, 0, 0];
        pm.r[SI] = rm.r[SP] & 0xFFFF;
        pm.r[DI] = callback.registers.1;
        pm.r[SP] = lpms_sp;
        pm.flags = rm.flags & !(FLAG_IF | FLAG_TF);
        let frame = [pm.flags, HOST_CODE3 as u32, RET_CALLBACK as u32];
        if !self.push_client(&mut pm, &frame) {
            self.abort_client(cpu, "DPMI: the locked stack can't be used.");
            return;
        }
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.lpms_sp = pm.r[SP];
        }
        pm.ip = callback.offset;
        self.resume(cpu, &pm);
    }

    /// A callback's procedure returned: its ES:(E)DI says how real mode goes on.
    fn dpmi_callback_return(&mut self, cpu: &mut Cpu) {
        let Some(frame) = self.pop_frame(&[FrameKind::Callback]) else {
            self.abort_client(cpu, "DPMI: a callback returned with no call open.");
            return;
        };
        let at = cpu.seg[ES].base.wrapping_add(cpu.r[DI]);
        let rm = self.read_rm_registers(at, &frame.context);
        self.enter_rm(cpu, &rm);
    }

    fn call_exception_handler(
        &mut self,
        cpu: &mut Cpu,
        _vector: u8,
        code: u32,
        context: Context,
        handler: (u16, u32),
    ) {
        if !self.push_frame(cpu, FrameKind::Exception, context, 0) {
            return;
        }
        let Some(mut locked) = self.locked_context(&context) else {
            return;
        };
        // The return to the host, then the error code and the state the handler
        // can change.
        let frame = [
            context.sregs[SS] as u32,
            context.r[SP],
            context.flags,
            context.sregs[CS] as u32,
            context.ip,
            code,
            HOST_CODE3 as u32,
            RET_EXCEPTION as u32,
        ];
        if !self.push_client(&mut locked, &frame) {
            self.abort_client(cpu, "DPMI: the locked stack can't be used.");
            return;
        }
        if let Some(client) = self.dpmi.clients.last_mut() {
            client.lpms_sp = locked.r[SP];
        }
        locked.sregs[CS] = handler.0;
        locked.ip = handler.1;
        locked.flags &= !(FLAG_IF | FLAG_TF);
        self.resume(cpu, &locked);
    }

    /// The handler returned with `RETF`, and what it left on the stack says where
    /// to go next, with the registers as it left them.
    fn dpmi_exception_return(&mut self, cpu: &mut Cpu) {
        let Some(frame) = self.pop_frame(&[FrameKind::Exception]) else {
            self.abort_client(cpu, "DPMI: an exception handler returned with no call open.");
            return;
        };
        let bytes = self.dpmi.clients.last().map_or(4, Client::frame_bytes);
        let mut context = cpu.context();
        let at = cpu.seg[SS].base.wrapping_add(cpu.r[SP]);
        let word = |bus: &Self, index: u32| -> u32 {
            let place = at.wrapping_add(index * bytes);
            if bytes == 4 {
                bus.mem.read32(place)
            } else {
                bus.mem.read16(place) as u32
            }
        };
        // Past the error code: EIP, CS, EFLAGS, ESP, SS.
        context.ip = word(self, 1);
        context.sregs[CS] = word(self, 2) as u16;
        let flags = word(self, 3);
        context.flags = (frame.context.flags & !0xFFFF) | (flags & 0xFFFF);
        context.r[SP] = word(self, 4);
        context.sregs[SS] = word(self, 5) as u16;
        self.resume(cpu, &context);
    }

    /// `AX=0204h`'s handler: chained to, with an `IRET` frame on the stack.
    fn dpmi_default_interrupt(&mut self, cpu: &mut Cpu, vector: u8) {
        let mut context = cpu.context();
        let bytes = self.dpmi.clients.last().map_or(4, Client::frame_bytes);
        let at = cpu.seg[SS].base.wrapping_add(cpu.sp_off());
        let word = |bus: &Self, index: u32| -> u32 {
            let place = at.wrapping_add(index * bytes);
            if bytes == 4 {
                bus.mem.read32(place)
            } else {
                bus.mem.read16(place) as u32
            }
        };
        context.ip = word(self, 0);
        context.sregs[CS] = word(self, 1) as u16;
        context.flags = word(self, 2);
        context.r[SP] = context.r[SP].wrapping_add(3 * bytes);
        let hardware = matches!(vector, 0x08..=0x0F | 0x1C | 0x70..=0x77);
        self.interrupt_default(cpu, vector, context, hardware);
    }

    /// `AX=0202h`'s handler: chained to, with an exception frame on the stack.
    fn dpmi_default_exception(&mut self, cpu: &mut Cpu, vector: u8) {
        let bytes = self.dpmi.clients.last().map_or(4, Client::frame_bytes);
        let at = cpu.seg[SS].base.wrapping_add(cpu.sp_off());
        let word = |bus: &Self, index: u32| -> u32 {
            let place = at.wrapping_add(index * bytes);
            if bytes == 4 {
                bus.mem.read32(place)
            } else {
                bus.mem.read16(place) as u32
            }
        };
        let mut context = cpu.context();
        let code = word(self, 2);
        context.ip = word(self, 3);
        context.sregs[CS] = word(self, 4) as u16;
        context.flags = word(self, 5);
        context.r[SP] = word(self, 6);
        context.sregs[SS] = word(self, 7) as u16;
        self.default_exception(cpu, vector, code, context);
    }

    fn default_exception(&mut self, cpu: &mut Cpu, vector: u8, code: u32, context: Context) {
        // The first few go down as interrupts, as DOS expects them to.
        if matches!(vector, 0..=5 | 7) {
            self.interrupt(cpu, vector, context, false);
            return;
        }
        let text: String = format!(
            "DPMI: exception {vector:02X}h, error {code:04X}, at {:04X}:{:08X}.",
            context.sregs[CS], context.ip
        );
        self.abort_client(cpu, &text);
    }

    /// Ends the client's program, from real mode.
    pub(crate) fn abort_client(&mut self, cpu: &mut Cpu, text: &str) {
        self.vga.write_line(&mut self.mem, text.as_bytes());
        let Some(client) = self.dpmi.clients.last_mut() else {
            self.exit_requested = true;
            self.exit_code = 0xFF;
            return;
        };
        client.frames.clear();
        let (rm_seg, rm_sp) = (client.rm_seg, client.rm_sp);
        let mut rm = cpu.context();
        rm.sregs = [rm_seg, HOST_SEGMENT, rm_seg, rm_seg, 0, 0];
        rm.ip = TERMINATE as u32;
        rm.r[SP] = rm_sp as u32;
        rm.flags &= !(FLAG_IOPL | FLAG_TF);
        self.enter_rm(cpu, &rm);
    }

    /// DOS has ended processes: their clients go, with what they held.
    pub(crate) fn dpmi_check_clients(&mut self) {
        while let Some(client) = self.dpmi.clients.last() {
            if self.dos.process_alive(client.psp) {
                return;
            }
            let Some(client) = self.dpmi.clients.pop() else {
                return;
            };
            for (vector, original) in client.hooked.iter().rev() {
                self.mem.write32(*vector as u32 * 4, *original);
            }
            for (handle, _, _) in &client.memory {
                self.xms.free_block(*handle);
            }
            self.xms.free_block(client.block.0);
        }
        if let Some((handle, _)) = self.dpmi.block.take() {
            self.xms.free_block(handle);
        }
    }

    /// Saving and restoring state, which takes no room: a far return at the size
    /// the client calls with.
    fn dpmi_far_return(&mut self, cpu: &mut Cpu) {
        let bytes = self.dpmi.clients.last().map_or(4, Client::frame_bytes);
        let mut context = cpu.context();
        let at = cpu.seg[SS].base.wrapping_add(cpu.sp_off());
        if bytes == 4 {
            context.ip = self.mem.read32(at);
            context.sregs[CS] = self.mem.read16(at.wrapping_add(4));
        } else {
            context.ip = self.mem.read16(at) as u32;
            context.sregs[CS] = self.mem.read16(at.wrapping_add(2));
        }
        context.r[SP] = context.r[SP].wrapping_add(bytes * 2);
        self.resume(cpu, &context);
    }

    /// The "MS-DOS" extensions' entry: `AX=0100h` gives a selector for the LDT,
    /// with which the client changes descriptors itself, and there's nothing else.
    fn dpmi_msdos_api(&mut self, cpu: &mut Cpu) {
        let mut alias = None;
        if cpu.r[AX] as u16 == 0x0100 {
            let base = self.dpmi.clients.last().map_or(0, |client| client.block.1);
            alias = self.alloc_entry(descriptor(base, LDT_SIZE - 1, 0xF2, false));
        }
        let mut flags = cpu.flags();
        match alias {
            Some(selector) => {
                cpu.r[AX] = (cpu.r[AX] & 0xFFFF_0000) | selector as u32;
                flags &= !FLAG_CF;
            }
            None => flags |= FLAG_CF,
        }
        cpu.set_flags(flags);
        self.dpmi_far_return(cpu);
    }

    fn dpmi_raw_to_pm(&mut self, cpu: &mut Cpu) {
        let mut context = cpu.context();
        let regs = context.r;
        context.sregs[DS] = regs[AX] as u16;
        context.sregs[ES] = regs[CX] as u16;
        context.sregs[SS] = regs[DX] as u16;
        context.r[SP] = regs[BX];
        context.sregs[CS] = regs[SI] as u16;
        context.ip = regs[DI];
        context.sregs[FS] = 0;
        context.sregs[GS] = 0;
        self.resume(cpu, &context);
    }

    fn dpmi_raw_to_rm(&mut self, cpu: &mut Cpu) {
        let mut context = cpu.context();
        let regs = context.r;
        context.sregs[DS] = regs[AX] as u16;
        context.sregs[ES] = regs[CX] as u16;
        context.sregs[SS] = regs[DX] as u16;
        context.r[SP] = regs[BX] & 0xFFFF;
        context.sregs[CS] = regs[SI] as u16;
        context.ip = regs[DI] & 0xFFFF;
        context.sregs[FS] = 0;
        context.sregs[GS] = 0;
        context.flags &= !FLAG_IOPL;
        self.enter_rm(cpu, &context);
    }

    /// The offset register at the client's width.
    fn client_offset(&self, value: u32) -> u32 {
        if self.dpmi.clients.last().is_some_and(|client| client.bits32) {
            value
        } else {
            value & 0xFFFF
        }
    }

    /// The conventional address of the top client's transfer buffer.
    fn transfer_base(&self) -> u32 {
        self.dpmi
            .clients
            .last()
            .map_or(0, |client| (client.rm_seg as u32) << 4)
    }

    /// Copies a name to the transfer buffer.
    fn copy_name(&mut self, selector: u16, offset: u32, at: u32) -> bool {
        let Some((base, _)) = self.selector_info(selector) else {
            return false;
        };
        let target = self.transfer_base() + at;
        for index in 0..0x100u32 {
            let byte = self.mem.read8(base.wrapping_add(offset).wrapping_add(index));
            self.mem.write8(target + index, byte);
            if byte == 0 {
                break;
            }
        }
        true
    }

    /// Copies between a client's memory and the transfer buffer.
    fn copy_transfer(&mut self, selector: u16, offset: u32, at: u32, len: u32, to_client: bool) -> bool {
        let Some((base, _)) = self.selector_info(selector) else {
            return false;
        };
        let target = self.transfer_base() + at;
        for index in 0..len {
            let there = base.wrapping_add(offset).wrapping_add(index);
            if to_client {
                let byte = self.mem.read8(target + index);
                self.mem.write8(there, byte);
            } else {
                let byte = self.mem.read8(there);
                self.mem.write8(target + index, byte);
            }
        }
        true
    }

    /// Runs DOS on the transfer buffer: DS and ES are its segment.
    fn dos_native(&mut self, context: &mut Context) {
        let segment = self.dpmi.clients.last().map_or(0, |client| client.rm_seg);
        let mut r = context.r;
        let mut sregs = [segment, 0xF000, segment, segment, 0, 0];
        let mut flags = (context.flags & !FLAG_CF) | 2;
        let SystemBus {
            dos,
            mem,
            vga,
            now_ms,
            exit_requested,
            exit_code,
            ..
        } = self;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        dos.int21(&mut regs, mem, vga, *now_ms, exit_requested, exit_code);
        context.r = r;
        context.flags = (context.flags & !0x0CD5) | (flags & 0x0CD5);
    }

    /// `INT 21h` in protected mode, reaching the host because no handler took it.
    fn int21_translate(&mut self, cpu: &mut Cpu, mut context: Context) {
        let ah = (context.r[AX] >> 8) as u8;
        let al = context.r[AX] as u8;
        let ds = context.sregs[DS];
        let es = context.sregs[ES];
        let edx = self.client_offset(context.r[DX]);
        let esi = self.client_offset(context.r[SI]);
        let edi = self.client_offset(context.r[DI]);
        let ebx = self.client_offset(context.r[BX]);
        let set16 = |context: &mut Context, index: usize, value: u32| {
            context.r[index] = (context.r[index] & 0xFFFF_0000) | (value & 0xFFFF);
        };
        let fail = |context: &mut Context, code: u32| {
            context.r[AX] = (context.r[AX] & 0xFFFF_0000) | code;
            context.flags |= FLAG_CF;
        };
        let name_call = matches!(ah, 0x39 | 0x3A | 0x3B | 0x3C | 0x3D | 0x41 | 0x43 | 0x4E | 0x5B);
        match ah {
            0x09 => {
                let Some((base, _)) = self.selector_info(ds) else {
                    return self.resume(cpu, &context);
                };
                let target = self.transfer_base();
                for index in 0..TRANSFER_SIZE - 1 {
                    let byte = self.mem.read8(base.wrapping_add(edx).wrapping_add(index));
                    self.mem.write8(target + index, byte);
                    if byte == b'$' {
                        break;
                    }
                }
                set16(&mut context, DX, 0);
                self.dos_native(&mut context);
                context.r[DX] = edx;
            }
            0x1A => {
                if let Some(client) = self.dpmi.clients.last_mut() {
                    client.dta = (ds, edx);
                }
            }
            0x2F => {
                if let Some(client) = self.dpmi.clients.last() {
                    context.sregs[ES] = client.dta.0;
                    context.r[BX] = client.dta.1;
                }
            }
            0x25 => self.set_pm_vector(al, ds, edx),
            0x35 => {
                let (selector, offset) = self.get_pm_vector(al);
                context.sregs[ES] = selector;
                context.r[BX] = offset;
            }
            0x4E | 0x4F => {
                if ah == 0x4E {
                    self.copy_name(ds, edx, 0);
                    context.r[DX] = 0;
                }
                let dta = self.dpmi.clients.last().map_or((0, 0), |client| client.dta);
                if ah == 0x4F {
                    self.copy_transfer(dta.0, dta.1, FIND_DTA, 0x2B, false);
                }
                let saved = self.dos.dta;
                self.dos.dta = self.transfer_base() + FIND_DTA;
                self.dos_native(&mut context);
                self.dos.dta = saved;
                if ah == 0x4E {
                    context.r[DX] = edx;
                }
                if context.flags & FLAG_CF == 0 {
                    self.copy_transfer(dta.0, dta.1, FIND_DTA, 0x2B, true);
                }
            }
            _ if name_call => {
                self.copy_name(ds, edx, 0);
                context.r[DX] = 0;
                self.dos_native(&mut context);
                context.r[DX] = edx;
            }
            0x56 => {
                self.copy_name(ds, edx, 0);
                self.copy_name(es, edi, 0x100);
                context.r[DX] = 0;
                context.r[DI] = 0x100;
                self.dos_native(&mut context);
                context.r[DX] = edx;
                context.r[DI] = edi;
            }
            0x6C => {
                self.copy_name(ds, esi, 0);
                context.r[SI] = 0;
                self.dos_native(&mut context);
                context.r[SI] = esi;
            }
            0x5A => {
                self.copy_name(ds, edx, 0);
                context.r[DX] = 0;
                self.dos_native(&mut context);
                self.copy_transfer(ds, edx, 0, 0x100, true);
                context.r[DX] = edx;
            }
            0x47 => {
                context.r[SI] = 0;
                self.dos_native(&mut context);
                self.copy_transfer(ds, esi, 0, 64, true);
                context.r[SI] = esi;
            }
            0x38 if context.r[DX] as u16 != 0xFFFF => {
                context.r[DX] = 0;
                self.dos_native(&mut context);
                self.copy_transfer(ds, edx, 0, 34, true);
                context.r[DX] = edx;
            }
            0x3F | 0x40 => {
                let total = if self.dpmi.clients.last().is_some_and(|c| c.bits32) {
                    context.r[CX]
                } else {
                    context.r[CX] & 0xFFFF
                };
                let mut done = 0u32;
                let mut call = context;
                while done < total || (total == 0 && ah == 0x40) {
                    let piece = (total - done).min(TRANSFER_SIZE);
                    self.charge(piece / 8);
                    if ah == 0x40 {
                        self.copy_transfer(ds, edx.wrapping_add(done), 0, piece, false);
                    }
                    call.r[AX] = context.r[AX];
                    call.r[CX] = piece;
                    call.r[DX] = 0;
                    self.dos_native(&mut call);
                    if call.flags & FLAG_CF != 0 {
                        context.r[AX] = call.r[AX];
                        context.flags |= FLAG_CF;
                        return self.resume(cpu, &context);
                    }
                    let moved = call.r[AX] & 0xFFFF;
                    if ah == 0x3F {
                        self.copy_transfer(ds, edx.wrapping_add(done), 0, moved, true);
                    }
                    done += moved.min(piece);
                    if moved < piece || total == 0 {
                        break;
                    }
                }
                context.r[AX] = done;
                context.flags &= !FLAG_CF;
            }
            0x44 if matches!(al, 0x02..=0x05) => {
                let len = context.r[CX] & 0xFFFF;
                let piece = len.min(TRANSFER_SIZE);
                let writing = matches!(al, 0x03 | 0x05);
                if writing {
                    self.copy_transfer(ds, edx, 0, piece, false);
                }
                context.r[DX] = 0;
                self.dos_native(&mut context);
                if !writing && context.flags & FLAG_CF == 0 {
                    self.copy_transfer(ds, edx, 0, context.r[AX] & 0xFFFF, true);
                }
                context.r[DX] = edx;
            }
            0x48 => {
                let paras = context.r[BX] & 0xFFFF;
                let kb = (paras * 16).div_ceil(1024).max(1);
                let block = if paras == 0xFFFF {
                    None
                } else {
                    self.xms.alloc_block(kb)
                };
                let selector = block.and_then(|(handle, address)| {
                    let selector = self.alloc_entry(descriptor(address, kb * 1024 - 1, 0xF2, false));
                    if selector.is_none() {
                        self.xms.free_block(handle);
                    }
                    selector.map(|selector| (handle, address, selector))
                });
                match selector {
                    Some((handle, address, selector)) => {
                        if let Some(client) = self.dpmi.clients.last_mut() {
                            client.memory.push((handle, address, kb * 1024));
                        }
                        set16(&mut context, AX, selector as u32);
                        context.flags &= !FLAG_CF;
                    }
                    None => {
                        let largest = (self.xms.largest_free_kb() * 64).min(0xFFFF);
                        set16(&mut context, BX, largest);
                        fail(&mut context, 8);
                    }
                }
            }
            0x49 | 0x4A => {
                let Ok((lo, hi)) = self.read_desc(es) else {
                    fail(&mut context, 9);
                    return self.resume(cpu, &context);
                };
                let base = desc_base(lo, hi);
                let paras = context.r[BX] & 0xFFFF;
                if base < 0x10_0000 {
                    // A DOS block, such as the program's own.
                    let segment = (base >> 4) as u16;
                    let SystemBus { dos, mem, .. } = self;
                    let done = if ah == 0x49 {
                        dos.mcb_free(mem, segment).map_err(|code| (code, 0))
                    } else {
                        dos.mcb_resize(mem, segment, paras as u16)
                    };
                    match done {
                        Ok(()) => context.flags &= !FLAG_CF,
                        Err((code, largest)) => {
                            if ah == 0x4A {
                                set16(&mut context, BX, largest as u32);
                            }
                            fail(&mut context, code as u32);
                        }
                    }
                } else {
                    let found = self.dpmi.clients.last().and_then(|client| {
                        client.memory.iter().position(|(_, address, _)| *address == base)
                    });
                    let Some(at) = found else {
                        fail(&mut context, 9);
                        return self.resume(cpu, &context);
                    };
                    let handle = self.dpmi.clients.last().map_or(0, |c| c.memory[at].0);
                    if ah == 0x49 {
                        self.xms.free_block(handle);
                        if let Some(client) = self.dpmi.clients.last_mut() {
                            client.memory.remove(at);
                            if let Some(index) = client.index(es) {
                                client.ldt_used[index] = false;
                            }
                        }
                        context.sregs[ES] = 0;
                        context.flags &= !FLAG_CF;
                    } else {
                        let kb = (paras * 16).div_ceil(1024).max(1);
                        match self.xms.resize_block(handle, kb, &mut self.mem) {
                            Some(address) => {
                                self.charge(kb * 1024 / 8);
                                let _ = self.set_desc(es, descriptor(address, kb * 1024 - 1, 0xF2, false));
                                if let Some(client) = self.dpmi.clients.last_mut() {
                                    client.memory[at] = (handle, address, kb * 1024);
                                }
                                context.flags &= !FLAG_CF;
                            }
                            None => {
                                let largest = (self.xms.largest_free_kb() * 64).min(0xFFFF);
                                set16(&mut context, BX, largest);
                                fail(&mut context, 8);
                            }
                        }
                    }
                }
            }
            0x4B if al == 0 => return self.exec_translate(cpu, context),
            0x51 | 0x62 => {
                let selector = self.dpmi.clients.last().map_or(0, |c| c.psp_sel);
                set16(&mut context, BX, selector as u32);
            }
            0x50 => {}
            _ => {
                let _ = ebx;
                return self.reflect(cpu, 0x21, context, false);
            }
        }
        self.resume(cpu, &context);
    }

    /// `EXEC`: the name and the parameter block go to the transfer buffer, and the
    /// call goes down, because a child needs real mode for its whole life.
    fn exec_translate(&mut self, cpu: &mut Cpu, mut context: Context) {
        let bits32 = self.dpmi.clients.last().is_some_and(|client| client.bits32);
        let edx = self.client_offset(context.r[DX]);
        let ebx = self.client_offset(context.r[BX]);
        let Some((block, _)) = self.selector_info(context.sregs[ES]) else {
            context.flags |= FLAG_CF;
            return self.resume(cpu, &context);
        };
        self.copy_name(context.sregs[DS], edx, 0);
        let target = self.transfer_base();
        let segment = (target >> 4) as u16;
        let at = block.wrapping_add(ebx);
        // Far pointers in the block: 32:16 for a 32-bit client, 16:16 otherwise.
        let size = if bits32 { 6 } else { 4 };
        let far = |bus: &Self, index: u32| -> (u16, u32) {
            let place = at.wrapping_add(2 + index * size);
            if bits32 {
                (bus.mem.read16(place.wrapping_add(4)), bus.mem.read32(place))
            } else {
                (bus.mem.read16(place.wrapping_add(2)), bus.mem.read16(place) as u32)
            }
        };
        let environment = self.mem.read16(at);
        let environment = match environment {
            0 => 0,
            selector => self
                .selector_info(selector)
                .map_or(0, |(base, _)| (base >> 4) as u16),
        };
        let (tail, fcb1, fcb2) = (far(self, 0), far(self, 1), far(self, 2));
        self.copy_transfer(tail.0, tail.1, EXEC_TAIL, 0x80, false);
        self.copy_transfer(fcb1.0, fcb1.1, EXEC_FCB1, 0x25, false);
        self.copy_transfer(fcb2.0, fcb2.1, EXEC_FCB2, 0x25, false);
        let parameters = target + EXEC_BLOCK;
        self.mem.write16(parameters, environment);
        for (index, offset) in [EXEC_TAIL, EXEC_FCB1, EXEC_FCB2].into_iter().enumerate() {
            let place = parameters + 2 + index as u32 * 4;
            self.mem.write16(place, offset as u16);
            self.mem.write16(place + 2, segment);
        }
        context.r[DX] = 0;
        context.r[BX] = EXEC_BLOCK;
        self.reflect(cpu, 0x21, context, false);
    }

    fn set_pm_vector(&mut self, vector: u8, selector: u16, offset: u32) {
        let is_default = selector == HOST_CODE3;
        let Some(client) = self.dpmi.clients.last_mut() else {
            return;
        };
        client.vectors[vector as usize] = (!is_default).then_some((selector, offset));
        if matches!(vector, 0x08..=0x0F | 0x1C | 0x23 | 0x24 | 0x70..=0x77) {
            self.hook(vector, !is_default);
        }
    }

    fn get_pm_vector(&self, vector: u8) -> (u16, u32) {
        self.dpmi
            .clients
            .last()
            .and_then(|client| client.vectors[vector as usize])
            .unwrap_or((HOST_CODE3, PM_DEFAULT_INT as u32 + vector as u32 * 4))
    }

    /// Puts or removes the host's stub for a vector real mode can see.
    fn hook(&mut self, vector: u8, on: bool) {
        let stub = ((HOST_SEGMENT as u32) << 16) | (RM_INT as u32 + vector as u32 * 4);
        let Some(client) = self.dpmi.clients.last_mut() else {
            return;
        };
        let at = client.hooked.iter().position(|(v, _)| *v == vector);
        match (on, at) {
            (true, None) => {
                client.hooked.push((vector, self.mem.read32(vector as u32 * 4)));
                self.mem.write32(vector as u32 * 4, stub);
            }
            (false, Some(index)) => {
                let (_, original) = client.hooked.remove(index);
                self.mem.write32(vector as u32 * 4, original);
            }
            _ => {}
        }
    }

    fn int31(&mut self, cpu: &mut Cpu, mut context: Context) {
        let ax = context.r[AX] as u16;
        let result = self.int31_service(cpu, ax, &mut context);
        match result {
            Ok(Done::Resume) => {
                context.flags &= !FLAG_CF;
                self.resume(cpu, &context);
            }
            Ok(Done::Elsewhere) => {}
            Err(code) => {
                context.r[AX] = (context.r[AX] & 0xFFFF_0000) | code as u32;
                context.flags |= FLAG_CF;
                self.resume(cpu, &context);
            }
        }
    }

    fn read_desc(&self, selector: u16) -> Result<(u32, u32), u16> {
        let client = self.dpmi.clients.last().ok_or(error::BAD_STATE)?;
        let index = client.index(selector).ok_or(error::BAD_SELECTOR)?;
        let at = client.block.1 + index as u32 * 8;
        Ok((self.mem.read32(at), self.mem.read32(at + 4)))
    }

    fn set_desc(&mut self, selector: u16, value: (u32, u32)) -> Result<(), u16> {
        let client = self.dpmi.clients.last().ok_or(error::BAD_STATE)?;
        let index = client.index(selector).ok_or(error::BAD_SELECTOR)?;
        let at = client.block.1 + index as u32 * 8;
        self.write_desc(at, value);
        Ok(())
    }

    fn int31_service(&mut self, cpu: &mut Cpu, ax: u16, context: &mut Context) -> Result<Done, u16> {
        let (bx, cx, dx, di) = (
            context.r[BX] as u16,
            context.r[CX] as u16,
            context.r[DX] as u16,
            context.r[DI] as u16,
        );
        let bits32 = self.dpmi.clients.last().ok_or(error::BAD_STATE)?.bits32;
        let set16 = |context: &mut Context, index: usize, value: u16| {
            context.r[index] = (context.r[index] & 0xFFFF_0000) | value as u32;
        };
        let offset32 = |context: &Context, index: usize| {
            if bits32 {
                context.r[index]
            } else {
                context.r[index] & 0xFFFF
            }
        };
        match ax {
            // Local descriptors.
            0x0000 => {
                let count = cx as usize;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                if count == 0 {
                    return Err(error::BAD_VALUE);
                }
                let first = client.free_entries(count).ok_or(error::NO_DESCRIPTORS)?;
                client.ldt_used[first..first + count].fill(true);
                let base = client.block.1;
                let big = client.bits32;
                for index in first..first + count {
                    self.write_desc(base + index as u32 * 8, descriptor(0, 0, 0xF2, big));
                }
                set16(context, AX, Client::selector(first));
            }
            0x0001 => {
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                let index = client.index(bx).ok_or(error::BAD_SELECTOR)?;
                client.ldt_used[index] = false;
                let at = client.block.1 + index as u32 * 8;
                self.write_desc(at, (0, 0));
                for index in [ES, DS, FS, GS] {
                    if context.sregs[index] == bx {
                        context.sregs[index] = 0;
                    }
                }
            }
            0x0002 => {
                // The selector for a real-mode segment, reused for the same one.
                let base = (bx as u32) << 4;
                let client = self.dpmi.clients.last().ok_or(error::BAD_STATE)?;
                let found = (RESERVED_LDT..LDT_ENTRIES).find(|&index| {
                    client.ldt_used[index]
                        && desc_base(
                            self.mem.read32(client.block.1 + index as u32 * 8),
                            self.mem.read32(client.block.1 + index as u32 * 8 + 4),
                        ) == base
                        && self.mem.read32(client.block.1 + index as u32 * 8 + 4) & 0xFF00
                            == 0xF200
                        && self.mem.read32(client.block.1 + index as u32 * 8) & 0xFFFF == 0xFFFF
                });
                let selector = match found {
                    Some(index) => Client::selector(index),
                    None => self
                        .alloc_entry(descriptor(base, 0xFFFF, 0xF2, false))
                        .ok_or(error::NO_DESCRIPTORS)?,
                };
                set16(context, AX, selector);
            }
            0x0003 => set16(context, AX, 8),
            // Selectors are never locked, because memory never moves.
            0x0004 | 0x0005 => {}
            0x0006 => {
                let (lo, hi) = self.read_desc(bx)?;
                let base = desc_base(lo, hi);
                set16(context, CX, (base >> 16) as u16);
                set16(context, DX, base as u16);
            }
            0x0007 => {
                let (lo, hi) = self.read_desc(bx)?;
                let base = ((cx as u32) << 16) | dx as u32;
                let lo = (lo & 0xFFFF) | ((base & 0xFFFF) << 16);
                let hi = (hi & 0x00FF_FF00) | ((base >> 16) & 0xFF) | (base & 0xFF00_0000);
                self.set_desc(bx, (lo, hi))?;
            }
            0x0008 => {
                let (lo, hi) = self.read_desc(bx)?;
                let limit = ((cx as u32) << 16) | dx as u32;
                if limit > 0xF_FFFF && limit & 0xFFF != 0xFFF {
                    return Err(error::BAD_VALUE);
                }
                let (raw, granular) = if limit > 0xF_FFFF {
                    (limit >> 12, true)
                } else {
                    (limit, false)
                };
                let lo = (lo & 0xFFFF_0000) | (raw & 0xFFFF);
                let hi = (hi & 0xFF70_FFFF) | (raw & 0xF_0000) | (u32::from(granular) << 23);
                self.set_desc(bx, (lo, hi))?;
            }
            0x0009 => {
                let (lo, hi) = self.read_desc(bx)?;
                if (cx as u8) & 0x60 != 0x60 || (cx as u8) & 0x10 == 0 {
                    return Err(error::BAD_VALUE);
                }
                let hi = (hi & 0xFF20_00FF) | ((cx as u32) & 0xFF) << 8 | ((cx as u32 >> 8) & 0xD0) << 16
                    | (hi & 0x000F_0000);
                self.set_desc(bx, (lo, hi))?;
            }
            0x000A => {
                let (lo, hi) = self.read_desc(bx)?;
                let hi = (hi & 0xFFFF_F0FF) | 0x0000_0200 | 0x0000_F000 | 0x1000;
                let selector = self.alloc_entry((lo, hi)).ok_or(error::NO_DESCRIPTORS)?;
                set16(context, AX, selector);
            }
            0x000B => {
                let (lo, hi) = self.read_desc(bx)?;
                let at = cpu_linear(cpu, ES, offset32(context, DI));
                self.mem.write32(at, lo);
                self.mem.write32(at.wrapping_add(4), hi);
            }
            0x000C => {
                let at = cpu_linear(cpu, ES, offset32(context, DI));
                let value = (self.mem.read32(at), self.mem.read32(at.wrapping_add(4)));
                if value.1 & 0x6000 != 0x6000 {
                    return Err(error::BAD_VALUE);
                }
                self.set_desc(bx, value)?;
            }
            0x000D => {
                let index = (bx >> 3) as usize;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                if bx & 7 != 7 || index >= RESERVED_LDT || index == 0 {
                    return Err(error::BAD_SELECTOR);
                }
                if client.ldt_used[index] {
                    return Err(error::NO_DESCRIPTORS);
                }
                client.ldt_used[index] = true;
                let at = client.block.1 + index as u32 * 8;
                let big = client.bits32;
                self.write_desc(at, descriptor(0, 0, 0xF2, big));
            }
            // DOS memory.
            0x0100 => {
                let owner = self.dos.psp;
                let (mem, dos) = (&mut self.mem, &mut self.dos);
                match dos.mcb_alloc(mem, bx, owner) {
                    Ok(segment) => {
                        let bytes = bx as u32 * 16;
                        let selector = self
                            .alloc_entry(descriptor(
                                (segment as u32) << 4,
                                bytes.saturating_sub(1),
                                0xF2,
                                false,
                            ))
                            .ok_or(error::NO_DESCRIPTORS)?;
                        set16(context, AX, segment);
                        set16(context, DX, selector);
                    }
                    Err((_, largest)) => {
                        set16(context, BX, largest);
                        return Err(error::DOS_MEMORY);
                    }
                }
            }
            0x0101 => {
                let (lo, hi) = self.read_desc(dx)?;
                let segment = (desc_base(lo, hi) >> 4) as u16;
                let (mem, dos) = (&mut self.mem, &mut self.dos);
                dos.mcb_free(mem, segment).map_err(|_| error::DOS_MEMORY)?;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                if let Some(index) = client.index(dx) {
                    client.ldt_used[index] = false;
                }
            }
            0x0102 => {
                let (lo, hi) = self.read_desc(dx)?;
                let segment = (desc_base(lo, hi) >> 4) as u16;
                let (mem, dos) = (&mut self.mem, &mut self.dos);
                match dos.mcb_resize(mem, segment, bx) {
                    Ok(()) => {
                        let limit = (bx as u32 * 16).saturating_sub(1);
                        let lo = (lo & 0xFFFF_0000) | (limit & 0xFFFF);
                        let hi = (hi & 0xFFF0_FFFF) | (limit & 0xF_0000);
                        self.set_desc(dx, (lo, hi))?;
                    }
                    Err((_, largest)) => {
                        set16(context, BX, largest);
                        return Err(error::DOS_MEMORY);
                    }
                }
            }
            // Vectors.
            0x0200 => {
                let handler = self.real_handler(bx as u8);
                set16(context, CX, (handler >> 16) as u16);
                set16(context, DX, handler as u16);
            }
            0x0201 => {
                let value = ((cx as u32) << 16) | dx as u32;
                let vector = bx as u8;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                match client.hooked.iter_mut().find(|(v, _)| *v == vector) {
                    Some((_, original)) => *original = value,
                    None => self.mem.write32(vector as u32 * 4, value),
                }
            }
            0x0202 => {
                let vector = bx as u8 as usize;
                let client = self.dpmi.clients.last().ok_or(error::BAD_STATE)?;
                if vector >= EXCEPTIONS {
                    return Err(error::BAD_VALUE);
                }
                let (selector, offset) = client.exceptions[vector]
                    .unwrap_or((HOST_CODE3, PM_DEFAULT_EXC as u32 + vector as u32 * 4));
                set16(context, CX, selector);
                context.r[DX] = offset;
            }
            0x0203 => {
                let vector = bx as u8 as usize;
                let offset = offset32(context, DX);
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                if vector >= EXCEPTIONS {
                    return Err(error::BAD_VALUE);
                }
                client.exceptions[vector] = Some((cx, offset));
            }
            0x0204 => {
                let (selector, offset) = self.get_pm_vector(bx as u8);
                set16(context, CX, selector);
                context.r[DX] = offset;
            }
            0x0205 => self.set_pm_vector(bx as u8, cx, offset32(context, DX)),
            // Calling real-mode code.
            0x0300 | 0x0301 | 0x0302 => return self.real_call(cpu, ax, context),
            0x0303 => {
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                let slot = client
                    .callbacks
                    .iter()
                    .position(Option::is_none)
                    .ok_or(error::NO_CALLBACK)?;
                client.callbacks[slot] = Some(Callback {
                    selector: context.sregs[DS],
                    offset: offset32(context, SI),
                    registers: (context.sregs[ES], offset32(context, DI)),
                });
                set16(context, CX, HOST_SEGMENT);
                set16(context, DX, CALLBACKS + slot as u16 * 4);
            }
            0x0304 => {
                let slot = (dx.wrapping_sub(CALLBACKS) / 4) as usize;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                if cx != HOST_SEGMENT || client.callbacks.get(slot).copied().flatten().is_none() {
                    return Err(error::BAD_VALUE);
                }
                client.callbacks[slot] = None;
            }
            0x0305 => {
                set16(context, AX, 0);
                set16(context, BX, HOST_SEGMENT);
                set16(context, CX, SAVE_RM);
                set16(context, SI, HOST_CODE3);
                context.r[DI] = SAVE_PM as u32;
            }
            0x0306 => {
                set16(context, BX, HOST_SEGMENT);
                set16(context, CX, RAW_TO_PM);
                set16(context, SI, HOST_CODE3);
                context.r[DI] = RAW_TO_RM as u32;
            }
            0x0400 => {
                set16(context, AX, 0x005A);
                // 32-bit programs, real mode for interrupts, no virtual memory.
                set16(context, BX, 0x0001 | 0x0002);
                context.r[CX] = (context.r[CX] & 0xFFFF_FF00) | 3;
                set16(context, DX, 0x0870);
            }
            // Memory.
            0x0500 => {
                let at = cpu_linear(cpu, ES, offset32(context, DI));
                let (largest, total) = self.xms.free_kb();
                let largest = largest * 1024;
                let field = |index: u32| at.wrapping_add(index);
                for index in 0..12u32 {
                    self.mem.write32(field(index * 4), u32::MAX);
                }
                self.mem.write32(at, largest);
                self.mem.write32(field(4), largest / 4096);
                self.mem.write32(field(8), largest / 4096);
                self.mem.write32(field(0x0C), total / 4);
                self.mem.write32(field(0x14), total / 4);
                self.mem.write32(field(0x18), total / 4);
                self.mem.write32(field(0x20), total / 4);
            }
            0x0501 | 0x0504 => {
                let bytes = if ax == 0x0501 {
                    ((bx as u32) << 16) | cx as u32
                } else {
                    context.r[CX]
                };
                if bytes == 0 {
                    return Err(error::BAD_VALUE);
                }
                let wanted = if ax == 0x0504 { context.r[BX] } else { 0 };
                let kb = bytes.div_ceil(4096).checked_mul(4).ok_or(error::NO_MEMORY)?;
                let found = if wanted != 0 {
                    self.xms.alloc_block_at(wanted, kb)
                } else {
                    self.xms.alloc_block(kb)
                };
                let (handle, address) = found.ok_or(if wanted != 0 {
                    error::NO_LINEAR
                } else {
                    error::NO_MEMORY
                })?;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                client.memory.push((handle, address, kb * 1024));
                if ax == 0x0501 {
                    set16(context, BX, (address >> 16) as u16);
                    set16(context, CX, address as u16);
                    set16(context, SI, 0);
                    set16(context, DI, handle);
                } else {
                    context.r[BX] = address;
                    context.r[SI] = handle as u32;
                }
            }
            0x0502 => {
                let handle = di;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                let at = client
                    .memory
                    .iter()
                    .position(|(h, _, _)| *h == handle)
                    .ok_or(error::BAD_HANDLE)?;
                client.memory.remove(at);
                self.xms.free_block(handle);
            }
            0x0503 => {
                let handle = di;
                let bytes = ((bx as u32) << 16) | cx as u32;
                if bytes == 0 {
                    return Err(error::BAD_VALUE);
                }
                let kb = bytes.div_ceil(4096).checked_mul(4).ok_or(error::NO_MEMORY)?;
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                let at = client
                    .memory
                    .iter()
                    .position(|(h, _, _)| *h == handle)
                    .ok_or(error::BAD_HANDLE)?;
                let (_, old, old_bytes) = client.memory[at];
                let address = self
                    .xms
                    .resize_block(handle, kb, &mut self.mem)
                    .ok_or(error::NO_MEMORY)?;
                if address != old {
                    // That was a copy, which the guest pays for.
                    self.charge(old_bytes.min(kb * 1024) / 8);
                }
                let client = self.dpmi.clients.last_mut().ok_or(error::BAD_STATE)?;
                client.memory[at] = (handle, address, kb * 1024);
                set16(context, BX, (address >> 16) as u16);
                set16(context, CX, address as u16);
            }
            // Nothing pages out, so locking and advice are no work.
            0x0600..=0x0603 | 0x0702 | 0x0703 => {}
            0x0604 => {
                set16(context, BX, 0);
                set16(context, CX, 4096);
            }
            0x0800 => {
                // Linear addresses are physical.
            }
            0x0801 => {}
            0x0900 => {
                let was = context.flags & FLAG_IF != 0;
                context.flags &= !FLAG_IF;
                context.r[AX] = (context.r[AX] & 0xFFFF_FF00) | u32::from(was);
            }
            0x0901 => {
                let was = context.flags & FLAG_IF != 0;
                context.flags |= FLAG_IF;
                context.r[AX] = (context.r[AX] & 0xFFFF_FF00) | u32::from(was);
            }
            0x0902 => {
                let on = context.flags & FLAG_IF != 0;
                context.r[AX] = (context.r[AX] & 0xFFFF_FF00) | u32::from(on);
            }
            0x0A00 => return Err(error::UNSUPPORTED),
            0x0B00..=0x0B03 => return Err(error::NO_HANDLES),
            0x0E00 => set16(context, AX, 0x0045),
            0x0E01 => {}
            _ => return Err(error::UNSUPPORTED),
        }
        Ok(Done::Resume)
    }

    /// `AX=0300h` to `0302h`: real-mode code with registers from a structure.
    fn real_call(&mut self, cpu: &mut Cpu, ax: u16, context: &Context) -> Result<Done, u16> {
        let client = self.dpmi.clients.last().ok_or(error::BAD_STATE)?;
        let bits32 = client.bits32;
        let (rm_seg, rm_sp) = (client.rm_seg, client.rm_sp);
        let registers_off = if bits32 {
            context.r[DI]
        } else {
            context.r[DI] & 0xFFFF
        };
        let at = cpu_linear(cpu, ES, registers_off);
        let mut rm = self.read_rm_registers(at, context);
        let ownstack = rm.r[SP] == 0 && rm.sregs[SS] == 0;
        if ownstack {
            rm.sregs[SS] = rm_seg;
            rm.r[SP] = rm_sp as u32;
        }
        // Parameters off the client's stack.
        let words = context.r[CX] & 0xFFFF;
        if words > 0 {
            let Some((base, big)) = self.selector_info(context.sregs[SS]) else {
                return Err(error::BAD_VALUE);
            };
            let top = if big {
                context.r[SP]
            } else {
                context.r[SP] & 0xFFFF
            };
            let mut sp = rm.r[SP] as u16;
            sp = sp.wrapping_sub((words * 2) as u16);
            for index in 0..words {
                let word = self.mem.read16(base.wrapping_add(top).wrapping_add(index * 2));
                self.mem
                    .write16(Cpu::physical(rm.sregs[SS], sp.wrapping_add((index * 2) as u16)), word);
            }
            rm.r[SP] = sp as u32;
        }
        let mut iret = true;
        match ax {
            0x0300 => {
                let target = self.real_handler(context.r[BX] as u8);
                rm.sregs[CS] = (target >> 16) as u16;
                rm.ip = target & 0xFFFF;
            }
            0x0301 => iret = false,
            _ => {}
        }
        rm.flags &= !(FLAG_IOPL | FLAG_TF);
        // The target's IF is the caller's.
        rm.flags = (rm.flags & !FLAG_IF) | (context.flags & FLAG_IF);
        if !self.push_frame(cpu, FrameKind::RealCall, *context, at) {
            return Ok(Done::Elsewhere);
        }
        self.real_call_frame(&mut rm, iret);
        if ax == 0x0300 {
            rm.flags &= !FLAG_IF;
        }
        if ownstack {
            if let Some(client) = self.dpmi.clients.last_mut() {
                client.rm_sp = rm.r[SP] as u16;
            }
        }
        self.enter_rm(cpu, &rm);
        Ok(Done::Elsewhere)
    }
}

/// What an `INT 31h` function did with the CPU.
enum Done {
    /// Return to the client with these registers.
    Resume,
    /// It has gone elsewhere, and will return by itself.
    Elsewhere,
}

/// A linear address through a segment register, from its hidden part.
fn cpu_linear(cpu: &Cpu, seg: usize, off: u32) -> u32 {
    cpu.seg[seg].base.wrapping_add(off)
}
