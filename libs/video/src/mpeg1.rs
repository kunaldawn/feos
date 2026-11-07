//! MPEG-1 video (ISO/IEC 11172-2), in elementary streams and program streams.
//!
//! Ported from PL_MPEG by Dominic Szablewski (MIT). The picture loop, the motion
//! compensation and the fixed-point inverse DCT all follow it. Hostile input is
//! handled here: reads past the end return zeros, every picture index is checked,
//! and all memory is taken fallibly and within the decoder's limits.

use alloc::vec::Vec;

use crate::audio::{push_packet, AudioCodec, AudioPacket, AudioTrack};
use crate::error::{Result, VideoError};
use crate::index::{DecoderLimits, Index, FLAG_KEY};
use crate::mpeg1_tables::*;
use crate::pixel::YuvCoefficients;

const SEQUENCE: u8 = 0xb3;
const GROUP: u8 = 0xb8;
const PICTURE: u8 = 0x00;
const PACK: u8 = 0xba;
const SYSTEM_HEADER: u8 = 0xbb;
const VIDEO_STREAM: u8 = 0xe0;

const INTRA: u8 = 1;
const PREDICTIVE: u8 = 2;
const BIDIRECTIONAL: u8 = 3;

/// The standard's picture rates, by code.
const RATES: [(u32, u32); 16] = [
    (25, 1),
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (25, 1),
    (25, 1),
    (25, 1),
    (25, 1),
    (25, 1),
    (25, 1),
    (25, 1),
];

/// Whether the data starts like one of the streams this reads.
pub(crate) fn is_magic(input: &[u8]) -> bool {
    input.len() >= 12
        && input[..3] == [0, 0, 1]
        && (input[3] == SEQUENCE || (input[3] == PACK && input[4] & 0xf0 == 0x20))
}

/// Finds the next start code at or after `pos`: its position after the prefix.
fn next_start_code(input: &[u8], mut pos: usize) -> Option<usize> {
    while pos + 3 < input.len() {
        match input[pos + 2] {
            0 => pos += 1,
            1 if input[pos] == 0 && input[pos + 1] == 0 => return Some(pos + 3),
            1 => pos += 3,
            _ => pos += 3,
        }
    }
    None
}

/// The video of a program stream, packet payloads joined together.
fn demux_program(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = next_start_code(input, pos) {
        let code = input[at];
        match code {
            PACK => pos = at + 9,
            SYSTEM_HEADER | 0xbc..=0xff => {
                let Some(length) = input.get(at + 1..at + 3) else {
                    break;
                };
                let length = u16::from_be_bytes([length[0], length[1]]) as usize;
                let end = (at + 3 + length).min(input.len());
                if code == VIDEO_STREAM {
                    let mut body = at + 3;
                    // Stuffing, then a buffer size, and then time stamps.
                    while input.get(body) == Some(&0xff) {
                        body += 1;
                    }
                    if input.get(body).is_some_and(|byte| byte & 0xc0 == 0x40) {
                        body += 2;
                    }
                    body += match input.get(body).map(|byte| byte & 0xf0) {
                        Some(0x20) => 5,
                        Some(0x30) => 10,
                        _ => 1,
                    };
                    if body < end {
                        out.try_reserve(end - body).map_err(|_| VideoError::OutOfMemory)?;
                        out.extend_from_slice(&input[body..end]);
                    }
                }
                pos = end;
            }
            _ => pos = at + 1,
        }
    }
    Ok(out)
}

/// A packet's time stamp and where its data starts, in MPEG-1 system syntax: stuffing,
/// perhaps a buffer size, and then perhaps time stamps.
fn pes_body(input: &[u8], at: usize, end: usize) -> Option<(usize, Option<u64>)> {
    let mut body = at;
    while input.get(body) == Some(&0xff) {
        body += 1;
    }
    if input.get(body)? & 0xc0 == 0x40 {
        body += 2;
    }
    let marker = *input.get(body)? & 0xf0;
    let pts = match marker {
        0x20 | 0x30 => {
            let bytes = input.get(body..body + 5)?;
            Some(
                ((bytes[0] as u64 >> 1) & 7) << 30
                    | (bytes[1] as u64) << 22
                    | (bytes[2] as u64 >> 1) << 15
                    | (bytes[3] as u64) << 7
                    | bytes[4] as u64 >> 1,
            )
        }
        _ => None,
    };
    body += match marker {
        0x20 => 5,
        0x30 => 10,
        _ => 1,
    };
    (body <= end).then_some((body, pts))
}

