//! `tone`: synthesize a chord and play it through the board's audio device.

use feos_app::{App, Context};
use feos_audio::{AudioBuffer, AudioFormat, Oscillator, Sample, Waveform};
use feos_console::{eprintln, println};
use feos_sync::Mutex;

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

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer([0; MAX_SAMPLES]));

/// How long to wait for room in the device's queue.
const WRITE_RETRY_MS: u64 = 20;

/// Plays a short A-major chord.
pub struct Tone;

impl App for Tone {
    fn name(&self) -> &'static str {
        "tone"
    }

    fn usage(&self) -> &'static str {
        "tone [ms]"
    }

    fn description(&self) -> &'static str {
        "play a synthesized chord (up to 3000 ms)"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(device) = context.platform.audio else {
            eprintln!("no audio device; start QEMU with -device intel-hda -device hda-output");
            return 1;
        };

        if !device.is_ready() {
            eprintln!("audio device is not ready");
            return 1;
        }

        let milliseconds = context
            .argv
            .first()
            .and_then(|text| text.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MS)
            .clamp(100, MAX_MS);
        let frames = SAMPLE_RATE as usize * milliseconds / 1000;
        let samples = frames * CHANNELS as usize;

        // The host audio device is a single stream. Serialize synthesis and
        // playback so two `tone` processes cannot overwrite each other's
        // samples or interleave them in the stream. The second one sleeps on
        // this mutex rather than spinning.
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

        // Streamed in pieces through the mixer, so other sound keeps playing, and
        // a kill can stop this between two of them. That releases the mutex at
        // once; a kill can't reap a task holding it.
        let stream = match feos_mixer::Stream::open(context.platform, format) {
            Ok(stream) => stream,
            Err(error) => {
                println!("no audio stream: {error:?}");
                return 1;
            }
        };
        let processes = context.platform.processes;
        let mut remaining = audio.samples();
        while !remaining.is_empty() {
            if processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            let accepted = stream.write(remaining);
            remaining = &remaining[accepted..];
            if accepted == 0 {
                context.platform.clock.idle_ms(WRITE_RETRY_MS);
            }
        }
        while stream.pending_frames() > 0 || device.queued() > 0 {
            if processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            let _ = stream.write(&[]);
            context.platform.clock.idle_ms(WRITE_RETRY_MS);
        }
        println!("done");
        0
    }
}
