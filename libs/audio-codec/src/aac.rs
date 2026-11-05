//! AAC-LC decoding, in fixed-point integer arithmetic.
//!
//! Input is either an ADTS stream ([`info`], [`decode`], [`AdtsDecoder`]) or
//! raw access units from a container, which go through
//! [`AacDecoder::decode_raw_frame`] with the MP4 `AudioSpecificConfig` parsed by
//! [`AudioSpecificConfig::parse`].
//!
//! Implemented: long and short blocks, both window shapes, M/S and intensity
//! stereo, perceptual noise substitution, TNS, pulse data and channel
//! configurations 1 to 7, with output in WAVE channel order.
//!
//! Refused, with [`AudioError::UnsupportedFeature`], since each would otherwise
//! decode to the wrong sound: any object type but LC, AAC Main prediction, gain
//! control, coupling channels, 960-sample frames, and streams whose channels are
//! defined by a program config element (channel configuration 0). An HE-AAC
//! stream, however it signals SBR and PS, plays its LC core at the base rate,
//! which is the behaviour the format was designed to allow. A rate that isn't in
//! the standard's table uses the bands of the nearest one, as the standard says.
//!
//! # Fixed point
//!
//! Spectral coefficients are Q3 on the scale of the standard, in `i32`. A full
//! scale sine then transforms to 2^28, leaving 8x headroom in the transform, and
//! its output reads as PCM in Q13. All products are 64-bit. Values that would
//! not fit wrap or saturate without a panic. The output is within one or two
//! 16-bit steps of a float decoder, as `tests/audio.rs` measures.
//!
//! The CPU cost of a frame is linear in its output, and a frame is decoded only
//! once its output is known to fit. A frame can be as short as 7 bytes yet
//! decodes to 1024 samples per channel: the caller's buffer bounds the total.

use alloc::vec::Vec;

use crate::aac_tables::{
    Code, BOOKS, KBD_LONG, KBD_SHORT, SCALEFACTOR, SWB_LONG, SWB_SHORT,
};
use crate::bits::{try_vec, MsbReader};
use crate::error::{AudioError, Result};
use crate::flac::id3_len;
use crate::mdct::{self, q62_to_q31, sin_turn, Imdct};
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// Samples per channel in a frame.
pub const FRAME_SAMPLES: usize = 1024;
const SHORT_SAMPLES: usize = 128;
/// The most channels a channel configuration can give.
const MAX_CHANNELS: usize = 8;
const MAX_SECTIONS: usize = 120;
const TNS_MAX_ORDER: usize = 12;

/// Fractional bits of a spectral coefficient.
const COEF_FRACTION: u32 = 3;
/// Fractional bits of the output of a long transform (`COEF_FRACTION + 10`).
const PCM_FRACTION: u32 = COEF_FRACTION + 10;
/// Limit on a coefficient, so that a hostile one cannot overflow a later sum.
const COEF_LIMIT: i64 = 1 << 30;

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8000,
    7350,
];
const TNS_MAX_BANDS_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
const TNS_MAX_BANDS_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

/// For each channel configuration, the decoded channel at each output position.
static OUTPUT_ORDER: [&[u8]; 8] = [
    &[],
    &[0],
    &[0, 1],
    &[1, 2, 0],
    &[1, 2, 0, 3],
    &[1, 2, 0, 3, 4],
    &[1, 2, 0, 5, 3, 4],
    &[3, 4, 0, 7, 5, 6, 1, 2],
];

/// 2^(n/4) in Q30, for n from 0 to 3.
const QUARTER_POWERS: [i64; 4] = [1_073_741_824, 1_276_901_417, 1_518_500_250, 1_805_811_301];

const fn cube_root(value: u64) -> u64 {
    // Bit by bit, so this also works at compile time.
    let mut root = 0u64;
    let mut bit = 21;
    loop {
        let candidate = root | (1 << bit);
        if candidate * candidate * candidate <= value {
            root = candidate;
        }
        if bit == 0 {
            return root;
        }
        bit -= 1;
    }
}

/// `q^(4/3)` in Q13.
const fn power43(q: u64) -> u64 {
    // q * cbrt(q), with the root taken with 13 fractional bits, 39 under the root.
    q * cube_root(q << 39)
}

const fn power43_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut q = 0;
    while q < 256 {
        table[q] = power43(q as u64) as u32;
        q += 1;
    }
    table
}

static POW43: [u32; 256] = power43_table();

/// The parsed fields of an MP4 `AudioSpecificConfig`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioSpecificConfig {
    /// MPEG-4 audio object type. This decoder handles only 2, AAC-LC.
    pub object_type: u8,
    /// Index into the standard's table of sampling frequencies.
    pub sample_rate_index: u8,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel configuration, 1 to 7.
    pub channel_config: u8,
}

impl AudioSpecificConfig {
    /// Parses an `AudioSpecificConfig`, as stored in an MP4 `esds` box.
    ///
    /// Anything the decoder cannot play is refused here, so a demuxer finds out
    /// when opening the file.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut reader = MsbReader::new(data);
        let mut object_type = reader.read(5)?;
        if object_type == 31 {
            object_type = 32 + reader.read(6)?;
        }
        let mut rate_index = reader.read(4)?;
        let mut explicit_rate = None;
        if rate_index == 15 {
            let rate = reader.read(24)?;
            rate_index = nearest_rate_index(rate) as u32;
            explicit_rate = Some(rate);
        }
        let channel_config = reader.read(4)?;
        // Explicitly signalled HE-AAC: an extension rate, and then the core's
        // object type. The core is plain LC and plays by itself at its base rate.
        if object_type == 5 || object_type == 29 {
            if reader.read(4)? == 15 {
                reader.read(24)?;
            }
            object_type = reader.read(5)?;
            if object_type == 31 {
                object_type = 32 + reader.read(6)?;
            }
        }
        if object_type != 2 {
            return Err(AudioError::UnsupportedFeature("AAC object type"));
        }
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC 960-sample frames"));
        }
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC core coder dependency"));
        }
        // The extension flag: reserved for object type 2.
        reader.bit()?;
        let mut config = Self::new(rate_index as u8, channel_config as u8)?;
        if let Some(rate) = explicit_rate {
            if rate == 0 {
                return Err(AudioError::InvalidData("AAC sample rate of zero"));
            }
            config.sample_rate = rate;
        }
        Ok(config)
    }

    /// A config for AAC-LC with the given sampling index and channels.
    pub fn new(sample_rate_index: u8, channel_config: u8) -> Result<Self> {
        let sample_rate = *SAMPLE_RATES
            .get(sample_rate_index as usize)
            .ok_or(AudioError::InvalidData("reserved AAC sampling index"))?;
        if channel_config == 0 {
            return Err(AudioError::UnsupportedFeature(
                "AAC program config element",
            ));
        }
        if channel_config > 7 {
            return Err(AudioError::InvalidData("reserved AAC channel configuration"));
        }
        Ok(Self {
            object_type: 2,
            sample_rate_index,
            sample_rate,
            channel_config,
        })
    }

    /// Number of interleaved output channels.
    pub fn channels(&self) -> usize {
        OUTPUT_ORDER
            .get(self.channel_config as usize)
            .map_or(0, |order| order.len())
    }
}

