//! Animated Windows cursors (`.ani`).
//!
//! An animated cursor is a RIFF file of form `ACON`:
//!
//! - `anih`: a 36-byte header giving the number of frames, the number of steps
//!   in one cycle, the default time a step shows (in jiffies, 1/60 s) and flags.
//!   Flag 1 says the frames are cursor or icon files (the only kind decoded
//!   here); flag 2 says a `seq ` chunk orders them.
//! - `rate` (optional): one time per step, in jiffies.
//! - `seq ` (optional): one frame index per step.
//! - `LIST` of type `fram`: one `icon` chunk per frame, each a whole `.cur`
//!   (or `.ico`) file, decoded with [`crate::cur`].
//! - Anything else, such as a `LIST` of type `INFO` with the title and author,
//!   is skipped.
//!
//! [`Animation::parse`] only finds the chunks; nothing is copied or allocated,
//! and a frame is decoded when it's asked for.

use crate::cur::{self, CursorInfo};
use crate::error::{ImageError, Result};

/// The most frames or steps accepted. Real cursors have a few dozen.
pub const MAX_STEPS: u32 = 4096;
/// Flag: frames are icon or cursor files rather than raw bitmaps.
const FLAG_ICON: u32 = 1;
/// Flag: a `seq ` chunk orders the frames.
const FLAG_SEQUENCE: u32 = 2;
/// Milliseconds in 60 jiffies.
const JIFFY_MS_TIMES_60: u64 = 1000;

/// A parsed animated cursor, borrowing the file.
#[derive(Clone, Copy, Debug)]
pub struct Animation<'a> {
    frames: u32,
    steps: u32,
    jiffies: u32,
    rate: Option<&'a [u8]>,
    sequence: Option<&'a [u8]>,
    list: &'a [u8],
}

fn read_u32(input: &[u8], at: usize) -> Option<u32> {
    let bytes = input.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Walks the chunks of `body`, calling `visit` with each id and payload. A chunk
/// that runs past the end is cut short to what's there.
fn chunks<'a>(body: &'a [u8], mut visit: impl FnMut(&'a [u8], &'a [u8]) -> Result<()>) -> Result<()> {
    let mut at = 0usize;
    while at + 8 <= body.len() {
        let id = &body[at..at + 4];
        let size = read_u32(body, at + 4).ok_or(ImageError::UnexpectedEnd)? as usize;
        let start = at + 8;
        let end = start.saturating_add(size).min(body.len());
        visit(id, &body[start..end])?;
        // Chunks are padded to an even length.
        at = start.saturating_add(size).saturating_add(size & 1);
    }
    Ok(())
}

/// Whether `input` looks like an animated cursor.
pub fn is_magic(input: &[u8]) -> bool {
    input.len() >= 12 && &input[..4] == b"RIFF" && &input[8..12] == b"ACON"
}

impl<'a> Animation<'a> {
    /// Finds the parts of an animated cursor and checks they agree.
    pub fn parse(input: &'a [u8]) -> Result<Self> {
        if !is_magic(input) {
            return Err(ImageError::UnknownFormat);
        }
        let declared = read_u32(input, 4).ok_or(ImageError::UnexpectedEnd)? as usize;
        let body = &input[12..input.len().min(declared.saturating_add(8)).max(12)];
        let mut header = None;
        let mut rate = None;
        let mut sequence = None;
        let mut list = None;
        chunks(body, |id, data| {
            match id {
                b"anih" => header = Some(data),
                b"rate" => rate = Some(data),
                b"seq " => sequence = Some(data),
                b"LIST" if data.len() >= 4 && &data[..4] == b"fram" => list = Some(&data[4..]),
                _ => {}
            }
            Ok(())
        })?;
        let header = header.ok_or(ImageError::InvalidData("ani has no anih chunk"))?;
        if header.len() < 36 {
            return Err(ImageError::InvalidData("ani header too short"));
        }
        let field = |index: usize| read_u32(header, index * 4).unwrap_or(0);
        let (frames, steps, jiffies, flags) = (field(1), field(2), field(7), field(8));
        if flags & FLAG_ICON == 0 {
            return Err(ImageError::UnsupportedFeature("ani with raw bitmap frames"));
        }
        if frames == 0 || frames > MAX_STEPS {
            return Err(ImageError::InvalidData("ani frame count"));
        }
        // Writers that leave the step count at zero mean one step a frame.
        let steps = if steps == 0 { frames } else { steps };
        if steps > MAX_STEPS {
            return Err(ImageError::InvalidData("ani step count"));
        }
        let list = list.ok_or(ImageError::InvalidData("ani has no frame list"))?;
        let rate = match rate {
            Some(rate) if rate.len() >= steps as usize * 4 => Some(rate),
            Some(_) => return Err(ImageError::InvalidData("ani rate chunk too short")),
            None => None,
        };
        let sequence = match sequence {
            Some(sequence) if flags & FLAG_SEQUENCE != 0 => {
                if sequence.len() < steps as usize * 4 {
                    return Err(ImageError::InvalidData("ani seq chunk too short"));
                }
                for step in 0..steps as usize {
                    if read_u32(sequence, step * 4).is_none_or(|frame| frame >= frames) {
                        return Err(ImageError::InvalidData("ani seq names a missing frame"));
                    }
                }
                Some(sequence)
            }
            _ => None,
        };
        if sequence.is_none() && steps > frames {
            return Err(ImageError::InvalidData("ani has more steps than frames"));
        }
        let animation = Self {
            frames,
            steps,
            jiffies,
            rate,
            sequence,
            list,
        };
        // Every frame must be there, so a draw can't find one missing later.
        let mut found = 0u32;
        chunks(list, |id, _| {
            if id == b"icon" {
                found += 1;
            }
            Ok(())
        })?;
        if found < frames {
            return Err(ImageError::InvalidData("ani frame list is short"));
        }
        Ok(animation)
    }

