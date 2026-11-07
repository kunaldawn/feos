//! `rec`: records from the sound device to a WAV file.

use feos_app::{App, Context};
use feos_console::{eprintln, println};
use feos_fs::VfsTree;
use feos_mixer::{Capture, OpenError};

/// How long it sleeps between looks at the device.
const POLL_MS: u64 = 20;
/// Bytes of the file written at a time.
const CHUNK_SAMPLES: usize = 8192;
/// The device's format, which is the only one there is.
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;

/// The `rec` command.
pub struct Rec;

/// A WAV header for this many bytes of sound.
fn wav_header(data_bytes: u32) -> [u8; 44] {
    let mut header = [0u8; 44];
    header[..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(data_bytes + 36).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&(CHANNELS as u16).to_le_bytes());
    header[24..28].copy_from_slice(&RATE.to_le_bytes());
    header[28..32].copy_from_slice(&(RATE * CHANNELS as u32 * 2).to_le_bytes());
    header[32..34].copy_from_slice(&((CHANNELS * 2) as u16).to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    header
}

/// What's been heard: enough to tell silence from sound, and which sound.
#[derive(Default)]
struct Measure {
    peak: [i32; CHANNELS],
    crossings: [u64; CHANNELS],
    last: [i16; CHANNELS],
}

impl Measure {
    fn add(&mut self, samples: &[i16]) {
        for frame in samples.chunks_exact(CHANNELS) {
            for channel in 0..CHANNELS {
                self.peak[channel] = self.peak[channel].max((frame[channel] as i32).abs());
                if self.last[channel] <= 0 && frame[channel] > 0 {
                    self.crossings[channel] += 1;
                }
                self.last[channel] = frame[channel];
            }
        }
    }
}

impl App for Rec {
    fn name(&self) -> &'static str {
        "rec"
    }

    fn usage(&self) -> &'static str {
        "rec <seconds> <file.wav>"
    }

    fn description(&self) -> &'static str {
        "record from the sound device to a WAV file"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let mut words = context.args.split_ascii_whitespace();
        let (Some(seconds), Some(path), None) = (
            words.next().and_then(|word| word.parse::<u64>().ok()),
            words.next(),
            words.next(),
        ) else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        let Some(tree) = context.platform.filesystem.and_then(|fs| fs.tree()) else {
            eprintln!("rec: nothing here can be written");
            return 1;
        };
        let capture = match Capture::open(context.platform) {
            Ok(capture) => capture,
            Err(OpenError::NoFreeStream) => {
                eprintln!("rec: something else is recording");
                return 1;
            }
            Err(_) => {
                eprintln!("rec: this machine has no sound input");
                return 1;
            }
        };
        if tree.write_file(path, &wav_header(0), false).is_err() {
            eprintln!("rec: {path}: cannot write it");
            return 1;
        }
        println!("recording {seconds} s to {path}");
        let processes = context.platform.processes;
        let wanted = seconds.saturating_mul(RATE as u64 * CHANNELS as u64);
        let (mut got, mut samples) = (0u64, alloc::vec![0i16; CHUNK_SAMPLES]);
        let mut measure = Measure::default();
        let mut bytes = alloc::vec::Vec::with_capacity(CHUNK_SAMPLES * 2);
        while got < wanted && !processes.kill_requested() {
            let room = (wanted - got).min(CHUNK_SAMPLES as u64) as usize;
            let count = capture.read(&mut samples[..room]);
            if count == 0 {
                processes.sleep_ms(POLL_MS);
                continue;
            }
            measure.add(&samples[..count]);
            bytes.clear();
            for sample in &samples[..count] {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            if !append(tree, path, 44 + got * 2, &bytes) {
                eprintln!("rec: {path}: the disk stopped taking it");
                return 1;
            }
            got += count as u64;
        }
        let device_frames = capture.position();
        drop(capture);
        let data = (got * 2).min(u32::MAX as u64 - 36) as u32;
        let _ = tree.write_at(path, 0, &wav_header(data));
        let per_second = |channel: usize| measure.crossings[channel] * RATE as u64 * CHANNELS as u64 / got.max(1);
        println!(
            "recorded {} frames of {device_frames}: peak {} {}, {} and {} Hz",
            got / CHANNELS as u64,
            measure.peak[0],
            measure.peak[1],
            per_second(0),
            per_second(1)
        );
        0
    }
}

fn append(tree: &dyn VfsTree, path: &str, offset: u64, data: &[u8]) -> bool {
    tree.write_at(path, offset, data).is_ok_and(|written| written == data.len())
}
