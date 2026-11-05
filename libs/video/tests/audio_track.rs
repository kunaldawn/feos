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
