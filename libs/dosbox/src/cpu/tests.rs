//! CPU conformance tests: small programs run through a plain memory bus, with
//! results checked against what the Intel manuals define.

use super::*;

/// A flat 2 MiB memory with port and service recording.
struct Mem {
    data: Vec<u8>,
    calls: Vec<u8>,
    blocked_once: bool,
    flags_to_return: u32,
}

impl Mem {
    fn new() -> Self {
        Self {
            data: vec![0; 0x20_0000],
            calls: Vec::new(),
            blocked_once: false,
            flags_to_return: 0,
        }
    }
}

impl Bus for Mem {
    fn read8(&mut self, addr: u32) -> u8 {
        self.data.get(addr as usize).copied().unwrap_or(0xFF)
    }
    fn write8(&mut self, addr: u32, value: u8) {
        if let Some(slot) = self.data.get_mut(addr as usize) {
            *slot = value;
        }
    }
    fn interrupt(&mut self, vector: u8, regs: &mut CpuRegs<'_>) -> InterruptAction {
        self.calls.push(vector);
        if self.blocked_once {
            self.blocked_once = false;
            return InterruptAction::Blocked;
        }
        *regs.flags = (*regs.flags & !(FLAG_CF | FLAG_ZF)) | self.flags_to_return;
        InterruptAction::Continue
    }
}

const CODE: u32 = 0x1_0000;

fn setup(code: &[u8]) -> (Cpu, Mem) {
    let mut cpu = Cpu::new();
    let mut mem = Mem::new();
    cpu.set_real_segment(CS, 0x1000);
    cpu.set_real_segment(DS, 0x2000);
    cpu.set_real_segment(ES, 0x3000);
    cpu.set_real_segment(SS, 0x4000);
    cpu.r[SP] = 0xFFFE;
    mem.data[CODE as usize..CODE as usize + code.len()].copy_from_slice(code);
    // A stub for every vector, at F000:n*4.
    for vector in 0..256u32 {
        let at = 0xF_0000 + vector as usize * 4;
        mem.data[at..at + 4].copy_from_slice(&[
            CALLBACK_OPCODE,
            CALLBACK_MODRM,
            vector as u8,
            0xCF,
        ]);
        let entry = vector as usize * 4;
        mem.data[entry..entry + 2].copy_from_slice(&((vector * 4) as u16).to_le_bytes());
        mem.data[entry + 2..entry + 4].copy_from_slice(&0xF000u16.to_le_bytes());
    }
    (cpu, mem)
}