    /// How many distinct images there are.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// How many steps one cycle shows.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// The image shown at `step`.
    pub fn frame_at_step(&self, step: u32) -> u32 {
        let step = step % self.steps;
        match self.sequence {
            Some(sequence) => read_u32(sequence, step as usize * 4).unwrap_or(0),
            None => step,
        }
    }

    /// How long `step` shows, in jiffies (1/60 s), never zero.
    pub fn jiffies_at_step(&self, step: u32) -> u32 {
        let step = step % self.steps;
        let jiffies = match self.rate {
            Some(rate) => read_u32(rate, step as usize * 4).unwrap_or(self.jiffies),
            None => self.jiffies,
        };
        jiffies.clamp(1, 60 * 60)
    }

    /// How long one cycle takes, in milliseconds, never zero.
    pub fn cycle_ms(&self) -> u64 {
        (0..self.steps).map(|step| self.step_ms(step)).sum()
    }

    /// How long `step` shows, in milliseconds, never zero.
    pub fn step_ms(&self, step: u32) -> u64 {
        (u64::from(self.jiffies_at_step(step)) * JIFFY_MS_TIMES_60 / 60).max(1)
    }

    /// The step showing `elapsed_ms` after the animation started, and how many
    /// milliseconds are left until the next one.
    pub fn step_at(&self, elapsed_ms: u64) -> (u32, u64) {
        let mut into = elapsed_ms % self.cycle_ms();
        for step in 0..self.steps {
            let length = self.step_ms(step);
            if into < length {
                return (step, length - into);
            }
            into -= length;
        }
        (self.steps - 1, 1)
    }

