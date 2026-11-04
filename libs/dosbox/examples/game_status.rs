//! Runs a program from a game ZIP in `data/games/dos` and reports how far it gets.
//!
//! ```text
//! cargo run -p feos-dosbox --example game_status --release \
//!     --target x86_64-unknown-linux-gnu -- <zip> <program> [frames] [budget] [out.ppm] [keys] [args]
//! ```
//!
//! A frame is `budget` instruction units, 50 000 by default, and the guest clock
//! advances with them. `keys` is text fed in at a steady rate after a third of the
//! frames, with `~` for Enter and `^` for Escape.

use std::fs;

use feos_dosbox::{DosBox, Vga};
use feos_gfx::Canvas;

fn save_ppm(machine: &mut DosBox, path: &str) {
    let mut pixels = vec![0u32; 640 * 480];
    let mut canvas = Canvas::new(&mut pixels, 640, 480, 640).expect("canvas");
    machine.bus.vga.dirty = true;
    machine.render(&mut canvas);
    let mut ppm = b"P6\n640 480\n255\n".to_vec();
    for pixel in pixels {
        ppm.push((pixel >> 16) as u8);
        ppm.push((pixel >> 8) as u8);
        ppm.push(pixel as u8);
    }
    fs::write(path, ppm).expect("write screenshot");
}

fn distinct_colours(vga: &mut Vga, machine_screen: &mut [u32], mem: &feos_dosbox::Memory) -> usize {
    vga.dirty = true;
    vga.render(mem, machine_screen, true);
    let mut seen = std::collections::HashSet::new();
    for pixel in machine_screen.iter() {
        seen.insert(*pixel);
    }
    seen.len()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let zip = args.get(1).expect("zip name");
    let program = args.get(2).expect("program");
    let frames: u32 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(2000);
    let budget: u32 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(50_000);
    let out = args.get(5).filter(|v| !v.is_empty() && *v != "-");
    let keys: Vec<u8> = args.get(6).map_or(Vec::new(), |v| v.as_bytes().to_vec());
    let tail = args.get(7).map_or("", String::as_str);

    let data = fs::read(format!("data/games/dos/{zip}.zip")).expect("read zip");
    let mut machine = DosBox::new(None);
    for _ in 0..16 {
        if !machine.shell.in_batch() {
            break;
        }
        machine.frame(0, 10_000);
    }
    let count = machine
        .bus
        .dos
        .mount_zip_data(&data, "GAME")
        .expect("extract");
    drop(data);
    let root = machine.bus.dos.disk.root;
    machine.bus.dos.cwd = machine.bus.dos.disk.lookup(root, "GAME").expect("dir");
    println!("extracted {count} files");
    if !machine.run_with_args(program, tail) {
        println!("did not start: {}", machine.text());
        return;
    }

    let mut now = 0u64;
    let mut last_mode = machine.video_mode();
    let mut key_at = 0;
    let started = std::time::Instant::now();
    let mut exited_at = None;
    for frame in 0..frames {
        machine.frame(now, budget);
        now += (budget / machine.cycles_per_ms()) as u64;
        if machine.video_mode() != last_mode {
            last_mode = machine.video_mode();
            println!("frame {frame}: video mode {last_mode:02X}h");
        }
        if !machine.is_running() {
            exited_at = Some(frame);
            break;
        }
        if frame > frames / 3 && frame % 40 == 0 && key_at < keys.len() {
            let key = keys[key_at];
            key_at += 1;
            match key {
                b'~' => machine.push_key(0x0D, 0x1C),
                b'^' => machine.push_key(0x1B, 0x01),
                _ => machine.push_key(key, 0),
            }
        }
    }
    let elapsed = started.elapsed();
    println!(
        "{} after {:?}, {} units in {:.2}s ({:.1} M/s)",
        match exited_at {
            Some(frame) => format!("exited at frame {frame}"),
            None => "still running".to_string(),
        },
        machine.cycles(),
        machine.cycles(),
        elapsed.as_secs_f64(),
        machine.cycles() as f64 / elapsed.as_secs_f64() / 1e6
    );
    println!(
        "mode {:02X}h {}x{} pmode={} cpl={} CS:IP {:04X}:{:08X} cr0={:08X} A20={}",
        machine.video_mode(),
        machine.bus.vga.width,
        machine.bus.vga.height,
        machine.cpu.protected_mode(),
        machine.cpu.cpl,
        machine.cpu.sregs[1],
        machine.cpu.ip,
        machine.cpu.cr0,
        machine.bus.mem.a20(),
    );
    let mut screen = vec![0u32; 640 * 480];
    let colours = {
        let bus = &mut machine.bus;
        distinct_colours(&mut bus.vga, &mut screen, &bus.mem)
    };
    println!("{colours} distinct colours on screen");
    let text = machine.text();
    for line in text.lines().filter(|line| !line.trim().is_empty()).take(25) {
        println!("| {line}");
    }
    if let Some(path) = out {
        save_ppm(&mut machine, path);
        println!("wrote {path}");
    }
}