fn run(code: &[u8], steps: usize) -> (Cpu, Mem) {
    let (mut cpu, mut mem) = setup(code);
    for _ in 0..steps {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    (cpu, mem)
}

/// Runs `MOV AX,a; MOV BX,b; <op bytes>` and returns AX and the flags.
fn alu_case(a: u16, b: u16, op: &[u8]) -> (u16, u32) {
    let mut code = vec![0xB8, a as u8, (a >> 8) as u8, 0xBB, b as u8, (b >> 8) as u8];
    code.extend_from_slice(op);
    let (cpu, _) = run(&code, 3);
    (cpu.r[AX] as u16, cpu.flags())
}

const STATUS: u32 = FLAG_CF | FLAG_PF | FLAG_AF | FLAG_ZF | FLAG_SF | FLAG_OF;

#[test]
fn alu_flag_table() {
    // (name, a, b, op, result, flags)
    let cases: &[(&str, u16, u16, &[u8], u16, u32)] = &[
        ("add carry", 0xFFFF, 1, &[0x01, 0xD8], 0, FLAG_CF | FLAG_ZF | FLAG_PF | FLAG_AF),
        ("add overflow", 0x7FFF, 1, &[0x01, 0xD8], 0x8000, FLAG_OF | FLAG_SF | FLAG_PF | FLAG_AF),
        ("sub borrow", 0, 1, &[0x29, 0xD8], 0xFFFF, FLAG_CF | FLAG_SF | FLAG_PF | FLAG_AF),
        ("sub overflow", 0x8000, 1, &[0x29, 0xD8], 0x7FFF, FLAG_OF | FLAG_PF | FLAG_AF),
        ("cmp equal", 5, 5, &[0x39, 0xD8], 5, FLAG_ZF | FLAG_PF),
        ("and clears", 0xFF00, 0x0F0F, &[0x21, 0xD8], 0x0F00, FLAG_PF),
        ("xor zero", 0x1234, 0x1234, &[0x31, 0xD8], 0, FLAG_ZF | FLAG_PF),
        ("or sign", 0x8000, 1, &[0x09, 0xD8], 0x8001, FLAG_SF),
        ("add8 half", 0x000F, 1, &[0x00, 0xD8], 0x0010, FLAG_AF),
    ];
    for (name, a, b, op, result, flags) in cases {
        let (ax, got) = alu_case(*a, *b, op);
        assert_eq!(ax, *result, "{name} result");
        assert_eq!(got & STATUS, *flags, "{name} flags");
    }
}

#[test]
fn adc_and_sbb_use_the_carry() {
    let (cpu, _) = run(&[0xF9, 0xB8, 1, 0, 0x83, 0xD0, 1], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 3);
    let (cpu, _) = run(&[0xF9, 0xB8, 1, 0, 0x83, 0xD8, 0], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0);
    assert_eq!(cpu.flags() & FLAG_ZF, FLAG_ZF);
}

#[test]
fn inc_and_dec_update_overflow_and_aux_but_preserve_carry() {
    // MOV AX,7FFFh; STC; INC AX
    let (cpu, _) = run(&[0xB8, 0xFF, 0x7F, 0xF9, 0x40], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x8000);
    let flags = cpu.flags();
    assert_ne!(flags & FLAG_OF, 0);
    assert_ne!(flags & FLAG_AF, 0);
    assert_ne!(flags & FLAG_CF, 0, "carry preserved");
    // MOV AX,8000h; CLC; DEC AX
    let (cpu, _) = run(&[0xB8, 0x00, 0x80, 0xF8, 0x48], 3);
    let flags = cpu.flags();
    assert_ne!(flags & FLAG_OF, 0);
    assert_eq!(flags & FLAG_CF, 0);
    // FE/FF forms too. MOV BL,7Fh; INC BL
    let (cpu, _) = run(&[0xB3, 0x7F, 0xFE, 0xC3], 2);
    assert_ne!(cpu.flags() & FLAG_OF, 0);
    // INC doesn't lose a carry a lazy ADD left behind.
    let (cpu, _) = run(&[0xB8, 0xFF, 0xFF, 0x05, 1, 0, 0x43], 3);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
}

#[test]
fn rotates_only_touch_carry_and_overflow() {
    // MOV AX,0; CMP AX,AX sets ZF and PF; MOV AL,81h; ROL AL,1
    let (cpu, _) = run(&[0xB8, 0, 0, 0x39, 0xC0, 0xB0, 0x81, 0xD0, 0xC0], 4);
    assert_eq!(cpu.r[AX] & 0xFF, 0x03);
    let flags = cpu.flags();
    assert_ne!(flags & FLAG_ZF, 0, "ZF kept");
    assert_ne!(flags & FLAG_PF, 0, "PF kept");
    assert_ne!(flags & FLAG_CF, 0);
    assert_ne!(flags & FLAG_OF, 0);
}

#[test]
fn rotate_right_by_one_computes_overflow() {
    // MOV AL,1; ROR AL,1 gives 80h, CF=1, OF = top two bits differ = 1
    let (cpu, _) = run(&[0xB0, 1, 0xD0, 0xC8], 2);
    assert_eq!(cpu.r[AX] & 0xFF, 0x80);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
    assert_ne!(cpu.flags() & FLAG_OF, 0);
    // MOV AL,3; ROR AL,1 gives 81h: OF = 1 != 0 = 1
    let (cpu, _) = run(&[0xB0, 2, 0xD0, 0xC8], 2);
    assert_eq!(cpu.r[AX] & 0xFF, 1);
    assert_eq!(cpu.flags() & FLAG_OF, 0);
}

#[test]
fn rcl_and_rcr_go_through_the_carry() {
    // STC; MOV AL,80h; RCL AL,1 -> AL=1, CF=1
    let (cpu, _) = run(&[0xF9, 0xB0, 0x80, 0xD0, 0xD0], 3);
    assert_eq!(cpu.r[AX] & 0xFF, 1);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
    // STC; MOV AL,1; RCR AL,1 -> AL=80h, CF=1
    let (cpu, _) = run(&[0xF9, 0xB0, 1, 0xD0, 0xD8], 3);
    assert_eq!(cpu.r[AX] & 0xFF, 0x80);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
}

#[test]
fn shift_table() {
    // (value, reg field, count, result, cf)
    let cases: &[(u16, u8, u8, u16, bool)] = &[
        (0x8001, 4, 1, 0x0002, true),
        (0x8001, 5, 1, 0x4000, true),
        (0x8001, 7, 1, 0xC000, true),
        (0x8000, 7, 15, 0xFFFF, false),
        (0x00FF, 4, 8, 0xFF00, false),
        (0x00FF, 5, 4, 0x000F, true),
        (0xFFFF, 4, 16, 0x0000, true),
        (0xFFFF, 4, 17, 0x0000, false),
    ];
    for (value, kind, count, result, cf) in cases {
        // MOV AX,value; MOV CL,count; shift AX,CL
        let code = [
            0xB8,
            *value as u8,
            (*value >> 8) as u8,
            0xB1,
            *count,
            0xD3,
            0xC0 | (kind << 3),
        ];
        let (cpu, _) = run(&code, 3);
        assert_eq!(cpu.r[AX] as u16, *result, "{value:x} {kind} {count}");
        assert_eq!(cpu.flags() & FLAG_CF != 0, *cf, "{value:x} {kind} {count} cf");
    }
    // A zero count changes no flags, and the count masks to five bits.
    let (cpu, _) = run(&[0xF9, 0xB8, 1, 0, 0xB1, 32, 0xD3, 0xE0], 4);
    assert_eq!(cpu.r[AX] & 0xFFFF, 1);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
}

#[test]
fn xlat_honours_segment_overrides() {
    let (mut cpu, mut mem) = setup(&[0x26, 0xD7]);
    cpu.r[BX] = 0x100;
    cpu.r[AX] = 5;
    mem.data[0x3_0105] = 0x77;
    mem.data[0x2_0105] = 0x11;
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.r[AX] & 0xFF, 0x77);
}

#[test]
fn imul_immediate_flags_mean_overflow_only() {
    // MOV BX,-1; IMUL AX,BX,1
    let (cpu, _) = run(&[0xBB, 0xFF, 0xFF, 0x6B, 0xC3, 0x01], 2);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0xFFFF);
    assert_eq!(cpu.flags() & (FLAG_CF | FLAG_OF), 0);
    // MOV BX,4000h; IMUL AX,BX,4 overflows.
    let (cpu, _) = run(&[0xBB, 0x00, 0x40, 0x6B, 0xC3, 0x04], 2);
    assert_eq!(cpu.flags() & (FLAG_CF | FLAG_OF), FLAG_CF | FLAG_OF);
    // IMUL with a word immediate.
    let (cpu, _) = run(&[0xBB, 0xFF, 0xFF, 0x69, 0xC3, 0x02, 0x00], 2);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0xFFFE);
    assert_eq!(cpu.flags() & (FLAG_CF | FLAG_OF), 0);
}

#[test]
fn division_table() {
    // MOV DX,hi; MOV AX,lo; MOV BX,div; DIV/IDIV BX
    let cases: &[(u16, u16, u16, u8, Option<(u16, u16)>)] = &[
        (0, 100, 7, 0xF3, Some((14, 2))),
        (1, 0, 2, 0xF3, Some((0x8000, 0))),
        (1, 0, 1, 0xF3, None),
        (0xFFFF, 0xFF9C, 7, 0xFB, Some((0xFFF2, 0xFFFE))),
        (0x8000, 0, 0xFFFF, 0xFB, None),
        (0, 1, 0, 0xF3, None),
    ];
    for (hi, lo, divisor, modrm, expected) in cases {
        let code = [
            0xBA,
            *hi as u8,
            (*hi >> 8) as u8,
            0xB8,
            *lo as u8,
            (*lo >> 8) as u8,
            0xBB,
            *divisor as u8,
            (*divisor >> 8) as u8,
            0xF7,
            *modrm,
        ];
        let (cpu, _mem) = run(&code, 4);
        match expected {
            Some((quotient, remainder)) => {
                assert_eq!(cpu.r[AX] as u16, *quotient);
                assert_eq!(cpu.r[DX] as u16, *remainder);
                assert_eq!(cpu.ip, code.len() as u32);
            }
            None => {
                // The fault went through vector 0, with IP left on the DIV.
                assert_eq!(cpu.sregs[CS], 0xF000);
                assert_eq!(cpu.ip, 0);
            }
        }
    }
}

