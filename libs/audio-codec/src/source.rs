//! A sound file played from where it is, for one too large to read into memory.
//!
//! [`AudioStream`](crate::AudioStream) borrows the whole file. This has the same
//! calls, and reads a little at a time from a [`Source`]. Reading blocks for as long as
//! the disk takes, so it belongs on a media task.
//!
//! Seeking can't walk a file it isn't reading, so each format gets there its own way:
//!
//! - PCM moves directly.
//! - FLAC frames and Ogg pages say where they are, so those search by bisection and
//!   land exactly.
//! - MP3 and ADTS frames don't. Their length comes from an Xing header or the first
//!   frames' bit rate, and a seek goes to the byte that proportion says. Constant bit
//!   rate lands exactly, and otherwise it's close.

use alloc::boxed::Box;
use alloc::vec::Vec;

use feos_source::{Cache, Source};

use crate::aac::{self, AacDecoder};
use crate::error::{AudioError, Result};
use crate::flac::{self, FileFrames, FlacPacketDecoder};
use crate::mp3_decode::Mp3Decoder;
use crate::ogg::{self, FLAG_CONTINUED, FLAG_EOS, MAX_PACKET};
use crate::opus::OpusPacketDecoder;
use crate::vorbis::VorbisDecoder;
use crate::{aiff, au, wav, AudioFormat, AudioInfo};

/// PCM frames per call.
const PCM_CHUNK_FRAMES: usize = 1024;
/// What compressed formats read at a time. Holds any frame in practice, and any Ogg page.
const WINDOW: usize = 128 * 1024;
/// The most interleaved samples one MP3 frame makes.
const MP3_CHUNK_SAMPLES: usize = 2304;
/// The most of a file looked at to judge a compressed stream's bit rate.
const SURVEY_BYTES: usize = 64 * 1024;
/// Where a bisection stops, and starts walking.
const BISECT_BYTES: u64 = 32 * 1024;

type ReadPcm = fn(&[u8], usize, &mut [i16]) -> Result<usize>;

struct Pcm {
    read: ReadPcm,
    /// The file's bytes before the sound, which the readers parse each time.
    header: Vec<u8>,
    data_offset: u64,
    data_len: u64,
    align: usize,
    next: u64,
}

struct Mp3 {
    decoder: Mp3Decoder,
    pos: u64,
    start: u64,
    /// Fractions of the file for 1% steps of time, from an Xing header.
    toc: Option<[u8; 100]>,
}

struct Adts {
    decoder: AacDecoder,
    pos: u64,
    start: u64,
    config: crate::aac::AudioSpecificConfig,
}

struct Flac {
    frames: FileFrames,
    pos: u64,
}

enum OggCodec {
    Opus(OpusPacketDecoder, u64),
    Vorbis(VorbisDecoder<'static>),
    Flac(FlacPacketDecoder),
}

/// The packets of one logical stream, from a file.
struct OggPackets {
    pos: u64,
    serial: u32,
    data: Vec<u8>,
    lacing: Vec<u8>,
    data_pos: usize,
    lacing_pos: usize,
    packet: Vec<u8>,
    partial: bool,
    last_page: bool,
    granule: u64,
}

struct Ogg {
    packets: OggPackets,
    codec: OggCodec,
    /// The stream's first packet, for starting Opus again from the start.
    head: Vec<u8>,
    /// Where the first page after the headers is, for going back to the start.
    audio_start: u64,
    /// Sound is thrown away, and `position` isn't believed, until a page ends and says
    /// what its granule is.
    syncing: bool,
}

enum Inner {
    Pcm(Pcm),
    Mp3(Box<Mp3>),
    Adts(Box<Adts>),
    Flac(Box<Flac>),
    Ogg(Box<Ogg>),
}

/// A sound file being decoded in order, from wherever it's kept.
pub struct SourceStream {
    file: Box<dyn Source>,
    cache: Cache,
    info: AudioInfo,
    inner: Inner,
    position: u64,
    skip_to: u64,
}

/// Bit rates in kbit/s for MPEG-1 layers I, II and III, and for MPEG-2 and 2.5 layer I,
/// and layers II and III.
const MPEG_BITRATES: [[u16; 16]; 5] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0],
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
];