/// The standard's mapping from any rate to the table that suits it.
fn nearest_rate_index(rate: u32) -> usize {
    const FROM: [u32; 12] = [
        92017, 75132, 55426, 46009, 37566, 27713, 23004, 18783, 13856, 11502, 9391, 0,
    ];
    FROM.iter().position(|&low| rate >= low).unwrap_or(11)
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum WindowSequence {
    #[default]
    OnlyLong,
    LongStart,
    EightShort,
    LongStop,
}

/// `ics_info`.
#[derive(Clone, Copy, Default)]
struct IcsInfo {
    sequence: WindowSequence,
    /// `true` for the Kaiser-Bessel-derived window.
    shape: bool,
    max_sfb: usize,
    /// Windows in each group, one group for a long block.
    group_lengths: [u8; 8],
    groups: usize,
}

const BAND_ZERO: u8 = 0;
const BAND_NOISE: u8 = 13;
const BAND_INTENSITY_OUT: u8 = 14;
const BAND_INTENSITY_IN: u8 = 15;

#[derive(Clone, Copy, Default)]
struct TnsFilter {
    start_band_count: u8,
    order: u8,
    backward: bool,
    /// Reflection coefficient indices, to be looked up by `resolution`.
    coefficients: [i8; TNS_MAX_ORDER],
}

/// `tns_data`, at most three filters a long window and one a short.
#[derive(Clone, Copy, Default)]
struct Tns {
    /// Filters per window.
    counts: [u8; 8],
    /// One filter set per window; a long window uses the first with up to 3.
    filters: [[TnsFilter; 3]; 8],
    /// `coef_res` per window: 3 or 4 bits.
    resolution: [u8; 8],
}

/// Everything read from one `individual_channel_stream` but its spectrum.
#[derive(Clone, Copy)]
struct Ics {
    info: IcsInfo,
    band_types: [u8; MAX_SECTIONS],
    scalefactors: [i16; MAX_SECTIONS],
    tns: Tns,
    tns_present: bool,
}

impl Ics {
    fn new() -> Self {
        Self {
            info: IcsInfo::default(),
            band_types: [0; MAX_SECTIONS],
            scalefactors: [0; MAX_SECTIONS],
            tns: Tns::default(),
            tns_present: false,
        }
    }
}

/// Decodes a symbol by binary search on the next bits of input.
#[inline]
fn read_code(reader: &mut MsbReader<'_>, table: &[Code]) -> Result<u16> {
    let bits = reader.peek(32);
    let index = table.partition_point(|code| code.code <= bits);
    let entry = index
        .checked_sub(1)
        .and_then(|index| table.get(index))
        .ok_or(AudioError::InvalidData("bad AAC Huffman code"))?;
    reader.skip(entry.length as u32)?;
    Ok(entry.symbol)
}

fn read_ics_info(reader: &mut MsbReader<'_>, rate: usize) -> Result<IcsInfo> {
    if reader.bit()? {
        return Err(AudioError::InvalidData("reserved AAC ics_info bit"));
    }
    let mut info = IcsInfo {
        sequence: match reader.read(2)? {
            0 => WindowSequence::OnlyLong,
            1 => WindowSequence::LongStart,
            2 => WindowSequence::EightShort,
            _ => WindowSequence::LongStop,
        },
        shape: reader.bit()?,
        ..IcsInfo::default()
    };
    if info.sequence == WindowSequence::EightShort {
        info.max_sfb = reader.read(4)? as usize;
        let grouping = reader.read(7)?;
        info.groups = 1;
        info.group_lengths[0] = 1;
        for window in 0..7 {
            if grouping & (0x40 >> window) != 0 {
                info.group_lengths[info.groups - 1] += 1;
            } else {
                info.group_lengths[info.groups] = 1;
                info.groups += 1;
            }
        }
        if info.max_sfb + 1 > SWB_SHORT[rate].len() {
            return Err(AudioError::InvalidData("AAC max_sfb exceeds the band count"));
        }
    } else {
        info.max_sfb = reader.read(6)? as usize;
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC Main prediction"));
        }
        info.groups = 1;
        info.group_lengths[0] = 1;
        if info.max_sfb + 1 > SWB_LONG[rate].len() {
            return Err(AudioError::InvalidData("AAC max_sfb exceeds the band count"));
        }
    }
    Ok(info)
}

fn read_sections(reader: &mut MsbReader<'_>, ics: &mut Ics) -> Result<()> {
    let short = ics.info.sequence == WindowSequence::EightShort;
    let (width, escape) = if short { (3, 7) } else { (5, 31) };
    let max_sfb = ics.info.max_sfb;
    for group in 0..ics.info.groups {
        let mut band = 0;
        while band < max_sfb {
            let book = reader.read(4)? as u8;
            if book == 12 {
                return Err(AudioError::InvalidData("reserved AAC codebook"));
            }
            let mut length = 0usize;
            loop {
                let increment = reader.read(width)? as usize;
                length += increment;
                if increment != escape {
                    break;
                }
                // Each step costs input, and the band count ends it.
                if length > max_sfb {
                    break;
                }
            }
            if length == 0 || band + length > max_sfb {
                return Err(AudioError::InvalidData("bad AAC section length"));
            }
            let index = group * max_sfb + band;
            ics.band_types
                .get_mut(index..index + length)
                .ok_or(AudioError::InvalidData("bad AAC section length"))?
                .fill(book);
            band += length;
        }
    }
    Ok(())
}

