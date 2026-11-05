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
fn opus_says_why_it_cannot_play() {
    let (_, input) = data!("tone.opus");
    assert!(matches!(
        AudioStream::open(input),
        Err(AudioError::UnsupportedFeature(_))
    ));
    assert_eq!(info(input).unwrap().format, AudioFormat::OggOpus);
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
