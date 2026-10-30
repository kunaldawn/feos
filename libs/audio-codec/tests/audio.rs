use feos_audio_codec::{
    aiff, decode, detect, info, ogg, opus, scratch_len, wav, AudioError, AudioFormat,
};

const WAV16: &[u8] = include_bytes!("data/pcm16.wav");
const WAV8: &[u8] = include_bytes!("data/pcm8.wav");
const WAV24: &[u8] = include_bytes!("data/pcm24.wav");
const WAV32: &[u8] = include_bytes!("data/pcm32.wav");
const WAV_FLOAT: &[u8] = include_bytes!("data/float32.wav");
const WAV_EXT: &[u8] = include_bytes!("data/extensible.wav");
const AIFF16: &[u8] = include_bytes!("data/pcm16.aiff");
const AU16: &[u8] = include_bytes!("data/pcm16.au");
const MP3: &[u8] = include_bytes!("data/tone.mp3");
const OPUS: &[u8] = include_bytes!("data/tone.opus");

fn decode_all(input: &[u8]) -> (Vec<i16>, feos_audio_codec::DecodedAudio) {
    let props = info(input).unwrap();
    let mut out = vec![0i16; props.sample_count().unwrap()];
    let mut scratch = vec![0u8; scratch_len(input).unwrap()];
    let decoded = decode(input, &mut out, &mut scratch).unwrap();
    out.truncate(decoded.frames * props.channels as usize);
    (out, decoded)
}

#[test]
fn detects_formats() {
    assert_eq!(detect(WAV16), Some(AudioFormat::Wav));
    assert_eq!(detect(AIFF16), Some(AudioFormat::Aiff));
    assert_eq!(detect(AU16), Some(AudioFormat::Au));
    assert_eq!(detect(MP3), Some(AudioFormat::Mp3));
    assert_eq!(detect(OPUS), Some(AudioFormat::OggOpus));
    assert_eq!(detect(b"nope"), None);
    assert_eq!(info(b"nope"), Err(AudioError::UnknownFormat));
}

#[test]
fn decodes_pcm16_wave() {
    let props = info(WAV16).unwrap();
    assert_eq!(
        (props.sample_rate, props.channels, props.frames),
        (8000, 1, 8)
    );
    let (samples, decoded) = decode_all(WAV16);
    assert_eq!(decoded.frames, 8);
    assert_eq!(samples, [0, 1000, -1000, 32767, -32768, 1234, -2345, 0]);
}

#[test]
fn decodes_wave_sample_formats() {
    let (pcm8, _) = decode_all(WAV8);
    assert_eq!(pcm8, [0, 32512, -32768, 16384]);

    let (pcm24, decoded) = decode_all(WAV24);
    assert_eq!(decoded.info.channels, 2);
    assert_eq!(decoded.frames, 3);
    assert_eq!(pcm24[0], 0);
    assert_eq!(pcm24[2], 0x7fff);
    assert_eq!(pcm24[3], -32768);
    assert_eq!(pcm24[4], 0x1234);

    let (pcm32, _) = decode_all(WAV32);
    assert_eq!(pcm32, [0, 32767, -32768, 16384]);

    let (float, _) = decode_all(WAV_FLOAT);
    assert_eq!(float[0], 0);
    assert!((float[1] as i32 - 16383).abs() <= 1);
    assert!((float[2] as i32 + 16383).abs() <= 1);
    assert_eq!(float[3], 32767);
    assert_eq!(float[4], -32767);

    let (ext, _) = decode_all(WAV_EXT);
    assert_eq!(ext, [1, 2, 3, 4]);
}

#[test]
fn decodes_aiff_and_au() {
    let (aiff_samples, aiff_info) = decode_all(AIFF16);
    assert_eq!(aiff_info.info.sample_rate, 8000);
    assert_eq!(
        aiff_samples,
        [0, 1000, -1000, 32767, -32768, 1234, -2345, 0]
    );

    let (au_samples, au_info) = decode_all(AU16);
    assert_eq!(au_info.info.sample_rate, 8000);
    assert_eq!(au_samples, aiff_samples);
}

#[test]
fn wave_and_aiff_roundtrip() {
    let samples = [0i16, 32767, -32768, 1234, -2345, 42];
    let mut wav_out = vec![0u8; 256];
    let len = wav::encode_i16(44100, 2, &samples, &mut wav_out).unwrap();
    let (decoded, _) = decode_all(&wav_out[..len]);
    assert_eq!(decoded, samples);

    let mut aiff_out = vec![0u8; 256];
    let len = aiff::encode_i16(44100, 2, &samples, &mut aiff_out).unwrap();
    let (decoded, info) = decode_all(&aiff_out[..len]);
    assert_eq!(info.info.sample_rate, 44100);
    assert_eq!(decoded, samples);
}

#[test]
fn decodes_mp3() {
    let props = info(MP3).unwrap();
    assert_eq!(props.channels, 2);
    assert_eq!(props.sample_rate, 44100);
    assert!(props.frames > 10_000, "frames = {}", props.frames);

    let (samples, decoded) = decode_all(MP3);
    assert_eq!(decoded.info.channels, 2);
    assert!(decoded.frames > 10_000, "frames = {}", decoded.frames);
    assert!(samples.iter().any(|&sample| sample != 0));
}

#[test]
fn inspects_ogg_opus() {
    let head = ogg::opus_head(OPUS).unwrap();
    assert_eq!(head.channels, 1);
    assert!(head.pre_skip > 0);

    let props = info(OPUS).unwrap();
    assert_eq!(props.format, AudioFormat::OggOpus);
    assert_eq!(props.sample_rate, 48_000);
    assert_eq!(props.channels, 1);
    assert!(props.frames > 10_000);

    let pages: Vec<_> = ogg::Pages::new(OPUS).collect::<Result<_, _>>().unwrap();
    assert!(pages.len() >= 2);
    assert_eq!(pages[0].flags & 0x02, 0x02);

    let mut out = [0i16; 16];
    assert!(matches!(
        decode(OPUS, &mut out, &mut []),
        Err(AudioError::UnsupportedFeature(_))
    ));

    let packet = opus::parse_packet(&[0x03, 0x03]).unwrap();
    assert_eq!(packet.config, 0);
    assert_eq!(packet.frame_count, 3);
    assert_eq!(packet.frame_samples, 480);
}

#[test]
fn rejects_small_output_buffers() {
    let mut out = [0i16; 2];
    assert!(matches!(
        decode(WAV16, &mut out, &mut []),
        Err(AudioError::BufferTooSmall { .. })
    ));
}
