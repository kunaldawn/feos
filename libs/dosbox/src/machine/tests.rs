//! Whole-machine tests: real `.COM` and `.EXE` images run through the CPU, the
//! IVT and the DOS kernel.

use super::*;
use crate::cpu::{AX, BX, CX, DX, ES, SP, SS};

fn drain_batch(dos: &mut DosBox) {
    for _ in 0..16 {
        if !dos.shell.in_batch() {
            break;
        }
        dos.frame(0, 10_000);
    }
}

/// A machine at the prompt with `program` as `TEST.COM`, already started.
fn boot(program: &[u8]) -> DosBox {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "TEST.COM", program.to_vec());
    assert!(dos.run("TEST.COM"));
    dos
}

fn frames(dos: &mut DosBox, count: usize) {
    for _ in 0..count {
        dos.frame(0, 100_000);
    }
}

fn type_line(dos: &mut DosBox, line: &str) {
    for byte in line.bytes() {
        dos.push_key(byte, 0);
    }
    dos.push_key(b'\r', 0x1C);
    dos.frame(0, 10_000);
    for _ in 0..256 {
        if !dos.is_running() && !dos.shell.in_batch() {
            break;
        }
        dos.frame(0, 100_000);
    }
}

/// A tiny assembler helper: a program that prints `text` and exits with `code`.
fn print_and_exit(text: &str, code: u8) -> Vec<u8> {
    let message_at = 0x100u16 + 12;
    let mut program = vec![
        0xBA,
        message_at as u8,
        (message_at >> 8) as u8, // MOV DX,message
        0xB4,
        0x09, // MOV AH,9
        0xCD,
        0x21, // INT 21h
        0xB8,
        code,
        0x4C, // MOV AX,4Cxx
        0xCD,
        0x21, // INT 21h
    ];
    program.extend_from_slice(text.as_bytes());
    program.push(b'$');
    program
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
    assert!(!dos.is_running());
    assert!(dos.text().contains("C:\\>"));
}

#[test]
fn subroutine_calling_int_21h_returns_to_its_caller() {
    // CALL F; MOV AH,2; MOV DL,'@'; INT 21h; JMP $; F: MOV AH,30h; INT 21h; RET
    let program = [
        0xE8, 0x08, 0x00, 0xB4, 0x02, 0xB2, b'@', 0xCD, 0x21, 0xEB, 0xFE, 0xB4, 0x30, 0xCD, 0x21,
        0xC3,
    ];
    let mut dos = boot(&program);
    assert!(!dos.text().contains('@'));
    frames(&mut dos, 32);
    assert!(dos.text().contains('@'), "subroutine never returned");
}

#[test]
fn int_leaves_the_stack_and_interrupt_flag_as_they_were() {
    // STI; INT 21h AH=30h; then spin.
    let program = [0xFB, 0xB4, 0x30, 0xCD, 0x21, 0xEB, 0xFE];
    let mut dos = boot(&program);
    let sp = dos.cpu.r[SP];
    frames(&mut dos, 2);
    assert_eq!(dos.cpu.r[SP], sp);
    assert_ne!(dos.cpu.flags() & cpu::FLAG_IF, 0);
}

#[test]
fn guest_can_hook_a_vector_and_chain_to_dos() {
    // Hook INT 21h with a handler that counts in CS:[counter], chain, then use it.
    #[rustfmt::skip]
    let program = [
        0xB8, 0x21, 0x35,             // MOV AX,3521h
        0xCD, 0x21,                   // INT 21h: ES:BX = old vector
        0x89, 0x1E, 0x40, 0x01,       // MOV [140h],BX
        0x8C, 0x06, 0x42, 0x01,       // MOV [142h],ES
        0xBA, 0x30, 0x01,             // MOV DX,130h
        0xB8, 0x21, 0x25,             // MOV AX,2521h
        0xCD, 0x21,                   // INT 21h
        0xB4, 0x02,                   // MOV AH,2
        0xB2, b'!',                   // MOV DL,'!'
        0xCD, 0x21,                   // INT 21h
        0xB8, 0x00, 0x4C,             // MOV AX,4C00h
        0xCD, 0x21,                   // INT 21h
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        // At 130h: INC WORD CS:[144h]; JMP FAR CS:[140h]
        0x2E, 0xFF, 0x06, 0x44, 0x01,
        0x2E, 0xFF, 0x2E, 0x40, 0x01,
    ];
    let mut dos = boot(&program);
    let psp = dos.cpu.sregs[cpu::CS];
    frames(&mut dos, 8);
    assert!(!dos.is_running());
    assert!(dos.text().contains('!'));
    assert_eq!(dos.bus.mem.read16(((psp as u32) << 4) + 0x144), 2);
    // The hook is gone again at the prompt.
    assert_eq!(dos.bus.mem.read16(0x21 * 4 + 2), ROM_SEGMENT);
}

#[test]
fn inverted_mouse_ranges_cannot_panic() {
    let program = [
        0xB8, 7, 0, 0xB9, 100, 0, 0xBA, 50, 0, 0xCD, 0x33, 0xB8, 4, 0, 0xCD, 0x33, 0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 8);
    assert_eq!(dos.bus.mouse.min_x, 50);
    assert_eq!(dos.bus.mouse.max_x, 100);
}

#[test]
fn a_segment_full_of_prefixes_returns_from_frame() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let program = [
            0xB8, 0x2E, 0x2E, 0x0E, 0x07, 0x31, 0xFF, 0xB9, 0x00, 0x80, 0xF3, 0xAB,
        ];
        let mut dos = boot(&program);
        frames(&mut dos, 8);
        tx.send(dos.text()).unwrap();
    });
    let text = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("frame() never returned");
    assert!(text.contains("Invalid opcode"), "{text}");
}

#[test]
fn inc_of_7fff_sets_overflow() {
    let program = [
        0xB8, 0xFF, 0x7F, 0x40, 0x70, 0x06, 0xB4, 0x02, 0xB2, b'#', 0xCD, 0x21, 0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 8);
    assert!(!dos.text().contains('#'), "INC 7FFFh did not set OF");
}

#[test]
fn a_directory_cannot_be_renamed_into_itself() {
    let mut dos = boot(&[0xEB, 0xFE]);
    let disk = &mut dos.bus.dos.disk;
    let root = disk.resolve(0, "\\").unwrap_or(0);
    let a = disk.create_dir(root, "A").unwrap();
    assert!(!disk.rename(a, a, "B"), "directory renamed into itself");
}

#[test]
fn rep_movsw_is_charged_against_the_budget() {
    // L: MOV CX,FFFFh; REP MOVSW; JMP L, with the data in a plain segment.
    let program = [0xB9, 0xFF, 0xFF, 0xF3, 0xA5, 0xEB, 0xF9];
    let mut dos = boot(&program);
    let start = dos.cycles();
    dos.frame(0, 50_000);
    let spent = dos.cycles() - start;
    assert!(spent >= 50_000, "spent {spent}");
    assert!(spent < 50_000 + 20_000, "overshot: spent {spent}");
}

#[test]
fn a_mode_set_costs_guest_time() {
    // L: MOV AX,0013h; INT 10h; JMP L
    let program = [0xB8, 0x13, 0x00, 0xCD, 0x10, 0xEB, 0xF9];
    let mut dos = boot(&program);
    let before = dos.cycles();
    dos.frame(0, 10);
    assert!(dos.cycles() - before < 50_000);
    let mut sets = 0;
    let start = dos.cycles();
    while dos.cycles() - start < 100_000 {
        dos.frame(0, 10_000);
        sets += 1;
        assert!(sets < 100);
    }
    assert!(
        sets < 15,
        "each frame must stop after a few mode sets: {sets}"
    );
}

