//! Container audio tracks, against files muxed by ffmpeg. See `tests/data/README.md`.

use feos_video::{audio_track, AudioCodec};

const MP4_AAC: &[u8] = include_bytes!("data/av-aac.mp4");
const MKV_MP3: &[u8] = include_bytes!("data/av-mp3.mkv");
const FLV_AAC: &[u8] = include_bytes!("data/av-aac.flv");
const AVI_PCM: &[u8] = include_bytes!("data/av-pcm.avi");
const MOV_PCM: &[u8] = include_bytes!("data/av-pcm.mov");
const MKV_FLAC: &[u8] = include_bytes!("data/av-flac.mkv");
const WEBM_VORBIS: &[u8] = include_bytes!("data/av-vorbis.mkv");

fn checked(input: &[u8], codec: AudioCodec, rate: u32, channels: u16, last_ms: u64) {
    let track = audio_track(input).unwrap();
    assert_eq!(track.codec, codec);
    assert_eq!(track.sample_rate, rate);
    assert_eq!(track.channels, channels);
    let last = track.packets.last().unwrap();
    assert!(
        last.pts_ms + 200 >= last_ms && last.pts_ms <= last_ms + 50,
        "{codec:?}: last packet at {} ms",
        last.pts_ms
    );
    let mut previous = 0;
    for packet in &track.packets {
        assert!(packet.pts_ms >= previous, "{codec:?}: times go forward");
        previous = packet.pts_ms;
        assert!(packet.offset + packet.size as usize <= input.len());
    }
    assert_eq!(track.packet(input, track.packets.len()), None);
}

#[test]
fn mp4_aac() {
    checked(MP4_AAC, AudioCodec::Aac, 44_100, 2, 1000);
    let track = audio_track(MP4_AAC).unwrap();
    assert!(!track.config.is_empty(), "the AudioSpecificConfig");
    assert_eq!(track.packets[track.packet_at(0)].pts_ms, 0);
    let middle = track.packet_at(500);
    assert!(track.packets[middle].pts_ms <= 500 && track.packets[middle + 1].pts_ms > 500);
}

#[test]
fn matroska_mp3() {
    checked(MKV_MP3, AudioCodec::Mp3, 44_100, 2, 1000);
}

#[test]
fn matroska_flac() {
    checked(MKV_FLAC, AudioCodec::Flac, 44_100, 2, 1000);
    assert!(!audio_track(MKV_FLAC).unwrap().config.is_empty());
}

#[test]
fn matroska_vorbis() {
    checked(WEBM_VORBIS, AudioCodec::Vorbis, 44_100, 2, 1000);
    assert!(!audio_track(WEBM_VORBIS).unwrap().config.is_empty());
}

#[test]
fn flv_aac() {
    checked(FLV_AAC, AudioCodec::Aac, 44_100, 2, 1000);
    assert!(!audio_track(FLV_AAC).unwrap().config.is_empty());
}

#[test]
fn avi_pcm() {
    let codec = AudioCodec::Pcm {
        bytes: 2,
        big_endian: false,
        float: false,
    };
    checked(AVI_PCM, codec, 44_100, 2, 1000);
    let track = audio_track(AVI_PCM).unwrap();
    let bytes: u64 = track.packets.iter().map(|packet| packet.size as u64).sum();
    assert!((176_000..=178_000).contains(&bytes), "{bytes}");
}

#[test]
fn mov_pcm_chunks_add_up() {
    let codec = AudioCodec::Pcm {
        bytes: 2,
        big_endian: false,
        float: false,
    };
    checked(MOV_PCM, codec, 44_100, 2, 1000);
    let track = audio_track(MOV_PCM).unwrap();
    let bytes: u64 = track.packets.iter().map(|packet| packet.size as u64).sum();
    assert!((176_000..=178_000).contains(&bytes), "{bytes}");
}

#[test]
fn a_file_with_no_audio_says_so() {
    assert_eq!(audio_track(include_bytes!("data/anim.gif")), None);
    assert_eq!(audio_track(include_bytes!("data/bframes.mp4")), None);
}

