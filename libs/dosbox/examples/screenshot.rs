//! Renders a FEOS-DOS screen to `/tmp/dosbox.rgb` for documentation.
//!
//! Run with:
//! `cargo run -p feos-dosbox --example screenshot --target x86_64-unknown-linux-gnu`

use feos_dosbox::DosBox;
use feos_gfx::Canvas;

fn main() {
    let mut dos = DosBox::new(None);
    for _ in 0..16 {
        if !dos.shell.in_batch() {
            break;
        }
        dos.frame(0, 10_000);
    }
    for byte in b"dir\r" {
        dos.push_key(*byte, 0);
    }
    dos.frame(0, 10_000);
    for byte in b"about\r" {
        dos.push_key(*byte, 0);
    }
    dos.frame(0, 10_000);

    let mut pixels = vec![0u32; 640 * 400];
    let mut canvas = Canvas::new(&mut pixels, 640, 400, 640).expect("canvas");
    dos.render(&mut canvas);

    let mut raw = Vec::with_capacity(640 * 400 * 3);
    for pixel in pixels {
        raw.push((pixel >> 16) as u8);
        raw.push((pixel >> 8) as u8);
        raw.push(pixel as u8);
    }
    std::fs::write("/tmp/dosbox.rgb", raw).expect("write /tmp/dosbox.rgb");
    println!("wrote /tmp/dosbox.rgb ({} bytes)", 640 * 400 * 3);
}
