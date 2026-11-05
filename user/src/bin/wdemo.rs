//! A windowed user program: it draws into a surface of its own, and the desktop shows
//! it. It reacts to the pointer and the keyboard through the same calls.
//!     wdemo [--crash] [--frames N]
//! `--crash` dereferences a bad pointer after a moment, to show the window just closes.

#![no_std]
#![no_main]

use feos_user::abi::{event, Event};
use feos_user::{entry, eprintln, parse_number, poll_event, println, sleep_ms, surface_create, surface_present, Args};

const WIDTH: usize = 320;
const HEIGHT: usize = 240;
/// The band across the top, which a boot test looks for on the screen.
const SIGNATURE: u32 = 0x00c8_3c1e;

fn main(args: Args) -> i32 {
    let crash = args.has("--crash");
    let frames = args.value_of("--frames").and_then(parse_number);
    let address = surface_create(WIDTH, HEIGHT);
    if address <= 0 {
        eprintln!("wdemo: no surface: {address}");
        return 1;
    }
    // SAFETY: the kernel mapped this much here, for this process's whole life.
    let pixels = unsafe { core::slice::from_raw_parts_mut(address as usize as *mut u32, WIDTH * HEIGHT) };
    println!("wdemo: surface ready {WIDTH}x{HEIGHT}");

    let mut tick = 0usize;
    let (mut pointer_x, mut pointer_y) = (WIDTH as i32 / 2, HEIGHT as i32 / 2);
    let mut keys = 0u32;
    loop {
        let mut incoming = Event::default();
        while poll_event(&mut incoming) == 1 {
            match incoming.kind {
                event::CLOSE => return 0,
                event::POINTER => {
                    pointer_x = incoming.a;
                    pointer_y = incoming.b;
                }
                event::KEY if incoming.b == 1 => {
                    keys += 1;
                    println!("wdemo: key {}", incoming.a);
                }
                _ => {}
            }
        }

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let color = if y < 24 {
                    SIGNATURE
                } else {
                    let r = (x * 255 / WIDTH) as u32;
                    let g = (y * 255 / HEIGHT) as u32;
                    let b = ((tick * 3 + keys as usize * 40) & 0xff) as u32;
                    (r << 16) | (g << 8) | b
                };
                pixels[y * WIDTH + x] = color;
            }
        }
        // A square that moves on its own, and a cross where the pointer is.
        let side = 32;
        let left = (tick * 2) % (WIDTH - side);
        for y in 100..100 + side {
            pixels[y * WIDTH + left..y * WIDTH + left + side].fill(0x00ff_ffff);
        }
        for offset in -6i32..=6 {
            let spots = [(pointer_x + offset, pointer_y), (pointer_x, pointer_y + offset)];
            for (x, y) in spots {
                if (0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y) {
                    pixels[y as usize * WIDTH + x as usize] = 0;
                }
            }
        }
        surface_present();
        tick += 1;

        if crash && tick == 30 {
            println!("wdemo: crashing on purpose");
            // SAFETY: not at all, and that's the point.
            unsafe { core::ptr::write_volatile(8 as *mut u32, 1) };
        }
        if frames.is_some_and(|limit| tick >= limit) {
            println!("wdemo: drew {tick} frames");
            return 0;
        }
        sleep_ms(33);
    }
}

entry!(main);