fn read_scalefactors(reader: &mut MsbReader<'_>, ics: &mut Ics, global_gain: u32) -> Result<()> {
    let mut gain = global_gain as i32;
    let mut noise = global_gain as i32 - 90;
    let mut intensity = 0i32;
    let mut noise_first = true;
    let bands = ics.info.groups * ics.info.max_sfb;
    for index in 0..bands {
        let book = *ics.band_types.get(index).ok_or(AudioError::Overflow)?;
        let value = match book {
            BAND_ZERO => 0,
            BAND_INTENSITY_OUT | BAND_INTENSITY_IN => {
                intensity += read_code(reader, &SCALEFACTOR)? as i32 - 60;
                if !(-155..=100).contains(&intensity) {
                    return Err(AudioError::InvalidData("AAC intensity position out of range"));
                }
                intensity
            }
            BAND_NOISE => {
                if noise_first {
                    noise_first = false;
                    noise += reader.read(9)? as i32 - 256;
                } else {
                    noise += read_code(reader, &SCALEFACTOR)? as i32 - 60;
                }
                if !(-100..=155).contains(&noise) {
                    return Err(AudioError::InvalidData("AAC noise energy out of range"));
                }
                noise
            }
            _ => {
                gain += read_code(reader, &SCALEFACTOR)? as i32 - 60;
                if !(0..=255).contains(&gain) {
                    return Err(AudioError::InvalidData("AAC scalefactor out of range"));
                }
                gain
            }
        };
        ics.scalefactors[index] = value as i16;
    }
    Ok(())
}

/// Pulse data: added to the quantized values once they are all read.
#[derive(Default)]
struct Pulses {
    count: usize,
    start_band: usize,
    offsets: [u8; 4],
    amplitudes: [u8; 4],
}

fn read_pulses(reader: &mut MsbReader<'_>, ics: &Ics) -> Result<Pulses> {
    if ics.info.sequence == WindowSequence::EightShort {
        return Err(AudioError::InvalidData("AAC pulse data in a short block"));
    }
    let mut pulses = Pulses {
        count: reader.read(2)? as usize + 1,
        start_band: reader.read(6)? as usize,
        ..Pulses::default()
    };
    if pulses.start_band > ics.info.max_sfb {
        return Err(AudioError::InvalidData("bad AAC pulse start"));
    }
    for index in 0..pulses.count {
        pulses.offsets[index] = reader.read(5)? as u8;
        pulses.amplitudes[index] = reader.read(4)? as u8;
    }
    Ok(pulses)
}

fn read_tns(reader: &mut MsbReader<'_>, ics: &mut Ics) -> Result<()> {
    let short = ics.info.sequence == WindowSequence::EightShort;
    let windows = if short { 8 } else { 1 };
    let (count_bits, length_bits, order_bits) = if short { (1, 4, 3) } else { (2, 6, 5) };
    ics.tns = Tns::default();
    for window in 0..windows {
        let count = reader.read(count_bits)? as usize;
        ics.tns.counts[window] = count as u8;
        if count == 0 {
            continue;
        }
        let resolution = if reader.bit()? { 4 } else { 3 };
        ics.tns.resolution[window] = resolution;
        for filter in ics.tns.filters[window].iter_mut().take(count) {
            filter.start_band_count = reader.read(length_bits)? as u8;
            let order = reader.read(order_bits)? as usize;
            if order > TNS_MAX_ORDER {
                return Err(AudioError::InvalidData("AAC TNS order too high"));
            }
            filter.order = order as u8;
            if order != 0 {
                filter.backward = reader.bit()?;
                let compress = reader.read(1)?;
                let width = resolution as u32 - compress;
                for slot in filter.coefficients.iter_mut().take(order) {
                    // Sign-extend from the coded width.
                    let value = reader.read(width)? as i32;
                    let shift = 32 - width;
                    *slot = ((value << shift) >> shift) as i8;
                }
            }
        }
    }
    Ok(())
}

/// `q^(4/3)` in Q13, for `q` up to 8191 + a pulse of 15 * 4.
fn inverse_quant(q: u32) -> u64 {
    match POW43.get(q as usize) {
        Some(&value) => value as u64,
        None => power43(q as u64),
    }
}

/// Multiplies a Q13 magnitude by 2^(exponent/4), to Q3.
fn apply_gain(value: u64, exponent: i32) -> i64 {
    let whole = exponent >> 2;
    let mantissa = QUARTER_POWERS[(exponent & 3) as usize];
    // Q13 * Q30 = Q43, to Q3.
    let product = (value as i128) * (mantissa as i128);
    let shift = 40 - whole;
    let scaled = if shift >= 127 {
        0
    } else if shift > 0 {
        (product + (1i128 << (shift - 1))) >> shift
    } else if shift > -64 {
        product.saturating_mul(1i128 << -shift)
    } else {
        i128::MAX
    };
    scaled.min(COEF_LIMIT as i128) as i64
}

/// Reads the spectrum as quantized integers, at each bin's final position.
fn read_spectrum(
    reader: &mut MsbReader<'_>,
    ics: &Ics,
    swb: &[u16],
    quantized: &mut [i32],
) -> Result<()> {
    quantized.fill(0);
    let short = ics.info.sequence == WindowSequence::EightShort;
    let span = if short { SHORT_SAMPLES } else { FRAME_SAMPLES };
    let max_sfb = ics.info.max_sfb;
    let mut window_start = 0;
    for group in 0..ics.info.groups {
        let length = ics.info.group_lengths[group] as usize;
        for band in 0..max_sfb {
            let book = ics.band_types[group * max_sfb + band];
            if book == BAND_ZERO || book >= BAND_NOISE {
                continue;
            }
            let start = swb[band] as usize;
            let end = swb[band + 1] as usize;
            for window in 0..length {
                let offset = window_start + window * span;
                let target = quantized
                    .get_mut(offset + start..offset + end)
                    .ok_or(AudioError::InvalidData("AAC band outside the frame"))?;
                read_band(reader, book, target)?;
            }
        }
        window_start += length * span;
    }
    Ok(())
}

/// Reads one band of one window.
fn read_band(reader: &mut MsbReader<'_>, book: u8, band: &mut [i32]) -> Result<()> {
    let table = *BOOKS
        .get(book as usize - 1)
        .ok_or(AudioError::InvalidData("reserved AAC codebook"))?;
    if book <= 4 {
        let signed = book <= 2;
        for chunk in band.chunks_mut(4) {
            let symbol = read_code(reader, table)? as i32;
            let mut values = [
                symbol / 27,
                (symbol / 9) % 3,
                (symbol / 3) % 3,
                symbol % 3,
            ];
            for value in values.iter_mut() {
                if signed {
                    *value -= 1;
                } else if *value != 0 && reader.bit()? {
                    *value = -*value;
                }
            }
            for (slot, &value) in chunk.iter_mut().zip(&values) {
                *slot = value;
            }
        }
    } else {
        let modulus = match book {
            5 | 6 => 9,
            7 | 8 => 8,
            9 | 10 => 13,
            _ => 17,
        };
        let signed = book <= 6;
        for chunk in band.chunks_mut(2) {
            let symbol = read_code(reader, table)? as i32;
            let mut values = [symbol / modulus, symbol % modulus];
            if signed {
                values[0] -= 4;
                values[1] -= 4;
            } else {
                let negative = [
                    values[0] != 0 && reader.bit()?,
                    values[1] != 0 && reader.bit()?,
                ];
                if book == 11 {
                    for value in values.iter_mut() {
                        if *value == 16 {
                            *value = read_escape(reader)?;
                        }
                    }
                }
                for (value, negative) in values.iter_mut().zip(negative) {
                    if negative {
                        *value = -*value;
                    }
                }
            }
            for (slot, &value) in chunk.iter_mut().zip(&values) {
                *slot = value;
            }
        }
    }
    Ok(())
}

