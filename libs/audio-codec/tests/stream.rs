//! The streaming API gives the same samples as decoding the whole file.

use feos_audio_codec::{decode, info, AudioError, AudioFormat, AudioStream};

macro_rules! data {
    ($name:literal) => {
        ($name, include_bytes!(concat!("data/", $name)) as &[u8])
    };
}

const FILES: &[(&str, &[u8])] = &[
    data!("pcm8.wav"),
    data!("pcm16.wav"),
    data!("pcm24.wav"),
    data!("pcm32.wav"),
    data!("float32.wav"),
    data!("extensible.wav"),
    data!("pcm16.aiff"),
    data!("pcm16-sowt.aifc"),
    data!("pcm16.au"),
    data!("mulaw_all.au"),
    data!("alaw_all.au"),
    data!("tone.mp3"),
    data!("tone.flac"),
    data!("deep24.flac"),
    data!("fixed8k.flac"),
    data!("tone-flac.oga"),
    data!("tone-stereo.ogg"),
    data!("noise-mono.ogg"),
    data!("clicks-q8.ogg"),
    data!("lc-stereo.aac"),
    data!("lc-mono16k.aac"),
    data!("lc-noise.aac"),
];

fn whole(input: &[u8]) -> Vec<i16> {
    let stated = info(input).unwrap();
    // A stated length is only a claim, and an estimate for some formats.
    let mut out = vec![0i16; stated.sample_count().unwrap() + 65536];
    let decoded = decode(input, &mut out, &mut []).unwrap();
    out.truncate(decoded.frames * stated.channels as usize);
    out
}

fn streamed(stream: &mut AudioStream<'_>) -> Vec<i16> {
    let mut chunk = vec![0i16; stream.max_chunk_samples()];
    let channels = stream.info().channels as usize;
    let mut out = Vec::new();
    while let Some(frames) = stream.next_chunk(&mut chunk).unwrap() {
        out.extend_from_slice(&chunk[..frames * channels]);
    }
    out
}

#[test]
fn streams_match_whole_decodes() {
    for (name, input) in FILES {
        let mut stream = AudioStream::open(input).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(streamed(&mut stream), whole(input), "{name}");
    }
}

#[test]
fn seeking_forward_gives_the_rest_of_the_stream() {
    for (name, input) in FILES {
        let expected = whole(input);
        let channels = info(input).unwrap().channels as usize;
        let total = expected.len() / channels;
        let target = total / 3;
        let mut stream = AudioStream::open(input).unwrap();
        stream.seek(target as u64).unwrap();
        assert_eq!(streamed(&mut stream), expected[target * channels..], "{name}");
    }
}

#[test]
fn seeking_back_restarts_cleanly() {
    for (name, input) in FILES {
        let expected = whole(input);
        let channels = info(input).unwrap().channels as usize;
        let mut stream = AudioStream::open(input).unwrap();
        let mut chunk = vec![0i16; stream.max_chunk_samples()];
        for _ in 0..3 {
            let _ = stream.next_chunk(&mut chunk).unwrap();
        }
        stream.seek(0).unwrap();
        assert_eq!(streamed(&mut stream).len(), expected.len(), "{name}");
        let total = expected.len() / channels;
        stream.seek(total as u64 / 2).unwrap();
        assert_eq!(
            streamed(&mut stream),
            expected[total / 2 * channels..],
            "{name} second half"
        );
    }
}

#[test]
fn a_short_buffer_is_reported_rather_than_overrun() {
    let (_, input) = data!("tone.flac");
    let mut stream = AudioStream::open(input).unwrap();
    let mut chunk = [0i16; 16];
    assert!(matches!(
        stream.next_chunk(&mut chunk),
        Err(AudioError::BufferTooSmall { .. })
    ));
}

