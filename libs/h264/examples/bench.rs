//! Times decoding of an Annex-B file: `bench <file> [repeats]`.

use feos_h264::{OrderedDecoder, annex_b_units};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let repeats: usize = args.get(2).map_or(3, |r| r.parse().unwrap());
    let mut best = f64::MAX;
    let mut frames = 0;
    for _ in 0..repeats {
        let start = std::time::Instant::now();
        let mut decoder = OrderedDecoder::new();
        frames = 0;
        for nal in annex_b_units(&data) {
            if let Ok(out) = decoder.decode_nal(&nal) {
                frames += out.len();
            }
        }
        frames += decoder.flush().len();
        best = best.min(start.elapsed().as_secs_f64());
    }
    println!("{frames} frames, best {best:.3} s, {:.1} fps", frames as f64 / best);
}