#[test]
fn bcd_adjust_table() {
    // MOV AL,15h; ADD AL,27h; DAA = 42h
    let (cpu, _) = run(&[0xB0, 0x15, 0x04, 0x27, 0x27], 3);
    assert_eq!(cpu.r[AX] & 0xFF, 0x42);
    // MOV AL,42h; SUB AL,15h; DAS = 27h
    let (cpu, _) = run(&[0xB0, 0x42, 0x2C, 0x15, 0x2F], 3);
    assert_eq!(cpu.r[AX] & 0xFF, 0x27);
    // MOV AX,0009h; ADD AL,1; AAA -> AX=0100h
    let (cpu, _) = run(&[0xB8, 9, 0, 0x04, 1, 0x37], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x0100);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
    // AAM, AAD
    let (cpu, _) = run(&[0xB0, 57, 0xD4, 10], 2);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x0507);
    let (cpu, _) = run(&[0xB8, 7, 5, 0xD5, 10], 2);
    assert_eq!(cpu.r[AX] & 0xFFFF, 57);
}

#[test]
fn identifies_as_a_386() {
    // PUSH SP pushes the old value.
    let (cpu, mem) = run(&[0x54], 1);
    let at = 0x4_0000 + cpu.r[SP] as usize;
    assert_eq!(mem.data[at] as u32 | (mem.data[at + 1] as u32) << 8, 0xFFFE);
    // Bits 12-14 of FLAGS can be changed in real mode, and bit 15 can't.
    // PUSH F000h; POPF; PUSHF; POP AX
    let (cpu, _) = run(&[0x68, 0x00, 0xF0, 0x9D, 0x9C, 0x58], 4);
    assert_eq!(cpu.r[AX] & 0xF000, 0x7000);
}

#[test]
fn undefined_opcodes_raise_int_6() {
    for code in [&[0x0F, 0xFF][..], &[0xFE, 0xFF][..], &[0xFF, 0xFF][..], &[0x8D, 0xC0][..]] {
        let (mut cpu, mut mem) = setup(code);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 6 * 4), "{code:02X?}");
    }
}

#[test]
fn prefix_runs_are_bounded() {
    let (mut cpu, mut mem) = setup(&[0x2E; 0x100]);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 6 * 4));
    // Fourteen prefixes and an opcode are legal.
    let mut code = vec![0x2E; 14];
    code.push(0x90);
    let (mut cpu, mut mem) = setup(&code);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.ip, 15);
}

#[test]
fn int_goes_through_the_vector_table_and_iret_restores() {
    // STI; INT 21h, then the stub's callback and IRET.
    let (mut cpu, mut mem) = setup(&[0xFB, 0xCD, 0x21, 0x90]);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    let sp = cpu.r[SP];
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 0x21 * 4));
    assert_eq!(cpu.flags() & FLAG_IF, 0);
    assert_eq!(cpu.r[SP], sp - 6);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(mem.calls, [0x21]);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0x1000, 3));
    assert_eq!(cpu.r[SP], sp, "the frame is gone");
    assert_ne!(cpu.flags() & FLAG_IF, 0, "IF comes back");
}

#[test]
fn hooked_vectors_run_and_can_chain() {
    // Vector 60h points at a handler that counts in BX and chains with PUSHF/CALL FAR.
    let handler = [
        0x43, // INC BX
        0x9C, // PUSHF
        0x9A, 0x80, 0x01, 0x00, 0xF0, // CALL F000:0180 (the stub for vector 60h)
        0xCF, // IRET
    ];
    let (mut cpu, mut mem) = setup(&[0xCD, 0x60, 0x90]);
    mem.data[0x1_0100..0x1_0100 + handler.len()].copy_from_slice(&handler);
    mem.data[0x180..0x184].copy_from_slice(&[0x00, 0x01, 0x00, 0x10]);
    for _ in 0..7 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert_eq!(cpu.r[BX], 1);
    assert_eq!(mem.calls, [0x60]);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0x1000, 2));
    assert_eq!(cpu.r[SP], 0xFFFE);
}

#[test]
fn callback_patches_cf_and_zf_into_the_stacked_flags() {
    // CLC; INT 16h with the service returning CF and ZF, others untouched.
    let (mut cpu, mut mem) = setup(&[0xFD, 0xF8, 0xCD, 0x16, 0x90]);
    mem.flags_to_return = FLAG_CF | FLAG_ZF;
    for _ in 0..5 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert_eq!(cpu.ip, 4);
    let flags = cpu.flags();
    assert_ne!(flags & FLAG_CF, 0);
    assert_ne!(flags & FLAG_ZF, 0);
    assert_ne!(flags & FLAG_DF, 0, "the rest of the caller's flags stand");
}

#[test]
fn blocked_services_stay_on_the_callback() {
    let (mut cpu, mut mem) = setup(&[0xCD, 0x16, 0x90]);
    mem.blocked_once = true;
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    let (ip, sp) = (cpu.ip, cpu.r[SP]);
    assert_eq!(cpu.step(&mut mem), Step::Blocked);
    assert_eq!(cpu.ip, ip, "stays on the callback");
    assert_eq!(cpu.r[SP], sp, "without pushing another frame");
    assert_ne!(cpu.flags() & FLAG_IF, 0, "interrupts can arrive while it waits");
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0x1000, 2));
    assert_eq!(mem.calls, [0x16, 0x16]);
}

#[test]
fn callback_opcode_outside_the_rom_is_undefined() {
    let (mut cpu, mut mem) = setup(&[CALLBACK_OPCODE, CALLBACK_MODRM, 0x21]);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert!(mem.calls.is_empty());
    assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 6 * 4));
}

#[test]
fn rep_movs_resumes_and_counts_its_work() {
    let (mut cpu, mut mem) = setup(&[0xF3, 0xA5, 0x90]);
    cpu.r[CX] = 5000;
    let start = cpu.cycles;
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.r[CX], 5000 - REP_CHUNK);
    assert_eq!(cpu.ip, 0, "resumes at the prefix");
    assert_eq!(cpu.cycles - start, 1 + REP_CHUNK as u64);
    while cpu.r[CX] != 0 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert_eq!(cpu.ip, 2);
    assert_eq!(cpu.r[SI] & 0xFFFF, 10000);
    assert_eq!(cpu.r[DI] & 0xFFFF, 10000);
    assert_eq!(cpu.cycles - start, 5 + 5000);
}