#[test]
fn the_stream_fits_on_a_task_stack() {
    // A worker keeps this in a local, on a 128 KiB stack.
    assert!(std::mem::size_of::<AudioStream<'_>>() < 32 * 1024);
}

#[test]
fn hostile_truncations_do_not_panic() {
    for (name, input) in FILES {
        for cut in [1usize, 2, 5, 13] {
            let cut = &input[..input.len() - input.len() / 2 / cut];
            let Ok(mut stream) = AudioStream::open(cut) else {
                continue;
            };
            let mut chunk = vec![0i16; stream.max_chunk_samples()];
            for _ in 0..100_000 {
                match stream.next_chunk(&mut chunk) {
                    Ok(None) => break,
                    Ok(Some(_)) => {}
                    Err(AudioError::BufferTooSmall { .. }) => break,
                    Err(_) => {}
                }
            }
            let _ = name;
        }
    }
}

// ---- The same files through a `Source`.

mod source {
    use feos_audio_codec::{AudioStream, Source, SourceStream};

    const SOUND: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../boards/qemu-virt/fs/media/sound");

    /// A file that hands over a little at a time.
    struct Slow(Vec<u8>);

    impl Source for Slow {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
            let at = (offset as usize).min(self.0.len());
            let count = out.len().min(self.0.len() - at).min(3000);
            out[..count].copy_from_slice(&self.0[at..at + count]);
            count
        }
    }

    fn drain_file(stream: &mut SourceStream, chunk: &mut Vec<i16>) -> Vec<i16> {
        let channels = stream.info().channels as usize;
        let mut all = Vec::new();
        let mut errors = 0;
        loop {
            match stream.next_chunk(chunk) {
                Ok(Some(frames)) => all.extend_from_slice(&chunk[..frames * channels]),
                Ok(None) => return all,
                Err(error) => {
                    errors += 1;
                    assert!(errors < 8, "{error:?}");
                }
            }
        }
    }

    fn drain_memory(stream: &mut AudioStream<'_>, chunk: &mut Vec<i16>) -> Vec<i16> {
        let channels = stream.info().channels as usize;
        let mut all = Vec::new();
        let mut errors = 0;
        loop {
            match stream.next_chunk(chunk) {
                Ok(Some(frames)) => all.extend_from_slice(&chunk[..frames * channels]),
                Ok(None) => return all,
                Err(error) => {
                    errors += 1;
                    assert!(errors < 8, "{error:?}");
                }
            }
        }
    }

    fn compare(name: &str, exact_length: bool) {
        let data = std::fs::read(format!("{SOUND}/{name}")).unwrap();
        let mut memory = AudioStream::open(&data).unwrap();
        let mut file = SourceStream::open(Box::new(Slow(data.clone()))).unwrap();
        assert_eq!(memory.info().sample_rate, file.info().sample_rate, "{name}");
        assert_eq!(memory.info().channels, file.info().channels, "{name}");
        if exact_length {
            assert_eq!(memory.info().frames, file.info().frames, "{name}");
        } else {
            let (a, b) = (memory.info().frames as f64, file.info().frames as f64);
            assert!((a - b).abs() / a < 0.1, "{name}: {a} against an estimated {b}");
        }
        let mut chunk = vec![0i16; 1 << 18];
        let a = drain_memory(&mut memory, &mut chunk);
        let b = drain_file(&mut file, &mut chunk);
        assert_eq!(a.len(), b.len(), "{name}");
        assert!(a == b, "{name}: the samples differ");

        // Seek: to a third of the way in, which should give the same sound again.
        let channels = file.info().channels as usize;
        let target = file.info().frames / 3;
        file.seek(target).unwrap();
        let again = drain_file(&mut file, &mut chunk);
        let want = &a[target as usize * channels..];
        if exact_length {
            assert_eq!(again.len(), want.len(), "{name} after a seek");
            // Past the first packets, whose overlap a seek loses for Vorbis.
            let settle = want.len() / 4;
            assert!(again[settle..] == want[settle..], "{name}: wrong sound after a seek");
        } else {
            let difference = (again.len() as i64 - want.len() as i64).unsigned_abs() as usize;
            assert!(difference < a.len() / 10, "{name}: landed {difference} samples away");
        }
    }

    #[test]
    fn pcm_formats() {
        compare("tone.wav", true);
        compare("tone.aiff", true);
        compare("tone.au", true);
    }

    #[test]
    fn flac() {
        compare("tone.flac", true);
    }

    #[test]
    fn vorbis() {
        compare("tone.ogg", true);
    }

    #[test]
    fn mp3() {
        compare("tone.mp3", false);
    }

    #[test]
    fn mp3_lengths_are_exact_and_a_seek_keeps_the_end() {
        let data = std::fs::read(format!("{SOUND}/tone.mp3")).unwrap();
        let memory = AudioStream::open(&data).unwrap();
        let mut file = SourceStream::open(Box::new(Slow(data.clone()))).unwrap();
        // Both read the LAME tag, so both know the length to the sample.
        let total = file.info().frames;
        assert_eq!(memory.info().frames, total);
        let channels = file.info().channels as usize;
        let mut chunk = vec![0i16; 1 << 18];
        assert_eq!(drain_file(&mut file, &mut chunk).len() as u64, total * channels as u64);
        // A jump lands near its target, and the padding at the end still goes.
        let target = total / 2;
        file.seek(target).unwrap();
        let rest = drain_file(&mut file, &mut chunk).len() as u64 / channels as u64;
        assert!(rest <= total - target, "{rest} frames after seeking to {target} of {total}");
        assert!(rest * 10 >= (total - target) * 9, "{rest} frames after seeking to {target} of {total}");
        // And from the top, it's the whole file again.
        file.seek(0).unwrap();
        assert_eq!(drain_file(&mut file, &mut chunk).len() as u64, total * channels as u64);
    }

    #[test]
    fn adts() {
        compare("tone.aac", false);
    }
}
