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
//!
//! `GAME_SCRIPT` runs input at given frames, as words of `frame:action`:
//! `key=x` taps a key (a character, or `Enter`, `Esc`, `Space`, `F1`..`F10`,
//! `Up`, `Down`, `Left`, `Right`), `down=x` and `up=x` press and release one,
//! `mouse=x,y` moves the pointer, `button=n,0|1` sets a button, and `shot=file`
//! writes a screenshot then.
//!
//! `GAME_SB=2|pro|16` picks the Sound Blaster, and `GAME_AUDIO=1` generates sound
//! and reports its level on each side. `GAME_DPMI=on|off|auto` and `GAME_EMM386=1` set
//! what the prompt's `DPMI` and `EMM386` set, `GAME_DELETE=NAME` removes a file from the
//! game first, `GAME_VGA=n` reports the palette every n frames, `GAME_GREY=1` forces a
//! grey palette for the screenshot and `GAME_DUMP=file` writes all of memory at the end.

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

/// The ASCII and scan code for a key name.
fn key_code(name: &str) -> (u8, u8) {
    match name {
        "Enter" => (0x0D, 0x1C),
        "Esc" => (0x1B, 0x01),
        "Space" => (0x20, 0x39),
        "Up" => (0, 0x48),
        "Down" => (0, 0x50),
        "Left" => (0, 0x4B),
        "Right" => (0, 0x4D),
        _ if name.starts_with('F') && name.len() > 1 => {
            let n: u8 = name[1..].parse().unwrap_or(1);
            (0, 0x3A + n)
        }
        _ => {
            let ch = name.chars().next().unwrap_or(' ');
            (
                feos_dosbox::keyboard::ascii_for_char(ch).unwrap_or(0),
                feos_dosbox::keyboard::scan_for_char(ch),
            )
        }
    }
}

