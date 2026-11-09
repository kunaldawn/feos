//! What a playlist shows about a file before it plays: the artist and title
//! from an MP3's ID3v2 tag, and how long it is, from its first frame.

use feos_text::Text;

/// The longest tag read, so a tag with a big picture in it costs no more.
pub const TAG_READ_MAX: usize = 16 * 1024;

/// What a file's tag says.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Tags {
    /// `TIT2`, or `TT2` in version 2.2.
    pub title: Text<64>,
    /// `TPE1`, or `TP1`.
    pub artist: Text<64>,
}

/// The bytes an ID3v2 tag at the start of `head` takes, header included, or 0.
pub fn id3_size(head: &[u8]) -> usize {
    if head.len() < 10 || &head[..3] != b"ID3" {
        return 0;
    }
    let size = synchsafe(&head[6..10]);
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

fn synchsafe(bytes: &[u8]) -> usize {
    bytes.iter().fold(0usize, |value, byte| (value << 7) | (*byte & 0x7f) as usize)
}

/// Reads the title and artist from an ID3v2 tag at the start of `data`.
pub fn read_id3(data: &[u8]) -> Tags {
    let mut tags = Tags::default();
    if data.len() < 10 || &data[..3] != b"ID3" {
        return tags;
    }
    let version = data[3];
    let flags = data[5];
    let end = id3_size(data).min(data.len());
    let mut at = 10;
    // An extended header is skipped; unsynchronised tags are too rare to bother.
    if flags & 0x40 != 0 && data.len() > at + 4 {
        let size = if version >= 4 {
            synchsafe(&data[at..at + 4])
        } else {
            u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize + 4
        };
        at += size;
    }
    let (id_len, header_len) = if version == 2 { (3, 6) } else { (4, 10) };
    while at + header_len <= end {
        let id = &data[at..at + id_len];
        if id[0] == 0 {
            break;
        }
        let size = match version {
            2 => ((data[at + 3] as usize) << 16) | ((data[at + 4] as usize) << 8) | data[at + 5] as usize,
            4 => synchsafe(&data[at + 4..at + 8]),
            _ => u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize,
        };
        let body_start = at + header_len;
        let body_end = body_start.saturating_add(size).min(end);
        let body = &data[body_start..body_end];
        match id {
            b"TIT2" | b"TT2" => decode_text(body, &mut tags.title),
            b"TPE1" | b"TP1" => decode_text(body, &mut tags.artist),
            _ => {}
        }
        if size == 0 {
            break;
        }
        at = body_start.saturating_add(size);
    }
    tags
}

/// A text frame's body: an encoding byte, then the text. Characters outside
/// ASCII become `?`, as the skin's font has only ASCII.
fn decode_text<const N: usize>(body: &[u8], out: &mut Text<N>) {
    out.clear();
    let Some((&encoding, text)) = body.split_first() else {
        return;
    };
    let mut push = |ch: char| {
        if ch == '\0' {
            return false;
        }
        let ch = if (' '..='~').contains(&ch) { ch } else { '?' };
        let mut buffer = [0u8; 4];
        out.push_str(ch.encode_utf8(&mut buffer))
    };
    match encoding {
        1 | 2 => {
            // UTF-16, with a byte-order mark (1) or big-endian without (2).
            let mut big = encoding == 2;
            let mut units = text.chunks_exact(2);
            let mut first = true;
            for pair in &mut units {
                let unit = if big {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                };
                if first && encoding == 1 {
                    first = false;
                    if unit == 0xfffe {
                        big = !big;
                        continue;
                    }
                    if unit == 0xfeff {
                        continue;
                    }
                }
                let ch = char::from_u32(unit as u32).unwrap_or('?');
                if !push(ch) {
                    break;
                }
            }
        }
        3 => {
            let text = match core::str::from_utf8(text) {
                Ok(text) => text,
                Err(error) => core::str::from_utf8(&text[..error.valid_up_to()]).unwrap_or(""),
            };
            for ch in text.chars() {
                if !push(ch) {
                    break;
                }
            }
        }
        _ => {
            for &byte in text {
                if !push(byte as char) {
                    break;
                }
            }
        }
    }
    let trimmed = Text::<N>::from_str(out.as_str().trim());
    *out = trimmed;
}

/// The bitrate (kbps) and sample rate (Hz) of an MPEG audio frame header.
fn frame_header(bytes: &[u8]) -> Option<(u32, u32, usize)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (bytes[1] >> 3) & 3; // 3: MPEG 1, 2: MPEG 2, 0: MPEG 2.5
    let layer = (bytes[1] >> 1) & 3; // 1: layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_index = (bytes[2] >> 4) as usize;
    let rate_index = ((bytes[2] >> 2) & 3) as usize;
    if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    const MPEG1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const MPEG2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let (bitrate, base_rate) = if version == 3 {
        (MPEG1[bitrate_index], [44_100, 48_000, 32_000][rate_index])
    } else {
        (MPEG2[bitrate_index], [22_050, 24_000, 16_000][rate_index])
    };
    let rate = if version == 0 { base_rate / 2 } else { base_rate };
    let samples_per_frame = if version == 3 { 1152 } else { 576 };
    Some((bitrate, rate, samples_per_frame))
}

