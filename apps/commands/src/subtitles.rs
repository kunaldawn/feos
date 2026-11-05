//! SubRip (`.srt`) subtitles.
//!
//! A file is a list of cues: a number, a line of times, and text up to a blank
//! line. This is forgiving about what real files do: CRLF line ends, a byte order
//! mark, a missing number, cues out of order, and `<i>`-style tags in the text.
//! All memory is taken fallibly, and a file is only so big.

use alloc::vec::Vec;

/// Largest subtitle file read, and the most cues kept.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_CUES: usize = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    /// Where its text is in [`Subtitles::text`].
    from: u32,
    to: u32,
}

/// A parsed subtitle file.
#[derive(Default)]
pub struct Subtitles {
    cues: Vec<Cue>,
    text: Vec<u8>,
}

/// Reads `hh:mm:ss,mmm`, or with a dot. The hours can have any width.
fn parse_time(text: &str) -> Option<u64> {
    let (clock, millis) = text.trim().split_once([',', '.'])?;
    let mut parts = clock.split(':');
    let hours: u64 = parts.next()?.trim().parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    let digits = millis.trim();
    let digits = digits.get(..digits.len().min(3))?;
    let mut value: u64 = digits.parse().ok()?;
    for _ in digits.len()..3 {
        value *= 10;
    }
    Some(hours.checked_mul(3600)?.checked_add(minutes * 60 + seconds)? * 1000 + value)
}

/// Reads `start --> end`, which may have position data after it.
fn parse_times(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    Some((parse_time(start)?, parse_time(end)?))
}

/// Copies text, leaving out tags: `<...>` and `{...}`.
fn push_without_tags(out: &mut Vec<u8>, line: &str) -> Option<()> {
    let mut skip_until = None;
    for ch in line.chars() {
        match skip_until {
            Some(end) => {
                if ch == end {
                    skip_until = None;
                }
                continue;
            }
            None if ch == '<' => skip_until = Some('>'),
            None if ch == '{' => skip_until = Some('}'),
            None => {
                let mut bytes = [0u8; 4];
                let bytes = ch.encode_utf8(&mut bytes).as_bytes();
                out.try_reserve(bytes.len()).ok()?;
                out.extend_from_slice(bytes);
            }
        }
    }
    Some(())
}

impl Subtitles {
    /// Parses a file. Text that isn't UTF-8 is read as Latin-1, as old files are.
    /// Returns `None` when memory is short or there are no cues at all.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let data = data.get(..data.len().min(MAX_FILE_BYTES))?;
        let data = data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data);
        let mut decoded = Vec::new();
        let text = match core::str::from_utf8(data) {
            Ok(text) => text,
            Err(_) => {
                decoded.try_reserve(data.len() * 2).ok()?;
                for &byte in data {
                    let mut bytes = [0u8; 4];
                    decoded.extend_from_slice((byte as char).encode_utf8(&mut bytes).as_bytes());
                }
                core::str::from_utf8(&decoded).ok()?
            }
        };

        let mut result = Self::default();
        let mut open: Option<Cue> = None;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(times) = parse_times(line) {
                // A cue's blank line can be missing.
                if let Some(cue) = open.take() {
                    result.finish(cue)?;
                }
                open = Some(Cue {
                    start_ms: times.0,
                    end_ms: times.1,
                    from: result.text.len() as u32,
                    to: 0,
                });
            } else if line.trim().is_empty() {
                if let Some(cue) = open.take() {
                    result.finish(cue)?;
                }
            } else if let Some(cue) = open.as_ref() {
                if cue.from as usize != result.text.len() {
                    result.text.try_reserve(1).ok()?;
                    result.text.push(b'\n');
                }
                push_without_tags(&mut result.text, line)?;
            }
        }
        if let Some(cue) = open.take() {
            result.finish(cue)?;
        }
        if result.cues.is_empty() {
            return None;
        }
        result.cues.sort_unstable_by_key(|cue| cue.start_ms);
        Some(result)
    }

    fn finish(&mut self, mut cue: Cue) -> Option<()> {
        if self.cues.len() >= MAX_CUES {
            return Some(());
        }
        cue.to = self.text.len() as u32;
        self.cues.try_reserve(1).ok()?;
        self.cues.push(cue);
        Some(())
    }

    /// The number of cues.
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Whether there are no cues.
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// What's on screen at a time, with `\n` between lines.
    pub fn at(&self, ms: u64) -> Option<&str> {
        let next = self.cues.partition_point(|cue| cue.start_ms <= ms);
        // Long cues can overlap the next one: look back a little.
        self.cues[next.saturating_sub(4)..next]
            .iter()
            .rev()
            .find(|cue| ms < cue.end_ms)
            .and_then(|cue| {
                core::str::from_utf8(self.text.get(cue.from as usize..cue.to as usize)?).ok()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_file_parses() {
        let subs = Subtitles::parse(
            b"1\n00:00:01,000 --> 00:00:02,500\nHello\nworld\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n",
        )
        .unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs.at(0), None);
        assert_eq!(subs.at(1000), Some("Hello\nworld"));
        assert_eq!(subs.at(2499), Some("Hello\nworld"));
        assert_eq!(subs.at(2500), None);
        assert_eq!(subs.at(3500), Some("Bye"));
        assert_eq!(subs.at(9000), None);
    }

    #[test]
    fn crlf_a_byte_order_mark_tags_and_dots_are_handled() {
        let subs = Subtitles::parse(
            b"\xef\xbb\xbf1\r\n00:00:01.5 --> 00:00:02.000 X1:0\r\n<i>Soft</i> {\\an8}words\r\n\r\n",
        )
        .unwrap();
        assert_eq!(subs.at(1500), Some("Soft words"));
        assert_eq!(subs.at(1499), None);
    }

    #[test]
    fn latin1_is_read_as_such() {
        let subs = Subtitles::parse(b"00:00:00,000 --> 00:00:01,000\ncaf\xe9\n").unwrap();
        assert_eq!(subs.at(0), Some("caf\u{e9}"));
    }

    #[test]
    fn cues_out_of_order_and_overlapping_are_found() {
        let subs = Subtitles::parse(
            b"00:00:05,000 --> 00:00:06,000\nlater\n\n00:00:01,000 --> 00:00:09,000\nlong\n\n00:00:02,000 --> 00:00:03,000\nshort\n",
        )
        .unwrap();
        assert_eq!(subs.at(1500), Some("long"));
        assert_eq!(subs.at(2500), Some("short"));
        assert_eq!(subs.at(3500), Some("long"));
        assert_eq!(subs.at(5500), Some("later"));
    }

    #[test]
    fn nonsense_is_not_a_subtitle_file_and_does_not_panic() {
        assert!(Subtitles::parse(b"").is_none());
        assert!(Subtitles::parse(b"no times here\n\nat all").is_none());
        assert!(Subtitles::parse(b"99:99:99,999 --> 00:00:01,000\nx").is_none());
        let big = b"99999999999999999999:00:00,000 --> 00:00:01,000\nx\n";
        assert!(Subtitles::parse(big).is_none());
        for cut in 0..40 {
            let file = b"1\n00:00:01,000 --> 00:00:02,500\nHello\xc3\xa9\n";
            let _ = Subtitles::parse(&file[..cut]);
        }
    }
}