/// The first audio stream of a program stream. Its packets don't keep to the audio's
/// frames, hence [`AudioCodec::MpegSplit`].
pub(crate) fn audio_track(input: &[u8]) -> Option<AudioTrack> {
    if input.get(3) != Some(&PACK) {
        return None;
    }
    let mut packets = Vec::new();
    let mut stream = None;
    let mut first_video = None;
    let mut pos = 0;
    let mut last_pts = None;
    let mut header = None;
    while let Some(at) = next_start_code(input, pos) {
        let code = input[at];
        match code {
            PACK => pos = at + 9,
            SYSTEM_HEADER | 0xbc..=0xff => {
                let length = input.get(at + 1..at + 3)?;
                let length = u16::from_be_bytes([length[0], length[1]]) as usize;
                let end = (at + 3 + length).min(input.len());
                pos = end;
                if code == VIDEO_STREAM && first_video.is_none() {
                    first_video = pes_body(input, at + 3, end).and_then(|(_, pts)| pts);
                } else if (0xc0..=0xdf).contains(&code) && *stream.get_or_insert(code) == code {
                    let Some((body, pts)) = pes_body(input, at + 3, end) else {
                        continue;
                    };
                    if body == end {
                        continue;
                    }
                    let pts = pts.or(last_pts);
                    last_pts = pts;
                    header = header.or_else(|| first_frame_format(&input[body..end]));
                    // Times are the picture's until the track knows what its first is.
                    push_packet(&mut packets, input.len(), body, end - body, pts.unwrap_or(0))?;
                }
            }
            _ => pos = at + 1,
        }
    }
    let (sample_rate, channels) = header?;
    // A stream's clock doesn't start at zero, and the picture's frames do.
    let base = first_video.or_else(|| packets.first().map(|packet: &AudioPacket| packet.pts_ms))?;
    for packet in &mut packets {
        packet.pts_ms = packet.pts_ms.saturating_sub(base) / 90;
    }
    Some(AudioTrack {
        codec: AudioCodec::MpegSplit,
        sample_rate,
        channels,
        config: 0..0,
        packets,
    })
}

/// The rate and channels of the first MPEG audio frame header in a packet.
fn first_frame_format(data: &[u8]) -> Option<(u32, u16)> {
    data.windows(4).find_map(|head| {
        if head[0] != 0xff || head[1] & 0xe0 != 0xe0 {
            return None;
        }
        let version = (head[1] >> 3) & 3;
        let layer = (head[1] >> 1) & 3;
        let rate = (head[2] >> 2) & 3;
        if version == 1 || layer == 0 || rate == 3 || head[2] >> 4 == 15 {
            return None;
        }
        let base = [44_100, 48_000, 32_000][rate as usize];
        let rate = match version {
            3 => base,
            2 => base / 2,
            _ => base / 4,
        };
        Some((rate, if head[3] >> 6 == 3 { 1 } else { 2 }))
    })
}

/// Properties from a sequence header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mpeg1Info {
    pub width: u32,
    pub height: u32,
    pub frame_rate: (u32, u32),
    pub frames: u32,
}

/// The elementary stream to look at: the input, or the first part of a program
/// stream's video.
fn sequence_header(stream: &[u8]) -> Result<usize> {
    let mut pos = 0;
    while let Some(at) = next_start_code(stream, pos) {
        if stream[at] == SEQUENCE {
            return Ok(at + 1);
        }
        pos = at;
    }
    Err(VideoError::InvalidData("MPEG-1 sequence header"))
}

fn header_fields(stream: &[u8]) -> Result<(u32, u32, (u32, u32))> {
    let at = sequence_header(stream)?;
    let bytes = stream.get(at..at + 4).ok_or(VideoError::UnexpectedEnd)?;
    let width = ((bytes[0] as u32) << 4) | (bytes[1] as u32 >> 4);
    let height = (((bytes[1] & 0x0f) as u32) << 8) | bytes[2] as u32;
    if width == 0 || height == 0 {
        return Err(VideoError::InvalidData("MPEG-1 frame size"));
    }
    Ok((width, height, RATES[(bytes[3] & 0x0f) as usize]))
}

/// Reads a file's properties without building anything that stays.
pub(crate) fn info(input: &[u8]) -> Result<Mpeg1Info> {
    let owned;
    let stream: &[u8] = if input[3] == PACK {
        owned = demux_program(input)?;
        &owned
    } else {
        input
    };
    let (width, height, frame_rate) = header_fields(stream)?;
    let mut frames = 0u32;
    let mut pos = 0;
    while let Some(at) = next_start_code(stream, pos) {
        if stream[at] == PICTURE {
            frames += 1;
        }
        pos = at;
    }
    Ok(Mpeg1Info {
        width,
        height,
        frame_rate,
        frames,
    })
}