    /// The encoded cursor file of image `frame`.
    pub fn frame(&self, frame: u32) -> Result<&'a [u8]> {
        let mut index = 0u32;
        let mut found = None;
        chunks(self.list, |id, data| {
            if id == b"icon" {
                if index == frame && found.is_none() {
                    found = Some(data);
                }
                index += 1;
            }
            Ok(())
        })?;
        found.ok_or(ImageError::InvalidData("ani frame missing"))
    }

    /// Decodes image `frame` (see [`cur::decode`]). Frames stored as icons
    /// rather than cursors decode with their hot spot at the top left.
    pub fn decode_frame(
        &self,
        frame: u32,
        out: &mut [u8],
        invert: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<CursorInfo> {
        let data = self.frame(frame)?;
        if crate::ico::is_magic(data) {
            let info = crate::ico::decode(data, out, scratch)?;
            if !invert.is_empty() {
                let len = (info.width as usize * info.height as usize).div_ceil(8);
                let available = invert.len();
                invert.get_mut(..len).ok_or(ImageError::BufferTooSmall {
                    required: len,
                    available,
                })?.fill(0);
            }
            return Ok(CursorInfo {
                width: info.width,
                height: info.height,
                hotspot_x: 0,
                hotspot_y: 0,
            });
        }
        cur::decode(data, out, invert, scratch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn chunk(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    /// A 1x1 cursor whose single pixel is white when `white`, black otherwise.
    fn pixel_cursor(white: bool) -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        dib.extend_from_slice(&[if white { 0x80 } else { 0 }, 0, 0, 0]);
        dib.extend_from_slice(&[0, 0, 0, 0]);
        let mut file = vec![0, 0, 2, 0, 1, 0, 1, 1, 2, 0, 0, 0, 0, 0];
        file.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        file.extend_from_slice(&22u32.to_le_bytes());
        file.extend_from_slice(&dib);
        file
    }

    fn header(frames: u32, steps: u32, jiffies: u32, flags: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for value in [36, frames, steps, 0, 0, 0, 0, jiffies, flags] {
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }
        out
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    fn riff(parts: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = parts.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"ACON");
        out.extend_from_slice(&body);
        out
    }

    fn frames(images: &[Vec<u8>]) -> Vec<u8> {
        let mut list = b"fram".to_vec();
        for image in images {
            list.extend_from_slice(&chunk(b"icon", image));
        }
        chunk(b"LIST", &list)
    }

    fn sample() -> Vec<u8> {
        riff(&[
            chunk(b"LIST", b"INFOINAM\x03\x00\x00\x00abc\x00"),
            chunk(b"anih", &header(2, 3, 6, 3)),
            chunk(b"rate", &words(&[6, 12, 30])),
            chunk(b"seq ", &words(&[0, 1, 0])),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ])
    }

    #[test]
    fn steps_follow_the_sequence_and_rates() {
        let file = sample();
        let animation = Animation::parse(&file).unwrap();
        assert_eq!((animation.frames(), animation.steps()), (2, 3));
        assert_eq!(animation.cycle_ms(), 800);
        assert_eq!(animation.step_at(0), (0, 100));
        assert_eq!(animation.step_at(150), (1, 150));
        assert_eq!(animation.step_at(350), (2, 450));
        assert_eq!(animation.step_at(850), (0, 50));
        assert_eq!(animation.frame_at_step(1), 1);
        assert_eq!(animation.frame_at_step(2), 0);
        let mut rgba = [0u8; 4];
        let info = animation.decode_frame(1, &mut rgba, &mut [], &mut []).unwrap();
        assert_eq!((info.width, info.hotspot_x), (1, 0));
        assert_eq!(rgba, [255, 255, 255, 255]);
        animation.decode_frame(0, &mut rgba, &mut [], &mut []).unwrap();
        assert_eq!(rgba, [0, 0, 0, 255]);
        assert!(animation.frame(2).is_err());
    }

    #[test]
    fn without_rate_or_sequence_each_frame_is_a_step() {
        let file = riff(&[
            chunk(b"anih", &header(2, 2, 30, 1)),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        let animation = Animation::parse(&file).unwrap();
        assert_eq!(animation.cycle_ms(), 1000);
        assert_eq!(animation.step_at(600).0, 1);
        assert_eq!(animation.frame_at_step(1), 1);
    }

    #[test]
    fn inconsistent_files_are_rejected() {
        let missing_frame = riff(&[
            chunk(b"anih", &header(3, 3, 6, 1)),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        assert!(Animation::parse(&missing_frame).is_err());
        let bad_sequence = riff(&[
            chunk(b"anih", &header(2, 2, 6, 3)),
            chunk(b"seq ", &words(&[0, 5])),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        assert!(Animation::parse(&bad_sequence).is_err());
        let short_rate = riff(&[
            chunk(b"anih", &header(2, 2, 6, 1)),
            chunk(b"rate", &words(&[6])),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        assert!(Animation::parse(&short_rate).is_err());
        let raw = riff(&[
            chunk(b"anih", &header(2, 2, 6, 0)),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        assert!(Animation::parse(&raw).is_err());
        assert!(Animation::parse(b"RIFF\x04\x00\x00\x00ACON").is_err());
    }

    #[test]
    fn zero_rates_still_advance() {
        let file = riff(&[
            chunk(b"anih", &header(2, 2, 0, 1)),
            frames(&[pixel_cursor(false), pixel_cursor(true)]),
        ]);
        let animation = Animation::parse(&file).unwrap();
        assert!(animation.cycle_ms() > 0);
        let _ = animation.step_at(u64::MAX);
    }

    #[test]
    fn every_truncation_fails_cleanly() {
        let file = sample();
        for end in 0..file.len() {
            if let Ok(animation) = Animation::parse(&file[..end]) {
                for frame in 0..animation.frames() {
                    let mut rgba = [0u8; 4];
                    let _ = animation.decode_frame(frame, &mut rgba, &mut [0u8; 1], &mut []);
                }
            }
        }
    }
}