#[test]
fn repe_cmps_stops_at_the_difference() {
    let (mut cpu, mut mem) = setup(&[0xF3, 0xA6]);
    cpu.r[CX] = 10;
    mem.data[0x2_0003] = 1;
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.r[CX], 6);
    assert_eq!(cpu.flags() & FLAG_ZF, 0);
    assert_eq!(cpu.ip, 2);
}

#[test]
fn rep_string_direction_and_count_zero() {
    // STD; REP STOSB with CX=3
    let (mut cpu, mut mem) = setup(&[0xFD, 0xF3, 0xAA]);
    cpu.r[CX] = 3;
    cpu.r[DI] = 2;
    cpu.r[AX] = 0x55;
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(&mem.data[0x3_0000..0x3_0003], &[0x55; 3]);
    assert_eq!(cpu.r[DI] & 0xFFFF, 0xFFFF);
    // CX=0 is a no-op.
    let (mut cpu, mut mem) = setup(&[0xF3, 0xAA, 0x90]);
    cpu.step(&mut mem);
    assert_eq!(cpu.ip, 2);
    assert_eq!(mem.data[0x3_0000], 0);
}

#[test]
fn sti_and_mov_ss_hold_off_interrupts_for_one_instruction() {
    let (mut cpu, mut mem) = setup(&[0xFA, 0xFB, 0x90, 0x8E, 0xD0, 0x90]);
    cpu.step(&mut mem);
    assert!(!cpu.interrupts_enabled());
    cpu.step(&mut mem);
    assert!(!cpu.interrupts_enabled(), "STI waits one instruction");
    cpu.step(&mut mem);
    assert!(cpu.interrupts_enabled());
    cpu.step(&mut mem);
    assert!(!cpu.interrupts_enabled(), "MOV SS waits one instruction");
    cpu.step(&mut mem);
    assert!(cpu.interrupts_enabled());
}

#[test]
fn trap_flag_single_steps_through_vector_1() {
    // PUSHF with TF set then POPF; TF takes effect after the following instruction.
    let (mut cpu, mut mem) = setup(&[0x68, 0x00, 0x01, 0x9D, 0x90, 0x90]);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 4));
    assert_eq!(cpu.flags() & FLAG_TF, 0);
    let frame = 0x4_0000 + cpu.r[SP] as usize;
    assert_eq!(mem.data[frame], 5);
}

#[test]
fn enter_copies_nested_frame_pointers() {
    // ENTER 8,2 with BP pointing at a frame holding a saved pointer.
    let (mut cpu, mut mem) = setup(&[0xC8, 8, 0, 2]);
    cpu.r[BP] = 0x1000;
    mem.data[0x4_0FFE..0x4_1000].copy_from_slice(&0x2222u16.to_le_bytes());
    cpu.step(&mut mem);
    let sp = cpu.r[SP] as usize;
    let word = |at: usize| u16::from_le_bytes([mem.data[0x4_0000 + at], mem.data[0x4_0001 + at]]);
    assert_eq!(cpu.r[BP], 0xFFFC, "BP = the frame after pushing the old BP");
    assert_eq!(word(0xFFFC), 0x1000, "old BP");
    assert_eq!(word(0xFFFA), 0x2222, "copied display entry");
    assert_eq!(word(0xFFF8), 0xFFFC, "own frame pointer");
    assert_eq!(sp, 0xFFF8 - 8);
}

#[test]
fn bound_raises_int_5_only_out_of_range() {
    let (mut cpu, mut mem) = setup(&[0x62, 0x06, 0x00, 0x01, 0x90]);
    mem.data[0x2_0100..0x2_0104].copy_from_slice(&[2, 0, 9, 0]);
    cpu.r[AX] = 5;
    cpu.step(&mut mem);
    assert_eq!(cpu.ip, 4);
    let (mut cpu, mut mem) = setup(&[0x62, 0x06, 0x00, 0x01, 0x90]);
    mem.data[0x2_0100..0x2_0104].copy_from_slice(&[2, 0, 9, 0]);
    cpu.r[AX] = 10;
    cpu.step(&mut mem);
    assert_eq!((cpu.sregs[CS], cpu.ip), (0xF000, 5 * 4));
}

#[test]
fn word_access_at_ffff_wraps_within_the_segment() {
    let (mut cpu, mut mem) = setup(&[0x8B, 0x07]);
    cpu.r[BX] = 0xFFFF;
    mem.data[0x2_FFFF] = 0x34;
    mem.data[0x2_0000] = 0x12;
    mem.data[0x3_0000] = 0x99;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x1234);
}

// ---------------------------------------------------------------------------
// 386 instructions in real mode
// ---------------------------------------------------------------------------

#[test]
fn operand_size_prefix_gives_32_bit_registers() {
    // MOV EAX,12345678h; ADD EAX,EAX
    let (cpu, _) = run(&[0x66, 0xB8, 0x78, 0x56, 0x34, 0x12, 0x66, 0x01, 0xC0], 2);
    assert_eq!(cpu.r[AX], 0x2468_ACF0);
}

#[test]
fn address_size_prefix_gives_scaled_indexes() {
    // MOV AX,[EBX+ECX*4+8]
    let (mut cpu, mut mem) = setup(&[0x67, 0x8B, 0x44, 0x8B, 0x08]);
    cpu.r[BX] = 0x100;
    cpu.r[CX] = 2;
    mem.data[0x2_0110..0x2_0112].copy_from_slice(&[0xCD, 0xAB]);
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0xABCD);
}

#[test]
fn movzx_movsx_and_setcc() {
    let (cpu, _) = run(&[0xB3, 0x80, 0x0F, 0xB6, 0xC3, 0x0F, 0xBE, 0xCB], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x0080);
    assert_eq!(cpu.r[CX] & 0xFFFF, 0xFF80);
    let (cpu, _) = run(&[0x31, 0xC0, 0x0F, 0x94, 0xC3], 2);
    assert_eq!(cpu.r[BX] & 0xFF, 1);
}