fn read_escape(reader: &mut MsbReader<'_>) -> Result<i32> {
    let mut count = 4;
    while reader.bit()? {
        count += 1;
        // The largest legal value, 8191, has 13 bits.
        if count > 12 {
            return Err(AudioError::InvalidData("AAC escape too long"));
        }
    }
    Ok((1 << count) + reader.read(count)? as i32)
}

fn integer_sqrt(value: u64) -> u64 {
    let mut root = 0u64;
    let mut bit = 1u64 << 31;
    while bit != 0 {
        let candidate = root | bit;
        if candidate.checked_mul(candidate).is_some_and(|square| square <= value) {
            root = candidate;
        }
        bit >>= 1;
    }
    root
}

/// A decoder for raw AAC-LC access units.
pub struct AacDecoder {
    config: AudioSpecificConfig,
    rate: usize,
    channels: usize,
    long: Imdct,
    short: Imdct,
    sine_long: Vec<i32>,
    sine_short: Vec<i32>,
    /// Q31 TNS reflection coefficients: 8 values at 3 bits, then 16 at 4 bits,
    /// each indexed by its signed value plus half the count.
    tns_table: [i32; 24],
    /// `channels * 1024` values, per channel.
    spectrum: Vec<i32>,
    overlap: Vec<i32>,
    /// One channel's working space.
    quantized: Vec<i32>,
    /// One transform's output, and a whole channel's windowed output.
    transform: Vec<i32>,
    time: Vec<i32>,
    /// Per channel: the window shape of the previous frame.
    previous_shape: [bool; MAX_CHANNELS],
    /// Noise generator state.
    noise: u32,
}

impl AacDecoder {
    /// Creates a decoder. The config must have come from
    /// [`AudioSpecificConfig::parse`] or [`AudioSpecificConfig::new`].
    pub fn new(config: AudioSpecificConfig) -> Result<Self> {
        let checked = AudioSpecificConfig::new(config.sample_rate_index, config.channel_config)?;
        let channels = checked.channels();
        let mut sine_long = try_vec(FRAME_SAMPLES, 0i32)?;
        let mut sine_short = try_vec(SHORT_SAMPLES, 0i32)?;
        for (window, bits) in [(&mut sine_long, 13u32), (&mut sine_short, 10)] {
            for (i, slot) in window.iter_mut().enumerate() {
                // sin((i + 1/2) pi / (2 length)): (2i + 1) / (8 length) of a turn.
                let phase = ((2 * i as u128 + 1) << (64 - bits)) as u64;
                *slot = q62_to_q31(sin_turn(phase));
            }
        }
        let mut tns_table = [0i32; 24];
        for (slot_index, slot) in tns_table.iter_mut().enumerate() {
            let (resolution, index) = if slot_index < 8 {
                (3u32, slot_index as i64 - 4)
            } else {
                (4u32, slot_index as i64 - 16)
            };
            // sin(i / iqfac) with iqfac = (2^(res-1) -+ 1/2) / (pi/2), which
            // works out at i / (2^(res+1) -+ 2) of a turn.
            let denominator = (1i128 << (resolution + 1)) + if index >= 0 { -2 } else { 2 };
            let phase = ((index as i128) << 64) / denominator;
            *slot = q62_to_q31(sin_turn(phase as u64));
        }
        Ok(Self {
            config: checked,
            rate: checked.sample_rate_index as usize,
            channels,
            long: Imdct::new(2 * FRAME_SAMPLES)?,
            short: Imdct::new(2 * SHORT_SAMPLES)?,
            sine_long,
            sine_short,
            tns_table,
            spectrum: try_vec(channels * FRAME_SAMPLES, 0)?,
            overlap: try_vec(channels * FRAME_SAMPLES, 0)?,
            quantized: try_vec(FRAME_SAMPLES, 0)?,
            transform: try_vec(2 * FRAME_SAMPLES, 0)?,
            time: try_vec(2 * FRAME_SAMPLES, 0)?,
            previous_shape: [false; MAX_CHANNELS],
            noise: 0x1f2e_3d4c,
        })
    }

    /// The config in use.
    pub fn config(&self) -> AudioSpecificConfig {
        self.config
    }

    /// Clears the overlap state, for a seek.
    pub fn reset(&mut self) {
        self.overlap.fill(0);
        self.previous_shape = [false; MAX_CHANNELS];
    }

