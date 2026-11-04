//! `tone`: synthesize a chord and play it through the board's audio device.

use feos_app::{App, Context};
use feos_audio::{AudioBuffer, AudioFormat, Oscillator, Sample, Waveform};
use feos_console::println;
use feos_sync::SpinLock;

/// Longest playable clip, matching the driver's DMA buffer.
const MAX_MS: usize = 3_000;
/// Default clip length.
const DEFAULT_MS: usize = 1_500;
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize * MAX_MS / 1000;

/// Synthesis buffer. Kept in `.bss` because a three-second stereo clip is far
/// larger than the kernel stack.
#[repr(align(128))]
struct Buffer([i16; MAX_SAMPLES]);

static BUFFER: SpinLock<Buffer> = SpinLock::new(Buffer([0; MAX_SAMPLES]));

/// Plays a short A-major chord.
pub struct Tone;

impl App for Tone {
    fn name(&self) -> &'static str {
        "tone"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["play"]
    }

    fn usage(&self) -> &'static str {
        "tone [ms]"
    }

    fn description(&self) -> &'static str {
        "play a synthesized chord (up to 3000 ms)"
    }

    fn run(&self, context: &mut Context<'_>) {
        let Some(device) = context.platform.audio else {
            println!("no audio device; start QEMU with -device intel-hda -device hda-output");
            return;
        };

        if !device.is_ready() {
            println!("audio device is not ready");
            return;
        }

        let milliseconds = context
            .args
            .trim()
            .parse::<usize>()
            .unwrap_or(DEFAULT_MS)
            .clamp(100, MAX_MS);
        let frames = SAMPLE_RATE as usize * milliseconds / 1000;
        let samples = frames * CHANNELS as usize;

        // The host audio device is a single stream. Serialize synthesis and
        // playback so two `tone` processes cannot overwrite each other's
        // samples or interleave DMA blocks.
        let mut buffer = BUFFER.lock();
        let format = AudioFormat::new(SAMPLE_RATE, CHANNELS).expect("valid format");
        let mut audio = AudioBuffer::new(&mut buffer.0[..samples], format).expect("whole frames");

        audio.clear(Sample::SILENCE);
        for (frequency, amplitude) in [(440, 9_000), (554, 7_000), (659, 6_000)] {
            let mut oscillator =
                Oscillator::new(Waveform::Sine, frequency, Sample::from_i16(amplitude));
            audio.mix_oscillator(&mut oscillator, Sample::MAX);
        }
        audio.fade_in((frames / 50).max(1));
        audio.fade_out((frames / 4).max(1));

        println!(
            "playing {} ms of an A-major chord at {} Hz, {} ch (peak {})",
            milliseconds,
            device.sample_rate(),
            device.channels(),
            audio.peak()
        );

        match device.play(audio.samples()) {
            Ok(()) => println!("done"),
            Err(error) => println!("playback failed: {error:?}"),
        }
    }
}