/// One coded picture.
#[derive(Clone, Copy)]
struct Picture {
    start: usize,
    end: usize,
    kind: u8,
    /// Where it comes in display order.
    display: u32,
}

/// A decoder.
pub(crate) struct Mpeg1Stream {
    /// The elementary stream, when it had to be pulled out of a program stream.
    owned: Vec<u8>,
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    luma_width: usize,
    chroma_width: usize,
    chroma_start: [usize; 2],
    frames: [Vec<u8>; 3],
    /// Which frame is which, and what's in each.
    current: usize,
    forward: usize,
    backward: usize,
    /// What each frame holds, by display number.
    holds: [Option<u32>; 3],
    pictures: Vec<Picture>,
    next: usize,
    intra_quant: [u8; 64],
    non_intra_quant: [u8; 64],
    coefficients: YuvCoefficients,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    #[inline]
    fn read1(&mut self) -> u32 {
        let byte = self.data.get(self.pos >> 3).copied().unwrap_or(0);
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        bit as u32
    }

    fn read(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.read1();
        }
        value
    }

    fn peek_nonzero(&self, count: usize) -> bool {
        (0..count).any(|at| {
            let pos = self.pos + at;
            self.data
                .get(pos >> 3)
                .is_some_and(|byte| byte >> (7 - (pos & 7)) & 1 != 0)
        })
    }

    fn done(&self) -> bool {
        self.pos >> 3 >= self.data.len()
    }

    #[inline]
    fn vlc(&mut self, table: &[(i16, i32)]) -> i32 {
        let mut at = 0usize;
        // No code is this long, so a damaged table walk ends too.
        for _ in 0..32 {
            let Some(&(next, value)) = table.get(at + self.read1() as usize) else {
                return 0;
            };
            if next <= 0 {
                return value;
            }
            at = next as usize;
        }
        0
    }
}

#[derive(Default)]
struct Motion {
    full_px: bool,
    r_size: u32,
    h: i32,
    v: i32,
    is_set: bool,
}

/// What's being decoded in one picture.
struct Work {
    kind: u8,
    forward: Motion,
    backward: Motion,
    address: i32,
    mb_row: usize,
    mb_col: usize,
    dc: [i32; 3],
    quantizer: i32,
    intra: bool,
    slice_begin: bool,
    block: [i32; 64],
}

/// Two different frames at once.
fn frame_pair(frames: &mut [Vec<u8>; 3], from: usize, to: usize) -> Option<(&[u8], &mut [u8])> {
    if from == to {
        return None;
    }
    let [a, b, c] = frames;
    match (from, to) {
        (0, 1) => Some((a.as_slice(), b.as_mut_slice())),
        (0, 2) => Some((a.as_slice(), c.as_mut_slice())),
        (1, 0) => Some((b.as_slice(), a.as_mut_slice())),
        (1, 2) => Some((b.as_slice(), c.as_mut_slice())),
        (2, 0) => Some((c.as_slice(), a.as_mut_slice())),
        (2, 1) => Some((c.as_slice(), b.as_mut_slice())),
        _ => None,
    }
}

#[inline]
fn clamp8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

impl Mpeg1Stream {
    /// Opens a stream, working out the order pictures are shown in.
    pub(crate) fn open(input: &[u8], limits: &DecoderLimits) -> Result<Self> {
        let owned = if input.get(3) == Some(&PACK) {
            demux_program(input)?
        } else {
            Vec::new()
        };
        let stream: &[u8] = if owned.is_empty() { input } else { &owned };
        let (width, height, _) = header_fields(stream)?;
        let (width, height) = (width as usize, height as usize);
        let mb_width = width.div_ceil(16);
        let mb_height = height.div_ceil(16);
        let luma = mb_width * 16 * mb_height * 16;
        let plane = luma + luma / 2;
        if plane.saturating_mul(3) > limits.max_memory {
            return Err(VideoError::LimitExceeded("MPEG-1 frame memory"));
        }
        let mut frames = [Vec::new(), Vec::new(), Vec::new()];
        for frame in &mut frames {
            frame
                .try_reserve_exact(plane)
                .map_err(|_| VideoError::OutOfMemory)?;
            frame.resize(plane, 128u8);
        }

        let mut pictures: Vec<Picture> = Vec::new();
        let mut pos = 0;
        let mut display = 0u32;
        // A reference picture shows when the next one has arrived.
        let mut waiting: Option<usize> = None;
        while let Some(at) = next_start_code(stream, pos) {
            pos = at;
            if stream[at] != PICTURE {
                continue;
            }
            let Some(head) = stream.get(at + 1..at + 3) else {
                break;
            };
            let kind = (head[1] >> 3) & 7;
            if pictures.len() >= limits.max_frames {
                return Err(VideoError::LimitExceeded("too many MPEG-1 pictures"));
            }
            if let Some(last) = pictures.last_mut() {
                last.end = at - 3;
            }
            pictures
                .try_reserve(1)
                .map_err(|_| VideoError::OutOfMemory)?;
            let index = pictures.len();
            let mut number = u32::MAX;
            if kind == BIDIRECTIONAL {
                number = display;
                display += 1;
            } else {
                if let Some(held) = waiting {
                    let picture: &mut Picture = &mut pictures[held];
                    picture.display = display;
                    display += 1;
                }
                waiting = Some(index);
            }
            pictures.push(Picture {
                start: at + 1,
                end: stream.len(),
                kind,
                display: number,
            });
        }
        if let Some(held) = waiting {
            pictures[held].display = display;
        }
        let mut result = Self {
            owned: Vec::new(),
            width,
            height,
            mb_width,
            mb_height,
            luma_width: mb_width * 16,
            chroma_width: mb_width * 8,
            chroma_start: [luma, luma + luma / 4],
            frames,
            current: 0,
            forward: 1,
            backward: 2,
            holds: [None; 3],
            pictures,
            next: 0,
            intra_quant: INTRA_QUANT,
            non_intra_quant: NON_INTRA_QUANT,
            coefficients: YuvCoefficients::new(crate::pixel::ColourMatrix::Bt601, false),
        };
        result.read_quant_matrices(stream);
        result.owned = owned;
        Ok(result)
    }