// ----------------------------------------------------------------------
// Fuzzing
// ----------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Random memory, registers and INT calls must never panic, and a frame must
/// return within a bounded time.
#[test]
fn guest_fuzz_never_panics_and_frames_stay_bounded() {
    let mut rng = Rng(0xFEED_F00D_1234_5678);
    let started = std::time::Instant::now();
    let mut worst = std::time::Duration::ZERO;
    for round in 0..40 {
        let mut program = vec![0u8; 0x1000];
        for byte in program.iter_mut() {
            *byte = rng.next() as u8;
        }
        // Mix in many interrupt calls, with random register loads before them.
        for _ in 0..120 {
            let at = (rng.next() as usize) % (program.len() - 16);
            let vector =
                [0x10u8, 0x16, 0x1A, 0x21, 0x33, 0x15, 0x2F, 0x67][rng.next() as usize % 8];
            let function = rng.next() as u8;
            let snippet = [
                0xB8,
                rng.next() as u8,
                function,
                0xBB,
                rng.next() as u8,
                rng.next() as u8,
                0xB9,
                rng.next() as u8,
                rng.next() as u8,
                0xBA,
                rng.next() as u8,
                rng.next() as u8,
                0xCD,
                vector,
            ];
            program[at..at + snippet.len()].copy_from_slice(&snippet);
        }
        let mut dos = boot(&program);
        if round % 3 == 0 {
            for at in 0x1000u32..0x9_0000 {
                if rng.next() % 16 == 0 {
                    dos.bus.mem.write8(at, rng.next() as u8);
                }
            }
        }
        for frame in 0..40 {
            let begin = std::time::Instant::now();
            dos.frame(frame * 16, 20_000);
            worst = worst.max(begin.elapsed());
            if !dos.is_running() {
                // Restart so that exits don't end the round early.
                let root = dos.bus.dos.disk.root;
                dos.bus.dos.cwd = root;
                dos.bus.dos.disk.quota = 4 * 1024 * 1024;
                if dos.bus.dos.resolve("TEST.COM").is_none() {
                    dos.bus
                        .dos
                        .disk
                        .create_file(root, "TEST.COM", program.clone());
                }
                dos.run("TEST.COM");
            }
            let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
            if frame % 8 == 0 {
                let SystemBus { vga, mem, .. } = &mut dos.bus;
                vga.render(mem, &mut screen, false);
            }
        }
    }
    assert!(
        worst < std::time::Duration::from_secs(5),
        "a frame took {worst:?}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(600));
}

// ----------------------------------------------------------------------
// PIC, PIT and the tick chain
// ----------------------------------------------------------------------

#[test]
fn timer_interrupts_advance_the_bios_tick_count() {
    let mut dos = boot(&[0xFB, 0xEB, 0xFE]);
    // 18.2 Hz at the default speed is one tick per 165 000 instruction units.
    for _ in 0..20 {
        dos.frame(0, 100_000);
    }
    let ticks = dos.bus.mem.read32(BDA_BASE + 0x6C);
    assert!((10..=14).contains(&ticks), "ticks = {ticks}");
}

#[test]
fn int_1ch_chain_runs_a_guest_handler() {
    #[rustfmt::skip]
    let program = [
        0xBA, 0x20, 0x01,       // MOV DX,120h
        0xB8, 0x1C, 0x25,       // MOV AX,251Ch
        0xCD, 0x21,             // INT 21h
        0xFB,                   // STI
        0xEB, 0xFE,             // JMP $
        0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x2E, 0xFF, 0x06, 0x00, 0x02, // INC WORD CS:[200h]
        0xCF,                   // IRET
    ];
    let mut dos = boot(&program);
    let psp = dos.cpu.sregs[cpu::CS];
    for _ in 0..20 {
        dos.frame(0, 100_000);
    }
    let count = dos.bus.mem.read16(((psp as u32) << 4) + 0x200);
    assert!(count >= 10, "INT 1Ch ran {count} times");
    assert_eq!(count as u32, dos.bus.mem.read32(BDA_BASE + 0x6C));
}

#[test]
fn hlt_wakes_on_the_next_interrupt() {
    // STI; HLT; MOV AL,'H'... prints after waking.
    let program = [0xFB, 0xF4, 0xB4, 0x02, 0xB2, b'%', 0xCD, 0x21, 0xEB, 0xFE];
    let mut dos = boot(&program);
    dos.frame(0, 1000);
    assert!(!dos.text().contains('%'));
    let before = dos.cycles();
    for _ in 0..4 {
        dos.frame(0, 100_000);
    }
    assert!(dos.text().contains('%'));
    // Idle time still counted, or the clock would stop.
    assert!(dos.cycles() - before >= 300_000);
}

#[test]
fn cli_hlt_stays_halted_without_stalling_the_host() {
    let mut dos = boot(&[0xFA, 0xF4, 0xB4, 0x02, 0xB2, b'%', 0xCD, 0x21]);
    frames(&mut dos, 16);
    assert!(!dos.text().contains('%'));
    assert!(dos.is_running());
}

#[test]
fn reprogrammed_pit_rate_changes_the_tick_rate_and_can_be_read_back() {
    #[rustfmt::skip]
    let program = [
        0xB0, 0x34, 0xE6, 0x43,       // MOV AL,34h; OUT 43h,AL
        0xB0, 0x00, 0xE6, 0x40,       // low byte
        0xB0, 0x10, 0xE6, 0x40,       // high byte: divisor 1000h
        0xFB, 0xEB, 0xFE,             // STI; JMP $
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 10);
    let ticks = dos.bus.mem.read32(BDA_BASE + 0x6C);
    // 16 times the usual rate.
    assert!((80..=110).contains(&ticks), "ticks = {ticks}");
    dos.bus.cycles = dos.cpu.cycles;
    dos.bus.out8(0x43, 0x00);
    let low = dos.bus.in8(0x40);
    let high = dos.bus.in8(0x40);
    let count = u16::from_le_bytes([low, high]);
    assert!(count <= 0x1000 && count != 0, "count {count:#x}");
}

// ----------------------------------------------------------------------
// Keyboard hardware
// ----------------------------------------------------------------------

#[test]
fn int_16h_gets_keys_through_irq_1_and_the_bda_ring() {
    // MOV AH,0; INT 16h; MOV DL,AL; MOV AH,2; INT 21h; INT 20h
    let program = [
        0xB4, 0x00, 0xCD, 0x16, 0x88, 0xC2, 0xB4, 0x02, 0xCD, 0x21, 0xCD, 0x20,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert!(dos.is_running(), "waits for a key");
    let sp = dos.cpu.r[SP];
    frames(&mut dos, 2);
    assert_eq!(dos.cpu.r[SP], sp, "blocking doesn't leak stack");
    dos.push_key(b'q', 0x10);
    frames(&mut dos, 2);
    assert!(!dos.is_running());
    assert!(dos.text().contains('q'));
}

#[test]
fn a_game_hook_on_int_9_reads_make_and_break_codes_from_port_60h() {
    #[rustfmt::skip]
    let program = [
        0xBA, 0x20, 0x01,       // MOV DX,120h
        0xB8, 0x09, 0x25,       // MOV AX,2509h
        0xCD, 0x21,             // INT 21h
        0xFB,                   // STI
        0xEB, 0xFE,             // JMP $
        0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x50,                   // PUSH AX
        0x53,                   // PUSH BX
        0xE4, 0x60,             // IN AL,60h
        0x2E, 0x8B, 0x1E, 0x00, 0x02, // MOV BX,CS:[200h]
        0x2E, 0x88, 0x87, 0x02, 0x02, // MOV CS:[BX+202h],AL
        0x2E, 0xFF, 0x06, 0x00, 0x02, // INC WORD CS:[200h]
        0xB0, 0x20, 0xE6, 0x20, // MOV AL,20h; OUT 20h,AL
        0x5B, 0x58, 0xCF,       // POP BX; POP AX; IRET
    ];
    let mut dos = boot(&program);
    let base = (dos.cpu.sregs[cpu::CS] as u32) << 4;
    frames(&mut dos, 2);
    dos.push_key(b'a', 0x1E);
    frames(&mut dos, 8);
    assert_eq!(dos.bus.mem.read16(base + 0x200), 2);
    assert_eq!(dos.bus.mem.read8(base + 0x202), 0x1E);
    assert_eq!(dos.bus.mem.read8(base + 0x203), 0x9E);
    // The hook didn't chain, so nothing reached the BIOS ring.
    assert_eq!(crate::keyboard::bda_peek(&dos.bus.mem), None);
}

#[test]
fn extended_keys_reach_dos_as_zero_then_scan_code() {
    // AH=7 twice, storing the results in BL and BH.
    let program = [
        0xB4, 0x07, 0xCD, 0x21, 0x88, 0xC3, 0xB4, 0x07, 0xCD, 0x21, 0x88, 0xC7, 0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 1);
    dos.keyboard().push_key(crate::keyboard::Key::Up);
    frames(&mut dos, 2);
    assert_eq!(dos.cpu.r[BX] & 0xFFFF, 0x4800);
}

#[test]
fn ah_0c_flushes_once_and_then_waits_for_a_new_key() {
    // AX=0C01h: flush, then read with echo.
    let program = [0xB8, 0x01, 0x0C, 0xCD, 0x21, 0x88, 0xC2, 0xEB, 0xFE];
    let mut dos = boot(&program);
    dos.frame(0, 3);
    frames(&mut dos, 2);
    dos.push_key(b'z', 0x2C);
    frames(&mut dos, 2);
    assert_eq!(
        dos.cpu.r[DX] & 0xFF,
        b'z' as u32,
        "the key survived retries"
    );
}

#[test]
fn port_64h_and_92h_drive_the_a20_gate() {
    let mut dos = boot(&[0xEB, 0xFE]);
    assert!(!dos.bus.mem.a20());
    dos.bus.out8(0x64, 0xD1);
    dos.bus.out8(0x60, 0xDF);
    assert!(dos.bus.mem.a20());
    dos.bus.out8(0x92, 0x00);
    assert!(!dos.bus.mem.a20());
    dos.bus.out8(0x92, 0x02);
    assert_eq!(dos.bus.in8(0x92) & 2, 2);
    // The refresh bit toggles with guest time, so delay loops terminate.
    let first = dos.bus.in8(0x61) & 0x10;
    dos.bus.cycles += 1000;
    let mut changed = false;
    for step in 0..64 {
        dos.bus.cycles += 3 + step;
        if dos.bus.in8(0x61) & 0x10 != first {
            changed = true;
        }
    }
    assert!(changed);
}

// ----------------------------------------------------------------------
// DOS process model
// ----------------------------------------------------------------------

#[test]
fn psp_command_tail_starts_at_81h() {
    // Print the tail: DS:81h for [80h] bytes with AH=40h on stdout.
    #[rustfmt::skip]
    let program = [
        0xBB, 0x01, 0x00,       // MOV BX,1
        0xB4, 0x00,             // MOV AH,0
        0xA0, 0x80, 0x00,       // MOV AL,[80h]
        0x89, 0xC1,             // MOV CX,AX
        0xBA, 0x81, 0x00,       // MOV DX,81h
        0xB4, 0x40,             // MOV AH,40h
        0xCD, 0x21,             // INT 21h
        0xCD, 0x20,             // INT 20h
    ];
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "ARGS.COM", program.to_vec());
    type_line(&mut dos, "ARGS one two");
    assert!(dos.text().contains(" one two"), "{}", dos.text());
    assert!(!dos.is_running());
}

#[test]
fn environment_block_holds_the_program_path() {
    #[rustfmt::skip]
    let program = [
        0x8E, 0x06, 0x2C, 0x00, // MOV ES,[2Ch]
        0x31, 0xFF,             // XOR DI,DI
        0x31, 0xC0,             // XOR AX,AX
        0xFC,                   // CLD
        // Scan to the double NUL.
        0xB9, 0xFF, 0x7F,       // MOV CX,7FFFh
        0xF2, 0xAE,             // REPNE SCASB
        0x26, 0x80, 0x3D, 0x00, // CMP BYTE ES:[DI],0
        0x75, 0xF8,             // JNE back to the scan
        0x47,                   // INC DI past the second NUL
        0x26, 0x8B, 0x1D,       // MOV BX,ES:[DI]: the count word
        0x83, 0xC7, 0x02,       // ADD DI,2
        0x26, 0x8A, 0x15,       // MOV DL,ES:[DI]: the path's first letter
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert_eq!(dos.cpu.r[BX] & 0xFFFF, 1);
    assert_eq!(dos.cpu.r[DX] & 0xFF, b'C' as u32);
}

#[test]
fn resizing_the_program_block_and_allocating_work() {
    #[rustfmt::skip]
    let program = [
        0xB4, 0x48, 0xBB, 0xFF, 0xFF, 0xCD, 0x21, // AH=48h BX=FFFFh: fails, BX = largest
        0x89, 0x1E, 0x00, 0x02,                   // MOV [200h],BX
        0xB4, 0x4A, 0xBB, 0x00, 0x10, 0xCD, 0x21, // AH=4Ah to 4096 paragraphs
        0x9F, 0xA2, 0x02, 0x02,                   // LAHF; MOV [202h],AH (flags)
        0xB4, 0x48, 0xBB, 0x00, 0x01, 0xCD, 0x21, // AH=48h 256 paragraphs
        0xA3, 0x04, 0x02,                         // MOV [204h],AX
        0x8E, 0xC0, 0xB4, 0x49, 0xCD, 0x21,       // ES=AX; AH=49h
        0x9F, 0x88, 0x26, 0x06, 0x02,             // LAHF; MOV [206h],AH
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    let psp = dos.cpu.sregs[cpu::CS] as u32;
    let base = psp << 4;
    frames(&mut dos, 2);
    // A .COM takes the largest block, so nothing's left until it shrinks.
    assert_eq!(dos.bus.mem.read16(base + 0x200), 0);
    assert_eq!(dos.bus.mem.read8(base + 0x202) & 1, 0, "resize succeeded");
    let block = dos.bus.mem.read16(base + 0x204) as u32;
    assert_eq!(block, psp + 0x1000 + 1, "right after the program's MCB");
    assert_eq!(dos.bus.mem.read8(base + 0x206) & 1, 0, "free succeeded");
    // The MCB's size matches the resize.
    assert_eq!(dos.bus.mem.read16(((psp - 1) << 4) + 3), 0x1000);
}

/// Builds a minimal MZ file with the given image and header values.
fn make_exe(
    image: &[u8],
    relocs: &[(u16, u16)],
    min: u16,
    max: u16,
    cs: u16,
    ip: u16,
    ss: u16,
    sp: u16,
) -> Vec<u8> {
    let header_paras = ((0x1C + relocs.len() * 4).div_ceil(16)) as u16;
    let total = header_paras as usize * 16 + image.len();
    let mut file = vec![0u8; header_paras as usize * 16];
    file[0] = b'M';
    file[1] = b'Z';
    let put = |file: &mut Vec<u8>, at: usize, value: u16| {
        file[at..at + 2].copy_from_slice(&value.to_le_bytes());
    };
    put(&mut file, 2, (total % 512) as u16);
    put(&mut file, 4, total.div_ceil(512) as u16);
    put(&mut file, 6, relocs.len() as u16);
    put(&mut file, 8, header_paras);
    put(&mut file, 0x0A, min);
    put(&mut file, 0x0C, max);
    put(&mut file, 0x0E, ss);
    put(&mut file, 0x10, sp);
    put(&mut file, 0x14, ip);
    put(&mut file, 0x16, cs);
    put(&mut file, 0x18, 0x1C);
    for (index, (offset, segment)) in relocs.iter().enumerate() {
        put(&mut file, 0x1C + index * 4, *offset);
        put(&mut file, 0x1E + index * 4, *segment);
    }
    file.extend_from_slice(image);
    file
}

#[test]
fn exe_loads_with_relocations_and_its_stack() {
    #[rustfmt::skip]
    let mut image = vec![
        0xB8, 0x00, 0x00,       // MOV AX,seg data (relocated)
        0x8E, 0xD8,             // MOV DS,AX
        0xBA, 0x00, 0x00,       // MOV DX,0
        0xB4, 0x09,             // MOV AH,9
        0xCD, 0x21,             // INT 21h
        0xE8, 0x02, 0x00,       // CALL +2
        0xEB, 0xFE,             // JMP $
        0xB8, 0x07, 0x4C,       // MOV AX,4C07h
        0xCD, 0x21,             // INT 21h
    ];
    image.resize(0x20, 0);
    image.extend_from_slice(b"EXE says hi$");
    image.resize(0x40, 0);
    let exe = make_exe(&image, &[(1, 0)], 0x10, 0xFFFF, 0, 0, 3, 0x100);
    // The data segment is image + 2 paragraphs: patch the MOV.
    let mut exe = exe;
    let header = exe.len() - image.len();
    exe[header + 1] = 2;
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(root, "T.EXE", exe);
    assert!(dos.run("T.EXE"));
    let ss = dos.cpu.sregs[SS];
    assert_eq!(ss, dos.bus.dos.psp + 0x10 + 3);
    assert_eq!(dos.cpu.r[SP], 0x100);
    frames(&mut dos, 2);
    assert!(dos.text().contains("EXE says hi"), "{}", dos.text());
    assert!(!dos.is_running());
    assert_eq!(dos.shell.errorlevel, 7);
    assert!(dos.text().contains("code 7"));
}

#[test]
fn exe_min_alloc_too_large_fails_cleanly_and_max_alloc_is_honoured() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    let huge = make_exe(&[0xEB, 0xFE], &[], 0xF000, 0xFFFF, 0, 0, 0, 0x100);
    dos.bus.dos.disk.create_file(root, "HUGE.EXE", huge);
    assert!(!dos.run("HUGE.EXE"));
    assert!(dos.text().contains("too big"), "{}", dos.text());
    let small = make_exe(&[0xEB, 0xFE], &[], 0x10, 0x20, 0, 0, 0, 0x100);
    dos.bus.dos.disk.create_file(root, "SMALL.EXE", small);
    assert!(dos.run("SMALL.EXE"));
    let psp = dos.bus.dos.psp as u32;
    let size = dos.bus.mem.read16(((psp - 1) << 4) + 3);
    assert_eq!(size as u32, 0x10 + 1 + 0x20);
}

#[test]
fn exec_runs_a_child_and_returns_its_exit_code() {
    #[rustfmt::skip]
    let parent = [
        0xB4, 0x4A, 0xBB, 0x00, 0x10, 0xCD, 0x21,   // shrink to 64K
        0xBC, 0xFE, 0xFF,                            // MOV SP,FFFEh
        0x1E, 0x07,                                  // PUSH DS; POP ES
        0xBB, 0x40, 0x01,                            // MOV BX,140h (param block)
        0xBA, 0x60, 0x01,                            // MOV DX,160h (path)
        0xB8, 0x00, 0x4B,                            // MOV AX,4B00h
        0xCD, 0x21,                                  // INT 21h
        0x73, 0x02,                                  // JNC +2
        0xEB, 0xFE,                                  // JMP $ on failure
        0x0E, 0x1F,                                  // PUSH CS; POP DS
        0xB4, 0x4D, 0xCD, 0x21,                      // AH=4Dh
        0xA3, 0x80, 0x01,                            // MOV [180h],AX
        0x88, 0xC2, 0x80, 0xC2, 0x30,                // MOV DL,AL; ADD DL,'0'
        0xB4, 0x02, 0xCD, 0x21,                      // print it
        0xB8, 0x05, 0x4C, 0xCD, 0x21,                // exit 5
    ];
    let mut image = parent.to_vec();
    image.resize(0x40, 0);
    // The parameter block: env 0, tail at CS:190h, FCBs unused.
    let block = [0u16, 0x190, 0xFFFF, 0x5C, 0xFFFF, 0x6C, 0xFFFF];
    for word in block {
        image.extend_from_slice(&word.to_le_bytes());
    }
    image.resize(0x60, 0);
    image.extend_from_slice(b"CHILD.COM\0");
    image.resize(0x90, 0);
    image.extend_from_slice(&[3, b' ', b'h', b'i', 0x0D]);
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(root, "PARENT.COM", image);
    dos.bus
        .dos
        .disk
        .create_file(root, "CHILD.COM", print_and_exit("[child]", 3));
    assert!(dos.run("PARENT.COM"));
    let parent_psp = dos.bus.dos.psp;
    // The tail's segment must be the parent's CS.
    let base = (parent_psp as u32) << 4;
    dos.bus.mem.write16(base + 0x100 + 0x44, parent_psp);
    dos.bus.mem.write16(base + 0x100 + 0x48, parent_psp);
    dos.bus.mem.write16(base + 0x100 + 0x4C, parent_psp);
    frames(&mut dos, 8);
    let text = dos.text();
    assert!(text.contains("[child]3"), "{text}");
    assert!(!dos.is_running());
    assert_eq!(dos.shell.errorlevel, 5);
    assert_eq!(dos.bus.dos.psp, COMMAND_PSP);
}

#[test]
fn exit_closes_handles_and_frees_memory() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let free = dos.bus.dos.largest_block(&mut dos.bus.mem);
    #[rustfmt::skip]
    let program = [
        0xB4, 0x3C, 0x31, 0xC9, 0xBA, 0x10, 0x01, 0xCD, 0x21, // create A.TXT
        0xB4, 0x4C, 0xCD, 0x21,                               // exit, handle open
        0, 0, 0,
        b'A', b'.', b'T', b'X', b'T', 0,
    ];
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "T.COM", program.to_vec());
    assert!(dos.run("T.COM"));
    frames(&mut dos, 2);
    assert!(!dos.is_running());
    assert_eq!(dos.bus.dos.largest_block(&mut dos.bus.mem), free);
    let node = dos.bus.dos.resolve("A.TXT").expect("file");
    assert_eq!(dos.bus.dos.disk.nodes[node].open_count, 0);
    // Only the standard devices are still open.
    assert_eq!(dos.bus.dos.sft.iter().flatten().count(), 3);
}