/// What an MPEG audio frame header says: `(bit rate, sample rate, samples per frame,
/// Xing header offset from the frame's start)`.
fn mpeg_header(bytes: &[u8]) -> Option<(u32, u32)> {
    let head = bytes.get(..4)?;
    if head[0] != 0xff || head[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (head[1] >> 3) & 3;
    let layer = (head[1] >> 1) & 3;
    let bitrate_index = (head[2] >> 4) as usize;
    let rate_index = ((head[2] >> 2) & 3) as usize;
    if version == 1 || layer == 0 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let table = match (mpeg1, layer) {
        (true, 3) => 0,
        (true, 2) => 1,
        (true, _) => 2,
        (false, 3) => 3,
        (false, _) => 4,
    };
    let bitrate = MPEG_BITRATES[table][bitrate_index] as u32 * 1000;
    let base = [44_100, 48_000, 32_000][rate_index];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    Some((bitrate, rate))
}

impl SourceStream {
    /// Opens a file and says why it can't play, as [`AudioStream::open`] does.
    ///
    /// [`AudioStream::open`]: crate::AudioStream::open
    pub fn open(file: Box<dyn Source>) -> Result<Self> {
        let mut cache = Cache::new(WINDOW).ok_or(AudioError::OutOfMemory)?;
        let file_len = file.len();
        let file_size = usize::try_from(file_len).map_err(|_| AudioError::Overflow)?;
        let head = cache.bytes(&*file, 0, WINDOW);
        let id3 = flac::id3_len(head) as u64;
        // A tag that holds a picture can end far beyond what's in hand, and the format's
        // first bytes come after it.
        let format = if id3 > 0 {
            let after = cache.bytes(&*file, id3, 16);
            let found = crate::detect(after);
            found.or(Some(AudioFormat::Mp3))
        } else {
            crate::detect(head)
        };
        let format = format.ok_or_else(|| {
            let head = cache.bytes(&*file, 0, WINDOW);
            if head.starts_with(b"OggS") {
                match ogg::find_stream(head) {
                    Err(error) => error,
                    Ok(_) => AudioError::UnsupportedFormat,
                }
            } else {
                AudioError::UnknownFormat
            }
        })?;
        let (info, inner) = match format {
            AudioFormat::Wav | AudioFormat::Aiff | AudioFormat::Au => {
                let head = cache.bytes(&*file, 0, WINDOW);
                let (info, data_offset, data_len, align) = match format {
                    AudioFormat::Wav => wav::layout(head, file_size)?,
                    AudioFormat::Aiff => aiff::layout(head, file_size)?,
                    _ => au::layout(head, file_size)?,
                };
                let header = head.get(..data_offset).ok_or(AudioError::UnexpectedEnd)?;
                let mut kept = Vec::new();
                kept.try_reserve_exact(header.len() + PCM_CHUNK_FRAMES * align)
                    .map_err(|_| AudioError::OutOfMemory)?;
                kept.extend_from_slice(header);
                let read: ReadPcm = match format {
                    AudioFormat::Wav => wav::read_frames,
                    AudioFormat::Aiff => aiff::read_frames,
                    _ => au::read_frames,
                };
                (
                    info,
                    Inner::Pcm(Pcm {
                        read,
                        header: kept,
                        data_offset: data_offset as u64,
                        data_len: data_len as u64,
                        align,
                        next: 0,
                    }),
                )
            }
            AudioFormat::Mp3 => Self::open_mp3(&*file, &mut cache, id3)?,
            AudioFormat::Aac => Self::open_adts(&*file, &mut cache, id3)?,
            AudioFormat::Flac => {
                let frames = FileFrames::open(&*file, &mut cache, id3)?;
                let stream = frames.stream_info();
                let info = AudioInfo {
                    sample_rate: stream.sample_rate,
                    channels: stream.channels,
                    frames: stream.total_frames,
                    format,
                };
                let pos = frames.audio_start;
                (info, Inner::Flac(Box::new(Flac { frames, pos })))
            }
            AudioFormat::OggVorbis | AudioFormat::OggFlac | AudioFormat::OggOpus => {
                Self::open_ogg(&*file, &mut cache, format)?
            }
        };
        Ok(Self {
            file,
            cache,
            info,
            inner,
            position: 0,
            skip_to: 0,
        })
    }

    fn open_mp3(file: &dyn Source, cache: &mut Cache, start: u64) -> Result<(AudioInfo, Inner)> {
        let data = cache.bytes(file, start, WINDOW);
        let at = (0..data.len().saturating_sub(4))
            .find(|&at| mpeg_header(&data[at..]).is_some_and(|(bitrate, _)| bitrate != 0))
            .ok_or(AudioError::InvalidData("no MPEG audio frame found"))?;
        let (bitrate, rate) = mpeg_header(&data[at..]).ok_or(AudioError::UnknownFormat)?;
        let channels = if data[at + 3] >> 6 == 3 { 1 } else { 2 };
        // The header and LAME tag, which give the length (without the encoder's
        // silence, which the decoder drops), and a seek table.
        let tag = crate::mp3_decode::xing_tag(&data[at..]);
        let toc = tag.and_then(|tag| tag.toc);
        let start = start + at as u64;
        let bytes = file.len().saturating_sub(start);
        let frames = match tag.and_then(|tag| tag.length()) {
            Some(length) => length,
            None => bytes.saturating_mul(8).saturating_mul(rate as u64) / bitrate.max(1) as u64,
        };
        let info = AudioInfo {
            sample_rate: rate,
            channels,
            frames,
            format: AudioFormat::Mp3,
        };
        Ok((
            info,
            Inner::Mp3(Box::new(Mp3 {
                decoder: Mp3Decoder::new(),
                pos: start,
                start,
                toc,
            })),
        ))
    }

    fn open_adts(file: &dyn Source, cache: &mut Cache, start: u64) -> Result<(AudioInfo, Inner)> {
        let data = cache.bytes(file, start, SURVEY_BYTES);
        let (at, config, frames, bytes) = aac::survey_adts(data)?;
        let start = start + at as u64;
        let decoder = AacDecoder::new(config)?;
        let total = file
            .len()
            .saturating_sub(start)
            .saturating_mul(frames)
            / bytes.max(1);
        let info = AudioInfo {
            sample_rate: config.output_rate(),
            channels: decoder.channel_count() as u16,
            frames: total,
            format: AudioFormat::Aac,
        };
        Ok((
            info,
            Inner::Adts(Box::new(Adts {
                decoder,
                pos: start,
                start,
                config,
            })),
        ))
    }

    fn open_ogg(
        file: &dyn Source,
        cache: &mut Cache,
        format: AudioFormat,
    ) -> Result<(AudioInfo, Inner)> {
        let head = cache.bytes(file, 0, WINDOW);
        let (_, page) = ogg::find_stream(head)?.ok_or(AudioError::UnsupportedFormat)?;
        let serial = page.serial;
        let mut packets = OggPackets::new(serial);
        let mut head = Vec::new();
        let (codec, sample_rate, channels) = match format {
            AudioFormat::OggVorbis => {
                let read = |packets: &mut OggPackets, cache: &mut Cache| -> Result<Vec<u8>> {
                    if !packets.next(file, cache)? {
                        return Err(AudioError::UnexpectedEnd);
                    }
                    let mut copy = Vec::new();
                    copy.try_reserve_exact(packets.packet.len())
                        .map_err(|_| AudioError::OutOfMemory)?;
                    copy.extend_from_slice(&packets.packet);
                    Ok(copy)
                };
                let ident = read(&mut packets, cache)?;
                let _comment = read(&mut packets, cache)?;
                let setup = read(&mut packets, cache)?;
                let decoder = VorbisDecoder::from_packets(&ident, &setup)?;
                let (rate, channels) = (decoder.sample_rate(), decoder.channels() as u16);
                (OggCodec::Vorbis(decoder), rate, channels)
            }
            AudioFormat::OggOpus => {
                if !packets.next(file, cache)? {
                    return Err(AudioError::UnexpectedEnd);
                }
                let config = crate::opus::OpusConfig::parse(&packets.packet)?;
                let decoder = OpusPacketDecoder::new(&packets.packet)?;
                head.try_reserve_exact(packets.packet.len())
                    .map_err(|_| AudioError::OutOfMemory)?;
                head.extend_from_slice(&packets.packet);
                // The tags.
                if !packets.next(file, cache)? {
                    return Err(AudioError::UnexpectedEnd);
                }
                (OggCodec::Opus(decoder, config.pre_skip as u64), 48_000, config.channels)
            }
            _ => {
                if !packets.next(file, cache)? {
                    return Err(AudioError::UnexpectedEnd);
                }
                // The mapping header: 0x7f FLAC, version, a count, fLaC, a block header
                // and STREAMINFO.
                let info = packets
                    .packet
                    .get(17..)
                    .ok_or(AudioError::InvalidData("bad Ogg FLAC header"))?;
                let decoder = FlacPacketDecoder::new(info)?;
                let stream = decoder.stream_info();
                (OggCodec::Flac(decoder), stream.sample_rate, stream.channels)
            }
        };
        let granules = last_granule(file, cache, serial);
        let frames = match &codec {
            OggCodec::Opus(_, pre_skip) => granules.saturating_sub(*pre_skip),
            _ => granules,
        };
        let info = AudioInfo {
            sample_rate,
            channels,
            frames,
            format,
        };
        // Where the audio starts: the next page, at the latest, once headers are over.
        let audio_start = packets.pos;
        Ok((
            info,
            Inner::Ogg(Box::new(Ogg {
                packets,
                codec,
                head,
                audio_start,
                syncing: false,
            })),
        ))
    }

    /// The stream's properties. `frames` is exact for PCM, FLAC with a stated length and
    /// Ogg, and may be an estimate for MP3 and ADTS.
    pub fn info(&self) -> AudioInfo {
        self.info
    }

    /// Sample frames handed out so far, counting from the file's start after a seek.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// How many interleaved samples [`next_chunk`](Self::next_chunk) needs room for.
    pub fn max_chunk_samples(&self) -> usize {
        match &self.inner {
            Inner::Pcm(_) => PCM_CHUNK_FRAMES * self.info.channels.max(1) as usize,
            Inner::Mp3(_) => MP3_CHUNK_SAMPLES,
            Inner::Adts(adts) => 8 * 1024 * adts.decoder.channel_count(),
            Inner::Flac(flac) => {
                let stream = flac.frames.stream_info();
                (stream.max_block_size as usize * stream.channels as usize).max(4096)
            }
            Inner::Ogg(ogg) => match &ogg.codec {
                OggCodec::Opus(decoder, _) => decoder.max_packet_samples(),
                OggCodec::Vorbis(decoder) => decoder.max_packet_samples(),
                OggCodec::Flac(decoder) => {
                    let stream = decoder.stream_info();
                    (stream.max_block_size as usize * stream.channels as usize).max(4096)
                }
            },
        }
    }

    /// Decodes one packet to `out`, as [`AudioStream::next_chunk`] does.
    ///
    /// [`AudioStream::next_chunk`]: crate::AudioStream::next_chunk
    pub fn next_chunk(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        let channels = self.info.channels.max(1) as usize;
        let file = &*self.file;
        let cache = &mut self.cache;
        let frames = match &mut self.inner {
            Inner::Pcm(pcm) => {
                let offset = pcm.next.saturating_mul(pcm.align as u64);
                let frames = (out.len() / channels)
                    .min(PCM_CHUNK_FRAMES)
                    .min((pcm.data_len.saturating_sub(offset) / pcm.align as u64) as usize);
                if frames == 0 {
                    return Ok(None);
                }
                let bytes = frames * pcm.align;
                let header = pcm.header.len();
                pcm.header.resize(header + bytes, 0);
                let read = file.read_full(pcm.data_offset + offset, &mut pcm.header[header..]);
                let whole = read / pcm.align * pcm.align;
                pcm.header.truncate(header + whole);
                if whole == 0 {
                    return Ok(None);
                }
                // The headers say more than this holds, and these readers cope.
                let count = (pcm.read)(&pcm.header, 0, out)?;
                pcm.header.truncate(header);
                pcm.next += count as u64;
                count
            }
            Inner::Mp3(mp3) => {
                let data = cache.bytes(file, mp3.pos, WINDOW);
                if data.is_empty() {
                    return Ok(None);
                }
                let mut written = 0;
                let consumed = mp3.decoder.decode_frame(data, out, &mut written);
                let consumed = match consumed {
                    Ok(consumed) => consumed,
                    Err(error @ AudioError::BufferTooSmall { .. }) => return Err(error),
                    Err(error) => {
                        mp3.pos = file.len();
                        return Err(error);
                    }
                };
                if consumed == 0 {
                    // A window with no frame in it, in the middle of a damaged file?
                    if data.len() == WINDOW {
                        mp3.pos += (WINDOW / 2) as u64;
                        return Ok(Some(0));
                    }
                    return Ok(None);
                }
                mp3.pos += consumed as u64;
                if mp3.decoder.channels() != channels {
                    return Ok(Some(0));
                }
                written / channels
            }
            Inner::Adts(adts) => {
                let data = cache.bytes(file, adts.pos, WINDOW);
                match aac::decode_adts(&mut adts.decoder, data, out) {
                    Ok(Some((consumed, frames))) => {
                        adts.pos += consumed as u64;
                        frames
                    }
                    Ok(None) => return Ok(None),
                    Err(error @ AudioError::BufferTooSmall { .. }) => return Err(error),
                    Err(error) => {
                        // Past the sync word, so the next call finds the next frame.
                        adts.pos += 2;
                        return Err(error);
                    }
                }
            }
            Inner::Flac(flac) => {
                let data = cache.bytes(file, flac.pos, WINDOW);
                if data.is_empty() {
                    return Ok(None);
                }
                let at_end = data.len() < WINDOW;
                let (consumed, frames) = flac.frames.decode(data, at_end, out)?;
                flac.pos += consumed as u64;
                frames
            }
            Inner::Ogg(ogg) => {
                loop {
                    if !ogg.packets.next(file, cache)? {
                        return Ok(None);
                    }
                    let packet = &ogg.packets.packet;
                    let frames = match &mut ogg.codec {
                        OggCodec::Opus(decoder, _) => {
                            match decoder.decode(packet, out) {
                                Ok(frames) => frames,
                                Err(error @ AudioError::BufferTooSmall { .. }) => return Err(error),
                                Err(_) => 0,
                            }
                        }
                        OggCodec::Vorbis(decoder) => {
                            match decoder.decode_packet(packet, out) {
                                Ok(frames) => frames,
                                Err(error @ AudioError::BufferTooSmall { .. }) => return Err(error),
                                Err(_) => 0,
                            }
                        }
                        OggCodec::Flac(decoder) => {
                            if packet.first() != Some(&0xff) {
                                continue;
                            }
                            decoder.decode(packet, out)?
                        }
                    };
                    if ogg.syncing {
                        if ogg.packets.ended_page() && ogg.packets.granule != u64::MAX {
                            ogg.syncing = false;
                            self.position = match &ogg.codec {
                                OggCodec::Opus(_, pre_skip) => ogg.packets.granule.saturating_sub(*pre_skip),
                                _ => ogg.packets.granule,
                            };
                        }
                        continue;
                    }
                    break frames;
                }
            }
        };
        let start = self.position;
        self.position += frames as u64;
        // Ogg's stated end cuts off the last packet's padding.
        let mut frames = frames;
        if matches!(self.inner, Inner::Ogg(_)) && self.info.frames != 0 && self.position > self.info.frames {
            frames = frames.saturating_sub((self.position - self.info.frames) as usize);
            self.position = self.info.frames;
        }
        if self.position <= self.skip_to {
            return Ok(Some(0));
        }
        let drop_frames = (self.skip_to.saturating_sub(start) as usize).min(frames);
        if drop_frames > 0 {
            out.copy_within(drop_frames * channels..frames * channels, 0);
        }
        Ok(Some(frames - drop_frames))
    }

    /// Makes `frame` the next sample frame, or about that for MP3 and ADTS: see the
    /// module's description. This reads a little of the file, however far it's going.
    pub fn seek(&mut self, frame: u64) -> Result<()> {
        let frame = if self.info.frames > 0 {
            frame.min(self.info.frames)
        } else {
            frame
        };
        let total = self.info.frames.max(1);
        let file = &*self.file;
        let cache = &mut self.cache;
        match &mut self.inner {
            Inner::Pcm(pcm) => {
                pcm.next = frame;
                self.position = frame;
                self.skip_to = frame;
            }
            Inner::Mp3(mp3) => {
                let bytes = file.len().saturating_sub(mp3.start);
                let fraction = match mp3.toc {
                    Some(toc) => {
                        // Time to a share of the file, by 256ths, in 1% steps.
                        let percent = (frame as u128 * 100 * 256 / total as u128) as u64;
                        let (whole, part) = ((percent / 256).min(99) as usize, percent % 256);
                        let low = toc[whole] as u64;
                        let high = toc.get(whole + 1).map_or(256, |&next| next as u64);
                        (low * 256 + (high.saturating_sub(low)) * part) as u128
                    }
                    None => frame as u128 * 65536 / total as u128,
                };
                mp3.pos = mp3.start + (bytes as u128 * fraction / 65536) as u64;
                // From the top, the header frame and leading silence are dropped
                // again; from anywhere else, only the end is still to be kept to.
                mp3.decoder = if frame == 0 {
                    Mp3Decoder::new()
                } else {
                    mp3.decoder.after_seek(frame)
                };
                self.position = frame;
                self.skip_to = frame;
            }
            Inner::Adts(adts) => {
                let bytes = file.len().saturating_sub(adts.start);
                adts.pos = adts.start + (bytes as u128 * frame as u128 / total as u128) as u64;
                adts.decoder = AacDecoder::new(adts.config)?;
                self.position = frame;
                self.skip_to = frame;
            }
            Inner::Flac(flac) => {
                let (mut low, mut high) = (flac.frames.audio_start, file.len());
                let mut low_sample = 0;
                while high.saturating_sub(low) > BISECT_BYTES {
                    let middle = low + (high - low) / 2;
                    let data = cache.bytes(file, middle, BISECT_BYTES as usize);
                    match flac.frames.find_frame(data) {
                        Some((at, sample)) if sample <= frame => {
                            low = middle + at as u64;
                            low_sample = sample;
                        }
                        _ => high = middle,
                    }
                }
                flac.pos = low;
                self.position = low_sample;
                self.skip_to = frame;
            }
            Inner::Ogg(ogg) => {
                // Opus takes 80 ms of packets to settle, and a page's granule counts the
                // part of the stream's start that gets dropped.
                let (target, pre_skip) = match &ogg.codec {
                    OggCodec::Opus(_, pre_skip) => ((frame + pre_skip).saturating_sub(3840), *pre_skip),
                    _ => (frame, 0),
                };
                let (at, sample) = ogg_bisect(file, cache, ogg.audio_start, ogg.packets.serial, target);
                ogg.packets.jump(at);
                match &mut ogg.codec {
                    OggCodec::Opus(decoder, _) => {
                        // Back at the start, what's dropped is dropped as it was.
                        if at == ogg.audio_start {
                            *decoder = OpusPacketDecoder::new(&ogg.head).map_err(|_| AudioError::UnsupportedFormat)?;
                        } else {
                            decoder.forget();
                        }
                    }
                    OggCodec::Vorbis(decoder) => decoder.forget_overlap(),
                    OggCodec::Flac(_) => {}
                }
                // Back at the very start, the first packet's granule isn't needed.
                ogg.syncing = at != ogg.audio_start;
                self.position = if ogg.syncing { sample.saturating_sub(pre_skip) } else { 0 };
                self.skip_to = frame;
            }
        }
        Ok(())
    }
}

impl OggPackets {
    fn new(serial: u32) -> Self {
        Self {
            pos: 0,
            serial,
            data: Vec::new(),
            lacing: Vec::new(),
            data_pos: 0,
            lacing_pos: 0,
            packet: Vec::new(),
            partial: false,
            last_page: false,
            granule: 0,
        }
    }

    fn jump(&mut self, pos: u64) {
        self.pos = pos;
        self.data.clear();
        self.lacing.clear();
        self.data_pos = 0;
        self.lacing_pos = 0;
        self.partial = false;
        self.last_page = false;
    }

    /// Whether the last packet was the last to end on its page.
    fn ended_page(&self) -> bool {
        self.lacing_pos >= self.lacing.len()
    }

    fn next_page(&mut self, file: &dyn Source, cache: &mut Cache) -> Result<bool> {
        if self.last_page {
            return Ok(false);
        }
        loop {
            let data = cache.bytes(file, self.pos, WINDOW);
            let Some(skip) = data.windows(4).position(|w| w == b"OggS") else {
                if data.len() < WINDOW {
                    return Ok(false);
                }
                self.pos += (WINDOW - 3) as u64;
                continue;
            };
            let (page, next) = match ogg::parse_page(data, skip) {
                Ok(parsed) => parsed,
                Err(AudioError::UnexpectedEnd) if data.len() < WINDOW => return Ok(false),
                Err(_) => {
                    self.pos += skip as u64 + 1;
                    continue;
                }
            };
            self.pos += next as u64;
            if page.serial != self.serial {
                continue;
            }
            if page.flags & FLAG_CONTINUED == 0 {
                self.partial = false;
            }
            self.data.clear();
            self.lacing.clear();
            self.data
                .try_reserve(page.data.len())
                .and_then(|()| self.lacing.try_reserve(page.lacing.len()))
                .map_err(|_| AudioError::OutOfMemory)?;
            self.data.extend_from_slice(page.data);
            self.lacing.extend_from_slice(page.lacing);
            self.data_pos = 0;
            self.lacing_pos = 0;
            self.last_page = page.flags & FLAG_EOS != 0;
            self.granule = page.granule;
            if page.flags & FLAG_CONTINUED != 0 && !self.partial {
                // The head of this packet is gone, so skip what's left of it.
                while let Some(&lace) = self.lacing.get(self.lacing_pos) {
                    self.lacing_pos += 1;
                    self.data_pos += lace as usize;
                    if lace != 255 {
                        break;
                    }
                }
            }
            return Ok(true);
        }
    }

    /// Puts the next whole packet in `self.packet`. `false` at the end of the stream.
    fn next(&mut self, file: &dyn Source, cache: &mut Cache) -> Result<bool> {
        loop {
            while let Some(&lace) = self.lacing.get(self.lacing_pos) {
                self.lacing_pos += 1;
                if !self.partial {
                    self.packet.clear();
                    self.partial = true;
                }
                let (start, end) = (self.data_pos, self.data_pos + lace as usize);
                self.data_pos = end;
                let fragment = self.data.get(start..end).ok_or(AudioError::UnexpectedEnd)?;
                if self.packet.len() + fragment.len() > MAX_PACKET {
                    return Err(AudioError::InvalidData("Ogg packet is too large"));
                }
                self.packet
                    .try_reserve(fragment.len())
                    .map_err(|_| AudioError::OutOfMemory)?;
                self.packet.extend_from_slice(fragment);
                if lace != 255 {
                    self.partial = false;
                    return Ok(true);
                }
            }
            if !self.next_page(file, cache)? {
                return Ok(false);
            }
        }
    }
}

/// The first page of `serial` at or after `pos`: `(offset, granule, next)`. Looks only in
/// what fits in `cache`.
fn find_page(file: &dyn Source, cache: &mut Cache, pos: u64, serial: u32) -> Option<(u64, u64, u64)> {
    let data = cache.bytes(file, pos, WINDOW);
    let mut at = 0;
    while let Some(skip) = data.get(at..)?.windows(4).position(|w| w == b"OggS") {
        at += skip;
        match ogg::parse_page(data, at) {
            Ok((page, next)) if page.serial == serial && page.granule != u64::MAX => {
                return Some((pos + at as u64, page.granule, pos + next as u64));
            }
            Ok((_, next)) => at = next,
            Err(_) => at += 1,
        }
    }
    None
}

/// Finds the page after which decoding should start for `frame`: the last whose granule
/// is before it. Returns its offset and granule, or the audio's start.
fn ogg_bisect(file: &dyn Source, cache: &mut Cache, start: u64, serial: u32, frame: u64) -> (u64, u64) {
    let (mut low, mut high) = (start, file.len());
    let mut found = (start, 0);
    while high.saturating_sub(low) > BISECT_BYTES {
        let middle = low + (high - low) / 2;
        match find_page(file, cache, middle, serial) {
            Some((at, granule, next)) if granule < frame => {
                found = (at, granule);
                low = next;
            }
            _ => high = middle,
        }
    }
    // Walk what's left.
    let mut pos = low;
    while let Some((at, granule, next)) = find_page(file, cache, pos, serial) {
        if granule >= frame {
            break;
        }
        found = (at, granule);
        pos = next;
    }
    found
}

/// The stream's length, from the end of the file.
fn last_granule(file: &dyn Source, cache: &mut Cache, serial: u32) -> u64 {
    let len = file.len();
    let mut pos = len.saturating_sub(WINDOW as u64);
    let mut last = 0;
    while let Some((_, granule, next)) = find_page(file, cache, pos, serial) {
        last = granule;
        pos = next;
    }
    last
}