    fn swb(&self, info: &IcsInfo) -> &'static [u16] {
        if info.sequence == WindowSequence::EightShort {
            SWB_SHORT[self.rate]
        } else {
            SWB_LONG[self.rate]
        }
    }

    /// Reads one `individual_channel_stream`, leaving the channel's spectrum
    /// in `self.spectrum`. For a shared window, `common` is the channel pair's.
    fn read_channel(
        &mut self,
        reader: &mut MsbReader<'_>,
        channel: usize,
        common: Option<IcsInfo>,
        ics: &mut Ics,
    ) -> Result<()> {
        let global_gain = reader.read(8)?;
        ics.info = match common {
            Some(info) => info,
            None => read_ics_info(reader, self.rate)?,
        };
        read_sections(reader, ics)?;
        read_scalefactors(reader, ics, global_gain)?;

        let pulses = if reader.bit()? {
            Some(read_pulses(reader, ics)?)
        } else {
            None
        };
        ics.tns_present = reader.bit()?;
        if ics.tns_present {
            read_tns(reader, ics)?;
        }
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC gain control"));
        }

        let swb = self.swb(&ics.info);
        read_spectrum(reader, ics, swb, &mut self.quantized)?;
        if let Some(pulses) = pulses {
            let mut position = *swb
                .get(pulses.start_band)
                .ok_or(AudioError::InvalidData("bad AAC pulse start"))?
                as usize;
            for index in 0..pulses.count {
                position += pulses.offsets[index] as usize;
                let slot = self
                    .quantized
                    .get_mut(position)
                    .ok_or(AudioError::InvalidData("AAC pulse outside the frame"))?;
                let amplitude = pulses.amplitudes[index] as i32;
                *slot += if *slot < 0 { -amplitude } else { amplitude };
            }
        }
        self.dequantize(channel, ics, swb);
        Ok(())
    }

    /// Scales quantized values to coefficients, filling noise bands.
    fn dequantize(&mut self, channel: usize, ics: &Ics, swb: &[u16]) {
        let start = channel * FRAME_SAMPLES;
        let Some(spectrum) = self.spectrum.get_mut(start..start + FRAME_SAMPLES) else {
            return;
        };
        spectrum.fill(0);
        let short = ics.info.sequence == WindowSequence::EightShort;
        let span = if short { SHORT_SAMPLES } else { FRAME_SAMPLES };
        let max_sfb = ics.info.max_sfb;
        let mut window_start = 0;
        for group in 0..ics.info.groups {
            let length = ics.info.group_lengths[group] as usize;
            for band in 0..max_sfb {
                let index = group * max_sfb + band;
                let book = ics.band_types[index];
                let (low, high) = (swb[band] as usize, swb[band + 1] as usize);
                for window in 0..length {
                    let offset = window_start + window * span;
                    let range = offset + low..offset + high;
                    match book {
                        BAND_ZERO | BAND_INTENSITY_IN | BAND_INTENSITY_OUT => {}
                        BAND_NOISE => {
                            let energy = ics.scalefactors[index] as i32;
                            fill_noise(&mut spectrum[range], energy, &mut self.noise);
                        }
                        _ => {
                            let gain = ics.scalefactors[index] as i32 - 100;
                            let input = &self.quantized[range.clone()];
                            for (slot, &q) in spectrum[range].iter_mut().zip(input) {
                                if q != 0 {
                                    let value = apply_gain(inverse_quant(q.unsigned_abs()), gain);
                                    *slot = (if q < 0 { -value } else { value }) as i32;
                                }
                            }
                        }
                    }
                }
            }
            window_start += length * span;
        }
    }

    /// M/S and intensity stereo over a pair of channels sharing a window.
    fn apply_stereo(
        &mut self,
        pair: (usize, usize),
        left: &Ics,
        right: &Ics,
        ms_present: u32,
        ms_mask: &[bool; MAX_SECTIONS],
    ) {
        let swb = self.swb(&right.info);
        let short = right.info.sequence == WindowSequence::EightShort;
        let span = if short { SHORT_SAMPLES } else { FRAME_SAMPLES };
        let max_sfb = right.info.max_sfb;
        let Some(both) = self.spectrum.get_mut(pair.0 * FRAME_SAMPLES..(pair.1 + 1) * FRAME_SAMPLES)
        else {
            return;
        };
        let (first, second) = both.split_at_mut(FRAME_SAMPLES);
        let mut window_start = 0;
        for group in 0..right.info.groups {
            let length = right.info.group_lengths[group] as usize;
            for band in 0..max_sfb {
                let index = group * max_sfb + band;
                let book = right.band_types[index];
                let (low, high) = (swb[band] as usize, swb[band + 1] as usize);
                let masked = ms_present != 0 && ms_mask[index];
                for window in 0..length {
                    let offset = window_start + window * span;
                    let range = offset + low..offset + high;
                    let left_band = &mut first[range.clone()];
                    let right_band = &mut second[range];
                    if book == BAND_INTENSITY_IN || book == BAND_INTENSITY_OUT {
                        // The standard reads as if only `ms_mask_present == 1`
                        // inverts; every decoder also does it for 2, the mask
                        // covering all bands, and encoders expect that.
                        let mut invert = book == BAND_INTENSITY_OUT;
                        if masked {
                            invert = !invert;
                        }
                        let position = right.scalefactors[index] as i32;
                        for (r, &l) in right_band.iter_mut().zip(left_band.iter()) {
                            // 0.5^(position/4)
                            let magnitude = l.unsigned_abs() as u64;
                            let scaled = apply_gain(magnitude << 10, -position) as i32;
                            let signed = if l < 0 { -scaled } else { scaled };
                            *r = if invert { -signed } else { signed };
                        }
                    } else if masked
                        && book < BAND_NOISE
                        && left.band_types[index] < BAND_NOISE
                    {
                        for (l, r) in left_band.iter_mut().zip(right_band.iter_mut()) {
                            let (m, s) = (*l, *r);
                            *l = m.wrapping_add(s);
                            *r = m.wrapping_sub(s);
                        }
                    }
                }
            }
            window_start += length * span;
        }
    }

    fn apply_tns(&mut self, channel: usize, ics: &Ics) {
        if !ics.tns_present {
            return;
        }
        let short = ics.info.sequence == WindowSequence::EightShort;
        let (windows, span) = if short { (8, SHORT_SAMPLES) } else { (1, FRAME_SAMPLES) };
        let swb = self.swb(&ics.info);
        let max_bands = if short {
            TNS_MAX_BANDS_SHORT[self.rate]
        } else {
            TNS_MAX_BANDS_LONG[self.rate]
        } as usize;
        let bands = swb.len() - 1;
        let start = channel * FRAME_SAMPLES;
        let Some(spectrum) = self.spectrum.get_mut(start..start + FRAME_SAMPLES) else {
            return;
        };
        for window in 0..windows {
            let resolution = ics.tns.resolution[window];
            let mut top = bands;
            for filter in ics.tns.filters[window]
                .iter()
                .take(ics.tns.counts[window] as usize)
            {
                let bottom = top.saturating_sub(filter.start_band_count as usize);
                let order = filter.order as usize;
                let limit = max_bands.min(ics.info.max_sfb).min(bands);
                let from = swb[bottom.min(limit)] as usize;
                let to = swb[top.min(limit)] as usize;
                top = bottom;
                if order == 0 || to <= from {
                    continue;
                }

                // Reflection coefficients to direct form, in Q24.
                let mut lpc = [0i64; TNS_MAX_ORDER + 1];
                let base = if resolution == 3 { 4 } else { 16 };
                for m in 0..order {
                    let index = base + filter.coefficients[m] as i32;
                    let k = (self.tns_table.get(index as usize).copied().unwrap_or(0) as i64) >> 7;
                    let mut next = lpc;
                    for i in 0..m {
                        next[i] = lpc[i].wrapping_add(k.wrapping_mul(lpc[m - 1 - i]) >> 24);
                    }
                    next[m] = k;
                    lpc = next;
                }

                // The all-pole filter, in the direction the stream says.
                let offset = window * span;
                let Some(band) = spectrum.get_mut(offset + from..offset + to) else {
                    continue;
                };
                let size = band.len();
                for m in 0..size {
                    let position = if filter.backward { size - 1 - m } else { m };
                    let mut sum = 0i64;
                    for (i, &coefficient) in lpc[..order.min(m)].iter().enumerate() {
                        let distance = i + 1;
                        let past = if filter.backward {
                            band[position + distance]
                        } else {
                            band[position - distance]
                        };
                        sum = sum.wrapping_add(coefficient.wrapping_mul(past as i64));
                    }
                    let value = (band[position] as i64)
                        .wrapping_sub(sum.wrapping_add(1 << 23) >> 24);
                    band[position] = value.clamp(-COEF_LIMIT, COEF_LIMIT) as i32;
                }
            }
        }
    }

    /// Transforms a channel's spectrum and writes it to `out` at `position`.
    fn synthesize(
        &mut self,
        channel: usize,
        ics: &Ics,
        out: &mut [i16],
        position: usize,
    ) {
        let info = &ics.info;
        let previous_shape = self.previous_shape[channel];
        let long_previous: &[i32] = if previous_shape { &KBD_LONG } else { &self.sine_long };
        let short_previous: &[i32] = if previous_shape { &KBD_SHORT } else { &self.sine_short };
        let long_current: &[i32] = if info.shape { &KBD_LONG } else { &self.sine_long };
        let short_current: &[i32] = if info.shape { &KBD_SHORT } else { &self.sine_short };

        let start = channel * FRAME_SAMPLES;
        let spectrum = &self.spectrum[start..start + FRAME_SAMPLES];
        let time = &mut self.time[..2 * FRAME_SAMPLES];

        if info.sequence == WindowSequence::EightShort {
            time.fill(0);
            let transform = &mut self.transform[..2 * SHORT_SAMPLES];
            for window in 0..8 {
                let input = &spectrum[window * SHORT_SAMPLES..(window + 1) * SHORT_SAMPLES];
                self.short.run(input, transform);
                let rising = if window == 0 { short_previous } else { short_current };
                let at = 448 + window * SHORT_SAMPLES;
                for i in 0..SHORT_SAMPLES {
                    // The short transform's scale is 2/256, so this is 8x low.
                    let head = mdct::window(transform[i], rising[i]);
                    let tail = mdct::window(
                        transform[SHORT_SAMPLES + i],
                        short_current[SHORT_SAMPLES - 1 - i],
                    );
                    time[at + i] = time[at + i].wrapping_add(head.wrapping_shl(3));
                    time[at + SHORT_SAMPLES + i] =
                        time[at + SHORT_SAMPLES + i].wrapping_add(tail.wrapping_shl(3));
                }
            }
        } else {
            self.long.run(spectrum, time);
            let (left, right) = time.split_at_mut(FRAME_SAMPLES);
            match info.sequence {
                WindowSequence::LongStop => {
                    left[..448].fill(0);
                    for (slot, &weight) in left[448..576].iter_mut().zip(short_previous) {
                        *slot = mdct::window(*slot, weight);
                    }
                }
                _ => {
                    for (slot, &weight) in left.iter_mut().zip(long_previous) {
                        *slot = mdct::window(*slot, weight);
                    }
                }
            }
            match info.sequence {
                WindowSequence::LongStart => {
                    for (slot, &weight) in right[448..576].iter_mut().zip(short_current.iter().rev())
                    {
                        *slot = mdct::window(*slot, weight);
                    }
                    right[576..].fill(0);
                }
                _ => {
                    for (slot, &weight) in right.iter_mut().zip(long_current.iter().rev()) {
                        *slot = mdct::window(*slot, weight);
                    }
                }
            }
        }

        let channels = self.channels;
        let overlap = &mut self.overlap[start..start + FRAME_SAMPLES];
        let rounding = 1i64 << (PCM_FRACTION - 1);
        for (i, (&saved, &new)) in overlap.iter().zip(time[..FRAME_SAMPLES].iter()).enumerate() {
            let sum = saved as i64 + new as i64;
            let sample = (sum + rounding) >> PCM_FRACTION;
            out[i * channels + position] = sample.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        }
        overlap.copy_from_slice(&time[FRAME_SAMPLES..]);
        self.previous_shape[channel] = info.shape;
    }

    /// Decodes one raw access unit (`raw_data_block`), such as an MP4 sample.
    ///
    /// Writes [`FRAME_SAMPLES`] interleaved frames to `out` and returns the
    /// count. Channels the frame doesn't mention are silent.
    pub fn decode_raw_frame(&mut self, frame: &[u8], out: &mut [i16]) -> Result<usize> {
        let mut reader = MsbReader::new(frame);
        self.decode_block(&mut reader, out)
    }

    fn decode_block(&mut self, reader: &mut MsbReader<'_>, out: &mut [i16]) -> Result<usize> {
        let required = FRAME_SAMPLES * self.channels;
        if out.len() < required {
            return Err(AudioError::BufferTooSmall {
                required,
                available: out.len(),
            });
        }
        let channels = self.channels;
        let order = OUTPUT_ORDER[self.config.channel_config as usize];
        let mut next_channel = 0usize;
        let mut ics = [Ics::new(), Ics::new()];
        let mut present = [false; MAX_CHANNELS];

        // Finished channels are written as soon as they are complete, which
        // also bounds the working memory to one channel pair.
        let mut positions = [0usize; MAX_CHANNELS];
        for (position, &channel) in order.iter().enumerate() {
            positions[channel as usize] = position;
        }

        loop {
            match reader.read(3)? {
                0 | 3 => {
                    // SCE or LFE.
                    reader.read(4)?;
                    if next_channel >= channels {
                        return Err(AudioError::InvalidData("too many AAC channels"));
                    }
                    let channel = next_channel;
                    next_channel += 1;
                    self.read_channel(reader, channel, None, &mut ics[0])?;
                    self.apply_tns(channel, &ics[0]);
                    self.synthesize(channel, &ics[0], out, positions[channel]);
                    present[channel] = true;
                }
                1 => {
                    // CPE.
                    reader.read(4)?;
                    if next_channel + 2 > channels {
                        return Err(AudioError::InvalidData("too many AAC channels"));
                    }
                    let (a, b) = (next_channel, next_channel + 1);
                    next_channel += 2;
                    let common = reader.bit()?;
                    let mut ms_present = 0;
                    let mut ms_mask = [false; MAX_SECTIONS];
                    let mut shared = None;
                    if common {
                        let info = read_ics_info(reader, self.rate)?;
                        shared = Some(info);
                        ms_present = reader.read(2)?;
                        match ms_present {
                            0 => {}
                            1 => {
                                let bands = info.groups * info.max_sfb;
                                for slot in ms_mask.iter_mut().take(bands) {
                                    *slot = reader.bit()?;
                                }
                            }
                            2 => ms_mask.fill(true),
                            _ => {
                                return Err(AudioError::InvalidData("reserved AAC M/S mask"));
                            }
                        }
                    }
                    let [first, second] = &mut ics;
                    self.read_channel(reader, a, shared, first)?;
                    self.read_channel(reader, b, shared, second)?;
                    if common {
                        self.apply_stereo((a, b), first, second, ms_present, &ms_mask);
                    }
                    self.apply_tns(a, first);
                    self.apply_tns(b, second);
                    self.synthesize(a, first, out, positions[a]);
                    self.synthesize(b, second, out, positions[b]);
                    present[a] = true;
                    present[b] = true;
                }
                2 => {
                    return Err(AudioError::UnsupportedFeature("AAC coupling channel"));
                }
                4 => {
                    // Data stream element.
                    reader.read(4)?;
                    let align = reader.bit()?;
                    let mut count = reader.read(8)? as usize;
                    if count == 255 {
                        count += reader.read(8)? as usize;
                    }
                    if align {
                        reader.align();
                    }
                    reader.skip_long(count * 8)?;
                }
                5 => {
                    return Err(AudioError::UnsupportedFeature(
                        "AAC program config element",
                    ));
                }
                6 => {
                    // Fill element, which holds nothing LC needs.
                    let mut count = reader.read(4)? as usize;
                    if count == 15 {
                        count += reader.read(8)? as usize;
                        count -= 1;
                    }
                    reader.skip_long(count * 8)?;
                }
                _ => break,
            }
        }

        // A channel the frame didn't carry is silent, but its old tail must
        // still ring out so that it fades instead of clicking.
        let silent = Ics::new();
        for channel in 0..channels {
            if !present[channel] {
                self.spectrum[channel * FRAME_SAMPLES..(channel + 1) * FRAME_SAMPLES].fill(0);
                self.synthesize(channel, &silent, out, positions[channel]);
            }
        }
        Ok(FRAME_SAMPLES)
    }
}