/// Runs INT 21h with registers set directly, through the callback, at the prompt.
fn dos_call(dos: &mut DosBox, set: impl FnOnce(&mut Cpu)) -> (u16, bool) {
    // Calls run in a program that spins in place after an INT 21h.
    let root = dos.bus.dos.disk.root;
    if !dos.is_running() {
        dos.bus
            .dos
            .disk
            .create_file(root, "CALL.COM", vec![0xCD, 0x21, 0xEB, 0xFE]);
        assert!(dos.run("CALL.COM"));
    }
    let cs = dos.bus.dos.psp;
    dos.cpu.set_real_segment(cpu::CS, cs);
    dos.cpu.set_real_segment(cpu::DS, cs);
    dos.cpu.ip = 0x100;
    dos.cpu.set_flags(0x0202);
    set(&mut dos.cpu);
    dos.frame(0, 200);
    (dos.cpu.r[AX] as u16, dos.cpu.flags() & cpu::FLAG_CF != 0)
}

fn poke_path(dos: &mut DosBox, text: &str) -> u16 {
    let base = (dos.bus.dos.psp as u32) << 4;
    dos.bus.mem.copy_from(base + 0x400, text.as_bytes());
    dos.bus.mem.write8(base + 0x400 + text.len() as u32, 0);
    0x400
}

