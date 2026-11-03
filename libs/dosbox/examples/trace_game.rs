//! Steps a program and prints the first instructions, for debugging.
//!
//! `cargo run -p feos-dosbox --example trace_game --target x86_64-unknown-linux-gnu -- <dir> <exe> [steps]`

use std::fs;
use std::path::{Path, PathBuf};

use feos_dosbox::cpu::{Bus, Cpu};
use feos_dosbox::DosBox;

fn add_tree(machine: &mut DosBox, dir: &Path, parent: usize) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if let Some(child) = machine.bus.dos.disk.create_dir(parent, &name) {
                add_tree(machine, &path, child);
            }
        } else {
            let data = fs::read(&path).expect("read file");
            machine.bus.dos.disk.create_file(parent, &name, data);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("data/games/dos/wolfenstein-3d");
    let program = args.get(2).map(String::as_str).unwrap_or("WOLF3D.EXE");
    let steps: usize = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);

    let mut machine = DosBox::new(None);
    for _ in 0..16 {
        if machine.shell.batch.is_empty() {
            break;
        }
        machine.frame(0, 10_000);
    }
    let root = machine.bus.dos.disk.root;
    add_tree(&mut machine, Path::new(dir), root);
    if !machine.run(program) {
        eprintln!("cannot run {program}");
        std::process::exit(1);
    }
    println!(
        "entry CS:IP {:04X}:{:04X} SS:SP {:04X}:{:04X} DS={:04X} ES={:04X}",
        machine.cpu.sregs[1],
        machine.cpu.ip,
        machine.cpu.sregs[2],
        machine.cpu.r[4],
        machine.cpu.sregs[3],
        machine.cpu.sregs[0]
    );
    let probe = Cpu::physical(machine.cpu.sregs[1], 0x0C);
    let bytes = [
        machine.bus.mem.read8(probe),
        machine.bus.mem.read8(probe + 1),
        machine.bus.mem.read8(probe + 2),
        machine.bus.mem.read8(probe + 3),
    ];
    println!(
        "CS:000C = {:02X} {:02X} {:02X} {:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    );

    for index in 0..steps {
        let cs = machine.cpu.sregs[1];
        let ip = machine.cpu.ip;
        let addr = Cpu::physical(cs, ip);
        let mut bytes = [0u8; 8];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = machine.bus.mem.read8(addr.wrapping_add(offset as u32));
        }
        let opcode = bytes[0];
        println!(
            "{index:4} {cs:04X}:{ip:04X} {opcode:02X} [{:02X} {:02X} {:02X} {:02X}] AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} SP={:04X} DS={:04X} ES={:04X} FL={:04X}",
            bytes[1], bytes[2], bytes[3], bytes[4],
            machine.cpu.r[0], machine.cpu.r[3], machine.cpu.r[1], machine.cpu.r[2],
            machine.cpu.r[6], machine.cpu.r[7], machine.cpu.r[4],
            machine.cpu.sregs[3], machine.cpu.sregs[0], machine.cpu.flags
        );
        let step = machine.cpu.step(&mut machine.bus);
        if index == 11 || index == 12 {
            for off in [0x00u32, 0x69, 0x77, 0x0AF0] {
                let a = Cpu::physical(0x5637, off as u16);
                let b = Cpu::physical(0x2A37, off as u16);
                println!(
                    "  off {off:04X}: 5637={:02X}{:02X} 2A37={:02X}{:02X}",
                    machine.bus.mem.read8(a),
                    machine.bus.mem.read8(a + 1),
                    machine.bus.mem.read8(b),
                    machine.bus.mem.read8(b + 1)
                );
            }
        }
        if step != feos_dosbox::cpu::Step::Ok {
            println!("step returned {step:?}");
            break;
        }
    }
}