#[test]
fn bit_scan_and_test_family() {
    // MOV AX,0x0110; BSF CX,AX; BSR DX,AX
    let (cpu, _) = run(&[0xB8, 0x10, 0x01, 0x0F, 0xBC, 0xC8, 0x0F, 0xBD, 0xD0], 3);
    assert_eq!(cpu.r[CX] & 0xFFFF, 4);
    assert_eq!(cpu.r[DX] & 0xFFFF, 8);
    assert_eq!(cpu.flags() & FLAG_ZF, 0);
    // BTS AX,3: CF is the old bit.
    let (cpu, _) = run(&[0xB8, 0x08, 0x00, 0x0F, 0xBA, 0xE8, 0x03, 0x0F, 0xBA, 0xE8, 0x04], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x18);
    assert_eq!(cpu.flags() & FLAG_CF, 0);
    // BT [mem],reg addresses beyond the word.
    let (mut cpu, mut mem) = setup(&[0x0F, 0xA3, 0x07]);
    cpu.r[AX] = 17;
    cpu.r[BX] = 0x100;
    mem.data[0x2_0102] = 2;
    cpu.step(&mut mem);
    assert_ne!(cpu.flags() & FLAG_CF, 0);
}

#[test]
fn double_shifts() {
    // MOV AX,1234h; MOV BX,ABCDh; SHLD AX,BX,4
    let (cpu, _) = run(&[0xB8, 0x34, 0x12, 0xBB, 0xCD, 0xAB, 0x0F, 0xA4, 0xD8, 4], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x234A);
    // SHRD AX,BX,4
    let (cpu, _) = run(&[0xB8, 0x34, 0x12, 0xBB, 0xCD, 0xAB, 0x0F, 0xAC, 0xD8, 4], 3);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0xD123);
    // 32-bit SHLD.
    let (mut cpu, mut mem) = setup(&[0x66, 0x0F, 0xA4, 0xD8, 8]);
    cpu.r[AX] = 0x1234_5678;
    cpu.r[BX] = 0xABCD_EF01;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0x3456_78AB);
}

#[test]
fn fs_and_gs_address_memory() {
    // MOV AX,5000h; MOV FS,AX; MOV AL,FS:[10h]
    let (mut cpu, mut mem) = setup(&[0xB8, 0x00, 0x50, 0x8E, 0xE0, 0x64, 0xA0, 0x10, 0x00]);
    mem.data[0x5_0010] = 0x42;
    for _ in 0..3 {
        cpu.step(&mut mem);
    }
    assert_eq!(cpu.r[AX] & 0xFF, 0x42);
}

#[test]
fn string_ops_use_32_bit_registers_with_prefixes() {
    // REP MOVSD with 67h: ECX, ESI, EDI.
    let (mut cpu, mut mem) = setup(&[0xF3, 0x66, 0x67, 0xA5]);
    cpu.r[CX] = 2;
    cpu.r[SI] = 0x10;
    cpu.r[DI] = 0x20;
    mem.data[0x2_0010..0x2_0018].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    cpu.step(&mut mem);
    assert_eq!(&mem.data[0x3_0020..0x3_0028], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(cpu.r[SI], 0x18);
    assert_eq!(cpu.r[CX], 0);
}

#[test]
fn stack_32_bit_pushes_and_pusha() {
    // PUSHAD; POPAD round trip with 66h.
    let (mut cpu, mut mem) = setup(&[0x66, 0x60, 0x66, 0x61]);
    cpu.r[AX] = 0xDEAD_BEEF;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[SP], 0xFFFE - 32);
    cpu.r[AX] = 0;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0xDEAD_BEEF);
    assert_eq!(cpu.r[SP], 0xFFFE);
}

#[test]
fn cmpxchg_xadd_and_bswap() {
    let (mut cpu, mut mem) = setup(&[0x0F, 0xB1, 0xCB, 0x0F, 0xC1, 0xCB, 0x66, 0x0F, 0xC8]);
    cpu.r[AX] = 5;
    cpu.r[BX] = 5;
    cpu.r[CX] = 9;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[BX] & 0xFFFF, 9);
    assert_ne!(cpu.flags() & FLAG_ZF, 0);
    cpu.step(&mut mem);
    assert_eq!(cpu.r[BX] & 0xFFFF, 18);
    assert_eq!(cpu.r[CX] & 0xFFFF, 9);
    cpu.r[AX] = 0x1234_5678;
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0x7856_3412);
}

#[test]
fn lss_and_friends_load_far_pointers() {
    // LFS AX,[100h]
    let (mut cpu, mut mem) = setup(&[0x0F, 0xB4, 0x06, 0x00, 0x01]);
    mem.data[0x2_0100..0x2_0104].copy_from_slice(&[0x11, 0x22, 0x00, 0x60]);
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX] & 0xFFFF, 0x2211);
    assert_eq!(cpu.sregs[FS], 0x6000);
    assert_eq!(cpu.seg[FS].base, 0x6_0000);
}

// ---------------------------------------------------------------------------
// Protected mode
// ---------------------------------------------------------------------------

const GDT: u32 = 0x8_0000;
const IDT: u32 = 0x8_1000;
const TSS: u32 = 0x8_2000;
const TSS2: u32 = 0x8_3000;
const PM_CODE: u32 = 0x10_0000;
const PM_STACK: u32 = 0x18_0000;

fn gdt_entry(base: u32, limit: u32, access: u8, flags: u8) -> u64 {
    (limit as u64 & 0xFFFF)
        | ((base as u64 & 0xFF_FFFF) << 16)
        | ((access as u64) << 40)
        | (((limit as u64 >> 16) & 0xF) << 48)
        | ((flags as u64 & 0xF) << 52)
        | (((base as u64 >> 24) & 0xFF) << 56)
}