    fn read_quant_matrices(&mut self, stream: &[u8]) {
        let Ok(at) = sequence_header(stream) else {
            return;
        };
        let mut bits = Bits {
            data: stream,
            pos: (at + 4) * 8 + 18 + 1 + 10 + 1,
        };
        if bits.read1() == 1 {
            for &slot in &ZIG_ZAG {
                self.intra_quant[slot as usize] = bits.read(8) as u8;
            }
        }
        if bits.read1() == 1 {
            for &slot in &ZIG_ZAG {
                self.non_intra_quant[slot as usize] = bits.read(8) as u8;
            }
        }
    }

    /// Builds the sample table, in display order, for timing and key frames. Its
    /// byte ranges aren't used: pictures are found through `pictures`.
    pub(crate) fn build_index(&self, input_len: usize, limits: &DecoderLimits) -> Result<Index> {
        let mut order: Vec<(u32, bool)> = Vec::new();
        order
            .try_reserve_exact(self.pictures.len())
            .map_err(|_| VideoError::OutOfMemory)?;
        for picture in &self.pictures {
            order.push((picture.display, picture.kind == INTRA));
        }
        order.sort_unstable();
        let mut index = Index::default();
        for (display, key) in order {
            let flags = if key { FLAG_KEY } else { 0 };
            index.push(limits, input_len, 0, 0, display as i64, flags)?;
        }
        Ok(index)
    }

    /// Works towards a frame, decoding at most `budget` pictures. Returns `true`
    /// when `out` holds it.
    pub(crate) fn advance(
        &mut self,
        input: &[u8],
        target: u32,
        budget: u32,
        out: &mut [u8],
    ) -> Result<bool> {
        crate::error::require_rgba(out, self.width as u32, self.height as u32)?;
        let mut fed = 0;
        loop {
            if let Some(frame) = self.holds.iter().position(|&held| held == Some(target)) {
                self.write_rgba(frame, out);
                return Ok(true);
            }
            let Some(wanted) = self.pictures.iter().position(|p| p.display == target) else {
                return Err(VideoError::FrameOutOfRange {
                    index: target,
                    count: self.pictures.len() as u32,
                });
            };
            // Backwards, or a long way ahead: start from an intra picture.
            if wanted < self.next {
                self.next = self.pictures[..=wanted]
                    .iter()
                    .rposition(|p| p.kind == INTRA)
                    .unwrap_or(0);
                self.holds = [None; 3];
            } else if wanted > self.next {
                let closest = self.pictures[self.next..=wanted]
                    .iter()
                    .rposition(|p| p.kind == INTRA)
                    .map_or(self.next, |offset| self.next + offset);
                if closest > self.next && self.holds.iter().all(|held| held.is_none_or(|d| d < target)) {
                    self.next = closest;
                }
            }
            if fed >= budget {
                return Ok(false);
            }
            fed += 1;
            let at = self.next;
            self.next += 1;
            let picture = self.pictures[at];
            // A B picture nobody's asking for can be passed over.
            if picture.kind == BIDIRECTIONAL && picture.display != target {
                continue;
            }
            // Moved out for the call, as decoding needs `self` too.
            let owned = core::mem::take(&mut self.owned);
            let stream: &[u8] = if owned.is_empty() { input } else { &owned };
            self.decode_picture(stream, picture);
            self.owned = owned;
            // A picture too damaged to start leaves nothing to show. Asking again
            // would only come back here.
            if at == wanted && !self.holds.contains(&Some(target)) {
                return Err(VideoError::InvalidData("damaged MPEG-1 picture"));
            }
        }
    }

