//! Runs a real DOS program from a host directory and saves screenshots.
//!
//! ```text
//! cargo run -p feos-dosbox --example run_game --target x86_64-unknown-linux-gnu -- \
//!     data/games/dos/wolfenstein-3d WOLF3D.EXE
//! ```
//!
//! The example is a host-side harness: it copies the directory into the
//! emulated `C:` RAM drive, runs the program, feeds a few keys and writes
//! `/tmp/feos-dos-frame-*.ppm` snapshots.

use std::fs;
use std::path::{Path, PathBuf};

use feos_dosbox::DosBox;
use feos_gfx::Canvas;

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

fn save(machine: &mut DosBox, name: &str) {
    let mut pixels = vec![0u32; 640 * 400];
    let mut canvas = Canvas::new(&mut pixels, 640, 400, 640).expect("canvas");
    machine.render(&mut canvas);
    let mut ppm = format!("P6\n640 400\n255\n").into_bytes();
    for pixel in pixels {
        ppm.push((pixel >> 16) as u8);
        ppm.push((pixel >> 8) as u8);
        ppm.push(pixel as u8);
    }
    let path = format!("/tmp/feos-dos-frame-{name}.ppm");
    fs::write(&path, ppm).expect("write screenshot");
    println!("wrote {path}");
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

    if !machine.run(program) {
        eprintln!("cannot run {program} from {dir}");
        std::process::exit(1);
    }
    println!(
        "started {program}: mode {:02X}h, running {}",
        machine.video_mode(),
        machine.is_running()
    );

    for frame in 0..2400u64 {
        let now = frame * 16;
        match frame {
            90 => machine.push_key(0x0D, 0x1C),
            150 => machine.push_key(b' ', 0x39),
            240 => machine.push_key(0x0D, 0x1C),
            360 => machine.push_key(0x1B, 0x01),
            480 => machine.push_key(0x0D, 0x1C),
            _ => {}
        }
        machine.frame(now, 120_000);
        if matches!(
            frame,
            60 | 90 | 120 | 150 | 240 | 360 | 480 | 720 | 1200 | 2000
        ) {
            save(&mut machine, &format!("{frame:04}"));
            println!(
                "frame {frame}: mode {:02X}h, cycles {}, running {}",
                machine.video_mode(),
                machine.cycles(),
                machine.is_running()
            );
        }
        if !machine.is_running() {
            println!("program exited at frame {frame}");
            let probe = feos_dosbox::cpu::Cpu::physical(0x5637, 0x69);
            println!("5637:0069 = {:02X}", machine.bus.mem.read8(probe));
            let start = machine.cpu.trace_index.wrapping_sub(32);
            for i in start..machine.cpu.trace_index {
                let e = machine.cpu.trace[i % feos_dosbox::cpu::TRACE_LEN];
                println!("  {:04X}:{:04X} {:02X} AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} BP={:04X} DS={:04X} ES={:04X} FL={:04X}",
                    e.cs, e.ip, e.opcode, e.ax, e.bx, e.cx, e.dx, e.si, e.di, e.bp, e.ds, e.es, e.flags);
            }
            save(&mut machine, "exit");
            break;
        }
    }
}