/// How long an MP3 lasts, in milliseconds, from the first frame after its tag:
/// a Xing or Info header's frame count if it has one, or else the bitrate
/// (right for constant-bitrate files). `head` is the start of the file and
/// `size` the whole file's length.
pub fn mp3_duration_ms(head: &[u8], size: u64) -> Option<u64> {
    let start = id3_size(head);
    let search = head.get(start..)?;
    let offset = search.windows(4).take(4096).position(|w| frame_header(w).is_some())?;
    let frame = &search[offset..];
    let (bitrate, rate, samples) = frame_header(frame)?;
    // A Xing/Info header sits after the side information.
    for marker in [&b"Xing"[..], &b"Info"[..]] {
        if let Some(at) = frame.windows(4).take(64).position(|w| w == marker) {
            let flags = frame.get(at + 4..at + 8)?;
            if flags[3] & 1 != 0 {
                let count = frame.get(at + 8..at + 12)?;
                let frames = u32::from_be_bytes([count[0], count[1], count[2], count[3]]) as u64;
                return Some(frames * samples as u64 * 1000 / rate as u64);
            }
        }
    }
    let audio_bytes = size.saturating_sub((start + offset) as u64);
    Some(audio_bytes * 8 / bitrate as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn tag(version: u8, frames: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, text) in frames {
            body.extend_from_slice(id);
            let size = text.len() as u32;
            if version == 4 {
                body.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
            } else {
                body.extend_from_slice(&size.to_be_bytes());
            }
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(text);
        }
        let size = body.len() as u32;
        let mut out = std::vec![b'I', b'D', b'3', version, 0, 0];
        out.extend_from_slice(&[(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn reads_latin1_and_utf8_text_frames() {
        let data = tag(3, &[(b"TIT2", b"\0Iron Pulse"), (b"TPE1", b"\x03The FeOS Band\0")]);
        let tags = read_id3(&data);
        assert_eq!(tags.title.as_str(), "Iron Pulse");
        assert_eq!(tags.artist.as_str(), "The FeOS Band");
        assert_eq!(id3_size(&data), data.len());
    }

    #[test]
    fn reads_utf16_with_either_byte_order() {
        let little = b"\x01\xff\xfeH\0i\0\0\0";
        let big = b"\x01\xfe\xff\0H\0i";
        let data = tag(4, &[(b"TIT2", little), (b"TPE1", big)]);
        let tags = read_id3(&data);
        assert_eq!(tags.title.as_str(), "Hi");
        assert_eq!(tags.artist.as_str(), "Hi");
    }

    #[test]
    fn junk_is_ignored() {
        assert_eq!(read_id3(b"not a tag at all"), Tags::default());
        let mut broken = tag(3, &[(b"TIT2", b"\0Short")]);
        broken[9] = 0x7f; // A size far past the end.
        let _ = read_id3(&broken);
        assert_eq!(read_id3(b"ID3\x03\0\0\0\0\0\x05\xff\xff"), Tags::default());
    }

    #[test]
    fn constant_bitrate_duration() {
        // MPEG 1 layer III, 128 kbps, 44.1 kHz.
        let mut head = tag(3, &[(b"TIT2", b"\0x")]);
        let start = head.len();
        head.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        head.resize(start + 400, 0);
        let size = start as u64 + 16_000 * 60; // a minute at 16000 bytes a second
        assert_eq!(mp3_duration_ms(&head, size), Some(60_000));
        assert_eq!(mp3_duration_ms(b"RIFF....", 100), None);
    }
}