    fn write_rgba(&self, frame: usize, out: &mut [u8]) {
        let plane = &self.frames[frame];
        let (cb, cr) = (self.chroma_start[0], self.chroma_start[1]);
        for y in 0..self.height {
            let luma = y * self.luma_width;
            let chroma = (y / 2) * self.chroma_width;
            let row = &mut out[y * self.width * 4..(y + 1) * self.width * 4];
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let (r, g, b) = self.coefficients.rgb(
                    plane[luma + x],
                    plane[cb + chroma + x / 2],
                    plane[cr + chroma + x / 2],
                );
                pixel.copy_from_slice(&[r, g, b, 255]);
            }
        }
    }

    fn decode_picture(&mut self, stream: &[u8], picture: Picture) {
        let data = stream.get(picture.start..picture.end).unwrap_or(&[]);
        let mut bits = Bits { data, pos: 0 };
        bits.read(10);
        let kind = bits.read(3) as u8;
        bits.read(16);
        if kind == 0 || kind > BIDIRECTIONAL {
            return;
        }
        let mut work = Work {
            kind,
            forward: Motion::default(),
            backward: Motion::default(),
            address: 0,
            mb_row: 0,
            mb_col: 0,
            dc: [128; 3],
            quantizer: 1,
            intra: false,
            slice_begin: false,
            block: [0; 64],
        };
        if kind != INTRA {
            work.forward.full_px = bits.read1() == 1;
            let code = bits.read(3);
            if code == 0 {
                return;
            }
            work.forward.r_size = code - 1;
        }
        if kind == BIDIRECTIONAL {
            work.backward.full_px = bits.read1() == 1;
            let code = bits.read(3);
            if code == 0 {
                return;
            }
            work.backward.r_size = code - 1;
        }
        let temp = self.forward;
        if kind != BIDIRECTIONAL {
            self.forward = self.backward;
        }
        self.holds[self.current] = None;
        // Slices begin on byte boundaries, each with a start code.
        let mut pos = bits.pos.div_ceil(8);
        while let Some(at) = next_start_code(data, pos) {
            pos = at;
            let code = data[at];
            if code == 0 || code > 0xaf {
                if code == 0xb2 || code == 0xb5 {
                    continue;
                }
                break;
            }
            bits.pos = (at + 1) * 8;
            self.decode_slice(&mut bits, &mut work, code as i32);
            pos = bits.pos.div_ceil(8).saturating_sub(3).max(at + 1);
        }
        if kind != BIDIRECTIONAL {
            self.holds[self.current] = Some(picture.display);
            self.backward = self.current;
            self.current = temp;
        } else {
            self.holds[self.current] = Some(picture.display);
        }
    }

    fn decode_slice(&mut self, bits: &mut Bits<'_>, work: &mut Work, slice: i32) {
        let mb_size = (self.mb_width * self.mb_height) as i32;
        work.slice_begin = true;
        work.address = (slice - 1) * self.mb_width as i32 - 1;
        work.forward.h = 0;
        work.forward.v = 0;
        work.backward.h = 0;
        work.backward.v = 0;
        work.dc = [128; 3];
        work.quantizer = bits.read(5) as i32;
        while bits.read1() == 1 && !bits.done() {
            bits.read(8);
        }
        loop {
            self.decode_macroblock(bits, work);
            if work.address >= mb_size - 1 || !bits.peek_nonzero(23) {
                break;
            }
        }
    }

    fn decode_macroblock(&mut self, bits: &mut Bits<'_>, work: &mut Work) {
        let mb_size = (self.mb_width * self.mb_height) as i32;
        let mut increment = 0;
        let mut t = bits.vlc(&ADDRESS_INCREMENT);
        while t == 34 && !bits.done() {
            t = bits.vlc(&ADDRESS_INCREMENT);
        }
        while t == 35 && !bits.done() {
            increment += 33;
            t = bits.vlc(&ADDRESS_INCREMENT);
        }
        increment += t;
        if work.slice_begin {
            work.slice_begin = false;
            work.address += increment;
        } else {
            if work.address + increment >= mb_size {
                return;
            }
            if increment > 1 {
                work.dc = [128; 3];
                if work.kind == PREDICTIVE {
                    work.forward.h = 0;
                    work.forward.v = 0;
                }
            }
            while increment > 1 {
                work.address += 1;
                work.mb_row = work.address as usize / self.mb_width;
                work.mb_col = work.address as usize % self.mb_width;
                self.predict(work);
                increment -= 1;
            }
            work.address += 1;
        }
        if work.address < 0 || work.address >= mb_size {
            return;
        }
        work.mb_row = work.address as usize / self.mb_width;
        work.mb_col = work.address as usize % self.mb_width;

        let kind = match work.kind {
            INTRA => bits.vlc(&TYPE_INTRA),
            PREDICTIVE => bits.vlc(&TYPE_PREDICTIVE),
            _ => bits.vlc(&TYPE_B),
        };
        work.intra = kind & 0x01 != 0;
        work.forward.is_set = kind & 0x08 != 0;
        work.backward.is_set = kind & 0x04 != 0;
        if kind & 0x10 != 0 {
            work.quantizer = bits.read(5) as i32;
        }
        if work.intra {
            work.forward.h = 0;
            work.forward.v = 0;
            work.backward.h = 0;
            work.backward.v = 0;
        } else {
            work.dc = [128; 3];
            if work.forward.is_set {
                let r = work.forward.r_size;
                work.forward.h = motion_vector(bits, r, work.forward.h);
                work.forward.v = motion_vector(bits, r, work.forward.v);
            } else if work.kind == PREDICTIVE {
                work.forward.h = 0;
                work.forward.v = 0;
            }
            if work.backward.is_set {
                let r = work.backward.r_size;
                work.backward.h = motion_vector(bits, r, work.backward.h);
                work.backward.v = motion_vector(bits, r, work.backward.v);
            }
            self.predict(work);
        }
        let cbp = if kind & 0x02 != 0 {
            bits.vlc(&CODE_BLOCK_PATTERN)
        } else if work.intra {
            0x3f
        } else {
            0
        };
        for block in 0..6 {
            if cbp & (0x20 >> block) != 0 {
                self.decode_block(bits, work, block);
            }
        }
    }

    fn predict(&mut self, work: &Work) {
        let scale = |motion: &Motion| {
            if motion.full_px {
                (motion.h << 1, motion.v << 1)
            } else {
                (motion.h, motion.v)
            }
        };
        let (fh, fv) = scale(&work.forward);
        if work.kind == BIDIRECTIONAL {
            let (bh, bv) = scale(&work.backward);
            if work.forward.is_set {
                self.copy_macroblock(work, self.forward, fh, fv, false);
                if work.backward.is_set {
                    self.copy_macroblock(work, self.backward, bh, bv, true);
                }
            } else {
                self.copy_macroblock(work, self.backward, bh, bv, false);
            }
        } else {
            self.copy_macroblock(work, self.forward, fh, fv, false);
        }
    }

    fn copy_macroblock(&mut self, work: &Work, from: usize, h: i32, v: i32, interpolate: bool) {
        let current = self.current;
        let (mb_row, mb_col) = (work.mb_row, work.mb_col);
        let (mb_width, mb_height) = (self.mb_width, self.mb_height);
        let (luma, cb, cr) = (self.chroma_start[0], self.chroma_start[0], self.chroma_start[1]);
        let Some((source, target)) = frame_pair(&mut self.frames, from, current) else {
            return;
        };
        let size = source.len();
        let place = (mb_row, mb_col, mb_width, mb_height);
        process(&source[..luma], &mut target[..luma], place, h, v, 16, interpolate);
        process(&source[cb..cr], &mut target[cb..cr], place, h / 2, v / 2, 8, interpolate);
        process(&source[cr..size], &mut target[cr..size], place, h / 2, v / 2, 8, interpolate);
    }

    fn decode_block(&mut self, bits: &mut Bits<'_>, work: &mut Work, block: usize) {
        let mut n = 0usize;
        let quant: &[u8; 64];
        if work.intra {
            let plane = if block > 3 { block - 3 } else { 0 };
            let size = bits.vlc(if plane == 0 {
                &DCT_SIZE_LUMINANCE
            } else {
                &DCT_SIZE_CHROMINANCE
            }) as u32;
            let mut value = work.dc[plane];
            if size > 0 && size < 12 {
                let differential = bits.read(size) as i32;
                value += if differential & (1 << (size - 1)) != 0 {
                    differential
                } else {
                    (-(1i32 << size)) | (differential + 1)
                };
            }
            work.dc[plane] = value;
            work.block[0] = value.wrapping_shl(8);
            quant = &self.intra_quant;
            n = 1;
        } else {
            quant = &self.non_intra_quant;
        }
        loop {
            let coefficient = bits.vlc(&DCT_COEFF) as u32;
            if coefficient == 0x0001 && n > 0 && bits.read1() == 0 {
                break;
            }
            let (run, mut level);
            if coefficient == 0xffff {
                run = bits.read(6) as usize;
                level = bits.read(8) as i32;
                if level == 0 {
                    level = bits.read(8) as i32;
                } else if level == 128 {
                    level = bits.read(8) as i32 - 256;
                } else if level > 128 {
                    level -= 256;
                }
            } else {
                run = (coefficient >> 8) as usize;
                level = (coefficient & 0xff) as i32;
                if bits.read1() == 1 {
                    level = -level;
                }
            }
            n += run;
            let Some(&slot) = ZIG_ZAG.get(n) else {
                work.block = [0; 64];
                return;
            };
            let slot = slot as usize;
            n += 1;
            level = level.wrapping_shl(1);
            if !work.intra {
                level += if level < 0 { -1 } else { 1 };
            }
            level = level
                .wrapping_mul(work.quantizer)
                .wrapping_mul(quant[slot] as i32)
                >> 4;
            if level & 1 == 0 {
                level -= if level > 0 { 1 } else { -1 };
            }
            level = level.clamp(-2048, 2047);
            work.block[slot] = level * PREMULTIPLIER[slot] as i32;
        }

        let (start, stride) = if block < 4 {
            let mut at = (work.mb_row * self.luma_width + work.mb_col) << 4;
            if block & 1 != 0 {
                at += 8;
            }
            if block & 2 != 0 {
                at += self.luma_width << 3;
            }
            (at, self.luma_width)
        } else {
            let plane = self.chroma_start[block - 4];
            (
                plane + ((work.mb_row * self.luma_width) << 2) + (work.mb_col << 3),
                self.chroma_width,
            )
        };
        let target = &mut self.frames[self.current];
        if n == 1 {
            let value = (work.block[0].wrapping_add(128)) >> 8;
            work.block[0] = 0;
            for y in 0..8 {
                for pixel in &mut target[start + y * stride..start + y * stride + 8] {
                    *pixel = if work.intra {
                        clamp8(value)
                    } else {
                        clamp8(*pixel as i32 + value)
                    };
                }
            }
        } else {
            idct(&mut work.block);
            for y in 0..8 {
                let row = &mut target[start + y * stride..start + y * stride + 8];
                for (x, pixel) in row.iter_mut().enumerate() {
                    let value = work.block[y * 8 + x];
                    *pixel = if work.intra {
                        clamp8(value)
                    } else {
                        clamp8(*pixel as i32 + value)
                    };
                }
            }
            work.block = [0; 64];
        }
    }
}