/// Fills a band with noise of the given energy, 2^(energy/4) over the band.
fn fill_noise(band: &mut [i32], energy: i32, state: &mut u32) {
    let mut total = 0u64;
    for slot in band.iter_mut() {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let value = ((*state >> 16) as i32) - (1 << 15);
        *slot = value;
        total += (value as i64 * value as i64) as u64;
    }
    let root = integer_sqrt(total).max(1);
    // value / root * 2^(energy/4), to Q3. The band's total energy is then
    // 2^(energy/2). A coefficient is the ratio in Q13, as `apply_gain` takes.
    for slot in band.iter_mut() {
        let magnitude = ((slot.unsigned_abs() as u64) << 13) / root;
        let scaled = apply_gain(magnitude, energy) as i32;
        *slot = if *slot < 0 { -scaled } else { scaled };
    }
}

/// A parsed ADTS frame header.
#[derive(Clone, Copy)]
struct AdtsHeader {
    config: AudioSpecificConfig,
    /// Bytes before the raw data.
    header_len: usize,
    /// Bytes in the whole frame.
    frame_len: usize,
    /// Raw data blocks in the frame.
    blocks: usize,
}

/// Parses the header at the start of `data`. `Ok(None)` means it isn't one;
/// a valid header that uses a refused feature is an error.
fn parse_adts(data: &[u8]) -> Result<Option<AdtsHeader>> {
    let Some(head) = data.get(..7) else {
        return Ok(None);
    };
    if head[0] != 0xff || head[1] & 0xf6 != 0xf0 {
        return Ok(None);
    }
    let protection_absent = head[1] & 1 != 0;
    let profile = head[2] >> 6;
    let rate_index = (head[2] >> 2) & 15;
    let channel_config = ((head[2] & 1) << 2) | (head[3] >> 6);
    let frame_len =
        (((head[3] & 3) as usize) << 11) | ((head[4] as usize) << 3) | ((head[5] as usize) >> 5);
    let blocks = (head[6] & 3) as usize + 1;
    let header_len = if protection_absent { 7 } else { 9 };
    if frame_len < header_len || rate_index as usize >= SAMPLE_RATES.len() {
        return Ok(None);
    }
    if profile != 1 {
        return Err(AudioError::UnsupportedFeature("AAC object type"));
    }
    if blocks > 1 && !protection_absent {
        return Err(AudioError::UnsupportedFeature(
            "AAC ADTS checksum over several blocks",
        ));
    }
    Ok(Some(AdtsHeader {
        config: AudioSpecificConfig::new(rate_index, channel_config)?,
        header_len,
        frame_len,
        blocks,
    }))
}

