//! Mounts the shipped game image and exercises `GAMES` / `MOUNT` / `DIR`.
//!
//! `cargo run -p feos-dosbox --example mount_game --target x86_64-unknown-linux-gnu`

use feos_dosbox::DosBox;
use feos_fs::{mount_image, FileSystem};

fn type_line(machine: &mut DosBox, line: &str) {
    for byte in line.bytes() {
        machine.push_key(byte, 0);
    }
    machine.push_key(b'\r', 0x1C);
    machine.frame(0, 200_000);
}

fn main() {
    let image: &'static [u8] = Box::leak(
        std::fs::read("data/games/dos.img")
            .expect("read data/games/dos.img")
            .into_boxed_slice(),
    );
    let fs: &'static dyn FileSystem = Box::leak(mount_image(image).expect("mount exFAT image"));
    let mut machine = DosBox::new(Some(fs));
    for _ in 0..16 {
        if !machine.shell.in_batch() {
            break;
        }
        machine.frame(0, 10_000);
    }

    type_line(&mut machine, "games");
    let text = machine.text();
    if !text.to_ascii_lowercase().contains(".zip") {
        println!("--- GAMES screen ---\n{text}\n---");
        panic!("GAMES did not print the image manifest");
    }
    println!("GAMES listed the image");

    type_line(&mut machine, "mount wolf3d");
    assert!(machine.text().contains("Mounted"), "MOUNT failed");
    type_line(&mut machine, "dir wolf3d");
    let text = machine.text();
    assert!(text.contains("WOLF3D.EXE"), "DIR did not list WOLF3D.EXE");
    assert!(text.contains("VSWAP.WL1"), "DIR did not list VSWAP.WL1");
    println!("MOUNT imported WOLF3D and DIR listed its files");
    type_line(&mut machine, "mount ssf2t");
    assert!(machine.text().contains("Mounted"), "nested MOUNT failed");
    type_line(&mut machine, "dir ssf2t");
    assert!(
        machine.text().contains("SPRITES"),
        "nested ZIP extraction did not create SSF2T/SPRITES"
    );
    println!("MOUNT extracted a nested ZIP tree (SSF2T/CONTINUE)");
    println!(
        "{}",
        text.lines().rev().take(12).collect::<Vec<_>>().join("\n")
    );
}