fn motion_vector(bits: &mut Bits<'_>, r_size: u32, motion: i32) -> i32 {
    let fscale = 1i32 << r_size;
    let code = bits.vlc(&MOTION);
    let d = if code != 0 && fscale != 1 {
        let r = bits.read(r_size) as i32;
        let d = ((code.abs() - 1) << r_size) + r + 1;
        if code < 0 {
            -d
        } else {
            d
        }
    } else {
        code
    };
    let mut motion = motion.wrapping_add(d);
    if motion > (fscale << 4) - 1 {
        motion -= fscale << 5;
    } else if motion < -(fscale << 4) {
        motion += fscale << 5;
    }
    motion
}

/// Copies or averages one plane's part of a macroblock from a reference frame.
fn process(
    source: &[u8],
    target: &mut [u8],
    (mb_row, mb_col, mb_width, mb_height): (usize, usize, usize, usize),
    motion_h: i32,
    motion_v: i32,
    size: usize,
    interpolate: bool,
) {
    let width = mb_width * size;
    let height = mb_height * size;
    let hp = motion_h >> 1;
    let vp = motion_v >> 1;
    let odd_h = (motion_h & 1) != 0;
    let odd_v = (motion_v & 1) != 0;
    let left = (mb_col * size) as i32 + hp;
    let top = (mb_row * size) as i32 + vp;
    // A vector off the picture is corrupt video: leave what's there.
    let right = left + size as i32 + odd_h as i32;
    let bottom = top + size as i32 + odd_v as i32;
    if left < 0 || top < 0 || right > width as i32 || bottom > height as i32 {
        return;
    }
    let (left, top) = (left as usize, top as usize);
    let origin = mb_row * size * width + mb_col * size;
    for y in 0..size {
        let source_row = (top + y) * width + left;
        let target_row = origin + y * width;
        for x in 0..size {
            let at = source_row + x;
            let value = match (odd_h, odd_v) {
                (false, false) => source[at] as u32,
                (false, true) => (source[at] as u32 + source[at + width] as u32 + 1) >> 1,
                (true, false) => (source[at] as u32 + source[at + 1] as u32 + 1) >> 1,
                (true, true) => {
                    (source[at] as u32
                        + source[at + 1] as u32
                        + source[at + width] as u32
                        + source[at + width + 1] as u32
                        + 2)
                        >> 2
                }
            };
            let slot = &mut target[target_row + x];
            *slot = if interpolate {
                ((*slot as u32 + value + 1) >> 1) as u8
            } else {
                value as u8
            };
        }
    }
}

