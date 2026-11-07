//! AAC against FFmpeg on real streams: FFmpeg's FATE samples, which include ISO conformance
//! streams, kept out of this repository. AAC_SAMPLES names a directory with them, each with a
//! `<name>.ref` of FFmpeg's decoding to 16-bit PCM. Without it, this does nothing.

use feos_audio_codec::{AudioError, PacketDecoder, PacketFormat};
use feos_video::{audio_track, AudioCodec};

fn check(path: &std::path::Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let reference = std::fs::read(format!("{}.ref", path.display())).map_err(|e| e.to_string())?;
    let track = audio_track(&data).ok_or("no audio track")?;
    if track.codec != AudioCodec::Aac {
        return Err("not AAC".into());
    }
    let config = &data[track.config.clone()];
    let mut decoder = PacketDecoder::new(PacketFormat::Aac(config), track.sample_rate, track.channels)
        .map_err(|e| format!("refused: {e}"))?;
    let mut channels = decoder.channels() as usize;
    let mut out = vec![0i16; 1 << 16];
    let mut pcm = Vec::new();
    let mut errors = 0;
    let mut first = None;
    for index in 0..track.packets.len() {
        match decoder.decode(track.packet(&data, index).unwrap(), &mut out) {
            Ok(frames) => {
                channels = decoder.channels() as usize;
                pcm.extend_from_slice(&out[..frames * channels])
            }
            Err(AudioError::BufferTooSmall { .. }) => return Err("buffer".into()),
            Err(error) => {
                errors += 1;
                first.get_or_insert(format!("{error}"));
                pcm.extend(std::iter::repeat_n(0, 1024 * channels));
            }
        }
    }
    if let Ok(dump) = std::env::var("AAC_DUMP") {
        if path.to_string_lossy().contains(&dump) {
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::write(std::env::var("AAC_DUMP_TO").unwrap_or("/tmp/feos-aac-dump.raw".into()), bytes).unwrap();
        }
    }
    let wanted: Vec<i16> = reference
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let (mut noise, mut signal, mut worst) = (0f64, 0f64, 0i32);
    for (a, b) in pcm.iter().zip(&wanted) {
        let difference = *a as i32 - *b as i32;
        worst = worst.max(difference.abs());
        noise += (difference * difference) as f64;
        signal += (*b as i32 * *b as i32) as f64;
    }
    let snr = 10.0 * (signal / noise.max(1.0)).log10();
    Ok(format!(
        "{channels} ch {} Hz, {} packets, {errors} errors{}; {} samples against {}, worst {worst}, SNR {snr:.1} dB",
        decoder.sample_rate(),
        track.packets.len(),
        first.map_or(String::new(), |error| format!(" ({error})")),
        pcm.len(),
        wanted.len(),
    ))
}

#[test]
fn real_streams() {
    let Ok(dir) = std::env::var("AAC_SAMPLES") else {
        return;
    };
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "mp4"))
        .collect();
    names.sort();
    for path in names {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        match check(&path) {
            Ok(line) => println!("{name}: {line}"),
            Err(why) => println!("{name}: {why}"),
        }
    }
}

/// Excerpts of those streams, made with `ffmpeg -c copy`, against FFmpeg's decoding of each.
#[test]
fn excerpts_match_ffmpeg() {
    const FIXTURES: [(&str, u32, u16, i32); 8] = [
        ("aac-960.mp4", 48_000, 1, 2),
        ("aac-ltp.mp4", 48_000, 2, 3),
        ("aac-main.mp4", 88_200, 1, 2),
        ("aac-pulse.mp4", 44_100, 1, 2),
        ("he-sbr.mp4", 48_000, 2, 2),
        ("he-ps.mp4", 32_000, 2, 2),
        ("he-ps-phase.mp4", 32_000, 2, 2),
        ("he-downsampled.mp4", 48_000, 2, 2),
    ];
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");
    for (name, rate, channels, tolerance) in FIXTURES {
        let data = std::fs::read(format!("{dir}{name}")).unwrap();
        let wanted = std::fs::read(format!("{dir}{name}.s16")).unwrap();
        let track = audio_track(&data).unwrap();
        let config = &data[track.config.clone()];
        let mut decoder = PacketDecoder::new(PacketFormat::Aac(config), track.sample_rate, track.channels).unwrap();
        assert_eq!((decoder.sample_rate(), decoder.channels()), (rate, channels), "{name}");
        let mut out = vec![0i16; 1 << 16];
        let mut pcm = Vec::new();
        for index in 0..track.packets.len() {
            let frames = decoder.decode(track.packet(&data, index).unwrap(), &mut out).unwrap();
            pcm.extend_from_slice(&out[..frames * channels as usize]);
        }
        let wanted: Vec<i16> = wanted.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(pcm.len(), wanted.len(), "{name}");
        let worst = pcm.iter().zip(&wanted).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= tolerance, "{name}: worst difference {worst}");
    }
}

/// Damaged access units never panic, whichever of AAC's kinds they go through. Bits are flipped
/// mostly near the end of a frame, where SBR's data is, and overflow checks are on here.
#[test]
fn damaged_frames_dont_panic() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/");
    let mut seed = 0x2545_f491u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for name in ["aac-960.mp4", "aac-ltp.mp4", "aac-main.mp4", "he-sbr.mp4", "he-ps.mp4", "he-ps-phase.mp4", "he-downsampled.mp4"] {
        let data = std::fs::read(format!("{dir}{name}")).unwrap();
        let track = audio_track(&data).unwrap();
        let config = data[track.config.clone()].to_vec();
        for round in 0..40 {
            let mut decoder = PacketDecoder::new(PacketFormat::Aac(&config), track.sample_rate, track.channels).unwrap();
            let mut out = vec![0i16; 1 << 16];
            for index in 0..track.packets.len() {
                let mut packet = track.packet(&data, index).unwrap().to_vec();
                if !packet.is_empty() {
                    for _ in 0..1 + random() % 6 {
                        let at = if random() % 3 == 0 {
                            random() as usize % packet.len()
                        } else {
                            packet.len() - 1 - random() as usize % (packet.len().min(60))
                        };
                        packet[at] ^= 1 << (random() % 8);
                    }
                    if round % 7 == 0 {
                        packet.truncate(1 + random() as usize % packet.len());
                    }
                }
                let _ = decoder.decode(&packet, &mut out);
            }
        }
    }
}

/// A config that promises SBR with any header it likes, and frames of random bits behind it.
#[test]
fn random_configs_dont_panic() {
    let mut seed = 0x9e37_79b9u32;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _ in 0..2000 {
        let config: Vec<u8> = (0..6).map(|_| random() as u8).collect();
        let Ok(mut decoder) = PacketDecoder::new(PacketFormat::Aac(&config), 44_100, 2) else {
            continue;
        };
        let mut out = vec![0i16; 1 << 16];
        for _ in 0..4 {
            let packet: Vec<u8> = (0..random() % 400).map(|_| random() as u8).collect();
            let _ = decoder.decode(&packet, &mut out);
        }
    }
}