/// Finds the next ADTS frame from `pos`. Returns its offset and header.
fn next_adts(input: &[u8], mut pos: usize) -> Result<Option<(usize, AdtsHeader)>> {
    while let Some(rest) = input.get(pos..) {
        if rest.len() < 7 {
            break;
        }
        match parse_adts(rest)? {
            Some(header) => return Ok(Some((pos, header))),
            None => pos += 1,
        }
    }
    Ok(None)
}

/// A stateful decoder for an ADTS stream, one frame at a time.
pub struct AdtsDecoder<'a> {
    input: &'a [u8],
    pos: usize,
    decoder: AacDecoder,
}

impl<'a> AdtsDecoder<'a> {
    /// Finds the first frame of an ADTS stream.
    pub fn new(input: &'a [u8]) -> Result<Self> {
        let start = id3_len(input);
        let (pos, header) =
            next_adts(input, start)?.ok_or(AudioError::InvalidData("no AAC frame found"))?;
        Ok(Self {
            input,
            pos,
            decoder: AacDecoder::new(header.config)?,
        })
    }

    /// The first frame's parameters; the format can't change mid-stream.
    pub fn config(&self) -> AudioSpecificConfig {
        self.decoder.config()
    }

    /// The most samples, counting every channel, one ADTS frame can produce.
    pub fn max_frame_samples(&self) -> usize {
        4 * FRAME_SAMPLES * self.decoder.channels
    }

