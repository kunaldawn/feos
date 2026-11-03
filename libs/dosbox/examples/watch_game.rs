//! Watches segment-register changes in the LZEXE decompressor.
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

fn dump(machine: &DosBox) {
    let start = machine.cpu.trace_index.wrapping_sub(24);
    for i in start..machine.cpu.trace_index {
        let e = machine.cpu.trace[i % feos_dosbox::cpu::TRACE_LEN];
        println!("  {:04X}:{:04X} {:02X} AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} BP={:04X} DS={:04X} ES={:04X} FL={:04X}",
            e.cs, e.ip, e.opcode, e.ax, e.bx, e.cx, e.dx, e.si, e.di, e.bp, e.ds, e.es, e.flags);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("data/games/dos/wolfenstein-3d");
    let program = args.get(2).map(String::as_str).unwrap_or("WOLF3D.EXE");
    let mut machine = DosBox::new(None);
    for _ in 0..16 {
        if machine.shell.batch.is_empty() {
            break;
        }
        machine.frame(0, 10_000);
    }
    let root = machine.bus.dos.disk.root;
    add_tree(&mut machine, Path::new(dir), root);
    assert!(machine.run(program));
    let mut last_es = 0u16;
    let mut last_ds = 0u16;
    for index in 0..2_000_000u64 {
        let step = machine.cpu.step(&mut machine.bus);
        if step != Step::Ok {
            println!("step {index} returned {step:?}");
            dump(&machine);
            break;
        }
        if index == 2000 {
            let a = 0x10100u32;
            print!("out@1010: ");
            for i in 0..32 {
                print!("{:02X} ", machine.bus.mem.read8(a + i));
            }
            println!();
            println!(
                "at 2000 ES={:04X} DI={:04X} DS={:04X} SI={:04X}",
                machine.cpu.sregs[0], machine.cpu.r[7], machine.cpu.sregs[3], machine.cpu.r[6]
            );
        }
        if index < 100 {
            continue;
        }
        let es = machine.cpu.sregs[0];
        let ds = machine.cpu.sregs[3];
        if index == 100 {
            last_es = es;
            last_ds = ds;
            println!("baseline at 100: DS={:04X} ES={:04X}", ds, es);
            continue;
        }
        if es != last_es || ds != last_ds {
            println!(
                "step {index}: DS {:04X}->{:04X} ES {:04X}->{:04X}",
                last_ds, ds, last_es, es
            );
            if es == 0x514B || ds == 0x5615 {
                dump(&machine);
                break;
            }
        }
        last_es = es;
        last_ds = ds;
    }
}
