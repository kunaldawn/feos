//! Render a short chord with `feos-audio` to a WAV file.
//!
//! This is a host-side example (not kernel code):
//!
//! ```sh
//! cargo run -p feos-audio --example tone --target x86_64-unknown-linux-gnu -- tone.wav
//! aplay tone.wav
//! ```
//!
//! The generated file is 48 kHz, 16-bit stereo PCM.

use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use feos_audio::{AudioBuffer, AudioFormat, Oscillator, Sample, Waveform};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const SECONDS: usize = 3;

fn main() -> io::Result<()> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("feos-tone.wav"));

    let format = AudioFormat::new(SAMPLE_RATE, CHANNELS).expect("valid audio format");
    let frames = SAMPLE_RATE as usize * SECONDS;
    let mut samples = vec![0i16; frames * CHANNELS as usize];

    {
        let mut buffer = AudioBuffer::new(&mut samples, format).expect("aligned sample buffer");

        // A major chord: A4, C#5 and E5.
        for (frequency, amplitude) in [(440, 10_000), (554, 8_000), (659, 7_000)] {
            let mut oscillator =
                Oscillator::new(Waveform::Sine, frequency, Sample::from_i16(amplitude));
            buffer.mix_oscillator(&mut oscillator, Sample::MAX);
        }

        // Avoid clicks at both ends.
        buffer.fade_in(SAMPLE_RATE as usize / 20);
        buffer.fade_out(SAMPLE_RATE as usize / 4);

        println!(
            "rendered {} frames ({} ms) at {} Hz, peak {}",
            buffer.frames(),
            buffer.duration_ms(),
            buffer.sample_rate(),
            buffer.peak()
        );
    }

    write_wav(&path, &samples, format)?;
    println!("wrote {}", path.display());
    println!(
        "play it with: aplay {}  (or paplay / ffplay -autoexit / afplay)",
        path.display()
    );
    Ok(())
}

fn write_wav(path: &Path, samples: &[i16], format: AudioFormat) -> io::Result<()> {
    let data_len = u32::try_from(samples.len() * 2).expect("WAV file too large");
    let channels = format.channels();
    let sample_rate = format.sample_rate();
    let block_align = channels * 2;
    let byte_rate = sample_rate * u32::from(block_align);

    let mut file = BufWriter::new(File::create(path)?);

    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVE")?;

    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // PCM format chunk size
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?; // bits per sample

    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for sample in samples {
        file.write_all(&sample.to_le_bytes())?;
    }

    file.flush()
}
