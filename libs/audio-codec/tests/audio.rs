use feos_audio_codec::{
    aac, aiff, decode, detect, flac, info, ogg, opus, scratch_len, vorbis, wav, AudioError,
    AudioFormat,
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
        Err(AudioError::BufferTooSmall { .. })
    ));
    let mut out = vec![0i16; props.frames as usize];
    assert_eq!(decode(OPUS, &mut out, &mut []).unwrap().frames, props.frames as usize);

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

#[test]
fn truncated_aifc_comm_is_an_error_not_a_panic() {
    // A declared 22-byte COMM chunk in a file that ends after 18 of them.
    let mut file = Vec::new();
    file.extend_from_slice(b"FORM\0\0\0\x30AIFC");
    file.extend_from_slice(b"COMM\0\0\0\x16");
    file.extend_from_slice(&[0, 1, 0, 0, 0, 1, 0, 16]);
    file.extend_from_slice(&[0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(info(&file), Err(AudioError::UnexpectedEnd));
    let mut out = [0i16; 4];
    assert_eq!(
        decode(&file, &mut out, &mut []),
        Err(AudioError::UnexpectedEnd)
    );

    // Every truncation of a complete file is safe, too.
    file.extend_from_slice(b"NONE");
    file.extend_from_slice(b"SSND\0\0\0\x0a\0\0\0\0\0\0\0\0\x12\x34");
    assert_eq!(info(&file).unwrap().frames, 1);
    for len in 0..file.len() {
        let _ = info(&file[..len]);
        let _ = decode(&file[..len], &mut out, &mut []);
    }
}

fn s16le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

#[test]
fn g711_matches_ffmpeg_for_every_code() {
    // Both references are ffmpeg's decode of all 256 codes; see data/README.md.
    let (mulaw, _) = decode_all(include_bytes!("data/mulaw_all.au"));
    assert_eq!(mulaw, s16le(include_bytes!("data/mulaw_all.s16")));
    // G.711: code 0x00 is the most negative µ-law value, 0x80 the most positive.
    assert_eq!((mulaw[0x00], mulaw[0x80]), (-32124, 32124));
    assert_eq!((mulaw[0x7f], mulaw[0xff]), (0, 0));

    let (alaw, _) = decode_all(include_bytes!("data/alaw_all.au"));
    assert_eq!(alaw, s16le(include_bytes!("data/alaw_all.s16")));
}

fn fnv1a(samples: &[i16]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for sample in samples {
        for byte in sample.to_le_bytes() {
            hash = (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// FLAC is lossless, so tolerance is zero: the decoded bytes must hash the same
/// as ffmpeg's `-f s16le` output. Hashes are generated as data/README.md shows.
#[test]
fn flac_matches_ffmpeg_exactly() {
    let cases: [(&[u8], AudioFormat, u32, u16, usize, u64); 4] = [
        (
            include_bytes!("data/tone.flac"),
            AudioFormat::Flac,
            44_100,
            2,
            6615,
            0x7683d5e049a37f36,
        ),
        (
            include_bytes!("data/fixed8k.flac"),
            AudioFormat::Flac,
            8000,
            1,
            1200,
            0x65989330ae8c9507,
        ),
        (
            include_bytes!("data/deep24.flac"),
            AudioFormat::Flac,
            44_100,
            2,
            6615,
            0x0b18433d40f9aa97,
        ),
        (
            include_bytes!("data/tone-flac.oga"),
            AudioFormat::OggFlac,
            44_100,
            2,
            6615,
            0x7683d5e049a37f36,
        ),
    ];
    for (file, format, rate, channels, frames, hash) in cases {
        assert_eq!(detect(file), Some(format));
        let props = info(file).unwrap();
        assert_eq!(props.format, format);
        assert_eq!(
            (props.sample_rate, props.channels, props.frames),
            (rate, channels, frames as u64)
        );
        let (samples, decoded) = decode_all(file);
        assert_eq!(decoded.frames, frames);
        assert_eq!(fnv1a(&samples), hash, "{format:?}");
    }
}

#[test]
fn flac_stream_decoder_and_unknown_length() {
    let file = include_bytes!("data/tone.flac");
    let mut decoder = flac::FlacDecoder::new(file).unwrap();
    let stream = decoder.stream_info();
    assert_eq!(stream.bits_per_sample, 16);
    let mut out = vec![0i16; stream.max_block_size as usize * 2];
    let mut all = Vec::new();
    while let Some(block) = decoder.next_frame(&mut out).unwrap() {
        all.extend_from_slice(&out[..block * 2]);
    }
    assert_eq!(fnv1a(&all), 0x7683d5e049a37f36);

    // With the length in STREAMINFO cleared, `info` walks the frames.
    let mut unknown = file.to_vec();
    let packed = 8 + 13; // the low 4 bits of this byte and the next 4 are the total.
    unknown[packed] &= 0xf0;
    unknown[packed + 1..packed + 5].fill(0);
    assert_eq!(info(&unknown).unwrap().frames, 6615);

    // A cut-off file gives its complete frames, without an error.
    let (first, second) = (&file[..file.len() * 3 / 4], &file[..file.len() - 1]);
    let mut out = vec![0i16; 6615 * 2];
    let half = decode(first, &mut out, &mut []).unwrap();
    assert!(half.frames > 0 && half.frames < 6615);
    let almost = decode(second, &mut out, &mut []).unwrap();
    assert!(almost.frames < 6615);

    // A flipped bit in the audio is reported.
    let mut damaged = file.to_vec();
    let last = 2000;
    damaged[last] ^= 1;
    assert!(decode(&damaged, &mut out, &mut []).is_err());

    // An ID3v2 tag in front changes nothing.
    let mut tagged = b"ID3\x04\0\0\0\0\0\x05hello".to_vec();
    tagged.extend_from_slice(file);
    assert_eq!(detect(&tagged), Some(AudioFormat::Flac));
    let (samples, _) = decode_all(&tagged);
    assert_eq!(fnv1a(&samples), 0x7683d5e049a37f36);
}

#[test]
fn unknown_ogg_codecs_are_reported_as_unsupported() {
    // One valid page, with a codec header this crate does not know.
    let mut page = Vec::new();
    page.extend_from_slice(b"OggS\0\x02");
    page.extend_from_slice(&[0; 8]);
    page.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    page.extend_from_slice(&[0; 4]);
    page.extend_from_slice(&[1, 5]);
    page.extend_from_slice(b"Theor");
    let mut crc = 0u32;
    for &byte in &page {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc >> 31 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(detect(&page), None);
    assert_eq!(info(&page), Err(AudioError::UnsupportedFormat));
    assert_eq!(
        decode(&page, &mut [], &mut []).map(|d| d.frames),
        Err(AudioError::UnsupportedFormat)
    );
    // A damaged page says so instead.
    page[30] ^= 1;
    assert!(matches!(info(&page), Err(AudioError::InvalidData(_))));
}

/// The worst sample error and the error's RMS, in 16-bit steps.
fn compare(actual: &[i16], expected: &[i16]) -> (i32, f64) {
    assert_eq!(actual.len(), expected.len());
    let mut worst = 0;
    let mut energy = 0f64;
    for (&a, &e) in actual.iter().zip(expected) {
        let error = a as i32 - e as i32;
        worst = worst.max(error.abs());
        energy += (error * error) as f64;
    }
    (worst, (energy / actual.len().max(1) as f64).sqrt())
}

/// Vorbis is lossy and this decoder is fixed point, so the comparison with
/// ffmpeg's float decoder has a tolerance: 2 steps at worst, 0.5 RMS, which is
/// rounding and far below the coding noise of these low-bitrate fixtures.
#[test]
fn vorbis_matches_ffmpeg_within_tolerance() {
    let cases: [(&[u8], &[u8], u32, u16); 3] = [
        (
            include_bytes!("data/clicks-q8.ogg"),
            include_bytes!("data/clicks-q8.ogg.s16"),
            32_000,
            2,
        ),
        (
            include_bytes!("data/tone-stereo.ogg"),
            include_bytes!("data/tone-stereo.ogg.s16"),
            44_100,
            2,
        ),
        (
            include_bytes!("data/noise-mono.ogg"),
            include_bytes!("data/noise-mono.ogg.s16"),
            22_050,
            1,
        ),
    ];
    for (file, reference, rate, channels) in cases {
        assert_eq!(detect(file), Some(AudioFormat::OggVorbis));
        let props = info(file).unwrap();
        assert_eq!(props.format, AudioFormat::OggVorbis);
        assert_eq!((props.sample_rate, props.channels), (rate, channels));
        let (samples, decoded) = decode_all(file);
        let reference = s16le(reference);
        // The granule position is the source's exact length.
        assert_eq!(decoded.frames as u64, props.frames);
        // ffmpeg ignores the end-of-stream granule, so it can run on past the
        // stream's true length or stop early; compare what both produced.
        let common = samples.len().min(reference.len());
        assert!(common * 10 >= samples.len() * 9);
        let (worst, rms) = compare(&samples[..common], &reference[..common]);
        println!("vorbis {rate} Hz: worst {worst}, rms {rms:.3}");
        assert!(worst <= 2, "worst sample error {worst}");
        assert!(rms <= 0.5, "RMS error {rms}");
    }
}

#[test]
fn mpeg_layers_1_and_2_match_ffmpeg() {
    // Both decoders are float, and differ by rounding. See `make_layer1.py`.
    for (name, file, reference) in [
        (
            "layer 2, MPEG-1",
            &include_bytes!("data/tone.mp2")[..],
            &include_bytes!("data/tone.mp2.s16")[..],
        ),
        (
            "layer 2, MPEG-2",
            &include_bytes!("data/mpeg2-stereo.mp2")[..],
            &include_bytes!("data/mpeg2-stereo.mp2.s16")[..],
        ),
        (
            "layer 1",
            &include_bytes!("data/layer1.mp1")[..],
            &include_bytes!("data/layer1.mp1.s16")[..],
        ),
    ] {
        let reference = s16le(reference);
        let stated = info(file).unwrap();
        let mut out = vec![0i16; reference.len() + 8192];
        let decoded = decode(file, &mut out, &mut []).unwrap();
        out.truncate(decoded.frames * stated.channels as usize);
        assert!(reference.iter().any(|&sample| sample.unsigned_abs() > 200), "{name}");
        let shared = out.len().min(reference.len());
        assert!(out.len().abs_diff(reference.len()) <= 4608, "{name}: {} and {}", out.len(), reference.len());
        let (worst, rms) = compare(&out[..shared], &reference[..shared]);
        assert!(worst <= 3 && rms <= 1.0, "{name}: worst {worst}, rms {rms}");
    }
}

#[test]
fn vorbis_floor_0_matches_ffmpeg() {
    // No encoder makes these: see `make_floor0.py`.
    let file = include_bytes!("data/floor0.ogg");
    let reference = s16le(include_bytes!("data/floor0.ogg.s16"));
    let mut out = vec![0i16; reference.len() + 4096];
    let decoded = decode(file, &mut out, &mut []).unwrap();
    assert_eq!(decoded.info.format, AudioFormat::OggVorbis);
    out.truncate(decoded.frames);
    assert_eq!(out.len(), reference.len());
    assert!(reference.iter().any(|&sample| sample.unsigned_abs() > 200), "it isn't silence");
    let (worst, rms) = compare(&out, &reference);
    assert!(worst <= 3 && rms <= 1.0, "worst {worst}, rms {rms}");
}

#[test]
fn vorbis_stream_decoder_matches_decode() {
    let file = include_bytes!("data/tone-stereo.ogg");
    let (whole, _) = decode_all(file);
    let mut decoder = vorbis::VorbisDecoder::new(file).unwrap();
    assert_eq!((decoder.channels(), decoder.sample_rate()), (2, 44_100));
    let mut out = vec![0i16; decoder.max_packet_samples()];
    let mut all = Vec::new();
    while let Some(frames) = decoder.next_packet(&mut out).unwrap() {
        all.extend_from_slice(&out[..frames * 2]);
    }
    assert_eq!(all, whole);

    // A buffer that is too small is reported, and loses nothing.
    let mut decoder = vorbis::VorbisDecoder::new(file).unwrap();
    assert_eq!(decoder.next_packet(&mut out).unwrap(), Some(0));
    assert!(matches!(
        decoder.next_packet(&mut []),
        Err(AudioError::BufferTooSmall { .. })
    ));

    // This fixture has one audio page. A file cut off inside it has no complete
    // page of audio: nothing to play, and no error. Cut in the headers, it fails.
    let cut = &file[..file.len() - 1];
    assert_eq!(detect(cut), Some(AudioFormat::OggVorbis));
    assert_eq!(info(cut).unwrap().frames, 0);
    let mut out = vec![0i16; whole.len()];
    assert_eq!(decode(cut, &mut out, &mut []).unwrap().frames, 0);
    let headers = &file[..1000];
    assert_eq!(info(headers).unwrap().frames, 0);
    assert_eq!(
        decode(headers, &mut out, &mut []).map(|d| d.frames),
        Err(AudioError::UnexpectedEnd)
    );
}

/// AAC is lossy and this decoder is fixed point, so as with Vorbis the comparison
/// with ffmpeg's float decoder has a tolerance: 2 steps at worst, 0.5 RMS.
#[test]
fn aac_matches_ffmpeg_within_tolerance() {
    let cases: [(&[u8], &[u8], u32, u16); 3] = [
        (
            include_bytes!("data/lc-stereo.aac"),
            include_bytes!("data/lc-stereo.aac.s16"),
            44_100,
            2,
        ),
        (
            include_bytes!("data/lc-mono16k.aac"),
            include_bytes!("data/lc-mono16k.aac.s16"),
            16_000,
            1,
        ),
        (
            include_bytes!("data/lc-intensity.aac"),
            include_bytes!("data/lc-intensity.aac.s16"),
            48_000,
            2,
        ),
    ];
    for (file, reference, rate, channels) in cases {
        assert_eq!(detect(file), Some(AudioFormat::Aac));
        let props = info(file).unwrap();
        assert_eq!(props.format, AudioFormat::Aac);
        assert_eq!((props.sample_rate, props.channels), (rate, channels));
        let (samples, decoded) = decode_all(file);
        assert_eq!(decoded.frames as u64, props.frames);
        let (worst, rms) = compare(&samples, &s16le(reference));
        println!("aac {rate} Hz: worst {worst}, rms {rms:.3}");
        assert!(worst <= 2, "worst sample error {worst}");
        assert!(rms <= 0.5, "RMS error {rms}");
    }
}

/// Noise substitution is random by design, so only its level can match. The
/// frames without noise still have to match closely.
#[test]
fn aac_noise_substitution_has_the_right_level() {
    let (samples, _) = decode_all(include_bytes!("data/lc-noise.aac"));
    let reference = s16le(include_bytes!("data/lc-noise.aac.s16"));
    assert_eq!(samples.len(), reference.len());
    let mut noisy = 0;
    for (ours, theirs) in samples
        .chunks_exact(aac::FRAME_SAMPLES)
        .zip(reference.chunks_exact(aac::FRAME_SAMPLES))
    {
        let energy = |frame: &[i16]| frame.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
        let level = 10.0 * ((energy(ours) + 1.0) / (energy(theirs) + 1.0)).log10();
        assert!(level.abs() < 1.0, "frame level differs by {level:.2} dB");
        if compare(ours, theirs).0 > 2 {
            noisy += 1;
        }
    }
    assert!(noisy > 0, "the fixture has no noise-substituted bands left");
}

/// What an MP4 demuxer does: strip nothing but the container's framing, and
/// hand over raw access units with the `AudioSpecificConfig`.
#[test]
fn aac_raw_frames_match_adts() {
    let file = include_bytes!("data/lc-stereo.aac");
    let (whole, _) = decode_all(file);
    // AAC-LC (2), 44.1 kHz (4), stereo (2).
    let config = aac::AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
    assert_eq!(config.channels(), 2);
    let mut decoder = aac::AacDecoder::new(config).unwrap();
    let mut out = vec![0i16; aac::FRAME_SAMPLES * 2];
    let mut all = Vec::new();
    let mut pos = 0;
    while pos < file.len() {
        let header = &file[pos..pos + 7];
        let length = (((header[3] & 3) as usize) << 11)
            | ((header[4] as usize) << 3)
            | ((header[5] as usize) >> 5);
        let frames = decoder.decode_raw_frame(&file[pos + 7..pos + length], &mut out).unwrap();
        all.extend_from_slice(&out[..frames * 2]);
        pos += length;
    }
    assert_eq!(all, whole);
    assert!(matches!(
        decoder.decode_raw_frame(&file[7..64], &mut [0i16; 16]),
        Err(AudioError::BufferTooSmall { .. })
    ));
}

#[test]
fn aac_truncated_and_unsupported_streams() {
    let file = include_bytes!("data/lc-stereo.aac");
    let (whole, _) = decode_all(file);
    // A cut-off last frame is left out, and so does not count in `info` either.
    let cut = &file[..file.len() - 20];
    let props = info(cut).unwrap();
    assert!(props.frames < whole.len() as u64 / 2);
    let mut out = vec![0i16; whole.len()];
    let decoded = decode(cut, &mut out, &mut []).unwrap();
    assert_eq!(decoded.frames as u64, props.frames);
    assert_eq!(out[..decoded.frames * 2], whole[..decoded.frames * 2]);

    // ADTS profile 2 is SSR, with gain control, which this decoder refuses honestly.
    let mut main = file.to_vec();
    main[2] = (main[2] & 0x3f) | 0x80;
    assert_eq!(
        info(&main),
        Err(AudioError::UnsupportedFeature("AAC object type"))
    );
    assert_eq!(
        decode(&main, &mut out, &mut []).map(|d| d.frames),
        Err(AudioError::UnsupportedFeature("AAC object type"))
    );
}

/// A program config element, from FFmpeg's encoder: 5.1 with channel configuration 0.
#[test]
fn aac_program_config_element() {
    const PCE: &[u8] = include_bytes!("data/lc-pce.aac");
    const REFERENCE: &[u8] = include_bytes!("data/lc-pce.aac.s16");
    let props = info(PCE).unwrap();
    assert_eq!(props.channels, 6);
    let mut out = vec![0i16; props.frames as usize * 6];
    let decoded = decode(PCE, &mut out, &mut []).unwrap();
    let wanted: Vec<i16> = REFERENCE
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    assert_eq!(decoded.frames * 6, wanted.len());
    let worst = out.iter().zip(&wanted).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 2, "differs from FFmpeg by up to {worst}");
}