    /// Decodes the next frame, appending interleaved samples to `out`.
    ///
    /// Returns the sample frames written, or `None` at the end of the stream,
    /// including when the last frame was cut short.
    pub fn next_frame(&mut self, out: &mut [i16]) -> Result<Option<usize>> {
        let Some((pos, header)) = next_adts(self.input, self.pos)? else {
            self.pos = self.input.len();
            return Ok(None);
        };
        let channels = self.decoder.channels;
        if header.config != self.decoder.config {
            return Err(AudioError::UnsupportedFeature(
                "AAC stream changes format mid-stream",
            ));
        }
        let required = header.blocks * FRAME_SAMPLES * channels;
        if out.len() < required {
            return Err(AudioError::BufferTooSmall {
                required,
                available: out.len(),
            });
        }
        let Some(frame) = self.input.get(pos + header.header_len..pos + header.frame_len) else {
            self.pos = self.input.len();
            return Ok(None);
        };
        self.pos = pos + header.frame_len;
        let mut reader = MsbReader::new(frame);
        for block in 0..header.blocks {
            let start = block * FRAME_SAMPLES * channels;
            self.decoder.decode_block(&mut reader, &mut out[start..required])?;
            reader.align();
        }
        Ok(Some(header.blocks * FRAME_SAMPLES))
    }
}

/// Returns the properties of an ADTS stream, by walking its frame headers.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let start = id3_len(input);
    let (mut pos, first) =
        next_adts(input, start)?.ok_or(AudioError::InvalidData("no AAC frame found"))?;
    let mut frames = 0u64;
    loop {
        let Some((at, header)) = next_adts(input, pos)? else {
            break;
        };
        // A cut-off last frame does not count: it will not decode.
        if at + header.frame_len > input.len() {
            break;
        }
        frames += (header.blocks * FRAME_SAMPLES) as u64;
        pos = at + header.frame_len;
    }
    Ok(AudioInfo {
        sample_rate: first.config.sample_rate,
        channels: first.config.channels() as u16,
        frames,
        format: AudioFormat::Aac,
    })
}

/// ADTS decoding takes no scratch space from the caller.
pub fn scratch_len(_input: &[u8]) -> Result<usize> {
    Ok(0)
}

/// Decodes an ADTS stream to interleaved signed 16-bit samples.
pub fn decode(input: &[u8], out: &mut [i16]) -> Result<DecodedAudio> {
    let mut decoder = AdtsDecoder::new(input)?;
    let config = decoder.config();
    let channels = config.channels();
    let mut written = 0usize;
    loop {
        let rest = out.get_mut(written..).unwrap_or(&mut []);
        match decoder.next_frame(rest) {
            Ok(Some(frames)) => written += frames * channels,
            Ok(None) => break,
            Err(AudioError::BufferTooSmall { required, .. }) => {
                return Err(AudioError::BufferTooSmall {
                    required: written + required,
                    available: out.len(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let frames = written / channels;
    Ok(DecodedAudio {
        info: AudioInfo {
            sample_rate: config.sample_rate,
            channels: channels as u16,
            frames: frames as u64,
            format: AudioFormat::Aac,
        },
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_quantizer_is_accurate() {
        assert_eq!(inverse_quant(0), 0);
        assert_eq!(inverse_quant(1), 1 << 13);
        assert_eq!(inverse_quant(8), 16 << 13);
        assert_eq!(inverse_quant(27), 81 << 13);
        // Past the table: 1000^(4/3) = 10000, and the top of the range.
        assert_eq!(inverse_quant(1000), 10_000 << 13);
        let top = inverse_quant(8191) as f64 / 8192.0;
        assert!((top - 8191f64.powf(4.0 / 3.0)).abs() < 1.1);
    }

    #[test]
    fn gain_is_accurate() {
        // 1.0 in Q13, at 2^0, to Q3.
        assert_eq!(apply_gain(1 << 13, 0), 8);
        assert_eq!(apply_gain(1 << 13, 4), 16);
        assert_eq!(apply_gain(1 << 13, 8), 32);
        // 2^(1/4) * 2^10 in Q3.
        assert_eq!(apply_gain(1 << 13, 41), 9742);
        assert_eq!(apply_gain(1 << 13, -1000), 0);
        assert_eq!(apply_gain(u32::MAX as u64, 1000), COEF_LIMIT);
    }

    #[test]
    fn config_parses_and_refuses() {
        // AAC-LC, 44.1 kHz, stereo: 00010 0100 0010 000.
        let config = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
        assert_eq!(config.sample_rate, 44_100);
        assert_eq!(config.channels(), 2);
        // Explicit HE-AAC: LC at 24 kHz, stereo, with SBR up to 48 kHz. Bits:
        // 00101 0110 0010 0011 00010 000
        let he = AudioSpecificConfig::parse(&[0x2b, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!((he.object_type, he.sample_rate, he.channel_config), (2, 24000, 2));
        // An explicit rate: 00010 1111 <24 bits of 44000> 0010 000.
        let explicit = AudioSpecificConfig::parse(&[0x17, 0x80, 0x55, 0xf0, 0x10]).unwrap();
        assert_eq!((explicit.sample_rate, explicit.sample_rate_index), (44000, 4));
        // Channel configuration 0 needs a program config element.
        assert!(matches!(
            AudioSpecificConfig::parse(&[0x12, 0x00]),
            Err(AudioError::UnsupportedFeature(_))
        ));
        assert_eq!(
            AudioSpecificConfig::parse(&[0x12]),
            Err(AudioError::UnexpectedEnd)
        );
    }

    #[test]
    fn windows_are_power_complementary() {
        for table in [&KBD_LONG[..], &KBD_SHORT[..]] {
            let n = table.len();
            for i in 0..n {
                let (a, b) = (table[i] as i64, table[n - 1 - i] as i64);
                assert!((a * a + b * b - (1 << 62)).abs() < 1 << 36, "{i}");
            }
        }
    }

    #[test]
    fn sine_windows_are_power_complementary() {
        let decoder = AacDecoder::new(AudioSpecificConfig::new(3, 1).unwrap()).unwrap();
        for table in [&decoder.sine_long[..], &decoder.sine_short[..]] {
            let n = table.len();
            for i in 0..n {
                let (a, b) = (table[i] as i64, table[n - 1 - i] as i64);
                assert!((a * a + b * b - (1 << 62)).abs() < 1 << 36, "{i}");
            }
        }
    }

    #[test]
    fn square_roots() {
        assert_eq!(integer_sqrt(0), 0);
        assert_eq!(integer_sqrt(15), 3);
        assert_eq!(integer_sqrt(16), 4);
        assert_eq!(integer_sqrt(u64::MAX), u32::MAX as u64);
    }
}