fn call_machine() -> DosBox {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "CALL.COM", vec![0xCD, 0x21, 0xEB, 0xFE]);
    assert!(dos.run("CALL.COM"));
    dos
}

#[test]
fn handle_semantics() {
    let mut dos = call_machine();
    let path = poke_path(&mut dos, "F.TXT");
    let (handle, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3C00;
        c.r[CX] = 0;
        c.r[DX] = path as u32;
    });
    assert!(!carry);
    // A duplicate shares the file position.
    let (dup, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4500;
        c.r[BX] = handle as u32;
    });
    assert!(!carry);
    let base = (dos.bus.dos.psp as u32) << 4;
    dos.bus.mem.copy_from(base + 0x500, b"abcd");
    let (written, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4000;
        c.r[BX] = handle as u32;
        c.r[CX] = 4;
        c.r[DX] = 0x500;
    });
    assert_eq!(written, 4);
    let (position, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4201;
        c.r[BX] = dup as u32;
        c.r[CX] = 0;
        c.r[DX] = 0;
    });
    assert_eq!(position, 4, "the dup moved too");
    // A zero-length write truncates at the position.
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x4200;
        c.r[BX] = handle as u32;
        c.r[CX] = 0;
        c.r[DX] = 2;
    });
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x4000;
        c.r[BX] = handle as u32;
        c.r[CX] = 0;
    });
    let node = dos.bus.dos.resolve("F.TXT").unwrap();
    assert_eq!(dos.bus.dos.disk.size(node), 2);
    // Closing one leaves the other working.
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x3E00;
        c.r[BX] = handle as u32;
    });
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4201;
        c.r[BX] = dup as u32;
        c.r[CX] = 0;
        c.r[DX] = 0;
    });
    assert!(!carry);
    // Read-only opens refuse writes.
    let path = poke_path(&mut dos, "F.TXT");
    let (read_only, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3D00;
        c.r[DX] = path as u32;
    });
    assert!(!carry);
    let (code, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4000;
        c.r[BX] = read_only as u32;
        c.r[CX] = 1;
        c.r[DX] = 0x500;
    });
    assert!(carry);
    assert_eq!(code, 5);
    // Device names work with any extension, and NUL swallows writes.
    let path = poke_path(&mut dos, "NUL.TXT");
    let (nul, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3D01;
        c.r[DX] = path as u32;
    });
    assert!(!carry);
    let (written, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4000;
        c.r[BX] = nul as u32;
        c.r[CX] = 9;
        c.r[DX] = 0x500;
    });
    assert_eq!(written, 9);
    // A huge seek and one-byte write fails without touching the heap.
    let (_, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4200;
        c.r[BX] = dup as u32;
        c.r[CX] = 0x7FFF;
        c.r[DX] = 0xFFFF;
    });
    let (written, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4000;
        c.r[BX] = dup as u32;
        c.r[CX] = 1;
        c.r[DX] = 0x500;
    });
    assert!(!carry);
    assert_eq!(written, 0);
}

#[test]
fn rmdir_only_takes_empty_directories_other_than_the_current_one() {
    let mut dos = call_machine();
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_dir(root, "SUB").unwrap();
    let path = poke_path(&mut dos, "README.TXT");
    let (code, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3A00;
        c.r[DX] = path as u32;
    });
    assert!(carry);
    assert_eq!(code, 5);
    assert!(dos.bus.dos.resolve("README.TXT").is_some());
    let path = poke_path(&mut dos, "SUB");
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x3B00;
        c.r[DX] = path as u32;
    });
    let path = poke_path(&mut dos, "\\SUB");
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3A00;
        c.r[DX] = path as u32;
    });
    assert!(carry, "the current directory can't go");
}

#[test]
fn getcwd_is_bounded_by_the_guest_buffer() {
    let mut dos = call_machine();
    let mut dir = dos.bus.dos.disk.root;
    for _ in 0..12 {
        dir = dos.bus.dos.disk.create_dir(dir, "LONGNAME.DIR").unwrap();
    }
    dos.bus.dos.cwd = dir;
    let base = (dos.bus.dos.psp as u32) << 4;
    dos.bus.mem.fill(base + 0x600, 128, 0xAA);
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4700;
        c.r[DX] = 0;
        c.r[crate::cpu::SI] = 0x600;
    });
    assert!(!carry);
    assert_eq!(dos.bus.mem.read8(base + 0x600 + 64), 0xAA, "buffer overrun");
    assert_eq!(dos.bus.mem.read8(base + 0x600 + 63), 0);
}

#[test]
fn nested_find_first_searches_keep_separate_state_in_their_dtas() {
    let mut dos = call_machine();
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(root, "NOEXT", vec![]).unwrap();
    let sub = dos.bus.dos.disk.create_dir(root, "SUB").unwrap();
    dos.bus.dos.disk.create_file(sub, "X.TXT", vec![]).unwrap();
    let base = (dos.bus.dos.psp as u32) << 4;
    let names = |dos: &mut DosBox, dta: u16, first_path: &str, attr: u32| -> Vec<String> {
        let path = poke_path(dos, first_path);
        dos_call(dos, |c| {
            c.r[AX] = 0x1A00;
            c.r[DX] = dta as u32;
        });
        let mut out = Vec::new();
        let (_, mut carry) = dos_call(dos, |c| {
            c.r[AX] = 0x4E00;
            c.r[CX] = attr;
            c.r[DX] = path as u32;
        });
        let mut guard = 0;
        while !carry && guard < 64 {
            let at = base + dta as u32 + 0x1E;
            let bytes = dos.bus.mem.read_cstring(0, at as u16, 13);
            let _ = bytes;
            let mut text = String::new();
            for index in 0..13 {
                let ch = dos.bus.mem.read8(at + index);
                if ch == 0 {
                    break;
                }
                text.push(ch as char);
            }
            out.push(text);
            carry = dos_call(dos, |c| c.r[AX] = 0x4F00).1;
            guard += 1;
        }
        out
    };
    let all = names(&mut dos, 0x700, "*.*", 0x10);
    assert!(all.iter().any(|name| name == "NOEXT"), "{all:?}");
    assert!(all.iter().any(|name| name == "SUB"), "{all:?}");
    let qm = names(&mut dos, 0x700, "????????.???", 0);
    assert!(qm.iter().any(|name| name == "README.TXT"));
    assert!(qm.iter().any(|name| name == "NOEXT"));
    assert!(!qm.iter().any(|name| name == "SUB"));

    // Two interleaved searches.
    let path = poke_path(&mut dos, "*.*");
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1A00;
        c.r[DX] = 0x700;
    });
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x4E00;
        c.r[CX] = 0;
        c.r[DX] = path as u32;
    });
    let first_name = dos.bus.mem.read8(base + 0x700 + 0x1E);
    let path = poke_path(&mut dos, "SUB\\*.*");
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1A00;
        c.r[DX] = 0x780;
    });
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x4E00;
        c.r[CX] = 0;
        c.r[DX] = path as u32;
    });
    assert!(!carry);
    assert_eq!(dos.bus.mem.read8(base + 0x780 + 0x1E), b'X');
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1A00;
        c.r[DX] = 0x700;
    });
    let (_, carry) = dos_call(&mut dos, |c| c.r[AX] = 0x4F00);
    assert!(!carry, "the outer search carries on");
    assert_ne!(dos.bus.mem.read8(base + 0x700 + 0x1E), first_name);
}

