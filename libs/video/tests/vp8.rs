//! VP8 against FFmpeg: the same pictures, or a failure saying which frame. The fixtures
//! come from libvpx. See `tests/data/generate.py`.

use feos_video::vp8::{Decoded, Vp8Decoder, Vp8Stats};
use feos_video::{info, Codec, DecoderLimits, Source, VideoDecoder};

fn adler32(parts: &[&[u8]]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32); // As framecrc starts it.
    for part in parts {
        for &byte in *part {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
    }
    (b << 16) | a
}

fn reference(name: &str) -> Vec<u32> {
    let path = format!("{}/tests/data/{name}.framecrc", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let crc = line.rsplit(',').next().unwrap().trim().trim_start_matches("0x");
            u32::from_str_radix(crc, 16).unwrap()
        })
        .collect()
}

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// The packets of a WebM or IVF file, by way of the track index.
fn packets(file: &[u8]) -> Vec<Vec<u8>> {
    // The video decoder keeps its index to itself, so this reads the containers' simplest
    // property: an IVF frame header, or a walk through Matroska clusters.
    if file.starts_with(b"DKIF") {
        let mut pos = u16::from_le_bytes([file[6], file[7]]) as usize;
        let mut out = Vec::new();
        while pos + 12 <= file.len() {
            let size = u32::from_le_bytes(file[pos..pos + 4].try_into().unwrap()) as usize;
            out.push(file[pos + 12..pos + 12 + size].to_vec());
            pos += 12 + size;
        }
        return out;
    }
    let mut out = Vec::new();
    webm_blocks(file, 0, file.len(), &mut out);
    out
}

fn vint(data: &[u8], pos: usize, keep_marker: bool) -> (u64, usize) {
    let first = data[pos];
    let len = first.leading_zeros() as usize + 1;
    let mut value = if keep_marker { first as u64 } else { (first as u64) & (0xff >> len) };
    for i in 1..len {
        value = (value << 8) | data[pos + i] as u64;
    }
    (value, len)
}

fn webm_blocks(data: &[u8], mut pos: usize, end: usize, out: &mut Vec<Vec<u8>>) {
    while pos < end {
        let (id, id_len) = vint(data, pos, true);
        let (size, size_len) = vint(data, pos + id_len, false);
        let body = pos + id_len + size_len;
        let unknown = size == (1u64 << (7 * size_len)) - 1;
        let stop = if unknown { end } else { body + size as usize };
        match id {
            0x18538067 | 0x1f43b675 | 0xa0 => webm_blocks(data, body, stop, out),
            0xa3 | 0xa1 => {
                let (_, track_len) = vint(data, body, false);
                out.push(data[body + track_len + 3..stop].to_vec());
            }
            _ => {}
        }
        pos = stop;
    }
}

fn frame_crc(decoder: &Vp8Decoder) -> u32 {
    let (frame, stride) = decoder.picture().unwrap();
    let (width, height) = decoder.size();
    let (width, height) = (width as usize, height as usize);
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut bytes = Vec::with_capacity(width * height * 3 / 2);
    for row in 0..height {
        bytes.extend_from_slice(&frame.y[row * stride..row * stride + width]);
    }
    for plane in [&frame.u, &frame.v] {
        for row in 0..ch {
            bytes.extend_from_slice(&plane[row * stride / 2..row * stride / 2 + cw]);
        }
    }
    adler32(&[&bytes])
}

/// Decodes a file, compares each picture with FFmpeg's, and says what the file used.
fn check(name: &str) -> Vp8Stats {
    let file = data(name);
    let want = reference(name);
    let mut decoder = Vp8Decoder::new(1 << 28);
    let mut shown = 0;
    for packet in packets(&file) {
        match decoder.decode(&packet).unwrap() {
            Decoded::Shown => {
                let got = frame_crc(&decoder);
                assert_eq!(
                    Some(&got),
                    want.get(shown),
                    "{name}: shown frame {shown} of {} differs from FFmpeg's",
                    want.len()
                );
                shown += 1;
            }
            Decoded::Hidden | Decoded::Nothing => {}
        }
    }
    assert_eq!(shown, want.len(), "{name}");
    assert!(shown > 0);
    decoder.stats()
}

#[test]
fn inter_frames_and_golden() {
    let stats = check("vp8-inter.webm");
    assert!(stats.frames >= 30);
    assert!(stats.golden_blocks > 0, "{stats:?}");
    assert!(stats.split_blocks > 0, "{stats:?}");
    assert!(stats.intra_in_inter > 0, "{stats:?}");
}