fn write64(mem: &mut Mem, at: u32, value: u64) {
    mem.data[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

fn write32_at(mem: &mut Mem, at: u32, value: u32) {
    mem.data[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
}

fn idt_gate(selector: u16, offset: u32, kind: u8, dpl: u8) -> u64 {
    (offset as u64 & 0xFFFF)
        | ((selector as u64) << 16)
        | ((0x80 | (dpl << 5) | kind) as u64) << 40
        | ((offset as u64 >> 16) << 48)
}

/// Builds a flat 32-bit world and jumps to `code` at ring 0.
///
/// Selectors: 08h code, 10h data, 18h ring 3 code, 20h ring 3 data, 28h TSS,
/// 30h a limited data segment, 38h second TSS, 40h 16-bit code.
fn protected(code: &[u8]) -> (Cpu, Mem) {
    let (mut cpu, mut mem) = setup(&[]);
    let entries = [
        0,
        gdt_entry(0, 0xFFFFF, 0x9A, 0xC),
        gdt_entry(0, 0xFFFFF, 0x92, 0xC),
        gdt_entry(0, 0xFFFFF, 0xFA, 0xC),
        gdt_entry(0, 0xFFFFF, 0xF2, 0xC),
        gdt_entry(TSS, 103, 0x89, 0x0),
        gdt_entry(0x5_0000, 0xFF, 0x92, 0x4),
        gdt_entry(TSS2, 103, 0x89, 0x0),
        gdt_entry(PM_CODE, 0xFFFF, 0x9A, 0x0),
    ];
    for (index, entry) in entries.iter().enumerate() {
        write64(&mut mem, GDT + index as u32 * 8, *entry);
    }
    write32_at(&mut mem, TSS + 4, 0x17_0000);
    mem.data[TSS as usize + 8] = 0x10;
    mem.data[TSS as usize + 102] = 104;
    mem.data[PM_CODE as usize..PM_CODE as usize + code.len()].copy_from_slice(code);
    // Every exception goes to a handler that halts: F4 at PM_CODE + 0xF000 + n.
    for vector in 0..64u32 {
        let handler = PM_CODE + 0xF000 + vector;
        mem.data[handler as usize] = 0xF4;
        write64(&mut mem, IDT + vector * 8, idt_gate(0x08, handler, 0xE, 3));
    }
    cpu.gdtr = TableReg {
        base: GDT,
        limit: entries.len() as u32 * 8 - 1,
    };
    cpu.idtr = TableReg {
        base: IDT,
        limit: 64 * 8 - 1,
    };
    assert!(cpu.write_cr0(cpu.cr0 | 1).is_ok());
    cpu.op32 = true;
    assert!(cpu.jmp_far(&mut mem, 0x08, PM_CODE).is_ok());
    assert!(cpu.load_seg(&mut mem, SS, 0x10).is_ok());
    assert!(cpu.load_seg(&mut mem, DS, 0x10).is_ok());
    assert!(cpu.load_seg(&mut mem, ES, 0x10).is_ok());
    cpu.r[SP] = PM_STACK;
    assert!(cpu.load_tr(&mut mem, 0x28).is_ok());
    (cpu, mem)
}

fn run_until_halt(cpu: &mut Cpu, mem: &mut Mem) -> u32 {
    for _ in 0..1000 {
        if cpu.step(mem) == Step::Halted {
            return cpu.ip.wrapping_sub(1).wrapping_sub(PM_CODE);
        }
    }
    panic!("no halt: at {:04X}:{:08X}", cpu.sregs[CS], cpu.ip);
}

#[test]
fn real_mode_guest_enters_protected_mode_itself() {
    let (mut cpu, mut mem) = setup(&[
        0x0F, 0x01, 0x16, 0x00, 0x01, // LGDT [0100h]
        0x0F, 0x20, 0xC0, // MOV EAX,CR0
        0x0C, 0x01, // OR AL,1
        0x0F, 0x22, 0xC0, // MOV CR0,EAX
        0x66, 0xEA, 0x00, 0x00, 0x10, 0x00, 0x08, 0x00, // JMP FAR 08:00100000
    ]);
    write64(&mut mem, GDT + 8, gdt_entry(0, 0xFFFFF, 0x9A, 0xC));
    mem.data[0x2_0100..0x2_0102].copy_from_slice(&15u16.to_le_bytes());
    mem.data[0x2_0102..0x2_0106].copy_from_slice(&GDT.to_le_bytes());
    // A 32-bit instruction at the target.
    mem.data[PM_CODE as usize..PM_CODE as usize + 5].copy_from_slice(&[0xB8, 1, 2, 3, 4]);
    for _ in 0..6 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert!(cpu.protected_mode());
    assert_eq!(cpu.sregs[CS], 0x08);
    assert_eq!(cpu.r[AX], 0x0403_0201);
}

#[test]
fn null_and_limit_violations_fault_with_gp() {
    // MOV AX,0; MOV DS,AX; MOV EAX,[0]: #GP(0).
    let (mut cpu, mut mem) = protected(&[
        0x66, 0xB8, 0, 0, 0x8E, 0xD8, 0xA1, 0, 0, 0, 0,
    ]);
    assert_eq!(run_until_halt(&mut cpu, &mut mem), 0xF000 + 13);
    // A write past a limited segment.
    let (mut cpu, mut mem) = protected(&[
        0x66, 0xB8, 0x30, 0, 0x8E, 0xD8, 0xC6, 0x05, 0, 1, 0, 0, 0x55,
    ]);
    assert_eq!(run_until_halt(&mut cpu, &mut mem), 0xF000 + 13);
    assert_eq!(mem.data[0x5_0100], 0);
    // A bad selector.
    let (mut cpu, mut mem) = protected(&[0x66, 0xB8, 0xF8, 0, 0x8E, 0xD8]);
    assert_eq!(run_until_halt(&mut cpu, &mut mem), 0xF000 + 13);
}

#[test]
fn faults_push_an_error_code_and_restart_address() {
    let (mut cpu, mut mem) = protected(&[0x66, 0xB8, 0xF8, 0, 0x8E, 0xD8]);
    let sp = cpu.r[SP];
    run_until_halt(&mut cpu, &mut mem);
    let word = |mem: &Mem, at: u32| {
        u32::from_le_bytes(mem.data[at as usize..at as usize + 4].try_into().unwrap())
    };
    let frame = sp - 16;
    assert_eq!(cpu.r[SP], frame);
    assert_eq!(word(&mem, frame), 0xF8, "error code");
    assert_eq!(word(&mem, frame + 4), PM_CODE + 4, "the faulting MOV DS");
    assert_eq!(word(&mem, frame + 8), 0x08);
}

#[test]
fn software_int_through_interrupt_and_trap_gates() {
    let (mut cpu, mut mem) = protected(&[0xFB, 0xCD, 0x30, 0x90]);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(cpu.ip, PM_CODE + 0xF000 + 0x30);
    assert_eq!(cpu.flags() & FLAG_IF, 0, "interrupt gates clear IF");
    let (mut cpu, mut mem) = protected(&[0xFB, 0xCD, 0x31, 0x90]);
    write64(&mut mem, IDT + 0x31 * 8, idt_gate(0x08, PM_CODE + 0xF000, 0xF, 0));
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_ne!(cpu.flags() & FLAG_IF, 0, "trap gates leave IF");
}

/// Drops to ring 3 with an IRET and returns the state ready to run `user`.
fn in_ring3(user: &[u8]) -> (Cpu, Mem) {
    let (mut cpu, mut mem) = protected(&[
        0x6A, 0x23, // PUSH 23h (SS)
        0x68, 0x00, 0x00, 0x16, 0x00, // PUSH 160000h (ESP)
        0x68, 0x02, 0x02, 0x00, 0x00, // PUSH 202h (EFLAGS)
        0x6A, 0x1B, // PUSH 1Bh (CS)
        0x68, 0x00, 0x10, 0x10, 0x00, // PUSH 101000h (EIP)
        0xCF, // IRET
    ]);
    mem.data[PM_CODE as usize + 0x1000..PM_CODE as usize + 0x1000 + user.len()]
        .copy_from_slice(user);
    for _ in 0..6 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert_eq!(cpu.cpl, 3);
    assert_eq!(cpu.sregs[CS], 0x1B);
    assert_eq!(cpu.r[SP], 0x16_0000);
    (cpu, mem)
}

#[test]
fn iret_drops_to_ring_3_and_nulls_inner_segments() {
    let (cpu, _) = in_ring3(&[0x90]);
    assert_eq!(cpu.sregs[DS], 0, "ring 0 data segments don't survive");
}

#[test]
fn ring_3_interrupt_switches_to_the_tss_stack() {
    let (mut cpu, mut mem) = in_ring3(&[0xCD, 0x30]);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.cpl, 0);
    assert_eq!(cpu.r[SP], 0x17_0000 - 20);
    assert_eq!(cpu.sregs[SS], 0x10);
    let word = |at: u32| u32::from_le_bytes(mem.data[at as usize..at as usize + 4].try_into().unwrap());
    let frame = cpu.r[SP];
    assert_eq!(word(frame), PM_CODE + 0x1002);
    assert_eq!(word(frame + 4), 0x1B);
    assert_eq!(word(frame + 12), 0x16_0000);
    assert_eq!(word(frame + 16), 0x23);
}

#[test]
fn ring_3_cant_run_privileged_instructions() {
    for code in [&[0xF4][..], &[0xFA][..], &[0x0F, 0x20, 0xC0][..], &[0xEC][..]] {
        let (mut cpu, mut mem) = in_ring3(code);
        assert_eq!(cpu.step(&mut mem), Step::Ok, "{code:02X?}");
        assert_eq!(cpu.ip, PM_CODE + 0xF000 + 13, "{code:02X?}");
    }
    // An INT through a gate it can't use.
    let (mut cpu, mut mem) = in_ring3(&[0xCD, 0x30]);
    write64(&mut mem, IDT + 0x30 * 8, idt_gate(0x08, PM_CODE, 0xE, 0));
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.ip, PM_CODE + 0xF000 + 13);
}

#[test]
fn ring_3_popf_cant_change_if_or_iopl() {
    let (mut cpu, mut mem) = in_ring3(&[0x68, 0x00, 0x30, 0x00, 0x00, 0x9D]);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(cpu.flags() & FLAG_IOPL, 0);
    assert_ne!(cpu.flags() & FLAG_IF, 0);
}

#[test]
fn call_gate_changes_privilege_and_retf_returns() {
    // 48h: a call gate to 08:PM_CODE+0x2000, DPL 3, copying one dword.
    let gate = (((PM_CODE + 0x2000) as u64) & 0xFFFF)
        | (0x08u64 << 16)
        | (1u64 << 32)
        | (0xECu64 << 40)
        | (((PM_CODE + 0x2000) as u64 >> 16) << 48);
    let (mut cpu, mut mem) = in_ring3(&[
        0x68, 0x44, 0x33, 0x22, 0x11, // PUSH 11223344h
        0x9A, 0x00, 0x00, 0x00, 0x00, 0x4B, 0x00, // CALL FAR 4B:0
        0x90,
    ]);
    write64(&mut mem, GDT + 0x48, gate);
    cpu.gdtr.limit = 0x4F;
    // Inner routine: read the parameter, return popping it.
    mem.data[PM_CODE as usize + 0x2000..PM_CODE as usize + 0x2000 + 7]
        .copy_from_slice(&[0x8B, 0x44, 0x24, 0x08, 0xCA, 0x04, 0x00]);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(cpu.cpl, 0);
    assert_eq!(cpu.r[SP], 0x17_0000 - 20);
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0x1122_3344);
    cpu.step(&mut mem);
    assert_eq!(cpu.cpl, 3);
    assert_eq!(cpu.ip, PM_CODE + 0x100C);
    assert_eq!(cpu.r[SP], 0x16_0000);
}