#[test]
fn fcb_create_write_read_and_delete() {
    let mut dos = call_machine();
    let base = (dos.bus.dos.psp as u32) << 4;
    let fcb = base + 0x800;
    dos.bus.mem.fill(fcb, 37, 0);
    dos.bus.mem.copy_from(fcb + 1, b"FCBFILE TXT");
    let (ax, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x1600;
        c.r[DX] = 0x800;
    });
    assert_eq!(ax & 0xFF, 0);
    dos.bus.mem.write16(fcb + 0x0E, 16);
    dos.bus.mem.copy_from(base + 0x900, b"sixteen bytes!!!");
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1A00;
        c.r[DX] = 0x900;
    });
    let (ax, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x1500;
        c.r[DX] = 0x800;
    });
    assert_eq!(ax & 0xFF, 0);
    let node = dos.bus.dos.resolve("FCBFILE.TXT").expect("created");
    assert_eq!(dos.bus.dos.disk.size(node), 16);
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1000;
        c.r[DX] = 0x800;
    });
    assert_eq!(dos.bus.dos.disk.nodes[node].open_count, 0);
    // Reopen and read it back with a random read.
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x0F00;
        c.r[DX] = 0x800;
    });
    assert_eq!(dos.bus.mem.read32(fcb + 0x10), 16);
    dos.bus.mem.write16(fcb + 0x0E, 16);
    dos.bus.mem.write32(fcb + 0x21, 0);
    dos.bus.mem.fill(base + 0x900, 16, 0);
    let (ax, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x2100;
        c.r[DX] = 0x800;
    });
    assert_eq!(ax & 0xFF, 0);
    assert_eq!(dos.bus.mem.read8(base + 0x900), b's');
    dos_call(&mut dos, |c| {
        c.r[AX] = 0x1000;
        c.r[DX] = 0x800;
    });
    let (ax, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x1300;
        c.r[DX] = 0x800;
    });
    assert_eq!(ax & 0xFF, 0);
    assert!(dos.bus.dos.resolve("FCBFILE.TXT").is_none());
}

#[test]
fn extended_open_temp_files_and_error_info() {
    let mut dos = call_machine();
    let path = poke_path(&mut dos, "NEW.TXT");
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x6C00;
        c.r[BX] = 2;
        c.r[CX] = 0;
        c.r[DX] = 0x01; // open only
        c.r[crate::cpu::SI] = path as u32;
    });
    assert!(carry, "doesn't exist");
    let (code, _) = dos_call(&mut dos, |c| c.r[AX] = 0x5900);
    assert_eq!(code, 2);
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x6C00;
        c.r[BX] = 2;
        c.r[CX] = 0;
        c.r[DX] = 0x10; // create if missing
        c.r[crate::cpu::SI] = path as u32;
    });
    assert!(!carry);
    assert_eq!(dos.cpu.r[CX] & 0xFFFF, 2, "created");
    let path = poke_path(&mut dos, "");
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x5A00;
        c.r[CX] = 0;
        c.r[DX] = path as u32;
    });
    assert!(!carry);
    assert_ne!(
        dos.bus.mem.read8(((dos.bus.dos.psp as u32) << 4) + 0x400),
        0
    );
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3800;
        c.r[DX] = 0xA00;
    });
    assert!(!carry);
    assert_eq!(
        dos.bus.mem.read8(((dos.bus.dos.psp as u32) << 4) + 0xA02),
        b'$'
    );
}

#[test]
fn date_reports_the_weekday_and_rtc_is_bcd() {
    let mut dos = call_machine();
    dos.bus.now_ms = 3 * 86_400_000;
    dos_call(&mut dos, |c| c.r[AX] = 0x2A00);
    // frame() replaced now_ms with 0: a Thursday.
    assert_eq!(dos.cpu.r[AX] & 0xFF, 4);
}

// ----------------------------------------------------------------------
// Pacing
// ----------------------------------------------------------------------

#[test]
fn paced_frames_run_guest_time_with_real_time_and_cap_it() {
    let mut dos = boot(&[0xFB, 0xEB, 0xFE]);
    dos.frame_paced(1000);
    let before = dos.cycles();
    dos.frame_paced(1016);
    let spent = dos.cycles() - before;
    assert!((47_000..=49_000).contains(&spent), "{spent}");
    // A long gap doesn't turn into a long frame.
    let before = dos.cycles();
    dos.frame_paced(9000);
    assert!(dos.cycles() - before <= MAX_PACED_MS * CYCLES_PER_MS as u64 + 1000);
    // The speed can be changed without disturbing the clock.
    let ticks = dos.bus.ticks();
    dos.set_cycles_per_ms(6000);
    assert!(dos.bus.ticks() >= ticks);
    assert_eq!(dos.cycles_per_ms(), 6000);
    let before = dos.cycles();
    dos.frame_paced(9016);
    assert!(dos.cycles() - before >= 90_000);
}