#[test]
fn hidden_alternate_reference_frames() {
    let stats = check("vp8-altref.webm");
    assert!(stats.hidden > 0, "{stats:?}");
    assert!(stats.altref_blocks > 0, "{stats:?}");
    assert!(stats.sign_biased > 0, "{stats:?}");
}

#[test]
fn several_token_partitions() {
    let stats = check("vp8-parts.webm");
    assert!(stats.partitioned > 0, "{stats:?}");
}

#[test]
fn segmentation() {
    let stats = check("vp8-seg.webm");
    assert!(stats.segmented > 0, "{stats:?}");
}

#[test]
fn the_other_profiles() {
    for (name, version) in [("vp8-profile1.webm", 1), ("vp8-profile2.webm", 2), ("vp8-profile3.webm", 3)] {
        let stats = check(name);
        assert_eq!(stats.version, version, "{name}");
    }
}

#[test]
fn sharpness_at_a_low_bit_rate() {
    check("vp8-sharp.webm");
}

#[test]
fn an_odd_size_in_ivf() {
    let file = data("vp8-odd.ivf");
    assert_eq!(info(&file).unwrap().codec, Codec::Vp8);
    check("vp8-odd.ivf");
}

/// Through the real decoder, in memory and through a source, seeking too.
#[test]
fn through_the_video_decoder() {
    for name in ["vp8-inter.webm", "vp8-altref.webm", "vp8-odd.ivf"] {
        let file = data(name);
        let mut memory = VideoDecoder::open(file.clone()).unwrap();
        assert!(memory.info().can_decode(), "{name}");
        let source: Box<dyn Source> = Box::new(Bytes(file));
        let mut disk = VideoDecoder::from_source(source, DecoderLimits::default()).unwrap();
        assert_eq!(memory.frame_count(), disk.frame_count(), "{name}");
        let len = memory.info().rgba_len().unwrap();
        let (mut a, mut b) = (vec![0u8; len], vec![0u8; len]);
        for index in 0..memory.frame_count() {
            memory.decode(index, &mut a).unwrap();
            disk.decode(index, &mut b).unwrap();
            assert!(a == b, "{name} frame {index}");
        }
        // A jump back gives the same picture as getting there in order.
        let last = memory.frame_count() - 1;
        memory.decode(last, &mut a).unwrap();
        let keep = a.clone();
        memory.decode(0, &mut a).unwrap();
        memory.decode(last, &mut a).unwrap();
        assert!(a == keep, "{name}: after a seek");
    }
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

/// Damage of every kind: it may fail, and may look wrong, and mustn't panic.
#[test]
fn malformed_input_never_panics() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for name in ["vp8-inter.webm", "vp8-altref.webm", "vp8-parts.webm", "vp8-seg.webm", "vp8-profile3.webm", "vp8-odd.ivf"] {
        let all = packets(&data(name));
        for round in 0..40 {
            let mut decoder = Vp8Decoder::new(1 << 26);
            for (index, packet) in all.iter().take(12).enumerate() {
                let mut packet = packet.clone();
                // The first is usually left alone, so that later ones get past "no key frame".
                if index > 0 || round % 4 == 0 {
                    for _ in 0..1 + next() % 8 {
                        let kind = next() % 4;
                        let at = next() as usize % packet.len().max(1);
                        match kind {
                            0 => packet[at] = next() as u8,
                            1 => packet[at] ^= 1 << (next() % 8),
                            2 => packet.truncate(at),
                            _ => packet[at.min(9)..].iter_mut().take(4).for_each(|b| *b = next() as u8),
                        }
                        if packet.is_empty() {
                            break;
                        }
                    }
                }
                let _ = decoder.decode(&packet);
                if decoder.picture().is_some() {
                    let _ = frame_crc(&decoder);
                }
            }
        }
    }
    let mut decoder = Vp8Decoder::new(1 << 26);
    for size in 0..64 {
        let junk: Vec<u8> = (0..size).map(|_| next() as u8).collect();
        let _ = decoder.decode(&junk);
    }
    // A key frame that asks for the largest picture there is, with too little memory.
    let huge = [0x10, 0, 0, 0x9d, 0x01, 0x2a, 0xff, 0x3f, 0xff, 0x3f, 0, 0, 0];
    assert!(Vp8Decoder::new(1 << 20).decode(&huge).is_err());
}