#[test]
fn paging_translates_faults_and_sets_accessed_and_dirty() {
    let dir = 0x9_0000u32;
    let table = 0x9_1000u32;
    let (mut cpu, mut mem) = protected(&[
        0xA1, 0x00, 0x00, 0x40, 0x00, // MOV EAX,[400000h]
        0xA3, 0x04, 0x00, 0x40, 0x00, // MOV [400004h],EAX
        0xA1, 0x00, 0x10, 0x40, 0x00, // MOV EAX,[401000h]: not present
    ]);
    // Identity-map the first 4 MiB, plus 400000h -> 200000h.
    let identity = 0x9_2000u32;
    write32_at(&mut mem, dir, identity | 7);
    for page in 0..1024u32 {
        write32_at(&mut mem, identity + page * 4, (page << 12) | 7);
    }
    write32_at(&mut mem, dir + 4, table | 7);
    write32_at(&mut mem, table, 0x20_0000u32 & 0x1F_FFFF | 0x1F_0000 | 3);
    mem.data[0x1F_0000..0x1F_0004].copy_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE]);
    cpu.cr3 = dir;
    assert!(cpu.write_cr0(cpu.cr0 | 0x8000_0000).is_ok());
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0xDEAD_BEEF);
    assert_eq!(mem.data[table as usize] & 0x60, 0x20, "accessed, not dirty");
    cpu.step(&mut mem);
    assert_eq!(&mem.data[0x1F_0004..0x1F_0008], &[0xEF, 0xBE, 0xAD, 0xDE]);
    assert_eq!(mem.data[table as usize] & 0x60, 0x60, "dirty now");
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.cr2, 0x40_1000);
    assert_eq!(cpu.ip, PM_CODE + 0xF000 + 14);
}

#[test]
fn page_protection_stops_ring_3() {
    let dir = 0x9_0000u32;
    let identity = 0x9_2000u32;
    let (mut cpu, mut mem) = in_ring3(&[
        0xA1, 0x00, 0x50, 0x00, 0x00, // MOV EAX,[5000h]: supervisor page
    ]);
    // DS was nulled on the way down; use a flat one for the test's read.
    cpu.sregs[DS] = 0x23;
    cpu.seg[DS] = Desc2::flat();
    write32_at(&mut mem, dir, identity | 7);
    for page in 0..1024u32 {
        write32_at(&mut mem, identity + page * 4, (page << 12) | 7);
    }
    write32_at(&mut mem, identity + 5 * 4, (5 << 12) | 1);
    cpu.cr3 = dir;
    assert!(cpu.write_cr0(cpu.cr0 | 0x8000_0000).is_ok());
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.cr2, 0x5000);
    assert_eq!(cpu.ip, PM_CODE + 0xF000 + 14);
    let frame = cpu.r[SP] as usize;
    assert_eq!(mem.data[frame], 0x05, "user, read, present");
}