fn run_script(machine: &mut DosBox, frame: u32, script: &[(u32, String)], ups: &mut Vec<(u32, u8)>) {
    ups.retain(|(at, scan)| {
        if *at <= frame {
            machine.keyboard().key_up(*scan);
            false
        } else {
            true
        }
    });
    for (at, action) in script {
        if *at != frame {
            continue;
        }
        let (verb, value) = action.split_once('=').unwrap_or((action, ""));
        match verb {
            "key" => {
                let (ascii, scan) = key_code(value);
                machine.keyboard().key_down(ascii, scan);
                ups.push((frame + 6, scan));
            }
            "down" => {
                let (ascii, scan) = key_code(value);
                machine.keyboard().key_down(ascii, scan);
            }
            "up" => {
                let (_, scan) = key_code(value);
                machine.keyboard().key_up(scan);
            }
            "mouse" => {
                let (x, y) = value.split_once(',').unwrap_or(("0", "0"));
                machine.bus.mouse.move_to(x.parse().unwrap_or(0), y.parse().unwrap_or(0));
            }
            "button" => {
                let (n, on) = value.split_once(',').unwrap_or(("0", "0"));
                machine.bus.mouse.set_button(n.parse().unwrap_or(0), on == "1");
            }
            "shot" => {
                save_ppm(machine, value);
                println!("frame {frame}: wrote {value}");
            }
            _ => println!("unknown action {action}"),
        }
    }
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
    if let Ok(card) = std::env::var("GAME_SB") {
        let model = match card.as_str() {
            "2" => feos_dosbox::sb::Model::Sb2,
            "pro" => feos_dosbox::sb::Model::SbPro,
            _ => feos_dosbox::sb::Model::Sb16,
        };
        machine.bus.sb = feos_dosbox::sb::SoundBlaster::with_model(model);
        machine.shell.set_env_public("BLASTER", model.blaster(), &mut machine.bus.dos, &mut machine.bus.mem);
    }
    // GAME_DPMI=on|off|auto and GAME_EMM386=1 set what `DPMI` and `EMM386` do.
    match std::env::var("GAME_DPMI").as_deref() {
        Ok("on") => machine.bus.dpmi.mode = feos_dosbox::dpmi::Mode::On,
        Ok("off") => machine.bus.dpmi.mode = feos_dosbox::dpmi::Mode::Off,
        _ => {}
    }
    machine.bus.emm.enabled = std::env::var("GAME_EMM386").is_ok();
    let audio = std::env::var("GAME_AUDIO").is_ok();
    if audio {
        machine.set_audio_rate(48_000);
    }
    let mut level = [0u64; 2];
    let mut apart = 0u64;
    let mut audio_frames = 0u64;
    let count = machine
        .bus
        .dos
        .mount_zip_data(&data, "GAME")
        .expect("extract");
    drop(data);
    let root = machine.bus.dos.disk.root;
    machine.bus.dos.cwd = machine.bus.dos.disk.lookup(root, "GAME").expect("dir");
    println!("extracted {count} files");
    // GAME_DELETE=NAME removes a file first, to show a title doesn't need it.
    if let Ok(name) = std::env::var("GAME_DELETE") {
        let cwd = machine.bus.dos.cwd;
        let node = machine.bus.dos.disk.lookup(cwd, &name).expect("file to delete");
        assert!(machine.bus.dos.disk.remove(node), "delete");
    }
    if !machine.run_with_args(program, tail) {
        println!("did not start: {}", machine.text());
        return;
    }

    let mut now = 0u64;
    let mut last_mode = machine.video_mode();
    let mut key_at = 0;
    let started = std::time::Instant::now();
    let mut exited_at = None;
    let script: Vec<(u32, String)> = std::env::var("GAME_SCRIPT").map_or(Vec::new(), |text| {
        text.split_whitespace()
            .filter_map(|word| {
                let (frame, action) = word.split_once(':')?;
                Some((frame.parse().ok()?, action.to_string()))
            })
            .collect()
    });
    let mut ups = Vec::new();
    // GAME_VGA=n prints how much of the palette is lit every n frames.
    let vga_watch: u32 = std::env::var("GAME_VGA").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    for frame in 0..frames {
        run_script(&mut machine, frame, &script, &mut ups);
        machine.frame(now, budget);
        now += (budget / machine.cycles_per_ms()) as u64;
        if audio {
            for pair in machine.take_audio().chunks_exact(2) {
                level[0] += pair[0].unsigned_abs() as u64;
                level[1] += pair[1].unsigned_abs() as u64;
                apart += u64::from(pair[0] != pair[1]);
                audio_frames += 1;
            }
        }
        if machine.video_mode() != last_mode {
            last_mode = machine.video_mode();
            println!("frame {frame}: video mode {last_mode:02X}h");
        }
        if !machine.is_running() {
            exited_at = Some(frame);
            break;
        }
        if vga_watch != 0 && frame % vga_watch == 0 {
            use feos_dosbox::cpu::Bus;
            let bus = &mut machine.bus;
            let mut lit = 0;
            bus.out8(0x3C7, 0);
            for _ in 0..768 {
                lit += usize::from(bus.in8(0x3C9) != 0);
            }
            println!("frame {frame}: DAC {lit}/768, ticks {}", bus.mem.read32(0x46C));
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
    if audio {
        println!(
            "audio: {} frames, mean level L {} R {}, {} frames differ between sides",
            audio_frames,
            level[0] / audio_frames.max(1),
            level[1] / audio_frames.max(1),
            apart,
        );
    }
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
    let base = machine.cpu.seg[1].base.wrapping_add(machine.cpu.ip);
    let code: Vec<String> = (0..16)
        .map(|i| {
            let byte = machine.cpu.read_linear(&mut machine.bus, base + i);
            byte.map_or("--".to_string(), |byte| format!("{byte:02X}"))
        })
        .collect();
    println!("code {}", code.join(" "));
    println!(
        "EAX={:08X} EBX={:08X} ECX={:08X} EDX={:08X} ESI={:08X} EDI={:08X} EBP={:08X} ESP={:08X} ES={:04X} SS={:04X} DS={:04X} flags={:08X}",
        machine.cpu.r[0], machine.cpu.r[3], machine.cpu.r[1], machine.cpu.r[2],
        machine.cpu.r[6], machine.cpu.r[7], machine.cpu.r[5], machine.cpu.r[4],
        machine.cpu.sregs[0], machine.cpu.sregs[2], machine.cpu.sregs[3], machine.cpu.flags(),
    );
    println!(
        "PIC imr {:02X}/{:02X} isr {:02X}/{:02X} irr {:02X}/{:02X} ticks {}",
        machine.bus.pics.master.imr,
        machine.bus.pics.slave.imr,
        machine.bus.pics.master.isr,
        machine.bus.pics.slave.isr,
        machine.bus.pics.master.irr,
        machine.bus.pics.slave.irr,
        machine.bus.mem.read32(0x46C),
    );
    let mut samples = std::collections::BTreeMap::new();
    if machine.is_running() {
        for _ in 0..4000 {
            machine.frame(now, 37);
            *samples
                .entry((machine.cpu.sregs[1], machine.cpu.ip & !0xF))
                .or_insert(0u32) += 1;
        }
        let mut top: Vec<_> = samples.into_iter().collect();
        top.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for ((cs, ip), count) in top.into_iter().take(6) {
            println!("  {cs:04X}:{ip:08X} x{count}");
        }
    }
    // The VGA's state, through its ports, for a screen that looks wrong.
    {
        use feos_dosbox::cpu::Bus;
        let bus = &mut machine.bus;
        let mut lit = 0;
        bus.out8(0x3C7, 0);
        for _ in 0..768 {
            lit += usize::from(bus.in8(0x3C9) != 0);
        }
        let crtc = |bus: &mut feos_dosbox::SystemBus, index: u8| {
            bus.out8(0x3D4, index);
            bus.in8(0x3D5)
        };
        let start = ((crtc(bus, 0x0C) as u16) << 8) | crtc(bus, 0x0D) as u16;
        println!(
            "vga: {lit}/768 DAC components lit, CRTC start {start:04X}, pel mask {:02X}",
            bus.in8(0x3C6)
        );
    }
    // GAME_DUMP=file writes all of memory, for a disassembler.
    if let Ok(path) = std::env::var("GAME_DUMP") {
        let bytes: Vec<u8> = (0..0x100_0000u32).map(|at| machine.bus.mem.read8(at)).collect();
        fs::write(path, bytes).expect("write dump");
    }
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
    if std::env::var("GAME_GREY").is_ok() {
        use feos_dosbox::cpu::Bus;
        machine.bus.out8(0x3C8, 0);
        for index in 0..256u32 {
            let level = (index % 64) as u8;
            machine.bus.out8(0x3C9, level);
            machine.bus.out8(0x3C9, level);
            machine.bus.out8(0x3C9, level);
        }
    }
    if let Some(path) = out {
        save_ppm(&mut machine, path);
        println!("wrote {path}");
    }
}