// ----------------------------------------------------------------------
// Sound
// ----------------------------------------------------------------------

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
fn adlib_check_from_a_guest_sees_timer_1_expire() {
    // The usual detection sequence, through the ports, with a delay loop.
    #[rustfmt::skip]
    let program = [
        0xBA, 0x88, 0x03,             // MOV DX,388h
        0xB0, 0x04, 0xEE, 0x42, 0xB0, 0x60, 0xEE, 0x4A, // reset timers
        0xB0, 0x04, 0xEE, 0x42, 0xB0, 0x80, 0xEE, 0x4A, // reset IRQ
        0xEC, 0x88, 0xC3,             // IN AL,DX; MOV BL,AL
        0xB0, 0x02, 0xEE, 0x42, 0xB0, 0xFF, 0xEE, 0x4A, // timer 1 = FFh
        0xB0, 0x04, 0xEE, 0x42, 0xB0, 0x21, 0xEE, 0x4A, // start it
        0xB9, 0x00, 0x10, 0xE2, 0xFE, // MOV CX,1000h; LOOP $
        0xEC, 0x88, 0xC7,             // IN AL,DX; MOV BH,AL
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert_eq!(dos.cpu.r[BX] & 0xE0, 0, "status starts clear");
    assert_eq!((dos.cpu.r[BX] >> 8) & 0xE0, 0xC0, "timer 1 expired");
}

#[test]
fn pc_speaker_plays_the_programmed_frequency() {
    let mut dos = DosBox::new(None);
    dos.set_audio_rate(48_000);
    // 1000 Hz: divisor 1193.
    dos.bus.out8(0x43, 0xB6);
    dos.bus.out8(0x42, (1193u16 & 0xFF) as u8);
    dos.bus.out8(0x42, (1193u16 >> 8) as u8);
    dos.bus.out8(0x61, 3);
    assert_eq!(dos.speaker_frequency(), Some(1000));
    dos.frame(100, 10_000);
    let samples = dos.take_audio();
    let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
    let edges = left
        .windows(2)
        .filter(|pair| (pair[0] > 0) != (pair[1] > 0))
        .count();
    // 100 ms of a 1 kHz square wave has about 200 edges: not one octave down.
    assert!((190..=210).contains(&edges), "{edges} edges");
    // A very high frequency doesn't overflow.
    dos.bus.out8(0x42, 2);
    dos.bus.out8(0x42, 0);
    dos.frame(116, 10_000);
    let _ = dos.take_audio();
}

#[test]
fn bell_is_a_square_wave() {
    let mut dos = DosBox::new(None);
    dos.set_audio_rate(48_000);
    dos.bus.vga.teletype(&mut dos.bus.mem, 0x07);
    dos.frame(60, 10_000);
    let samples = dos.take_audio();
    let beep = &samples[..2 * 48_000 * 40 / 1000];
    let left: Vec<i16> = beep.iter().step_by(2).copied().collect();
    assert!(
        left.iter().all(|sample| sample.abs() >= 1000),
        "square, not impulses"
    );
    let edges = left
        .windows(2)
        .filter(|pair| (pair[0] > 0) != (pair[1] > 0))
        .count();
    assert!((60..=80).contains(&edges), "{edges} edges at 880 Hz");
}

#[test]
fn sound_blaster_transfer_raises_its_irq_in_guest_time() {
    #[rustfmt::skip]
    let program = [
        0xBA, 0x30, 0x01, 0xB8, 0x0F, 0x25, 0xCD, 0x21, // INT 0Fh (IRQ 7) handler at 130h
        0xE4, 0x21, 0x24, 0x7F, 0xE6, 0x21,             // unmask IRQ 7
        0xFB, 0xEB, 0xFE,                               // STI; JMP $
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x2E, 0xFE, 0x06, 0x00, 0x02,                   // INC BYTE CS:[200h]
        0xB0, 0x20, 0xE6, 0x20, 0xCF,                   // EOI; IRET
    ];
    let mut dos = boot(&program);
    let base = (dos.cpu.sregs[cpu::CS] as u32) << 4;
    frames(&mut dos, 1);
    // A DMA channel and a short block at 11 kHz: 100 samples is about 9 ms.
    for (port, value) in [
        (0x0Cu16, 0u8),
        (0x0B, 0x49),
        (2, 0),
        (2, 0),
        (3, 99),
        (3, 0),
        (0x83, 3),
        (0x0A, 1),
    ] {
        dos.bus.out8(port, value);
    }
    for (port, value) in [
        (0x22C, 0xD1),
        (0x22C, 0x40),
        (0x22C, 165),
        (0x22C, 0x14),
        (0x22C, 99),
        (0x22C, 0),
    ] {
        dos.bus.out8(port, value);
    }
    assert_eq!(dos.bus.mem.read8(base + 0x200), 0);
    frames(&mut dos, 2);
    assert_eq!(dos.bus.mem.read8(base + 0x200), 1);
    assert_eq!(dos.bus.dma.channels[1].address, 100);
}

// ----------------------------------------------------------------------
// BIOS, EMS, XMS, mouse and joystick from the guest's side
// ----------------------------------------------------------------------

#[test]
fn bios_rom_probes_answer_like_an_at() {
    let mut dos = call_machine();
    assert_eq!(dos.bus.mem.read8(0xF_FFFE), 0xFC);
    assert_eq!(dos.bus.mem.read16(BDA_BASE + 0x13), 640);
    assert_eq!(dos.bus.mem.read16(BDA_BASE + 0x1A), 0x1E);
    // ROM can't be overwritten by the guest.
    let before = dos.bus.read8(0xF_0000);
    dos.bus.write8(0xF_0000, before ^ 0xFF);
    assert_eq!(dos.bus.read8(0xF_0000), before);
    assert_eq!(dos.bus.mem.read8(0xF_FFF0), 0xEA);
    // Joystick: nothing plugged in reads as all ones.
    assert_eq!(dos.bus.in8(0x201), 0xFF);
    let _ = dos.cpu.flags();
    let _ = dos_call(&mut dos, |c| c.r[AX] = 0x3000);
    assert_eq!(dos.cpu.r[AX] & 0xFF, 5);
}

#[test]
fn joystick_axes_time_out_and_buttons_read_low_when_pressed() {
    let mut dos = call_machine();
    dos.bus.joystick = Some(Joystick {
        axes: [Some(100), Some(100), None, None],
        buttons: 0b0001,
    });
    dos.bus.cycles = dos.cpu.cycles;
    dos.bus.out8(0x201, 0);
    let value = dos.bus.in8(0x201);
    assert_eq!(value & 0x03, 0x03, "both timers running");
    assert_eq!(value & 0x10, 0, "button A pressed reads low");
    assert_ne!(value & 0x20, 0);
    dos.bus.cycles += 3000;
    assert_eq!(dos.bus.in8(0x201) & 0x03, 0);
}

#[test]
fn xms_entry_is_found_through_int_2fh_and_callable() {
    #[rustfmt::skip]
    let program = [
        0xB8, 0x00, 0x43, 0xCD, 0x2F,       // AX=4300h
        0xA2, 0x00, 0x02,                   // MOV [200h],AL
        0xB8, 0x10, 0x43, 0xCD, 0x2F,       // AX=4310h: ES:BX = entry
        0x89, 0x1E, 0x02, 0x02,             // MOV [202h],BX
        0x8C, 0x06, 0x04, 0x02,             // MOV [204h],ES
        0xB4, 0x09, 0xBA, 0x00, 0x04,       // AH=9, DX=1024 KiB
        0xFF, 0x1E, 0x02, 0x02,             // CALL FAR [202h]
        0xA3, 0x06, 0x02,                   // MOV [206h],AX
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    let base = (dos.cpu.sregs[cpu::CS] as u32) << 4;
    let sp = dos.cpu.r[SP];
    frames(&mut dos, 2);
    assert_eq!(dos.bus.mem.read8(base + 0x200), 0x80);
    assert_eq!(dos.bus.mem.read16(base + 0x206), 1, "allocated");
    assert_eq!(dos.cpu.r[SP], sp, "no flags frame to pop");
    assert_ne!(dos.cpu.r[DX] & 0xFFFF, 0);
}

#[test]
fn ems_page_frame_reaches_expanded_memory_through_the_bus() {
    let mut dos = call_machine();
    let header = ((dos.bus.mem.read16(0x67 * 4 + 2) as u32) << 4) + 0x0A;
    let name: Vec<u8> = (0..8).map(|i| dos.bus.mem.read8(header + i)).collect();
    assert_eq!(&name, b"EMMXXXX0");
    let mut r = [0u32; 8];
    let mut sregs = [0u16; 6];
    let mut flags = 0;
    r[AX] = 0x4300;
    r[BX] = 1;
    let mut regs = CpuRegs {
        r: &mut r,
        sregs: &mut sregs,
        flags: &mut flags,
    };
    dos.bus.interrupt(0x67, &mut regs);
    let handle = regs.dx();
    regs.set_ax(0x4400);
    regs.set_bx(0);
    regs.set_dx(handle);
    dos.bus.interrupt(0x67, &mut regs);
    dos.bus.write8(0xD_0010, 0x5A);
    assert_eq!(dos.bus.read8(0xD_0010), 0x5A);
    assert_eq!(dos.bus.read8(0xD_4010), 0xFF, "unmapped");
    assert_eq!(dos.bus.mem.read8(0xD_0010), 0, "not plain memory");
}

#[test]
fn mouse_handler_runs_through_irq_12_and_returns() {
    #[rustfmt::skip]
    let program = [
        0x0E, 0x07,                   // PUSH CS; POP ES
        0xB8, 0x0C, 0x00,             // MOV AX,0Ch
        0xB9, 0x02, 0x00,             // MOV CX,2 (left press)
        0xBA, 0x20, 0x01,             // MOV DX,120h
        0xCD, 0x33,                   // INT 33h
        0xFB, 0xEB, 0xFE,             // STI; JMP $
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        // Handler: record AX and BX in CS:200h.
        0x2E, 0xA3, 0x00, 0x02,       // MOV CS:[200h],AX
        0x2E, 0x89, 0x1E, 0x02, 0x02, // MOV CS:[202h],BX
        0xCB,                         // RETF
    ];
    let mut dos = boot(&program);
    let base = (dos.cpu.sregs[cpu::CS] as u32) << 4;
    frames(&mut dos, 1);
    let sp = dos.cpu.r[SP];
    let ax_ip = dos.cpu.sregs[ES];
    let _ = ax_ip;
    dos.bus.mouse.set_button(0, true);
    frames(&mut dos, 1);
    assert_eq!(dos.bus.mem.read16(base + 0x200), 2);
    assert_eq!(dos.bus.mem.read16(base + 0x202), 1);
    assert!(!dos.bus.mouse.in_handler);
    assert_eq!(dos.cpu.r[SP], sp, "all the way back");
    assert_eq!(dos.bus.pics.slave.isr, 0, "acknowledged");
    assert_eq!(dos.bus.pics.master.isr, 0);
}

#[test]
fn mode_set_resets_the_range_but_other_video_calls_dont() {
    // Set a range, then AH=0Eh output; then a mode set.
    let program = [
        0xB8, 7, 0, 0xB9, 10, 0, 0xBA, 20, 0, 0xCD, 0x33, // range 10-20
        0xB8, 0x41, 0x0E, 0xCD, 0x10, // teletype
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert_eq!((dos.bus.mouse.min_x, dos.bus.mouse.max_x), (10, 20));
    let mut r = [0u32; 8];
    let mut sregs = [0u16; 6];
    let mut flags = 0;
    r[AX] = 0x0013;
    let mut regs = CpuRegs {
        r: &mut r,
        sregs: &mut sregs,
        flags: &mut flags,
    };
    dos.bus.interrupt(0x10, &mut regs);
    assert_eq!(
        (dos.bus.mouse.min_x, dos.bus.mouse.max_x),
        (0, 639),
        "mode 13h is 640 wide"
    );
    assert_eq!(dos.bus.mouse.max_y, 199);
}

// ----------------------------------------------------------------------
// VGA through the bus
// ----------------------------------------------------------------------

#[test]
fn mode_13h_memory_goes_through_the_vga_pipeline() {
    let program = [
        0xB8, 0x13, 0x00, 0xCD, 0x10, // MOV AX,13h; INT 10h
        0xB8, 0x00, 0xA0, 0x8E, 0xC0, // MOV AX,A000h; MOV ES,AX
        0x26, 0xC6, 0x06, 0x00, 0x00, 0x2A, // MOV ES:[0],2Ah
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert_eq!(dos.video_mode(), 0x13);
    assert_eq!(dos.bus.read8(0xA_0000), 0x2A);
    assert_eq!(dos.bus.mem.read8(0xA_0000), 0);
}

#[test]
fn dos_file_reads_land_in_planar_video_memory() {
    let mut dos = call_machine();
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "PIC.BIN", vec![0xFF; 8])
        .unwrap();
    let path = poke_path(&mut dos, "PIC.BIN");
    let (handle, _) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3D00;
        c.r[DX] = path as u32;
    });
    let SystemBus { vga, mem, .. } = &mut dos.bus;
    vga.set_mode(mem, 0x0D);
    let (_, carry) = dos_call(&mut dos, |c| {
        c.r[AX] = 0x3F00;
        c.r[BX] = handle as u32;
        c.r[CX] = 8;
        c.r[DX] = 0;
        c.set_real_segment(cpu::DS, 0xA000);
    });
    assert!(!carry);
    assert_eq!(dos.bus.mem.read8(0xA_0000), 0, "skipped plain memory");
    assert_eq!(dos.bus.vga.read_pixel(&dos.bus.mem, 0, 0), 0x0F);
}

// ----------------------------------------------------------------------
// Save states
// ----------------------------------------------------------------------

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
fn bad_save_states_are_refused_without_a_panic() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let text = dos.text();
    let state = dos.save_state();
    let mut rng = Rng(0xABCD_1234_5678_9001);
    // Not the format at all, truncated, and extended.
    assert!(!dos.load_state(b"FEOSDOS1"));
    assert!(!dos.load_state(&state[..state.len() / 2]));
    let mut longer = state.clone();
    longer.push(0);
    assert!(!dos.load_state(&longer));
    // Random corruption anywhere outside guest RAM must leave a machine that
    // still renders and runs, whether it was accepted or not.
    let header = 8 + 32 + 12 + 4 + 4 + 1 + 8 + 1 + 6 + 80;
    let after_ram = header + 0x10_0000;
    let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
    for _ in 0..200 {
        let mut bad = state.clone();
        for _ in 0..8 {
            let at = after_ram + rng.next() as usize % (bad.len() - after_ram);
            bad[at] = rng.next() as u8;
        }
        dos.load_state(&bad);
        let SystemBus { vga, mem, .. } = &mut dos.bus;
        vga.dirty = true;
        vga.render(mem, &mut screen, true);
        dos.frame(0, 1000);
    }
    assert!(dos.load_state(&state));
    assert_eq!(dos.text(), text);
}

// ----------------------------------------------------------------------
// The prompt
// ----------------------------------------------------------------------

#[test]
fn dir_command_lists_readme() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    type_line(&mut dos, "dir");
    assert!(dos.text().contains("README.TXT"));
}

