//! Prints every software interrupt a program makes in real mode, with registers.
//!
//! ```text
//! cargo run -p feos-dosbox --example trace_dos --release \
//!     --target x86_64-unknown-linux-gnu -- <zip> <program> [instructions] [vectors,in,hex]
//! ```

use std::fs;

use feos_dosbox::cpu::{Bus, Step};
use feos_dosbox::DosBox;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let zip = args.get(1).expect("zip name");
    let program = args.get(2).expect("program");
    let limit: u64 = args
        .get(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let vectors: Vec<u8> = args
        .get(4)
        .map_or(vec![0x21, 0x2F, 0x31, 0x15, 0x67], |list| {
            list.split(',')
                .filter_map(|v| u8::from_str_radix(v, 16).ok())
                .collect()
        });
    let data = fs::read(format!("data/games/dos/{zip}.zip")).expect("read zip");
    let mut m = DosBox::new(None);
    for _ in 0..16 {
        if !m.shell.in_batch() {
            break;
        }
        m.frame(0, 10_000);
    }
    m.bus.dos.mount_zip_data(&data, "GAME").expect("extract");
    let root = m.bus.dos.disk.root;
    m.bus.dos.cwd = m.bus.dos.disk.lookup(root, "GAME").expect("dir");
    assert!(m.run(program));
    for _ in 0..limit {
        let at = m.cpu.seg[1].base.wrapping_add(m.cpu.ip);
        let op = m.bus.read8(at);
        let vector = m.bus.read8(at.wrapping_add(1));
        if op == 0xCD && vectors.contains(&vector) {
            let path = m
                .bus
                .mem
                .read_cstring(m.cpu.sregs[3], m.cpu.r[2] as u16, 40);
            println!(
                "{:04X}:{:04X} INT {vector:02X} AX={:04X} BX={:04X} CX={:04X} DX={:04X} DS={:04X} ES={:04X} pm={} [{}]",
                m.cpu.sregs[1],
                m.cpu.ip,
                m.cpu.r[0] & 0xFFFF,
                m.cpu.r[3] & 0xFFFF,
                m.cpu.r[1] & 0xFFFF,
                m.cpu.r[2] & 0xFFFF,
                m.cpu.sregs[3],
                m.cpu.sregs[0],
                m.cpu.protected_mode(),
                String::from_utf8_lossy(&path),
            );
        }
        // One slice at a time, so interrupts and timers run as usual.
        match m.cpu.step(&mut m.bus) {
            Step::Ok => {}
            Step::Halted | Step::Blocked => {
                if m.bus.exit_requested {
                    println!("exit code {}", m.bus.exit_code);
                    return;
                }
                m.frame(0, 200);
            }
            Step::Fault(vector) => {
                println!("shutdown, vector {vector:02X}");
                return;
            }
        }
    }
}