/// The inverse DCT, on premultiplied coefficients. Wrapping, as hostile
/// coefficients can overflow, and that may only make a mess of the picture.
fn idct(block: &mut [i32; 64]) {
    let m = |a: i32, b: i32| a.wrapping_mul(b).wrapping_add(128) >> 8;
    for i in 0..8 {
        let b1 = block[4 * 8 + i];
        let b3 = block[2 * 8 + i].wrapping_add(block[6 * 8 + i]);
        let b4 = block[5 * 8 + i].wrapping_sub(block[3 * 8 + i]);
        let tmp1 = block[8 + i].wrapping_add(block[7 * 8 + i]);
        let tmp2 = block[3 * 8 + i].wrapping_add(block[5 * 8 + i]);
        let b6 = block[8 + i].wrapping_sub(block[7 * 8 + i]);
        let b7 = tmp1.wrapping_add(tmp2);
        let m0 = block[i];
        let x4 = (b6.wrapping_mul(473).wrapping_sub(b4.wrapping_mul(196)).wrapping_add(128) >> 8)
            .wrapping_sub(b7);
        let x0 = x4.wrapping_sub(m(tmp1.wrapping_sub(tmp2), 362));
        let x1 = m0.wrapping_sub(b1);
        let x2 = m(block[2 * 8 + i].wrapping_sub(block[6 * 8 + i]), 362).wrapping_sub(b3);
        let x3 = m0.wrapping_add(b1);
        let y3 = x1.wrapping_add(x2);
        let y4 = x3.wrapping_add(b3);
        let y5 = x1.wrapping_sub(x2);
        let y6 = x3.wrapping_sub(b3);
        let y7 = x0
            .wrapping_neg()
            .wrapping_sub(b4.wrapping_mul(473).wrapping_add(b6.wrapping_mul(196)).wrapping_add(128) >> 8);
        block[i] = b7.wrapping_add(y4);
        block[8 + i] = x4.wrapping_add(y3);
        block[2 * 8 + i] = y5.wrapping_sub(x0);
        block[3 * 8 + i] = y6.wrapping_sub(y7);
        block[4 * 8 + i] = y6.wrapping_add(y7);
        block[5 * 8 + i] = x0.wrapping_add(y5);
        block[6 * 8 + i] = y3.wrapping_sub(x4);
        block[7 * 8 + i] = y4.wrapping_sub(b7);
    }
    for row in block.chunks_exact_mut(8) {
        let b1 = row[4];
        let b3 = row[2].wrapping_add(row[6]);
        let b4 = row[5].wrapping_sub(row[3]);
        let tmp1 = row[1].wrapping_add(row[7]);
        let tmp2 = row[3].wrapping_add(row[5]);
        let b6 = row[1].wrapping_sub(row[7]);
        let b7 = tmp1.wrapping_add(tmp2);
        let m0 = row[0];
        let x4 = (b6.wrapping_mul(473).wrapping_sub(b4.wrapping_mul(196)).wrapping_add(128) >> 8)
            .wrapping_sub(b7);
        let x0 = x4.wrapping_sub(m(tmp1.wrapping_sub(tmp2), 362));
        let x1 = m0.wrapping_sub(b1);
        let x2 = m(row[2].wrapping_sub(row[6]), 362).wrapping_sub(b3);
        let x3 = m0.wrapping_add(b1);
        let y3 = x1.wrapping_add(x2);
        let y4 = x3.wrapping_add(b3);
        let y5 = x1.wrapping_sub(x2);
        let y6 = x3.wrapping_sub(b3);
        let y7 = x0
            .wrapping_neg()
            .wrapping_sub(b4.wrapping_mul(473).wrapping_add(b6.wrapping_mul(196)).wrapping_add(128) >> 8);
        let out = |value: i32| value.wrapping_add(128) >> 8;
        row[0] = out(b7.wrapping_add(y4));
        row[1] = out(x4.wrapping_add(y3));
        row[2] = out(y5.wrapping_sub(x0));
        row[3] = out(y6.wrapping_sub(y7));
        row[4] = out(y6.wrapping_add(y7));
        row[5] = out(x0.wrapping_add(y5));
        row[6] = out(y3.wrapping_sub(x4));
        row[7] = out(y4.wrapping_sub(b7));
    }
}