#[test]
fn batch_language_runs_parameters_goto_if_for_call_and_shift() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    let disk = &mut dos.bus.dos.disk;
    disk.create_file(
        root,
        "T.BAT",
        b"@ECHO OFF\r\nECHO first=%1\r\nSHIFT\r\nECHO second=%1\r\nSET V=hello\r\nECHO v=%V%\r\n\
          IF \"%1\"==\"b\" GOTO yes\r\nECHO wrong\r\n:yes\r\nIF NOT EXIST NOPE.TXT ECHO nope\r\n\
          FOR %%X IN (p q) DO ECHO item%%X\r\nCALL SUB.BAT\r\nECHO back\r\nEXITCODE\r\n\
          IF ERRORLEVEL 3 ECHO level3\r\nIF ERRORLEVEL 4 ECHO level4\r\nECHO %%done\r\n"
            .to_vec(),
    );
    disk.create_file(root, "SUB.BAT", b"ECHO in-sub\r\n".to_vec());
    disk.create_file(root, "EXITCODE.COM", print_and_exit("", 3));
    type_line(&mut dos, "T a b");
    let text = dos.text();
    for wanted in [
        "first=a", "second=b", "v=hello", "nope", "itemp", "itemq", "in-sub", "back", "level3",
        "%done",
    ] {
        assert!(text.contains(wanted), "{wanted} missing in:\n{text}");
    }
    assert!(!text.contains("wrong"));
    assert!(!text.contains("level4"));
    assert!(dos
        .shell
        .env
        .iter()
        .any(|(key, value)| key == "V" && value == "hello"));
}

#[test]
fn environment_set_at_the_prompt_reaches_programs() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    #[rustfmt::skip]
    let program = [
        0x8E, 0x1E, 0x2C, 0x00, 0x31, 0xD2, // DS = env, DX = 0
        0xB9, 0x80, 0x00, 0xBB, 0x01, 0x00, // CX=128, BX=1
        0xB4, 0x40, 0xCD, 0x21,             // write the block
        0xCD, 0x20,
    ];
    dos.bus
        .dos
        .disk
        .create_file(root, "ENV.COM", program.to_vec());
    type_line(&mut dos, "SET FOO=bar");
    type_line(&mut dos, "ENV > ENV.TXT");
    let node = dos.bus.dos.resolve("ENV.TXT").expect("redirected");
    let data = String::from_utf8_lossy(&dos.bus.dos.disk.nodes[node].data).into_owned();
    assert!(data.contains("FOO=bar"), "{data:?}");
    // Standard output came back afterwards.
    type_line(&mut dos, "ECHO visible");
    assert!(dos.text().contains("visible"));
}

#[test]
fn builtin_output_can_be_redirected_and_appended() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    type_line(&mut dos, "ECHO one > A.TXT");
    type_line(&mut dos, "ECHO two >> A.TXT");
    assert!(!dos.text().contains("one\n"));
    let node = dos.bus.dos.resolve("A.TXT").expect("file");
    assert_eq!(dos.bus.dos.disk.nodes[node].data, b"one \r\ntwo \r\n");
}

#[test]
fn path_finds_programs_elsewhere() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    let bin = dos.bus.dos.disk.create_dir(root, "BIN").unwrap();
    dos.bus
        .dos
        .disk
        .create_file(bin, "HI.COM", print_and_exit("from-bin", 0));
    type_line(&mut dos, "HI");
    assert!(dos.text().contains("Bad command"));
    type_line(&mut dos, "PATH C:\\BIN");
    type_line(&mut dos, "HI");
    assert!(dos.text().contains("from-bin"));
}

#[test]
fn pause_waits_for_a_key() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(
        root,
        "P.BAT",
        b"@ECHO OFF\r\nPAUSE\r\nECHO after\r\n".to_vec(),
    );
    for byte in b"P\r" {
        dos.push_key(*byte, 0);
    }
    for _ in 0..8 {
        dos.frame(0, 1000);
    }
    assert!(dos.text().contains("Press any key"));
    assert!(!dos.text().contains("after"));
    dos.push_key(b' ', 0);
    for _ in 0..8 {
        dos.frame(0, 1000);
    }
    assert!(dos.text().contains("after"));
}

#[test]
fn mount_failures_clean_up_and_unmount_returns_space() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    assert!(dos.bus.dos.mount_zip_data(b"not a zip", "X").is_err());
    assert!(dos.bus.dos.resolve("X").is_none());
    let root = dos.bus.dos.disk.root;
    let dir = dos.bus.dos.disk.create_dir(root, "GAME").unwrap();
    dos.bus
        .dos
        .disk
        .create_file(dir, "BIG.DAT", vec![0; 1000])
        .unwrap();
    let used = dos.bus.dos.disk.total_bytes();
    type_line(&mut dos, "UNMOUNT GAME");
    assert!(dos.text().contains("Removed"));
    assert!(dos.bus.dos.resolve("GAME").is_none());
    assert_eq!(dos.bus.dos.disk.total_bytes(), used - 1000);
    type_line(&mut dos, "UNMOUNT GAME");
    assert!(dos.text().contains("UNMOUNT:"));
}

#[test]
fn state_after_a_killed_protected_mode_program_is_clean() {
    // A program that enters protected mode with no IDT and faults to shutdown.
    #[rustfmt::skip]
    let program = [
        0xFA,                         // CLI
        0x0F, 0x01, 0x1E, 0x20, 0x01, // LIDT [120h]
        0x0F, 0x01, 0x16, 0x20, 0x01, // LGDT [120h]
        0x0F, 0x20, 0xC0, 0x0C, 0x01, 0x0F, 0x22, 0xC0, // set PE
        0xCC,                         // INT3 with nothing to catch it
        0xEB, 0xFE,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, // a null limit and base
    ];
    let mut dos = boot(&program);
    frames(&mut dos, 2);
    assert!(!dos.is_running());
    assert!(dos.text().contains("CPU shutdown"), "{}", dos.text());
    assert!(!dos.cpu.protected_mode());
    // The prompt still works, and so do programs.
    assert!(dos.run("HELLO.COM"));
    frames(&mut dos, 2);
    assert!(dos.text().contains("Hello from"));
}

#[test]
fn fast_rep_paths_match_the_slow_ones() {
    // REP MOVSB with an overlap propagates; a non-overlapping one copies; REP
    // STOSW fills; and all of it stops at the video memory boundary correctly.
    #[rustfmt::skip]
    let program = [
        0x1E, 0x07,                   // PUSH DS; POP ES
        0xC6, 0x06, 0x00, 0x20, 0x41, // MOV BYTE [2000h],'A'
        0xBE, 0x00, 0x20,             // MOV SI,2000h
        0xBF, 0x01, 0x20,             // MOV DI,2001h
        0xB9, 0x00, 0x01,             // MOV CX,100h
        0xFC, 0xF3, 0xA4,             // CLD; REP MOVSB
        0xBF, 0x00, 0x30,             // MOV DI,3000h
        0xB8, 0x34, 0x12,             // MOV AX,1234h
        0xB9, 0x00, 0x40,             // MOV CX,4000h
        0xF3, 0xAB,                   // REP STOSW
        0xBE, 0x00, 0x30,             // MOV SI,3000h
        0xBF, 0x00, 0xB0,             // MOV DI,B000h
        0xB9, 0x00, 0x10,             // MOV CX,1000h
        0xF3, 0xA5,                   // REP MOVSW
        0xEB, 0xFE,
    ];
    let mut dos = boot(&program);
    let base = (dos.cpu.sregs[cpu::CS] as u32) << 4;
    frames(&mut dos, 2);
    assert_eq!(dos.bus.mem.read8(base + 0x2100), b'A', "overlap propagated");
    assert_eq!(dos.bus.mem.read16(base + 0x3000 + 0x7FFE), 0x1234);
    assert_eq!(dos.bus.mem.read16(base + 0xB000 + 0x1FFE), 0x1234);
    assert_eq!(dos.cpu.r[CX] & 0xFFFF, 0);
}

#[test]
fn machine_state_is_small_enough_to_move_on_a_kernel_stack() {
    assert!(
        core::mem::size_of::<DosBox>() < 16 * 1024,
        "{}",
        core::mem::size_of::<DosBox>()
    );
}

// ----------------------------------------------------------------------
// Real programs from the game library
// ----------------------------------------------------------------------

