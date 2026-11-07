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
