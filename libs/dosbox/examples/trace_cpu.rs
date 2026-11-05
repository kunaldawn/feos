//! Single-steps a program from a game ZIP and logs control register changes, with
//! the instructions leading up to each one. It runs far slower than `game_status`.
//!
//! ```text
//! cargo run -p feos-dosbox --example trace_cpu --release \
//!     --target x86_64-unknown-linux-gnu -- <zip> <program> <max units> [history] [from unit]
//! ```

use std::collections::VecDeque;
use std::fs;

use feos_dosbox::DosBox;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let zip = args.get(1).expect("zip name");
    let program = args.get(2).expect("program");
    let max: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20_000_000);
    let history: usize = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(24);
    let from: u64 = args.get(5).and_then(|v| v.parse().ok()).unwrap_or(0);

    let data = fs::read(format!("data/games/dos/{zip}.zip")).expect("read zip");
    let mut m = DosBox::new(None);
    for _ in 0..16 {
        if !m.shell.in_batch() {
            break;
        }
        m.frame(0, 10_000);
    }
    m.bus.dos.mount_zip_data(&data, "GAME").expect("extract");
    drop(data);
    let root = m.bus.dos.disk.root;
    m.bus.dos.cwd = m.bus.dos.disk.lookup(root, "GAME").expect("dir");
    m.bus.emm.enabled = std::env::var("GAME_EMM386").is_ok();
    assert!(m.run_with_args(program, ""));

    let mut ring: VecDeque<String> = VecDeque::with_capacity(history + 1);
    let mut last_cr0 = m.cpu.cr0;
    let mut last_cr3 = m.cpu.cr3;
    // TRACE_DUMP=<unit>:<file> writes the first 16 MiB of physical memory.
    let mut dump = std::env::var("TRACE_DUMP").ok().and_then(|v| {
        let (unit, file) = v.split_once(':')?;
        Some((unit.parse::<u64>().ok()?, file.to_string()))
    });
    // TRACE_INT=10,21 prints services by their callback vector.
    let services: Vec<u8> = std::env::var("TRACE_INT").map_or(Vec::new(), |list| {
        list.split(',')
            .filter_map(|v| u8::from_str_radix(v, 16).ok())
            .collect()
    });
    let trace_host = std::env::var("TRACE_HOST").is_ok();
    // TRACE_STOP=cs:ip (in hex) prints the history when that's reached, and stops.
    let stop = std::env::var("TRACE_STOP").ok().and_then(|v| {
        let (cs, ip) = v.split_once(':')?;
        Some((u16::from_str_radix(cs, 16).ok()?, u32::from_str_radix(ip, 16).ok()?))
    });
    while m.cycles() < max && m.is_running() {
        if stop == Some((m.cpu.sregs[1], m.cpu.ip)) && m.cycles() >= from {
            let here = m.cpu.seg[1].base.wrapping_add(m.cpu.ip);
            let code: Vec<String> = (0..16)
                .map(|i| format!("{:02X}", m.cpu.read_linear(&mut m.bus, here + i).unwrap_or(0xEE)))
                .collect();
            println!(
                "reached at {} (base {:08X}): {} DS={:04X} ES={:04X} SS={:04X} FS={:04X} GS={:04X} EAX={:08X} EBX={:08X} ECX={:08X} EDX={:08X} ESI={:08X} EDI={:08X}",
                m.cycles(), m.cpu.seg[1].base, code.join(" "), m.cpu.sregs[3], m.cpu.sregs[0], m.cpu.sregs[2],
                m.cpu.sregs[4], m.cpu.sregs[5], m.cpu.r[0], m.cpu.r[3], m.cpu.r[1], m.cpu.r[2], m.cpu.r[6], m.cpu.r[7]
            );
            for line in &ring {
                println!("  {line}");
            }
            println!(
                "gdtr {:?} idtr {:?} tr {:?} ldtr {:?} cpl {} SS {:?} flags {:08X}",
                m.cpu.gdtr, m.cpu.idtr, m.cpu.tr, m.cpu.ldtr, m.cpu.cpl, m.cpu.seg[2], m.cpu.flags()
            );
            let sp = m.cpu.r[4];
            let base = m.cpu.seg[2].base;
            let stack: Vec<String> = (0..16)
                .map(|i| {
                    let at = base.wrapping_add(sp).wrapping_add(i * 4);
                    let bytes: Vec<u8> = (0..4)
                        .map(|b| m.cpu.read_linear(&mut m.bus, at + b).unwrap_or(0xEE))
                        .collect();
                    format!("{:08X}", u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                })
                .collect();
            println!("stack {}", stack.join(" "));
            return;
        }
        // TRACE_HOST=1 prints every host call of the DPMI host.
        if trace_host {
            let at = m.cpu.seg[1].base.wrapping_add(m.cpu.ip);
            if (0xF_6000..0xF_8000).contains(&at) {
                let sp = m.cpu.seg[2].base.wrapping_add(m.cpu.r[4]);
                let word = |m: &DosBox, i: u32| m.bus.mem.read32(sp + i * 4);
                println!(
                    "[{}] host {:04X}:{:04X} pm={} AX={:04X} BX={:04X} CX={:04X} DX={:04X} DI={:08X} DS={:04X} ES={:04X} SS={:04X} SP={:08X} stk {:08X} {:08X} {:08X} {:08X}",
                    m.cycles(), m.cpu.sregs[1], m.cpu.ip, m.cpu.protected_mode(),
                    m.cpu.r[0] & 0xFFFF, m.cpu.r[3] & 0xFFFF, m.cpu.r[1] & 0xFFFF, m.cpu.r[2] & 0xFFFF,
                    m.cpu.r[7], m.cpu.sregs[3], m.cpu.sregs[0], m.cpu.sregs[2], m.cpu.r[4],
                    word(&m, 0), word(&m, 1), word(&m, 2), word(&m, 3),
                );
            }
        }
        if !services.is_empty() && !m.cpu.protected_mode() {
            let at = m.cpu.seg[1].base.wrapping_add(m.cpu.ip);
            if at >= 0xF_0000
                && m.bus.mem.read8(at) == 0xFE
                && m.bus.mem.read8(at + 1) & 0xFE == 0x38
                && services.contains(&m.bus.mem.read8(at + 2))
            {
                println!(
                    "[{}] svc {:02X} AX={:04X} BX={:04X} CX={:04X} DX={:04X} SI={:04X} DI={:04X} DS={:04X} ES={:04X}",
                    m.cycles(),
                    m.bus.mem.read8(at + 2),
                    m.cpu.r[0] & 0xFFFF,
                    m.cpu.r[3] & 0xFFFF,
                    m.cpu.r[1] & 0xFFFF,
                    m.cpu.r[2] & 0xFFFF,
                    m.cpu.r[6] & 0xFFFF,
                    m.cpu.r[7] & 0xFFFF,
                    m.cpu.sregs[3],
                    m.cpu.sregs[0],
                );
            }
        }
        if m.cycles() >= from {
            let base = m.cpu.seg[1].base.wrapping_add(m.cpu.ip);
            let code: Vec<String> = (0..8)
                .map(|i| {
                    let byte = m.cpu.read_linear(&mut m.bus, base.wrapping_add(i));
                    byte.map_or("--".to_string(), |b| format!("{b:02X}"))
                })
                .collect();
            ring.push_back(format!(
                "{:04X}:{:08X} {} A={:08X} B={:08X} C={:08X} D={:08X} S={:08X} SP={:08X}",
                m.cpu.sregs[1],
                m.cpu.ip,
                code.join(" "),
                m.cpu.r[0],
                m.cpu.r[3],
                m.cpu.r[1],
                m.cpu.r[2],
                m.cpu.r[6],
                m.cpu.r[4],
            ));
            if ring.len() > history {
                ring.pop_front();
            }
        }
        m.frame(0, 1);
        if let Some((unit, file)) = &dump {
            if m.cycles() >= *unit {
                let bytes: Vec<u8> = (0..0x100_0000u32).map(|at| m.bus.mem.read8(at)).collect();
                fs::write(file, bytes).expect("write dump");
                println!(
                    "dumped at {}: CS {:04X} base {:08X} IP {:08X} DS base {:08X} SS base {:08X} A20 {} CR3 {:08X}",
                    m.cycles(), m.cpu.sregs[1], m.cpu.seg[1].base, m.cpu.ip,
                    m.cpu.seg[3].base, m.cpu.seg[2].base, m.bus.mem.a20(), m.cpu.cr3
                );
                dump = None;
            }
        }
        let quiet = !services.is_empty() || trace_host;
        if m.cpu.cr0 != last_cr0 || m.cpu.cr3 != last_cr3 {
            if quiet {
                last_cr0 = m.cpu.cr0;
                last_cr3 = m.cpu.cr3;
                continue;
            }
            println!(
                "--- unit {} cr0 {:08X} -> {:08X} cr3 {:08X} -> {:08X}",
                m.cycles(),
                last_cr0,
                m.cpu.cr0,
                last_cr3,
                m.cpu.cr3
            );
            last_cr0 = m.cpu.cr0;
            last_cr3 = m.cpu.cr3;
            if m.cycles() >= from {
                for line in &ring {
                    println!("  {line}");
                }
            }
        }
    }
    if !m.is_running() {
        for line in &ring {
            println!("  {line}");
        }
    }
    println!("end at {} running={} CS:IP {:04X}:{:08X}", m.cycles(), m.is_running(), m.cpu.sregs[1], m.cpu.ip);
    println!("{}", m.text());
}
