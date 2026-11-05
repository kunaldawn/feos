//! Packet decoding against whole-file decoding of the same audio.

use feos_audio_codec::{decode, info, AudioError, PacketDecoder, PacketFormat};

#[test]
fn pcm_packets_in_every_layout() {
    let cases: [(PacketFormat<'_>, &[u8], i16); 6] = [
        (
            PacketFormat::Pcm { bytes: 2, big_endian: false, float: false },
            &[0x34, 0x12],
            0x1234,
        ),
        (
            PacketFormat::Pcm { bytes: 2, big_endian: true, float: false },
            &[0x12, 0x34],
            0x1234,
        ),
        (
            PacketFormat::Pcm { bytes: 3, big_endian: true, float: false },
            &[0x12, 0x34, 0x56],
            0x1234,
        ),
        (
            PacketFormat::Pcm { bytes: 4, big_endian: false, float: false },
            &[0x78, 0x56, 0x34, 0x12],
            0x1234,
        ),
        (
            PacketFormat::Pcm { bytes: 4, big_endian: false, float: true },
            &0.5f32.to_le_bytes(),
            16383,
        ),
        (PacketFormat::Pcm { bytes: 1, big_endian: false, float: false }, &[0x12], 0x1200),
    ];
    for (format, packet, expected) in cases {
        let mut decoder = PacketDecoder::new(format, 8000, 1).unwrap();
        let mut out = [0i16; 1];
        assert_eq!(decoder.decode(packet, &mut out).unwrap(), 1, "{format:?}");
        assert_eq!(out[0], expected, "{format:?}");
    }
}

#[test]
fn a_short_buffer_can_be_grown_and_retried() {
    let format = PacketFormat::Pcm { bytes: 2, big_endian: false, float: false };
    let mut decoder = PacketDecoder::new(format, 8000, 2).unwrap();
    let packet = [0u8; 400];
    let mut small = [0i16; 10];
    assert!(matches!(
        decoder.decode(&packet, &mut small),
        Err(AudioError::BufferTooSmall { required: 200, .. })
    ));
    let mut large = [0i16; 200];
    assert_eq!(decoder.decode(&packet, &mut large).unwrap(), 100);
}

#[test]
fn mp3_frames_as_packets_match_the_whole_file() {
    let input: &[u8] = include_bytes!("data/tone.mp3");
    let stated = info(input).unwrap();
    let mut whole = vec![0i16; stated.sample_count().unwrap() + 8192];
    let done = decode(input, &mut whole, &mut []).unwrap();
    whole.truncate(done.frames * stated.channels as usize);

    // Packets of arbitrary length that happen to hold whole frames: the whole
    // file is the extreme case.
    let mut decoder = PacketDecoder::new(PacketFormat::Mp3, 1, 1).unwrap();
    let mut out = vec![0i16; whole.len() + 8192];
    let frames = decoder.decode(input, &mut out).unwrap();
    out.truncate(frames * decoder.channels() as usize);
    assert_eq!(out, whole);
    assert_eq!(decoder.sample_rate(), stated.sample_rate);
}

#[test]
fn bad_configurations_are_errors() {
    assert!(PacketDecoder::new(PacketFormat::Aac(&[]), 44100, 2).is_err());
    assert!(PacketDecoder::new(PacketFormat::Vorbis(&[2, 30]), 44100, 2).is_err());
    assert!(PacketDecoder::new(PacketFormat::Flac(b"fLaC"), 44100, 2).is_err());
    assert!(PacketDecoder::new(PacketFormat::Mp3, 44100, 0).is_err());
    let bad_pcm = PacketFormat::Pcm { bytes: 5, big_endian: false, float: false };
    assert!(PacketDecoder::new(bad_pcm, 44100, 2).is_err());
}