/// Starts a program from a game ZIP. Returns `None` when the library isn't there.
fn boot_game(zip: &str, program: &str) -> Option<DosBox> {
    let path = format!(
        "{}/../../data/games/dos/{zip}.zip",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(path).ok()?;
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    dos.bus.dos.mount_zip_data(&data, "GAME").expect("extract");
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.cwd = dos.bus.dos.disk.lookup(root, "GAME").expect("dir");
    assert!(dos.run(program), "{program} didn't start");
    Some(dos)
}

/// Runs until the program sets a video mode and draws something.
fn reaches_graphics(
    zip: &str,
    program: &str,
    mode: u8,
    frames: usize,
    budget: u32,
) -> Option<DosBox> {
    let mut dos = boot_game(zip, program)?;
    for _ in 0..frames {
        dos.frame(0, budget);
        assert!(dos.is_running(), "{program} exited:\n{}", dos.text());
    }
    assert_eq!(dos.video_mode(), mode, "{program}:\n{}", dos.text());
    let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
    let SystemBus { vga, mem, .. } = &mut dos.bus;
    vga.render(mem, &mut screen, false);
    let first = screen[0];
    assert!(
        screen.iter().any(|pixel| *pixel != first),
        "{program}: blank"
    );
    Some(dos)
}

#[test]
fn wolfenstein_gets_past_its_retrace_loop_to_the_title_screen() {
    reaches_graphics("wolfenstein-3d", "WOLF3D.EXE", 0x13, 200, 50_000);
}

#[test]
fn mario_bros_reaches_its_title_screen() {
    reaches_graphics("mario-bros-vga", "MARIO.EXE", 0x13, 100, 50_000);
}

#[test]
fn digger_reaches_its_title_screen() {
    reaches_graphics("digger-remastered", "DIGGER.EXE", 0x12, 100, 50_000);
}

#[test]
fn tank_wars_reaches_its_title_screen() {
    reaches_graphics("tank-wars", "BOMB.EXE", 0x13, 100, 50_000);
}

#[test]
fn galacta_reaches_its_title_screen() {
    reaches_graphics(
        "galacta-the-battle-for-saturn",
        "galacta.exe",
        0x13,
        300,
        50_000,
    );
}

#[test]
fn doom_runs_under_dos4gw_in_protected_mode() {
    if let Some(dos) = reaches_graphics("doom", "DOOM.EXE", 0x13, 400, 100_000) {
        assert!(dos.cpu.protected_mode());
    }
}

// ----------------------------------------------------------------------
// The DPMI host and the memory manager, from a client's side
// ----------------------------------------------------------------------

/// Runs a program to its end: the exit code and the screen. The sources of the
/// images are in `testdata`.
fn run_to_exit(program: &[u8], setup: impl FnOnce(&mut DosBox)) -> (u16, String) {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    setup(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus
        .dos
        .disk
        .create_file(root, "TEST.COM", program.to_vec());
    assert!(dos.run("TEST.COM"));
    for _ in 0..200 {
        if !dos.is_running() {
            break;
        }
        dos.frame(0, 100_000);
    }
    assert!(!dos.is_running(), "it should have ended by now: {}", dos.text());
    (dos.bus.dos.last_return, dos.text())
}

#[test]
fn a_16_bit_dpmi_client_uses_the_hosts_services() {
    let (code, text) = run_to_exit(include_bytes!("testdata/dpmi16.com"), |_| {});
    assert_eq!(code & 0xFF, 42, "stopped at stage {code}: {text}");
    assert!(text.contains("OK"), "{text}");
}

#[test]
fn a_32_bit_dpmi_client_uses_the_hosts_services() {
    let (code, text) = run_to_exit(include_bytes!("testdata/dpmi32.com"), |_| {});
    assert_eq!(code & 0xFF, 42, "stopped at stage {code}: {text}");
    assert!(text.contains("OK"), "{text}");
}

#[test]
fn the_host_is_not_offered_when_it_is_off() {
    let (code, _) = run_to_exit(include_bytes!("testdata/dpmi16.com"), |dos| {
        dos.bus.dpmi.mode = crate::dpmi::Mode::Off;
    });
    assert_eq!(code & 0xFF, 0xFE, "the client finds no host");
}

#[test]
fn a_client_ending_gives_its_memory_back() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let before = dos.bus.xms.free_kb();
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(
        root,
        "TEST.COM",
        include_bytes!("testdata/dpmi16.com").to_vec(),
    );
    assert!(dos.run("TEST.COM"));
    for _ in 0..200 {
        if !dos.is_running() {
            break;
        }
        dos.frame(0, 100_000);
    }
    assert!(!dos.bus.dpmi.active());
    assert_eq!(dos.bus.xms.free_kb(), before);
}

#[test]
fn programs_that_bring_an_extender_are_passed_over_on_auto() {
    assert!(crate::dpmi::brings_its_own_extender(b"MZ..DOS/4GW Professional"));
    assert!(crate::dpmi::brings_its_own_extender(b"..32rtm error: no.."));
    assert!(!crate::dpmi::brings_its_own_extender(b"MZ..go32 stub CWSDPMI.EXE"));
}

#[test]
fn a_vcpi_client_crosses_to_protected_mode_and_back() {
    let (code, text) = run_to_exit(include_bytes!("testdata/vcpi.com"), |dos| {
        dos.bus.emm.enabled = true;
    });
    assert_eq!(code & 0xFF, 42, "stopped at stage {code}: {text}");
}

#[test]
fn the_dpmi_host_works_under_the_memory_manager() {
    let (code, text) = run_to_exit(include_bytes!("testdata/dpmi32.com"), |dos| {
        dos.bus.emm.enabled = true;
    });
    assert_eq!(code & 0xFF, 42, "stopped at stage {code}: {text}");
}

#[test]
fn the_monitor_stops_a_program_taking_the_cpu() {
    let (code, text) = run_to_exit(include_bytes!("testdata/privileged.com"), |dos| {
        dos.bus.emm.enabled = true;
    });
    assert_ne!(code & 0xFF, 42);
    assert!(text.contains("EMM386: privileged"), "{text}");
}

#[test]
fn vcpi_is_not_there_without_the_memory_manager() {
    let program = [
        0xB8, 0x00, 0xDE, // MOV AX,DE00h
        0xCD, 0x67, // INT 67h
        0x88, 0xE0, // MOV AL,AH
        0xB4, 0x4C, // MOV AH,4Ch
        0xCD, 0x21, // INT 21h
    ];
    let (code, _) = run_to_exit(&program, |_| {});
    assert_ne!(code & 0xFF, 0, "AH says it isn't supported");
    let (code, _) = run_to_exit(&program, |dos| dos.bus.emm.enabled = true);
    assert_eq!(code & 0xFF, 0);
}

#[test]
fn a_real_mode_program_runs_in_v86_under_the_monitor() {
    let (code, text) = run_to_exit(&print_and_exit("Hi", 7), |dos| dos.bus.emm.enabled = true);
    assert_eq!(code & 0xFF, 7);
    assert!(text.contains("Hi"));
}

#[test]
fn stray_jumps_into_the_hosts_rom_are_harmless() {
    for offset in (0..0x40u16)
        .step_by(4)
        .chain([0x0400, 0x0420, 0x0800, 0x1000, 0x1084, 0x10C4, 0x1400, 0x14C4, 0x1800, 0x1834])
    {
        // JMP FAR F600:offset, then whatever follows.
        let program = [0xEA, offset as u8, (offset >> 8) as u8, 0x00, 0xF6];
        let mut dos = boot(&program);
        for _ in 0..8 {
            dos.frame(0, 10_000);
        }
        assert!(!dos.bus.dpmi.active());
    }
}

/// Enters protected mode, then calls `INT 31h` with made-up registers for ever.
const DPMI_FUZZER: &[u8] = include_bytes!("testdata/dpmifuzz.com");

#[test]
fn a_client_calling_int_31h_with_garbage_cannot_panic() {
    for seed in [0x1234u16, 0xBEEF, 0x8001] {
        let mut program = DPMI_FUZZER.to_vec();
        program[3..5].copy_from_slice(&seed.to_le_bytes());
        let mut dos = boot(&program);
        for _ in 0..60 {
            dos.frame(0, 20_000);
            if !dos.is_running() {
                break;
            }
        }
    }
}

#[test]
fn a_file_read_into_the_ems_frame_reaches_the_mapped_page() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    let root = dos.bus.dos.disk.root;
    dos.bus.dos.disk.create_file(root, "DATA.BIN", vec![0x5A; 16]);
    let program: &[u8] = &[
        // AH=43h: 1 page; AX=4400h: map it at physical page 3.
        0xB4, 0x43, 0xBB, 0x01, 0x00, 0xCD, 0x67, // MOV AH,43h; MOV BX,1; INT 67h
        0xB8, 0x03, 0x44, 0x31, 0xDB, 0xCD, 0x67, // MOV AX,4403h; XOR BX,BX; INT 67h
        // Open and read into D000:C000.
        0xB8, 0x00, 0x3D, 0xBA, 0x40, 0x01, 0xCD, 0x21, // MOV AX,3D00h; MOV DX,name; INT 21h
        0x89, 0xC3, // MOV BX,AX
        0xB8, 0x00, 0xD0, 0x1E, 0x8E, 0xD8, // MOV AX,D000h; PUSH DS; MOV DS,AX
        0xB4, 0x3F, 0xB9, 0x10, 0x00, 0xBA, 0x00, 0xC0, 0xCD, 0x21, // read 16 bytes to DS:C000
        // Read it back through the frame as the exit code.
        0xA0, 0x00, 0xC0, // MOV AL,[C000h]
        0x1F, // POP DS
        0xB4, 0x4C, 0xCD, 0x21, // exit
    ];
    let mut image = program.to_vec();
    image.resize(0x40, 0x90);
    image.extend_from_slice(b"DATA.BIN\0");
    dos.bus.dos.disk.create_file(root, "TEST.COM", image);
    assert!(dos.run("TEST.COM"));
    for _ in 0..16 {
        if !dos.is_running() {
            break;
        }
        dos.frame(0, 100_000);
    }
    assert_eq!(dos.bus.dos.last_return & 0xFF, 0x5A);
}

#[test]
fn demo_box_has_straight_sides() {
    let mut dos = DosBox::new(None);
    drain_batch(&mut dos);
    type_line(&mut dos, "DEMO");
    assert!(!dos.is_running());
    let text = dos.text();
    let lines: Vec<Vec<char>> = text.lines().map(|line| line.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|line| line.contains(&'\u{c9}'))
        .expect("no top-left corner");
    let column = |row: usize, ch: char| lines[row].iter().rposition(|&c| c == ch);
    let left = lines[top].iter().position(|&c| c == '\u{c9}');
    let right = column(top, '\u{bb}');
    assert!(right.is_some(), "no top-right corner");
    // The middle row's last side and the bottom corners line up with the top.
    assert_eq!(lines[top + 1].iter().position(|&c| c == '\u{ba}'), left);
    assert_eq!(column(top + 1, '\u{ba}'), right);
    assert_eq!(lines[top + 2].iter().position(|&c| c == '\u{c8}'), left);
    assert_eq!(column(top + 2, '\u{bc}'), right);
    assert!(lines[top + 1].iter().collect::<String>().contains("FEOS-DOS"));
}
