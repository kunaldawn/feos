//! Renders presets on the host, to check how they look without booting.
//!
//! ```sh
//! ffmpeg -i song.mp3 -f s16le -ac 2 -ar 44100 song.raw
//! cargo run --release --example render -p feos-milkdrop --target x86_64-unknown-linux-gnu -- \
//!     --audio song.raw --seconds 8 --out /tmp/shots presets/*.milk
//! ```
//!
//! Each preset gets `<out>/<index>.ppm`: its last frame. `--size WxH` picks the
//! resolution (320x240 by default). A line per preset reports compile errors
//! and how long frames took.

use std::time::Instant;

use feos_milkdrop::Visualizer;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut audio = Vec::new();
    let mut seconds = 6.0f32;
    let mut out = String::from(".");
    let (mut width, mut height) = (320usize, 240usize);
    let mut presets = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--audio" => {
                let bytes = std::fs::read(args.next().expect("--audio path")).expect("audio");
                audio = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            }
            "--seconds" => seconds = args.next().expect("seconds").parse().expect("number"),
            "--out" => out = args.next().expect("out dir"),
            "--size" => {
                let size = args.next().expect("WxH");
                let (w, h) = size.split_once('x').expect("WxH");
                width = w.parse().expect("width");
                height = h.parse().expect("height");
            }
            _ => presets.push(arg),
        }
    }
    std::fs::create_dir_all(&out).expect("out dir");
    let fps = 30.0;
    let per_frame = (44_100.0 / fps) as usize;
    for (index, path) in presets.iter().enumerate() {
        let text = String::from_utf8_lossy(&std::fs::read(path).expect("preset")).into_owned();
        let mut visualizer = Visualizer::new(width, height, 44_100).expect("visualizer");
        let report = match visualizer.load_text(&text) {
            Ok(report) => report,
            Err(error) => {
                println!("{index:3} FAILED {error:?}  {path}");
                continue;
            }
        };
        let frames = (seconds * fps) as usize;
        let started = Instant::now();
        for frame in 0..frames {
            // Ten seconds into the song, where it's going.
            let at = (10 * 44_100 + frame * per_frame) * 2;
            let window = audio.get(at..at + per_frame * 2).unwrap_or(&[]);
            visualizer.render(window, 1.0 / fps);
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        println!("{index:3} errors={} shaders={} {ms:5.2} ms/frame  {path}", report.errors, report.has_shaders);
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for pixel in visualizer.pixels() {
            ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
        }
        std::fs::write(format!("{out}/{index:03}.ppm"), ppm).expect("write");
    }
}