struct Desc2;
impl Desc2 {
    fn flat() -> Seg {
        Seg {
            base: 0,
            lo: 0,
            hi: u32::MAX,
            big: true,
            readable: true,
            writable: true,
            code: false,
            conforming: false,
            dpl: 3,
        }
    }
}

#[test]
fn hardware_task_switch_saves_and_loads_state() {
    let (mut cpu, mut mem) = protected(&[
        0xB8, 0x11, 0x00, 0x00, 0x00, // MOV EAX,11h
        0xEA, 0, 0, 0, 0, 0x38, 0, // JMP FAR 38:0
        0xF4,
    ]);
    // The incoming task.
    write32_at(&mut mem, TSS2 + 0x20, PM_CODE + 0x3000);
    write32_at(&mut mem, TSS2 + 0x24, 0x0202);
    write32_at(&mut mem, TSS2 + 0x28, 0x77);
    write32_at(&mut mem, TSS2 + 0x38, 0x17_8000);
    mem.data[TSS2 as usize + 0x4C] = 0x08;
    mem.data[TSS2 as usize + 0x50] = 0x10;
    mem.data[TSS2 as usize + 0x48] = 0x10;
    mem.data[TSS2 as usize + 0x54] = 0x10;
    cpu.step(&mut mem);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.tr.sel, 0x38);
    assert_eq!(cpu.ip, PM_CODE + 0x3000);
    assert_eq!(cpu.r[AX], 0x77);
    assert_eq!(cpu.r[SP], 0x17_8000);
    assert_ne!(cpu.cr0 & 8, 0, "TS is set");
    // The old task's state was saved, and its descriptor isn't busy any more.
    let word = |at: u32| u32::from_le_bytes(mem.data[at as usize..at as usize + 4].try_into().unwrap());
    assert_eq!(word(TSS + 0x20), PM_CODE + 12);
    assert_eq!(word(TSS + 0x28), 0x11);
    assert_eq!(mem.data[GDT as usize + 0x28 + 5] & 0x02, 0);
    assert_ne!(mem.data[GDT as usize + 0x38 + 5] & 0x02, 0);
}

#[test]
fn returning_to_real_mode_keeps_the_guest_running() {
    // In 16-bit protected code: clear PE and far jump back.
    let (mut cpu, mut mem) = protected(&[
        0xEA, 0x08, 0x00, 0x00, 0x00, 0x40, 0x00, // JMP FAR 40:8 (16-bit code)
        0x90,
        // At offset 8, 16-bit:
        0x0F, 0x20, 0xC0, // MOV EAX,CR0
        0x66, 0x83, 0xE0, 0xFE, // AND EAX,-2
        0x0F, 0x22, 0xC0, // MOV CR0,EAX
        0xEA, 0x00, 0x00, 0x00, 0x10, // JMP FAR 1000:0
    ]);
    mem.data[CODE as usize] = 0x90;
    for _ in 0..5 {
        assert_eq!(cpu.step(&mut mem), Step::Ok);
    }
    assert!(!cpu.protected_mode());
    assert_eq!(cpu.sregs[CS], 0x1000);
    assert_eq!(cpu.seg[CS].base, 0x1_0000);
    assert_eq!(cpu.ip, 0);
    assert_eq!(cpu.step(&mut mem), Step::Ok);
    assert_eq!(cpu.ip, 1);
}

#[test]
fn big_real_mode_keeps_a_large_limit() {
    let (mut cpu, mut mem) = protected(&[0x90]);
    assert!(cpu.write_cr0(cpu.cr0 & !1).is_ok());
    assert!(cpu.load_seg(&mut mem, DS, 0).is_ok());
    assert_eq!(cpu.seg[DS].hi, 0xFFFF_FFFF);
    assert_eq!(cpu.rd8(&mut mem, DS, 0x10_0000), Ok(0x90));
}

#[test]
fn triple_fault_reports_a_shutdown() {
    let (mut cpu, mut mem) = protected(&[0xCC]);
    cpu.idtr.limit = 0;
    let step = cpu.step(&mut mem);
    assert!(matches!(step, Step::Fault(_)), "got {step:?}");
}

#[test]
fn lar_lsl_verr_and_verw() {
    // LSL EAX,selector 30h; LAR ECX,selector 28h system; VERW 08h fails.
    let (mut cpu, mut mem) = protected(&[
        0x66, 0xBB, 0x30, 0x00, // MOV BX,30h
        0x0F, 0x03, 0xC3, // LSL EAX,EBX
        0x0F, 0x00, 0xEB, // VERW BX
        0x66, 0xBB, 0x08, 0x00, // MOV BX,8
        0x0F, 0x00, 0xEB, // VERW BX
    ]);
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(cpu.r[AX], 0xFF);
    assert_ne!(cpu.flags() & FLAG_ZF, 0);
    cpu.step(&mut mem);
    assert_ne!(cpu.flags() & FLAG_ZF, 0, "data is writable");
    cpu.step(&mut mem);
    cpu.step(&mut mem);
    assert_eq!(cpu.flags() & FLAG_ZF, 0, "code isn't");
}

/// Random bytes through the CPU must never panic, whatever they decode to, in
/// real or protected mode.
#[test]
fn random_code_never_panics() {
    let mut seed = 0x1234_5678_9ABC_DEFu64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..200 {
        let (mut cpu, mut mem) = if round % 2 == 0 {
            setup(&[])
        } else {
            protected(&[])
        };
        let base = if round % 2 == 0 { CODE } else { PM_CODE } as usize;
        for at in 0..0x400 {
            mem.data[base + at] = next() as u8;
        }
        for reg in cpu.r.iter_mut() {
            *reg = next() as u32;
        }
        for _ in 0..4000 {
            let flags = next() as u32 & 0xCD5;
            if cpu.step(&mut mem) == Step::Halted {
                cpu.halted = false;
                cpu.ip = next() as u32 & 0x3FF;
                let _ = flags;
            }
        }
    }
}
