//! Steps Wolf3D until the LZEXE final jump and dumps the patched header.
use feos_dosbox::cpu::{Cpu, Step};
use feos_dosbox::DosBox;
use std::fs;
use std::path::{Path, PathBuf};

fn add_tree(machine: &mut DosBox, dir: &Path, parent: usize) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if let Some(child) = machine.bus.dos.disk.create_dir(parent, &name) {
                add_tree(machine, &path, child);
            }
        } else {
            machine
                .bus
                .dos
                .disk
                .create_file(parent, &name, fs::read(&path).expect("file"));
        }
    }
}

fn main() {
    let mut machine = DosBox::new(None);
    for _ in 0..16 {
        if machine.shell.batch.is_empty() {
            break;
        }
        machine.frame(0, 10_000);
    }
    let root = machine.bus.dos.disk.root;
    add_tree(
        &mut machine,
        Path::new("data/games/dos/wolfenstein-3d"),
        root,
    );
    assert!(machine.run("WOLF3D.EXE"));
    for index in 0..5_000_000u64 {
        let cs = machine.cpu.sregs[1];
        let ip = machine.cpu.ip;
        if ip == 0x155 && cs == 0x5637 {
            println!(
                "hit final jump at step {index}: CS:IP {:04X}:{:04X}",
                cs, ip
            );
            println!("AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} BP={:04X} SP={:04X} SS={:04X} DS={:04X} ES={:04X}",
                machine.cpu.r[0], machine.cpu.r[3], machine.cpu.r[1], machine.cpu.r[2], machine.cpu.r[6], machine.cpu.r[7], machine.cpu.r[5], machine.cpu.r[4], machine.cpu.sregs[2], machine.cpu.sregs[3], machine.cpu.sregs[0]);
            let base = Cpu::physical(cs, 0);
            print!("header:");
            for i in 0..32 {
                print!(" {:02X}", machine.bus.mem.read8(base + i));
            }
            println!();
            let ptr = machine.bus.mem.read16(base) as u32
                | ((machine.bus.mem.read16(base + 2) as u32) << 16);
            println!(
                "far ptr at CS:0000 = {:04X}:{:04X}",
                ptr & 0xFFFF,
                ptr >> 16
            );
            let start = Cpu::physical(0x1010, 0);
            let mut image = Vec::new();
            for i in 0..0x2000 {
                image.push(machine.bus.mem.read8(start + i));
            }
            std::fs::write("/tmp/wolf_decompressed.bin", image).unwrap();
            println!("wrote /tmp/wolf_decompressed.bin");
            return;
        }
        let step = machine.cpu.step(&mut machine.bus);
        if step != Step::Ok {
            println!("step {index} returned {step:?} at {:04X}:{:04X}", cs, ip);
            return;
        }
    }
    println!("never reached final jump");
}