#[test]
fn garbage_in_a_container_gives_no_track_and_no_panic() {
    for input in [MP4_AAC, MKV_MP3, FLV_AAC, AVI_PCM, MOV_PCM, MKV_FLAC, WEBM_VORBIS] {
        for cut in [3usize, 7, 2, 11] {
            let _ = audio_track(&input[..input.len() / cut]);
        }
        let mut bent = input.to_vec();
        for step in 0..512 {
            let at = 16 + step * 613 % (bent.len() - 16);
            bent[at] ^= 0xa5;
            let _ = audio_track(&bent);
        }
    }
}

/// An MPEG-1 program stream's sound, against ffmpeg's decoding of the same file.
#[test]
fn program_stream_mp2() {
    use feos_audio_codec::{PacketDecoder, PacketFormat};
    const MPG: &[u8] = include_bytes!("data/av-mp2.mpg");
    const REFERENCE: &[u8] = include_bytes!("data/av-mp2.mpg.s16");

    checked(MPG, AudioCodec::MpegSplit, 44_100, 2, 2000);
    let track = audio_track(MPG).unwrap();
    let mut decoder = PacketDecoder::new(PacketFormat::Mp3Split, 44_100, 2).unwrap();
    let mut out = vec![0i16; decoder.typical_samples()];
    let mut pcm = Vec::new();
    for index in 0..track.packets.len() {
        let frames = decoder.decode(track.packet(MPG, index).unwrap(), &mut out).unwrap();
        pcm.extend_from_slice(&out[..frames * 2]);
    }
    let reference: Vec<i16> = REFERENCE
        .chunks_exact(2)
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    assert!(
        pcm.len().abs_diff(reference.len()) <= 2304,
        "{} samples against {}",
        pcm.len(),
        reference.len()
    );
    let mut worst = 0;
    for (a, b) in pcm.iter().zip(&reference) {
        worst = worst.max((*a as i32 - *b as i32).abs());
    }
    assert!(worst <= 2, "differs from ffmpeg by up to {worst}");
}

/// A packet that doesn't fit is asked about again, and gives what it would have.
#[test]
fn split_frames_survive_a_short_buffer() {
    use feos_audio_codec::{AudioError, PacketDecoder, PacketFormat};
    const MPG: &[u8] = include_bytes!("data/av-mp2.mpg");
    let track = audio_track(MPG).unwrap();
    let mut decoder = PacketDecoder::new(PacketFormat::Mp3Split, 44_100, 2).unwrap();
    let mut tiny = vec![0i16; 16];
    let mut enough = vec![0i16; 1 << 16];
    let mut total = 0;
    for index in 0..track.packets.len() {
        let packet = track.packet(MPG, index).unwrap();
        match decoder.decode(packet, &mut tiny) {
            Err(AudioError::BufferTooSmall { .. }) => {
                total += decoder.decode(packet, &mut enough).unwrap();
            }
            Ok(frames) => total += frames,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert!(total > 80_000);
}

#[test]
fn an_elementary_stream_has_no_sound() {
    assert!(audio_track(include_bytes!("../../../boards/qemu-virt/fs/media/mpeg1-sample.mpg")).is_none());
}

/// Opus in WebM, against libopus: the packet path a video's sound goes down.
#[test]
fn webm_opus() {
    use feos_audio_codec::{PacketDecoder, PacketFormat};
    const WEBM: &[u8] = include_bytes!("data/av-opus.webm");
    const REFERENCE: &[u8] = include_bytes!("data/av-opus.webm.s16");
    let track = audio_track(WEBM).unwrap();
    assert_eq!(track.codec, AudioCodec::Opus);
    assert!(WEBM[track.config.clone()].starts_with(b"OpusHead"));
    let mut decoder =
        PacketDecoder::new(PacketFormat::Opus(&WEBM[track.config.clone()]), 48_000, 1).unwrap();
    assert_eq!(decoder.sample_rate(), 48_000);
    let channels = decoder.channels() as usize;
    let mut out = vec![0i16; decoder.typical_samples()];
    let mut pcm = Vec::new();
    for index in 0..track.packets.len() {
        let frames = decoder.decode(track.packet(WEBM, index).unwrap(), &mut out).unwrap();
        pcm.extend_from_slice(&out[..frames * channels]);
    }
    let reference: Vec<i16> = REFERENCE
        .chunks_exact(2)
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    assert!(pcm.len().abs_diff(reference.len()) <= 960 * channels, "{} and {}", pcm.len(), reference.len());
    let worst = pcm.iter().zip(&reference).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 1, "differs from libopus by up to {worst}");
}
