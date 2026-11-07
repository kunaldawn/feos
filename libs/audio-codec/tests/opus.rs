//! Opus against libopus, by way of FFmpeg: generated with its encoder, each stream
//! decoded by it too. `make_opus.sh` makes these. See there for what each is for.

use feos_audio_codec::ogg::Pages;
use feos_audio_codec::{decode, info, AudioStream, Source, SourceStream};

fn file(name: &str) -> Vec<u8> {
    let dir = std::env::var("OPUS_FIXTURES")
        .unwrap_or_else(|_| format!("{}/tests/data/opus", env!("CARGO_MANIFEST_DIR")));
    std::fs::read(format!("{dir}/{name}")).unwrap()
}

/// What's in a file: its packets' configurations, from their first bytes.
fn configs(data: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for page in Pages::new(data).map_while(Result::ok).skip(2) {
        let mut at = 0usize;
        let mut start = true;
        for &lace in page.lacing {
            if start {
                out.push(page.data[at] >> 3);
            }
            at += lace as usize;
            start = lace != 255;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Worst difference, in 16-bit steps, and the signal-to-noise ratio in dB.
fn compare(name: &str) -> (i32, f64) {
    let data = file(&format!("{name}.opus"));
    let reference = file(&format!("{name}.flac"));
    let reference_info = info(&reference).unwrap();
    let mut wanted = vec![0i16; reference_info.frames as usize * reference_info.channels as usize];
    decode(&reference, &mut wanted, &mut []).unwrap();
    let props = info(&data).unwrap();
    assert_eq!(props.sample_rate, 48_000, "{name}");
    let mut pcm = vec![0i16; props.frames as usize * props.channels as usize];
    let got = decode(&data, &mut pcm, &mut []).unwrap();
    let channels = props.channels as usize;
    let frames = got.frames.min(wanted.len() / channels);
    assert!(
        got.frames.abs_diff(wanted.len() / channels) <= 1,
        "{name}: {} frames against {}",
        got.frames,
        wanted.len() / channels
    );
    let (mut worst, mut noise, mut signal) = (0i32, 0f64, 0f64);
    for frame in 0..frames {
        for channel in 0..channels {
            let ours = pcm[frame * channels + channel] as i32;
            let theirs = wanted[frame * channels + channel] as i32;
            let difference = ours - theirs;
            worst = worst.max(difference.abs());
            noise += (difference * difference) as f64;
            signal += (theirs * theirs) as f64;
        }
    }
    let snr = if noise == 0.0 { f64::INFINITY } else { 10.0 * (signal / noise).log10() };
    println!("{name}: configs {:?}, worst difference {worst}, SNR {snr:.1} dB", configs(&data));
    (worst, snr)
}

fn close_to_libopus(name: &str, mode: &[u8]) {
    let data = file(&format!("{name}.opus"));
    let found = configs(&data);
    assert!(
        found.iter().any(|config| mode.contains(config)),
        "{name} has configurations {found:?}, and isn't what it's for"
    );
    let (worst, snr) = compare(name);
    assert!(worst <= 2, "{name}: differs from libopus by up to {worst}, SNR {snr:.1} dB");
}

/// Mapping family 1, with each channel where WAVE order has it.
#[test]
fn more_than_two_channels() {
    for name in ["multi-3", "multi-51"] {
        let (worst, snr) = compare(name);
        assert!(worst <= 2, "{name}: differs from libopus by up to {worst}, SNR {snr:.1} dB");
    }
}

const SILK: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const HYBRID: &[u8] = &[12, 13, 14, 15];
const CELT: &[u8] = &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];

#[test]
fn silk_narrow_band() {
    close_to_libopus("silk-nb", SILK);
}

#[test]
fn silk_wide_band() {
    close_to_libopus("silk-wb", SILK);
}

#[test]
fn silk_stereo() {
    close_to_libopus("silk-stereo", SILK);
}

#[test]
fn silk_60_ms() {
    close_to_libopus("silk-60", SILK);
}

#[test]
fn hybrid_super_wide_band() {
    close_to_libopus("hybrid-swb", HYBRID);
}

#[test]
fn hybrid_full_band() {
    close_to_libopus("hybrid-fb", HYBRID);
}

#[test]
fn celt_at_a_high_rate() {
    close_to_libopus("celt-128", CELT);
}

#[test]
fn celt_low_delay() {
    close_to_libopus("celt-low", CELT);
}

#[test]
fn celt_2_5_ms() {
    close_to_libopus("celt-2.5", CELT);
}

#[test]
fn celt_40_ms() {
    close_to_libopus("celt-40", CELT);
}

struct Bytes(Vec<u8>);

impl Source for Bytes {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        (&*self.0).read_at(offset, out)
    }
}

fn drain(next: &mut dyn FnMut(&mut [i16]) -> Option<usize>, channels: usize) -> Vec<i16> {
    let mut chunk = vec![0i16; 1 << 16];
    let mut all = Vec::new();
    while let Some(frames) = next(&mut chunk) {
        all.extend_from_slice(&chunk[..frames * channels]);
    }
    all
}

#[test]
fn from_a_source_the_same_and_it_seeks() {
    for name in ["silk-wb", "hybrid-fb", "celt-128"] {
        let data = file(&format!("{name}.opus"));
        let mut memory = AudioStream::open(&data).unwrap();
        let mut disk = SourceStream::open(Box::new(Bytes(data.clone()))).unwrap();
        assert_eq!(memory.info().frames, disk.info().frames, "{name}");
        let channels = memory.info().channels as usize;
        let a = drain(&mut |out| memory.next_chunk(out).unwrap(), channels);
        let b = drain(&mut |out| disk.next_chunk(out).unwrap(), channels);
        assert!(a == b, "{name}: {} samples and {} samples", a.len(), b.len());

        // Seek in: after the decoder has settled, the same sound.
        let target = disk.info().frames * 2 / 3;
        disk.seek(target).unwrap();
        let again = drain(&mut |out| disk.next_chunk(out).unwrap(), channels);
        let want = &a[target as usize * channels..];
        assert_eq!(again.len(), want.len(), "{name} after a seek");
        // SILK's long-term prediction takes a while to forget, as it does in libopus.
        let settled = want.len() / 2;
        let (mut noise, mut signal) = (0f64, 0f64);
        for (x, y) in again[settled..].iter().zip(&want[settled..]) {
            let difference = *x as f64 - *y as f64;
            noise += difference * difference;
            signal += *y as f64 * *y as f64;
        }
        let snr = 10.0 * (signal / noise.max(1.0)).log10();
        println!("{name}: {snr:.1} dB after a seek, against playing through");
        assert!(snr > 10.0, "{name}: only {snr:.1} dB after a seek");

        // And back to the start.
        disk.seek(0).unwrap();
        let start = drain(&mut |out| disk.next_chunk(out).unwrap(), channels);
        assert!(start == a, "{name}: the start after going back");
    }
}

/// Damage of every kind, and it mustn't panic.
#[test]
fn malformed_packets_never_panic() {
    use feos_audio_codec::opus::OpusPacketDecoder;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let head = {
        let mut head = b"OpusHead\x01\x02\x38\x01\x80\xbb\0\0\0\0\0".to_vec();
        head.truncate(19);
        head
    };
    for name in ["silk-nb", "silk-stereo", "hybrid-swb", "hybrid-fb", "celt-low", "celt-2.5", "celt-40"] {
        let data = file(&format!("{name}.opus"));
        let mut packets = Vec::new();
        for page in Pages::new(&data).map_while(Result::ok).skip(2) {
            let mut at = 0;
            for &lace in page.lacing {
                packets.push(page.data[at..at + lace as usize].to_vec());
                at += lace as usize;
            }
        }
        let mut out = vec![0i16; 5760 * 2];
        for _ in 0..60 {
            let mut decoder = OpusPacketDecoder::new(&head).unwrap();
            for packet in packets.iter().take(30) {
                let mut packet = packet.clone();
                for _ in 0..next() % 6 {
                    if packet.is_empty() {
                        break;
                    }
                    let at = next() as usize % packet.len();
                    match next() % 3 {
                        0 => packet[at] = next() as u8,
                        1 => packet[at] ^= 1 << (next() % 8),
                        _ => packet.truncate(at),
                    }
                }
                let _ = decoder.decode(&packet, &mut out);
            }
        }
        for size in 0..256 {
            let junk: Vec<u8> = (0..size).map(|_| next() as u8).collect();
            let mut decoder = OpusPacketDecoder::new(&head).unwrap();
            let _ = decoder.decode(&junk, &mut out);
        }
    }
}

/// The official test vectors of RFC 8251, which are too large to keep here. Point
/// Each vector within what's been reached, and the three things that were wrong kept fixed:
/// mono hybrid folding, the SILK reset after CELT and the fade in from concealment.
/// OPUS_VECTORS at an unpacked `opus_testvectors-rfc8251.tar.gz` to run this. Each packet's
/// final range says whether the entropy decoding agrees exactly with the encoder's.
#[test]
fn rfc_8251_vectors() {
    use feos_opus::OpusDecoder;
    let Ok(dir) = std::env::var("OPUS_VECTORS") else {
        return;
    };
    for number in 1..=12 {
        let stream = std::fs::read(format!("{dir}/testvector{number:02}.bit")).unwrap();
        let wanted = std::fs::read(format!("{dir}/testvector{number:02}.dec")).unwrap();
        let wanted: Vec<i16> = wanted
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();
        let mut decoder = OpusDecoder::new(48_000, 2).unwrap();
        let mut out = vec![0i16; 5760 * 2];
        let mut pcm = Vec::new();
        let (mut pos, mut packets, mut range_errors, mut decode_errors) = (0, 0u32, 0u32, 0u32);
        while pos + 8 <= stream.len() {
            let len = u32::from_be_bytes(stream[pos..pos + 4].try_into().unwrap()) as usize;
            let range = u32::from_be_bytes(stream[pos + 4..pos + 8].try_into().unwrap());
            let packet = &stream[pos + 8..pos + 8 + len];
            pos += 8 + len;
            packets += 1;
            match decoder.decode(packet, &mut out, false) {
                Ok(frames) => pcm.extend_from_slice(&out[..frames * 2]),
                Err(_) => decode_errors += 1,
            }
            if !packet.is_empty() && decoder.final_range() != range {
                range_errors += 1;
            }
        }
        let (mut noise, mut signal, mut worst) = (0f64, 0f64, 0i32);
        for (ours, theirs) in pcm.iter().zip(&wanted) {
            let difference = *ours as i32 - *theirs as i32;
            worst = worst.max(difference.abs());
            noise += (difference * difference) as f64;
            signal += (*theirs as i32 * *theirs as i32) as f64;
        }
        let snr = 10.0 * (signal / noise.max(1.0)).log10();
        println!(
            "vector {number:02}: {packets} packets, {range_errors} final range errors, {decode_errors} decode errors, {} samples against {}, worst {worst}, SNR {snr:.1} dB",
            pcm.len(),
            wanted.len()
        );
        assert_eq!(range_errors, 0, "vector {number}");
        assert!(worst <= 33, "vector {number}: worst {worst}");
    }
}
