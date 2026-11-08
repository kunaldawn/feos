//! AAC decoding, in fixed-point integer arithmetic.
//!
//! Input is either an ADTS stream ([`info`], [`decode`], [`AdtsDecoder`]) or
//! raw access units from a container, which go through
//! [`AacDecoder::decode_raw_frame`] with the MP4 `AudioSpecificConfig` parsed by
//! [`AudioSpecificConfig::parse`].
//!
//! Implemented: LC and Main (with its backward-adaptive prediction), long and short
//! blocks, both window shapes, M/S and intensity stereo, perceptual noise substitution,
//! TNS, pulse data, channel configurations 1 to 7 and program config elements, with
//! output in WAVE channel order.
//!
//! Coupling channels are too, dependent and independent.
//!
//! 960-sample frames and long term prediction (AAC-LTP) are done too.
//!
//! Refused, with [`AudioError::UnsupportedFeature`], since each would otherwise
//! decode to the wrong sound: any other object type and gain control. HE-AAC, in
//! whichever way it signals SBR and PS, is decoded by `sbr.rs` and `ps.rs`. With
//! 960-sample frames, those are ignored and the core plays by itself. A rate that isn't
//! in the standard's table uses the bands of the nearest one, as the standard says.
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
    Code, BOOKS, KBD_LONG, KBD_LONG_960, KBD_SHORT, KBD_SHORT_120, SCALEFACTOR, SWB_LONG,
    SWB_LONG_960, SWB_SHORT, SWB_SHORT_120,
};
use crate::bits::{try_vec, MsbReader};
use crate::error::{AudioError, Result};
use crate::flac::id3_len;
use crate::mdct::{self, q62_to_q31, sin_turn, Imdct};
use crate::sbr::SbrDecoder;
use crate::{AudioFormat, AudioInfo, DecodedAudio};

/// Samples per channel in a frame, at most.
pub const FRAME_SAMPLES: usize = 1024;
/// Bands LTP can apply to.
const LTP_MAX_BANDS: usize = 40;
/// The object types: Main, LC and LTP.
const OBJECT_MAIN: u8 = 1;
const OBJECT_LC: u8 = 2;
const OBJECT_LTP: u8 = 4;
/// The gain of an LTP prediction, from the standard's table, in Q15.
const LTP_GAINS: [i64; 8] = [18705, 22827, 26640, 29862, 32273, 34993, 39145, 44877];
/// The most channels a channel configuration can give.
/// The most channels this decodes.
pub const MAX_CHANNELS: usize = 8;
const MAX_SECTIONS: usize = 120;
/// The most a TNS filter can have: 12 for LC, and 20 for Main.
const TNS_MAX_ORDER: usize = 20;

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
    /// MPEG-4 audio object type of the core: 1 Main, 2 LC or 4 LTP.
    pub object_type: u8,
    /// Index into the standard's table of sampling frequencies.
    pub sample_rate_index: u8,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel configuration, 1 to 7, or 0 with a program config element.
    pub channel_config: u8,
    /// The channels, when a program config element says what they are.
    pub program: Option<ProgramConfig>,
    /// 960-sample frames, rather than 1024.
    pub short_frames: bool,
    /// Spectral band replication: on, off, or `None` for not knowing until a frame shows it.
    pub sbr: Option<bool>,
    /// Parametric stereo, likewise.
    pub ps: Option<bool>,
    /// The output rate a config states for SBR, if it does.
    pub extension_rate: Option<u32>,
}

/// One element a program config element describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ProgramElement {
    /// The syntactic element: 0 for a single channel, 1 for a pair, 3 for LFE.
    pub kind: u8,
    /// Its instance tag.
    pub tag: u8,
    /// The decoded channel it starts at.
    pub channel: u8,
}

/// The channels a program config element lays out, up to [`MAX_CHANNELS`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ProgramConfig {
    /// Elements in decoding order.
    pub elements: [ProgramElement; MAX_CHANNELS],
    /// How many of `elements` there are.
    pub element_count: u8,
    /// Decoded channels in all.
    pub channels: u8,
    /// For each decoded channel, its position in the output.
    pub positions: [u8; MAX_CHANNELS],
}

/// Reads a `program_config_element`, starting after its element id, if any.
fn read_program_config(reader: &mut MsbReader<'_>) -> Result<ProgramConfig> {
    reader.read(4)?; // Its own instance tag.
    reader.read(2)?; // Object type, which the stream's other headers say.
    reader.read(4)?; // Sampling index, likewise.
    let front = reader.read(4)? as usize;
    let side = reader.read(4)? as usize;
    let back = reader.read(4)? as usize;
    let lfe = reader.read(2)? as usize;
    let data = reader.read(3)? as usize;
    let coupling = reader.read(4)? as usize;
    if reader.bit()? {
        reader.read(4)?;
    }
    if reader.bit()? {
        reader.read(4)?;
    }
    if reader.bit()? {
        reader.read(3)?;
    }

    // Laid out in WAVE order: the ranks are positions before any gaps are closed.
    let mut config = ProgramConfig::default();
    let mut ranks = [(u8::MAX, 0u8); MAX_CHANNELS];
    let mut channels = 0usize;
    let mut elements = 0usize;
    let mut add = |kind: u8, tag: u8, rank: &[u8], config: &mut ProgramConfig| -> Result<()> {
        if channels + rank.len() > MAX_CHANNELS || elements >= MAX_CHANNELS {
            return Err(AudioError::UnsupportedFeature("AAC with more than eight channels"));
        }
        config.elements[elements] = ProgramElement {
            kind,
            tag,
            channel: channels as u8,
        };
        for &value in rank {
            ranks[channels] = (value, channels as u8);
            channels += 1;
        }
        elements += 1;
        Ok(())
    };
    // The list runs from the centre out, so the last pair is the outermost: left and
    // right, with any others between them and the centre.
    let mut fronts = [(false, 0u8); 16];
    let mut last_pair = None;
    for (index, slot) in fronts.iter_mut().enumerate().take(front) {
        *slot = (reader.bit()?, reader.read(4)? as u8);
        if slot.0 {
            last_pair = Some(index);
        }
    }
    for (index, &(pair, tag)) in fronts.iter().enumerate().take(front) {
        if pair {
            let rank: &[u8] = if last_pair == Some(index) { &[0, 1] } else { &[6, 7] };
            add(1, tag, rank, &mut config)?;
        } else {
            add(0, tag, &[2], &mut config)?;
        }
    }
    for _ in 0..side {
        let pair = reader.bit()?;
        let tag = reader.read(4)? as u8;
        add(pair as u8, tag, if pair { &[9, 10] } else { &[11] }, &mut config)?;
    }
    for _ in 0..back {
        let pair = reader.bit()?;
        let tag = reader.read(4)? as u8;
        add(pair as u8, tag, if pair { &[4, 5] } else { &[8] }, &mut config)?;
    }
    for _ in 0..lfe {
        let tag = reader.read(4)? as u8;
        add(3, tag, &[3], &mut config)?;
    }
    for _ in 0..data {
        reader.read(4)?;
    }
    for _ in 0..coupling {
        reader.read(5)?;
    }
    reader.align();
    let comment = reader.read(8)? as usize;
    reader.skip_long(comment * 8)?;
    if channels == 0 {
        return Err(AudioError::InvalidData("AAC program config with no channels"));
    }
    ranks[..channels].sort_unstable();
    for (position, &(_, channel)) in ranks[..channels].iter().enumerate() {
        config.positions[channel as usize] = position as u8;
    }
    config.channels = channels as u8;
    config.element_count = elements as u8;
    Ok(config)
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
        // Explicitly signalled HE-AAC: an extension rate, and then the core's object type.
        let (mut sbr, mut ps, mut extension_rate) = (None, None, None);
        if object_type == 5 || object_type == 29 {
            sbr = Some(true);
            if object_type == 29 {
                ps = Some(true);
            }
            let index = reader.read(4)? as usize;
            extension_rate = if index == 15 {
                Some(reader.read(24)?)
            } else {
                SAMPLE_RATES.get(index).copied()
            };
            object_type = reader.read(5)?;
            if object_type == 31 {
                object_type = 32 + reader.read(6)?;
            }
        }
        if !matches!(object_type as u8, OBJECT_MAIN | OBJECT_LC | OBJECT_LTP) {
            return Err(AudioError::UnsupportedFeature("AAC object type"));
        }
        let short_frames = reader.bit()?;
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC core coder dependency"));
        }
        // The extension flag: reserved for these object types.
        reader.bit()?;
        let mut config = if channel_config == 0 {
            let program = read_program_config(&mut reader)?;
            let mut config = Self::with_program(rate_index as u8)?;
            config.program = Some(program);
            config
        } else {
            Self::new(rate_index as u8, channel_config as u8)?
        };
        config.object_type = object_type as u8;
        config.short_frames = short_frames;
        // Backward compatible signalling comes after everything else.
        if sbr.is_none() && reader.bits_left() >= 16 && reader.peek(11) == 0x2b7 {
            reader.read(11)?;
            if reader.read(5)? == 5 {
                let present = reader.bit()?;
                sbr = Some(present);
                if present {
                    let index = reader.read(4)? as usize;
                    extension_rate = if index == 15 {
                        Some(reader.read(24)?)
                    } else {
                        SAMPLE_RATES.get(index).copied()
                    };
                    if reader.bits_left() >= 12 && reader.peek(11) == 0x548 {
                        reader.read(11)?;
                        ps = Some(reader.bit()?);
                    }
                }
            }
        }
        config.sbr = sbr;
        config.ps = ps;
        config.extension_rate = extension_rate;
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
            program: None,
            short_frames: false,
            sbr: None,
            ps: None,
            extension_rate: None,
        })
    }

    /// A config whose channels a program config element will say.
    fn with_program(sample_rate_index: u8) -> Result<Self> {
        let sample_rate = *SAMPLE_RATES
            .get(sample_rate_index as usize)
            .ok_or(AudioError::InvalidData("reserved AAC sampling index"))?;
        Ok(Self {
            object_type: 2,
            sample_rate_index,
            sample_rate,
            channel_config: 0,
            program: None,
            short_frames: false,
            sbr: None,
            ps: None,
            extension_rate: None,
        })
    }

    /// Whether SBR will run: what a config says, if this decoder can do it there.
    fn sbr_active(&self) -> bool {
        self.sbr == Some(true) && !self.short_frames && self.sample_rate <= 48_000
    }

    /// Output at the core's own rate, from an SBR stream that says so.
    fn downsampled(&self) -> bool {
        self.sbr_active() && self.extension_rate == Some(self.sample_rate)
    }

    /// The rate of what comes out, which SBR can make twice the core's.
    pub fn output_rate(&self) -> u32 {
        if self.sbr_active() && !self.downsampled() {
            2 * self.sample_rate
        } else {
            self.sample_rate
        }
    }

    /// Samples per channel out of each frame.
    pub fn output_frame(&self) -> usize {
        if self.short_frames {
            960
        } else if self.sbr_active() && !self.downsampled() {
            2 * FRAME_SAMPLES
        } else {
            FRAME_SAMPLES
        }
    }

    /// Channels of what comes out. A mono core with SBR might have parametric stereo, and
    /// that can't be known in advance, so it's stereo unless the config rules it out.
    pub fn output_channels(&self) -> usize {
        if self.sbr_active() && self.channels() == 1 && self.ps != Some(false) {
            2
        } else {
            self.channels()
        }
    }

    /// Whether a frame could yet turn out to have SBR nothing has mentioned.
    fn may_be_implicit(&self) -> bool {
        self.sbr.is_none() && !self.short_frames && self.sample_rate <= 24_000
    }

    /// Number of channels the core has.
    pub fn channels(&self) -> usize {
        match &self.program {
            Some(program) => program.channels as usize,
            None => OUTPUT_ORDER
                .get(self.channel_config as usize)
                .map_or(0, |order| order.len()),
        }
    }

    /// Whether an ADTS header describes the stream this config is for.
    fn matches(&self, other: &Self) -> bool {
        self.sample_rate_index == other.sample_rate_index
            && self.channel_config == other.channel_config
            && self.object_type == other.object_type
    }

    /// Fills in the program config element from the first raw data block of an ADTS
    /// stream with channel configuration 0, where it's the first thing.
    fn with_program_from(mut self, block: &[u8]) -> Result<Self> {
        if self.channel_config != 0 {
            return Ok(self);
        }
        let mut reader = MsbReader::new(block);
        if reader.read(3)? != 5 {
            return Err(AudioError::InvalidData("ADTS stream has no channels to go by"));
        }
        self.program = Some(read_program_config(&mut reader)?);
        Ok(self)
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
    /// Main only: prediction is on for this frame.
    predictor_present: bool,
    /// Main only: a group of predictors to start over, numbered from 1, or 0.
    predictor_reset: u8,
    /// Main only: bands that use prediction, a bit each.
    prediction_used: u64,
    /// LTP only: each channel's, for a pair that shares this.
    ltp: [Ltp; 2],
}

/// `ltp_data`: where to look back to, how strongly, and in which bands.
#[derive(Clone, Copy, Default)]
struct Ltp {
    present: bool,
    lag: u16,
    gain: u8,
    used: u64,
}

/// How a stream's frames are laid out, as reading them needs to know.
#[derive(Clone, Copy)]
struct Layout {
    rate: usize,
    object_type: u8,
    short_frames: bool,
}

impl Layout {
    fn bands(self, short: bool) -> &'static [u16] {
        match (short, self.short_frames) {
            (false, false) => SWB_LONG[self.rate],
            (true, false) => SWB_SHORT[self.rate],
            (false, true) => SWB_LONG_960[self.rate],
            (true, true) => SWB_SHORT_120[self.rate],
        }
    }

    fn short_samples(self) -> usize {
        if self.short_frames { 120 } else { 128 }
    }

    fn frame_samples(self) -> usize {
        if self.short_frames { 960 } else { 1024 }
    }
}

/// The most coupling channels in a frame this keeps track of.
const MAX_COUPLINGS: usize = 4;
/// The most gain lists in a coupling channel: as many as `target_count` allows.
const MAX_GAIN_LISTS: usize = 9;
const COUPLE_BEFORE_TNS: u8 = 0;
const COUPLE_AFTER_TNS: u8 = 1;
const COUPLE_AFTER_IMDCT: u8 = 2;

#[derive(Clone, Copy, Default)]
struct CouplingTarget {
    is_pair: bool,
    tag: u8,
    left: bool,
    right: bool,
    first_list: usize,
}

/// A coupling channel element: a spectrum to add to other channels, at some gain.
#[derive(Clone)]
struct Coupling {
    point: u8,
    target_count: usize,
    targets: [CouplingTarget; 8],
    info: IcsInfo,
    band_types: [u8; MAX_SECTIONS],
    gains: [[f32; MAX_SECTIONS]; MAX_GAIN_LISTS],
}

impl Coupling {
    fn new() -> Self {
        Self {
            point: COUPLE_BEFORE_TNS,
            target_count: 0,
            targets: [CouplingTarget::default(); 8],
            info: IcsInfo::default(),
            band_types: [0; MAX_SECTIONS],
            gains: [[1.0; MAX_SECTIONS]; MAX_GAIN_LISTS],
        }
    }
}

/// Bands that can use prediction, for each sampling index.
const PRED_SFB_MAX: [usize; 13] = [33, 33, 38, 40, 40, 40, 41, 41, 37, 37, 37, 34, 34];

/// One spectral line's predictor state: a second-order backward-adaptive lattice, as
/// ISO/IEC 13818-7 lays out, with its state kept to 16 bits of mantissa.
#[derive(Clone, Copy)]
struct Predictor {
    r0: f32,
    r1: f32,
    cor0: f32,
    cor1: f32,
    var0: f32,
    var1: f32,
}

impl Predictor {
    const START: Self = Self {
        r0: 0.0,
        r1: 0.0,
        cor0: 0.0,
        cor1: 0.0,
        var0: 1.0,
        var1: 1.0,
    };

    /// Runs one line. Returns the prediction, which `value` has yet to have added, for
    /// `value` is what came before the prediction when `used`, and otherwise is the line.
    fn run(&mut self, value: f32, used: bool) -> f32 {
        const A: f32 = 61.0 / 64.0;
        const ALPHA: f32 = 29.0 / 32.0;
        let k1 = if self.var0 > 1.0 { self.cor0 * round_even(A / self.var0) } else { 0.0 };
        let k2 = if self.var1 > 1.0 { self.cor1 * round_even(A / self.var1) } else { 0.0 };
        let predicted = round_nearest(k1 * self.r0 + k2 * self.r1);
        let e0 = if used { value + predicted } else { value };
        let e1 = e0 - k1 * self.r0;
        self.cor1 = truncate(ALPHA * self.cor1 + self.r1 * e1);
        self.var1 = truncate(ALPHA * self.var1 + 0.5 * (self.r1 * self.r1 + e1 * e1));
        self.cor0 = truncate(ALPHA * self.cor0 + self.r0 * e0);
        self.var0 = truncate(ALPHA * self.var0 + 0.5 * (self.r0 * self.r0 + e0 * e0));
        self.r1 = truncate(A * (self.r0 - k1 * e0));
        self.r0 = truncate(A * e0);
        e0
    }
}

fn round_nearest(value: f32) -> f32 {
    f32::from_bits(value.to_bits().wrapping_add(0x8000) & 0xffff_0000)
}

fn round_even(value: f32) -> f32 {
    let bits = value.to_bits();
    f32::from_bits(bits.wrapping_add(0x7fff).wrapping_add((bits & 0x1_0000) >> 16) & 0xffff_0000)
}

/// `q^(4/3)`, as near as a float gets.
fn float_power43(q: u32) -> f32 {
    let value = q as f64;
    // From what's known to about one part in 8192, two steps are plenty.
    let mut root = (inverse_quant(q) as f64 / 8192.0 / value).max(1.0);
    for _ in 0..2 {
        root = (2.0 * root + value / (root * root)) / 3.0;
    }
    (root * value) as f32
}

/// `2^(exponent/4)`, likewise.
fn float_gain(exponent: i32) -> f32 {
    const PARTS: [f32; 4] = [1.0, 1.189_207_1, core::f32::consts::SQRT_2, 1.681_792_8];
    let whole = exponent.div_euclid(4).clamp(-126, 127);
    PARTS[exponent.rem_euclid(4) as usize] * f32::from_bits(((whole + 127) as u32) << 23)
}

fn truncate(value: f32) -> f32 {
    f32::from_bits(value.to_bits() & 0xffff_0000)
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
pub(crate) fn read_code(reader: &mut MsbReader<'_>, table: &[Code]) -> Result<u16> {
    let bits = reader.peek(32);
    let index = table.partition_point(|code| code.code <= bits);
    let entry = index
        .checked_sub(1)
        .and_then(|index| table.get(index))
        .ok_or(AudioError::InvalidData("bad AAC Huffman code"))?;
    reader.skip(entry.length as u32)?;
    Ok(entry.symbol)
}

fn read_ltp(reader: &mut MsbReader<'_>, max_sfb: usize) -> Result<Ltp> {
    let mut ltp = Ltp {
        present: true,
        lag: reader.read(11)? as u16,
        gain: reader.read(3)? as u8,
        used: 0,
    };
    for band in 0..max_sfb.min(LTP_MAX_BANDS) {
        ltp.used |= (reader.bit()? as u64) << band;
    }
    Ok(ltp)
}

fn read_ics_info(reader: &mut MsbReader<'_>, layout: Layout, common: bool) -> Result<IcsInfo> {
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
        if info.max_sfb + 1 > layout.bands(true).len() {
            return Err(AudioError::InvalidData("AAC max_sfb exceeds the band count"));
        }
    } else {
        info.max_sfb = reader.read(6)? as usize;
        info.predictor_present = reader.bit()?;
        if info.predictor_present {
            match layout.object_type {
                OBJECT_MAIN => {
                    if reader.bit()? {
                        info.predictor_reset = reader.read(5)? as u8;
                    }
                    let bands = info.max_sfb.min(PRED_SFB_MAX[layout.rate]);
                    for band in 0..bands {
                        info.prediction_used |= (reader.bit()? as u64) << band;
                    }
                }
                OBJECT_LTP => {
                    if reader.bit()? {
                        info.ltp[0] = read_ltp(reader, info.max_sfb)?;
                    }
                    if common && reader.bit()? {
                        info.ltp[1] = read_ltp(reader, info.max_sfb)?;
                    }
                }
                _ => return Err(AudioError::UnsupportedFeature("AAC prediction outside Main")),
            }
        }
        info.groups = 1;
        info.group_lengths[0] = 1;
        if info.max_sfb + 1 > layout.bands(false).len() {
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

fn read_tns(reader: &mut MsbReader<'_>, ics: &mut Ics, main: bool) -> Result<()> {
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
            if order > if main { TNS_MAX_ORDER } else { 12 } {
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
    span: usize,
    quantized: &mut [i32],
) -> Result<()> {
    quantized.fill(0);
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

/// A decoder for raw AAC access units.
pub struct AacDecoder {
    config: AudioSpecificConfig,
    layout: Layout,
    /// Samples per channel in a frame, and in a short window.
    frame: usize,
    short_len: usize,
    kbd_long: &'static [i32],
    kbd_short: &'static [i32],
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
    previous_shape: [bool; MAX_CHANNELS + MAX_COUPLINGS],
    /// Noise generator state.
    noise: u32,
    /// Main only: `channels * 1024` of them.
    predictors: Vec<Predictor>,
    /// Main only: the spectrum in floating point too. The predictors keep 16 bits of float, so
    /// their results can jump by one of those steps at the slightest difference, and the
    /// standard's are matched only by coming to the same floats it does.
    float_spectrum: Vec<f32>,
    /// LTP only: for each channel, the last two frames it output, in the same format as
    /// `overlap`, whose tail is what the prediction looks at after them.
    ltp_history: Vec<i32>,
    /// LTP only: a prediction's spectrum.
    ltp_spectrum: Vec<i32>,
    /// SBR, once a stream is known to have it, and the core's output for it, in Q13.
    sbr: Vec<SbrDecoder>,
    core_pcm: Vec<i32>,
    /// Frames decoded, as far as a first one mattering goes.
    decoded_any: bool,
    /// Coupling channels' details, allocated when a stream first has one.
    couplings: Vec<Coupling>,
    /// An independently switched coupling channel's output for this frame.
    coupling_time: Vec<Vec<i16>>,
    coupling_output: Vec<i16>,
}

impl AacDecoder {
    /// Creates a decoder. The config must have come from
    /// [`AudioSpecificConfig::parse`] or [`AudioSpecificConfig::new`].
    pub fn new(config: AudioSpecificConfig) -> Result<Self> {
        let mut checked = if config.program.is_some() {
            config
        } else {
            let mut checked = AudioSpecificConfig::new(config.sample_rate_index, config.channel_config)?;
            checked.object_type = config.object_type;
            checked
        };
        let channels = checked.channels();
        checked.short_frames = config.short_frames;
        checked.sbr = config.sbr;
        checked.ps = config.ps;
        checked.extension_rate = config.extension_rate;
        if config.program.is_none() {
            checked.sample_rate = config.sample_rate;
        }
        let layout = Layout {
            rate: checked.sample_rate_index as usize,
            object_type: checked.object_type,
            short_frames: checked.short_frames,
        };
        let frame = layout.frame_samples();
        let short_len = layout.short_samples();
        let predictors = if checked.object_type == OBJECT_MAIN {
            try_vec(channels * FRAME_SAMPLES, Predictor::START)?
        } else {
            Vec::new()
        };
        let mut sine_long = try_vec(frame, 0i32)?;
        let mut sine_short = try_vec(short_len, 0i32)?;
        for window in [&mut sine_long, &mut sine_short] {
            let length = window.len() as u128;
            for (i, slot) in window.iter_mut().enumerate() {
                // sin((i + 1/2) pi / (2 length)): (2i + 1) / (8 length) of a turn.
                let phase = (((2 * i as u128 + 1) << 61) / length) as u64;
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
        // One, or none: on the heap, so that moving a decoder doesn't take much stack.
        let mut sbr = Vec::new();
        if checked.sbr_active() {
            sbr.try_reserve_exact(1).map_err(|_| AudioError::OutOfMemory)?;
            sbr.push(open_sbr(&checked)?);
        }
        Ok(Self {
            config: checked,
            core_pcm: if !sbr.is_empty() {
                try_vec(channels * FRAME_SAMPLES, 0)?
            } else {
                Vec::new()
            },
            sbr,
            decoded_any: false,
            layout,
            frame,
            short_len,
            kbd_long: if layout.short_frames { &KBD_LONG_960 } else { &KBD_LONG },
            kbd_short: if layout.short_frames { &KBD_SHORT_120 } else { &KBD_SHORT },
            channels,
            long: Imdct::new(2 * frame)?,
            short: Imdct::new(2 * short_len)?,
            sine_long,
            sine_short,
            tns_table,
            spectrum: try_vec((channels + MAX_COUPLINGS) * FRAME_SAMPLES, 0)?,
            overlap: try_vec((channels + MAX_COUPLINGS) * FRAME_SAMPLES, 0)?,
            quantized: try_vec(FRAME_SAMPLES, 0)?,
            transform: try_vec(2 * FRAME_SAMPLES, 0)?,
            time: try_vec(2 * FRAME_SAMPLES, 0)?,
            previous_shape: [false; MAX_CHANNELS + MAX_COUPLINGS],
            noise: 0x1f2e_3d4c,
            predictors,
            float_spectrum: if checked.object_type == OBJECT_MAIN {
                try_vec(channels * FRAME_SAMPLES, 0.0)?
            } else {
                Vec::new()
            },
            ltp_history: if checked.object_type == OBJECT_LTP {
                try_vec(channels * 2 * FRAME_SAMPLES, 0)?
            } else {
                Vec::new()
            },
            ltp_spectrum: if checked.object_type == OBJECT_LTP {
                try_vec(FRAME_SAMPLES, 0)?
            } else {
                Vec::new()
            },
            couplings: {
                let mut list = Vec::new();
                list.try_reserve_exact(MAX_COUPLINGS).map_err(|_| AudioError::OutOfMemory)?;
                list.resize(MAX_COUPLINGS, Coupling::new());
                list
            },
            coupling_time: {
                let mut list = Vec::new();
                list.try_reserve_exact(MAX_COUPLINGS).map_err(|_| AudioError::OutOfMemory)?;
                for _ in 0..MAX_COUPLINGS {
                    list.push(try_vec(FRAME_SAMPLES, 0i16)?);
                }
                list
            },
            coupling_output: try_vec(FRAME_SAMPLES, 0i16)?,
        })
    }

    /// The config in use.
    pub fn config(&self) -> AudioSpecificConfig {
        self.config
    }

    /// Clears the overlap state, for a seek.
    pub fn reset(&mut self) {
        self.overlap.fill(0);
        self.previous_shape = [false; MAX_CHANNELS + MAX_COUPLINGS];
        self.predictors.fill(Predictor::START);
        self.ltp_history.fill(0);
        for sbr in &mut self.sbr {
            sbr.reset();
        }
    }

    /// Adds Main's prediction to a channel's spectrum, and updates the predictors from
    /// the result, as they do in every frame whether it's used or not.
    fn apply_prediction(&mut self, channel: usize, ics: &Ics) {
        if self.predictors.is_empty() {
            return;
        }
        let start = channel * FRAME_SAMPLES;
        let predictors = &mut self.predictors[start..start + FRAME_SAMPLES];
        let info = &ics.info;
        // The state doesn't survive a short block.
        if info.sequence == WindowSequence::EightShort {
            predictors.fill(Predictor::START);
            return;
        }
        let swb = self.layout.bands(false);
        let bands = PRED_SFB_MAX[self.layout.rate].min(swb.len() - 1);
        let spectrum = &mut self.spectrum[start..start + FRAME_SAMPLES];
        for band in 0..bands {
            let used = info.predictor_present && info.prediction_used & (1 << band) != 0;
            for line in swb[band] as usize..swb[band + 1] as usize {
                // Coefficients are Q3 here, and the predictor runs on the standard's scale.
                let value = self.float_spectrum[start + line];
                let e0 = predictors[line].run(value, used);
                if used {
                    spectrum[line] = (e0 * 8.0) as i32;
                }
            }
        }
        if info.predictor_present && info.predictor_reset > 0 {
            let mut line = info.predictor_reset as usize - 1;
            while line < FRAME_SAMPLES {
                predictors[line] = Predictor::START;
                line += 30;
            }
        }
    }

    /// Where a program config element puts this element's channels. `None` means by
    /// order of appearance, which is everything without one.
    fn programmed(&self, kind: u8, tag: u8) -> Option<usize> {
        let program = self.config.program.as_ref()?;
        program.elements[..program.element_count as usize]
            .iter()
            .find(|element| element.kind == kind && element.tag == tag)
            .map(|element| element.channel as usize)
    }

    fn swb(&self, info: &IcsInfo) -> &'static [u16] {
        self.layout.bands(info.sequence == WindowSequence::EightShort)
    }

    /// How far apart a frame's windows are in the spectrum.
    fn span(&self, info: &IcsInfo) -> usize {
        if info.sequence == WindowSequence::EightShort {
            self.short_len
        } else {
            self.frame
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
            None => read_ics_info(reader, self.layout, false)?,
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
            read_tns(reader, ics, self.config.object_type == OBJECT_MAIN)?;
        }
        if reader.bit()? {
            return Err(AudioError::UnsupportedFeature("AAC gain control"));
        }

        let swb = self.swb(&ics.info);
        let span = self.span(&ics.info);
        read_spectrum(reader, ics, swb, span, &mut self.quantized)?;
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
                // The standard's rule: a value of zero goes negative.
                *slot += if *slot > 0 { amplitude } else { -amplitude };
            }
        }
        self.dequantize(channel, ics, swb);
        Ok(())
    }

    /// Scales quantized values to coefficients, filling noise bands.
    fn dequantize(&mut self, channel: usize, ics: &Ics, swb: &[u16]) {
        let span = self.span(&ics.info);
        let start = channel * FRAME_SAMPLES;
        let Some(spectrum) = self.spectrum.get_mut(start..start + FRAME_SAMPLES) else {
            return;
        };
        spectrum.fill(0);
        let want_floats = channel < self.channels && !self.float_spectrum.is_empty();
        if want_floats {
            self.float_spectrum[start..start + FRAME_SAMPLES].fill(0.0);
        }
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
                            fill_noise(&mut spectrum[range.clone()], energy, &mut self.noise);
                            if want_floats {
                                for line in range {
                                    self.float_spectrum[start + line] = spectrum[line] as f32 / 8.0;
                                }
                            }
                        }
                        _ => {
                            let gain = ics.scalefactors[index] as i32 - 100;
                            let input = &self.quantized[range.clone()];
                            for (slot, &q) in spectrum[range.clone()].iter_mut().zip(input) {
                                if q != 0 {
                                    let value = apply_gain(inverse_quant(q.unsigned_abs()), gain);
                                    *slot = (if q < 0 { -value } else { value }) as i32;
                                }
                            }
                            if want_floats {
                                let scale = float_gain(gain);
                                for (slot, &q) in self.float_spectrum[start + range.start..start + range.end]
                                    .iter_mut()
                                    .zip(input)
                                {
                                    if q != 0 {
                                        let value = float_power43(q.unsigned_abs()) * scale;
                                        *slot = if q < 0 { -value } else { value };
                                    }
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
        intensity: bool,
    ) {
        let swb = self.swb(&right.info);
        let span = self.span(&right.info);
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
                        if !intensity {
                            continue;
                        }
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
                    } else if !intensity
                        && masked
                        && book < BAND_NOISE
                        && left.band_types[index] < BAND_NOISE
                    {
                        for (l, r) in left_band.iter_mut().zip(right_band.iter_mut()) {
                            let (m, s) = (*l, *r);
                            *l = m.wrapping_add(s);
                            *r = m.wrapping_sub(s);
                        }
                        if !self.float_spectrum.is_empty() {
                            let a = pair.0 * FRAME_SAMPLES + offset;
                            let b = pair.1 * FRAME_SAMPLES + offset;
                            for line in low..high {
                                let (m, s) = (self.float_spectrum[a + line], self.float_spectrum[b + line]);
                                self.float_spectrum[a + line] = m + s;
                                self.float_spectrum[b + line] = m - s;
                            }
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
        let start = channel * FRAME_SAMPLES;
        let mut spectrum = core::mem::take(&mut self.spectrum);
        if let Some(lines) = spectrum.get_mut(start..start + FRAME_SAMPLES) {
            self.tns_filter(lines, ics, false);
        }
        self.spectrum = spectrum;
    }

    /// Runs a channel's TNS filters over `spectrum`: the all-pole ones decoding uses, or for
    /// `analysis` their all-zero inverse, which is what LTP's prediction wants, having
    /// come from the signal with its shaping in it.
    fn tns_filter(&self, spectrum: &mut [i32], ics: &Ics, analysis: bool) {
        let short = ics.info.sequence == WindowSequence::EightShort;
        let (windows, span) = if short { (8, self.short_len) } else { (1, self.frame) };
        let swb = self.swb(&ics.info);
        let max_bands = if short {
            TNS_MAX_BANDS_SHORT[self.layout.rate]
        } else {
            TNS_MAX_BANDS_LONG[self.layout.rate]
        } as usize;
        let bands = swb.len() - 1;
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
                if analysis {
                    // Past inputs rather than outputs, so last to first leaves them unchanged.
                    for m in (0..size).rev() {
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
                            .wrapping_add(sum.wrapping_add(1 << 23) >> 24);
                        band[position] = value.clamp(-COEF_LIMIT, COEF_LIMIT) as i32;
                    }
                    continue;
                }
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

    /// Long term prediction: what this channel output a lag ago, transformed as this frame
    /// was, added to the bands that asked for it. The newest part of the past is the
    /// last frame's overlap, aliased as it is.
    fn apply_ltp(&mut self, channel: usize, ics: &Ics, ltp: &Ltp) {
        if !ltp.present || self.ltp_history.is_empty() || ics.info.sequence == WindowSequence::EightShort {
            return;
        }
        let (frame, short_len) = (self.frame, self.short_len);
        let flat = (frame - short_len) / 2;
        let lag = ltp.lag as usize;
        let gain = LTP_GAINS[ltp.gain as usize & 7];
        let history = &self.ltp_history[channel * 2 * FRAME_SAMPLES..][..2 * frame];
        let overlap = &self.overlap[channel * FRAME_SAMPLES..][..frame];
        let time = &mut self.time[..2 * frame];
        // In the spectrum's Q3 rather than Q13, as the transform won't scale it.
        let reach = if lag < frame { lag + frame } else { 2 * frame };
        for (i, slot) in time.iter_mut().enumerate() {
            let at = 2 * frame + i;
            *slot = if i >= reach || at < lag {
                0
            } else {
                let at = at - lag;
                let value = if at < 2 * frame {
                    history[at]
                } else {
                    overlap.get(at - 2 * frame).copied().unwrap_or(0)
                };
                ((value as i64 * gain) >> (15 + PCM_FRACTION - COEF_FRACTION)) as i32
            };
        }

        let previous_shape = self.previous_shape[channel];
        let long_previous: &[i32] = if previous_shape { self.kbd_long } else { &self.sine_long };
        let short_previous: &[i32] = if previous_shape { self.kbd_short } else { &self.sine_short };
        let long_current: &[i32] = if ics.info.shape { self.kbd_long } else { &self.sine_long };
        let short_current: &[i32] = if ics.info.shape { self.kbd_short } else { &self.sine_short };
        let (left, right) = time.split_at_mut(frame);
        if ics.info.sequence == WindowSequence::LongStop {
            left[..flat].fill(0);
            for (slot, &weight) in left[flat..flat + short_len].iter_mut().zip(short_previous) {
                *slot = mdct::window(*slot, weight);
            }
        } else {
            for (slot, &weight) in left.iter_mut().zip(long_previous) {
                *slot = mdct::window(*slot, weight);
            }
        }
        if ics.info.sequence == WindowSequence::LongStart {
            for (slot, &weight) in right[flat..flat + short_len].iter_mut().zip(short_current.iter().rev()) {
                *slot = mdct::window(*slot, weight);
            }
            right[flat + short_len..].fill(0);
        } else {
            for (slot, &weight) in right.iter_mut().zip(long_current.iter().rev()) {
                *slot = mdct::window(*slot, weight);
            }
        }

        // The forward transform is a DCT-IV of the frame folded in half, and the inverse
        // transform's middle is the same DCT-IV.
        let half = frame / 2;
        let mut lines = core::mem::take(&mut self.ltp_spectrum);
        let quantized = &mut self.quantized[..frame];
        for n in 0..half {
            quantized[n] = time_fold_low(&self.time, frame, n);
            quantized[n + half] = time_fold_high(&self.time, frame, n);
        }
        let output = &mut self.transform[..2 * frame];
        self.long.run(quantized, output);
        for (m, slot) in lines[..frame].iter_mut().enumerate() {
            *slot = output[3 * frame / 2 - 1 - m].wrapping_neg().wrapping_mul(2);
        }
        lines[frame..].fill(0);
        if ics.tns_present {
            self.tns_filter(&mut lines, ics, true);
        }
        let swb = self.layout.bands(false);
        let spectrum = &mut self.spectrum[channel * FRAME_SAMPLES..][..FRAME_SAMPLES];
        for band in 0..ics.info.max_sfb.min(LTP_MAX_BANDS).min(swb.len() - 1) {
            if ltp.used & (1 << band) != 0 {
                let range = swb[band] as usize..swb[band + 1] as usize;
                for (slot, &value) in spectrum[range.clone()].iter_mut().zip(&lines[range]) {
                    *slot = slot.wrapping_add(value);
                }
            }
        }
        self.ltp_spectrum = lines;
    }

    /// Transforms a channel's spectrum and writes it to `out` at `position`.
    fn synthesize(
        &mut self,
        channel: usize,
        ics: &Ics,
        out: &mut [i16],
        position: usize,
        channels: usize,
    ) {
        let info = &ics.info;
        let previous_shape = self.previous_shape[channel];
        let long_previous: &[i32] = if previous_shape { self.kbd_long } else { &self.sine_long };
        let short_previous: &[i32] = if previous_shape { self.kbd_short } else { &self.sine_short };
        let long_current: &[i32] = if info.shape { self.kbd_long } else { &self.sine_long };
        let short_current: &[i32] = if info.shape { self.kbd_short } else { &self.sine_short };

        let (frame, short_len) = (self.frame, self.short_len);
        let flat = (frame - short_len) / 2;
        let start = channel * FRAME_SAMPLES;
        let spectrum = &self.spectrum[start..start + frame];
        let time = &mut self.time[..2 * frame];

        if info.sequence == WindowSequence::EightShort {
            time.fill(0);
            let transform = &mut self.transform[..2 * short_len];
            for window in 0..8 {
                let input = &spectrum[window * short_len..(window + 1) * short_len];
                self.short.run(input, transform);
                let rising = if window == 0 { short_previous } else { short_current };
                let at = flat + window * short_len;
                for i in 0..short_len {
                    // The short transform's scale is 8 times the long one's, so this is 8x low.
                    let head = mdct::window(transform[i], rising[i]);
                    let tail = mdct::window(
                        transform[short_len + i],
                        short_current[short_len - 1 - i],
                    );
                    time[at + i] = time[at + i].wrapping_add(head.wrapping_shl(3));
                    time[at + short_len + i] =
                        time[at + short_len + i].wrapping_add(tail.wrapping_shl(3));
                }
            }
        } else {
            self.long.run(spectrum, time);
            let (left, right) = time.split_at_mut(frame);
            match info.sequence {
                WindowSequence::LongStop => {
                    left[..flat].fill(0);
                    for (slot, &weight) in left[flat..flat + short_len].iter_mut().zip(short_previous) {
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
                    for (slot, &weight) in
                        right[flat..flat + short_len].iter_mut().zip(short_current.iter().rev())
                    {
                        *slot = mdct::window(*slot, weight);
                    }
                    right[flat + short_len..].fill(0);
                }
                _ => {
                    for (slot, &weight) in right.iter_mut().zip(long_current.iter().rev()) {
                        *slot = mdct::window(*slot, weight);
                    }
                }
            }
        }
        // The scale above is right for 1024 samples, and 2/N is 16/15 of that at 960.
        if self.layout.short_frames {
            for slot in time.iter_mut() {
                *slot = ((*slot as i64 * 2_290_649_225) >> 31) as i32;
            }
        }

        let overlap = &mut self.overlap[start..start + frame];
        let rounding = 1i64 << (PCM_FRACTION - 1);
        // SBR's analysis wants more than 16 bits.
        if !self.core_pcm.is_empty() && channel < self.channels {
            let pcm = &mut self.core_pcm[start..start + frame];
            for (slot, (&saved, &new)) in pcm.iter_mut().zip(overlap.iter().zip(time[..frame].iter())) {
                *slot = (saved as i64 + new as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
            overlap.copy_from_slice(&time[frame..]);
            self.previous_shape[channel] = info.shape;
            return;
        }
        let keep = !self.ltp_history.is_empty() && channel < self.channels;
        if keep {
            let history = &mut self.ltp_history[channel * 2 * FRAME_SAMPLES..][..2 * frame];
            history.copy_within(frame.., 0);
            for (slot, (&saved, &new)) in history[frame..]
                .iter_mut()
                .zip(overlap.iter().zip(time[..frame].iter()))
            {
                *slot = (saved as i64 + new as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
        }
        for (i, (&saved, &new)) in overlap.iter().zip(time[..frame].iter()).enumerate() {
            let sum = saved as i64 + new as i64;
            let sample = (sum + rounding) >> PCM_FRACTION;
            out[i * channels + position] = sample.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        }
        overlap.copy_from_slice(&time[frame..]);
        self.previous_shape[channel] = info.shape;
    }

    /// Decodes one raw access unit (`raw_data_block`), such as an MP4 sample.
    ///
    /// Writes a frame's interleaved samples to `out` and returns how many for each
    /// channel: [`FRAME_SAMPLES`], or 960 if the config says so. Channels the frame
    /// doesn't mention are silent.
    pub fn decode_raw_frame(&mut self, frame: &[u8], out: &mut [i16]) -> Result<usize> {
        let mut reader = MsbReader::new(frame);
        self.decode_block(&mut reader, out)
    }

    fn decode_block(&mut self, reader: &mut MsbReader<'_>, out: &mut [i16]) -> Result<usize> {
        // A first frame might turn out to need room for what SBR makes of it.
        let required = if self.config.may_be_implicit() && !self.decoded_any {
            2 * FRAME_SAMPLES * self.channels.max(2)
        } else {
            self.config.output_frame() * self.config.output_channels()
        };
        if out.len() < required {
            return Err(AudioError::BufferTooSmall {
                required,
                available: out.len(),
            });
        }
        let channels = self.channels;
        let order = OUTPUT_ORDER[self.config.channel_config as usize];
        let program_positions = self.config.program.map(|program| program.positions);
        let mut next_channel = 0usize;
        let mut ics = [Ics::new(); MAX_CHANNELS];
        let mut present = [false; MAX_CHANNELS];
        // What each channel is: which element, for coupling to find.
        let mut targets = [(0u8, 0u8, 0u8); MAX_CHANNELS];

        let mut positions = [0usize; MAX_CHANNELS];
        for (position, &channel) in order.iter().enumerate() {
            positions[channel as usize] = position;
        }
        if let Some(program) = program_positions {
            for (slot, &position) in positions.iter_mut().zip(&program) {
                *slot = position as usize;
            }
        }
        let mut couplings = 0usize;
        // The element a fill element's SBR data goes with: its first channel, and if a pair.
        let mut last_element = None;

        loop {
            match reader.read(3)? {
                id @ (0 | 3) => {
                    // SCE or LFE.
                    let tag = reader.read(4)? as u8;
                    let channel = match self.programmed(id as u8, tag) {
                        Some(channel) => channel,
                        None => {
                            next_channel += 1;
                            next_channel - 1
                        }
                    };
                    if channel >= channels {
                        return Err(AudioError::InvalidData("too many AAC channels"));
                    }
                    self.read_channel(reader, channel, None, &mut ics[channel])?;
                    self.apply_prediction(channel, &ics[channel]);
                    targets[channel] = (id as u8, tag, 0);
                    present[channel] = true;
                    if id == 0 {
                        last_element = Some((channel, false));
                    }
                }
                1 => {
                    // CPE.
                    let tag = reader.read(4)? as u8;
                    let a = match self.programmed(1, tag) {
                        Some(channel) => channel,
                        None => {
                            next_channel += 2;
                            next_channel - 2
                        }
                    };
                    let b = a + 1;
                    if b >= channels {
                        return Err(AudioError::InvalidData("too many AAC channels"));
                    }
                    let common = reader.bit()?;
                    let mut ms_present = 0;
                    let mut ms_mask = [false; MAX_SECTIONS];
                    let mut shared = None;
                    if common {
                        let info = read_ics_info(reader, self.layout, true)?;
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
                    let (head, tail) = ics.split_at_mut(b);
                    let (first, second) = (&mut head[a], &mut tail[0]);
                    self.read_channel(reader, a, shared, first)?;
                    self.read_channel(reader, b, shared, second)?;
                    if common {
                        // The second channel's prediction came along with the first's.
                        second.info.ltp[0] = second.info.ltp[1];
                    }
                    if common {
                        self.apply_stereo((a, b), first, second, ms_present, &ms_mask, false);
                    }
                    self.apply_prediction(a, first);
                    self.apply_prediction(b, second);
                    if common {
                        self.apply_stereo((a, b), first, second, ms_present, &ms_mask, true);
                    }
                    last_element = Some((a, true));
                    targets[a] = (1, tag, 0);
                    targets[b] = (1, tag, 1);
                    present[a] = true;
                    present[b] = true;
                }
                2 => {
                    if couplings >= MAX_COUPLINGS {
                        return Err(AudioError::UnsupportedFeature("this many AAC coupling channels"));
                    }
                    self.read_coupling(reader, couplings)?;
                    couplings += 1;
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
                    // Read at the start. One that differs later isn't followed.
                    read_program_config(reader)?;
                }
                6 => {
                    // Fill element, which holds nothing LC needs, but might hold SBR's data.
                    let mut count = reader.read(4)? as usize;
                    if count == 15 {
                        count += reader.read(8)? as usize;
                        count -= 1;
                    }
                    if count > 0 {
                        self.read_extension(reader.clone(), count, last_element)?;
                    }
                    reader.skip_long(count * 8)?;
                }
                _ => break,
            }
        }

        // Everything is read, so coupling channels can go where they're going.
        for index in 0..couplings {
            self.finish_coupling(index);
        }
        let silent = Ics::new();
        for channel in 0..channels {
            // A channel the frame didn't carry is silent, but its old tail must still ring
            // out so that it fades instead of clicking.
            if !present[channel] {
                self.spectrum[channel * FRAME_SAMPLES..(channel + 1) * FRAME_SAMPLES].fill(0);
                self.synthesize(channel, &silent, out, positions[channel], channels);
                continue;
            }
            self.apply_dependent(channel, &ics[channel], targets[channel], couplings, COUPLE_BEFORE_TNS);
            let ltp = ics[channel].info.ltp[0];
            self.apply_ltp(channel, &ics[channel], &ltp);
            self.apply_tns(channel, &ics[channel]);
            self.apply_dependent(channel, &ics[channel], targets[channel], couplings, COUPLE_AFTER_TNS);
            self.synthesize(channel, &ics[channel], out, positions[channel], channels);
            self.apply_independent(channel, targets[channel], couplings, out, positions[channel]);
        }
        self.decoded_any = true;
        if self.config.sbr.is_none() && self.config.may_be_implicit() {
            // A first frame without it means there isn't any.
            self.config.sbr = Some(false);
        }
        let mut holder = core::mem::take(&mut self.sbr);
        if let Some(sbr) = holder.first_mut() {
            let out_channels = self.config.output_channels();
            for channel in 0..channels {
                let (kind, _, side) = targets[channel];
                if side == 1 {
                    continue;
                }
                let pair = kind == 1 && present[channel] && channel + 1 < channels;
                let places = if pair {
                    (positions[channel], positions[channel + 1])
                } else if out_channels == 2 && channels == 1 {
                    (0, 1)
                } else {
                    (positions[channel], positions[channel])
                };
                sbr.apply(channel, pair, &self.core_pcm, FRAME_SAMPLES, out, out_channels, places);
            }
        }
        self.sbr = holder;
        Ok(self.config.output_frame())
    }

    /// A fill element's contents, `count` bytes at `reader`. SBR data there goes to the
    /// element before it, and one that's damaged is dropped without bothering the core.
    fn read_extension(
        &mut self,
        mut reader: MsbReader<'_>,
        count: usize,
        element: Option<(usize, bool)>,
    ) -> Result<()> {
        let kind = reader.read(4)?;
        let (13 | 14, Some((first, pair))) = (kind, element) else {
            return Ok(());
        };
        if self.config.may_be_implicit() && !self.decoded_any && self.sbr.is_empty() {
            self.config.sbr = Some(true);
            self.sbr.try_reserve_exact(1).map_err(|_| AudioError::OutOfMemory)?;
            self.sbr.push(open_sbr(&self.config)?);
            self.core_pcm = try_vec(self.channels * FRAME_SAMPLES, 0)?;
        }
        if let Some(sbr) = self.sbr.first_mut() {
            let _ = sbr.read(&mut reader, count * 8 - 4, kind == 14, first, pair);
        }
        Ok(())
    }

    /// Reads a `coupling_channel_element`. Its spectrum goes in a spare channel of
    /// `self.spectrum`, its gains in `self.couplings[index]`.
    fn read_coupling(&mut self, reader: &mut MsbReader<'_>, index: usize) -> Result<()> {
        reader.read(4)?; // Its instance tag: nothing refers to one.
        let independent = reader.bit()?;
        let targets = reader.read(3)? as usize + 1;
        let mut coupling = Coupling::new();
        coupling.point = if independent { COUPLE_AFTER_IMDCT } else { COUPLE_BEFORE_TNS };
        coupling.target_count = targets;
        let mut gain_lists = 0;
        for slot in coupling.targets.iter_mut().take(targets) {
            let is_pair = reader.bit()?;
            let tag = reader.read(4)? as u8;
            let (left, right) = if is_pair {
                (reader.bit()?, reader.bit()?)
            } else {
                (true, false)
            };
            *slot = CouplingTarget {
                is_pair,
                tag,
                left,
                right,
                first_list: gain_lists,
            };
            gain_lists += if is_pair && left && right { 2 } else { 1 };
        }
        let domain = reader.bit()?;
        if !independent && domain {
            coupling.point = COUPLE_AFTER_TNS;
        }
        let sign = reader.bit()?;
        let scale_shift = reader.read(2)?;
        // A step of the gain is 2^(1 / 8), 2^(1 / 4), 2^(1 / 2) or 2.
        let per_octave = 8 >> scale_shift;

        let channel = self.channels + index;
        let mut ics = Ics::new();
        self.read_channel(reader, channel, None, &mut ics)?;
        // A coupling channel's own TNS goes first, as nothing else will do it.
        self.apply_tns(channel, &ics);
        coupling.info = ics.info;
        coupling.band_types = ics.band_types;

        for list in 0..gain_lists {
            let keep = list.min(MAX_GAIN_LISTS - 1);
            let common = list == 0 || independent || reader.bit()?;
            let mut gain = 0i32;
            let mut cache = 1.0f32;
            if list > 0 && common {
                gain = read_code(reader, &SCALEFACTOR)? as i32 - 60;
                cache = power_of_two(-gain, per_octave);
            }
            if independent {
                coupling.gains[keep][0] = cache;
                continue;
            }
            let bands = ics.info.groups * ics.info.max_sfb;
            for band in 0..bands {
                if ics.band_types[band] == BAND_ZERO {
                    continue;
                }
                if !common {
                    let delta = read_code(reader, &SCALEFACTOR)? as i32 - 60;
                    if delta != 0 {
                        gain += delta;
                        let (exponent, negative) = if sign {
                            (gain >> 1, gain & 1 != 0)
                        } else {
                            (gain, false)
                        };
                        cache = power_of_two(-exponent, per_octave);
                        if negative {
                            cache = -cache;
                        }
                    }
                }
                coupling.gains[keep][band] = cache;
            }
        }
        // A coupling's copy, as `spectrum` gets used for its synthesis.
        self.couplings[index] = coupling;
        Ok(())
    }

    /// An independently switched coupling channel has a time signal of its own.
    fn finish_coupling(&mut self, index: usize) {
        if self.couplings[index].point != COUPLE_AFTER_IMDCT {
            return;
        }
        let mut ics = Ics::new();
        ics.info = self.couplings[index].info;
        let channel = self.channels + index;
        let mut output = core::mem::take(&mut self.coupling_output);
        if output.len() == FRAME_SAMPLES {
            self.synthesize(channel, &ics, &mut output, 0, 1);
            let frame = self.frame;
            self.coupling_time[index][..frame].copy_from_slice(&output[..frame]);
        }
        self.coupling_output = output;
    }

    /// The gain list for a channel from a coupling that targets it, if it does.
    fn gain_list(coupling: &Coupling, target: (u8, u8, u8)) -> Option<usize> {
        let (kind, tag, side) = target;
        if kind == 3 {
            return None;
        }
        for slot in &coupling.targets[..coupling.target_count] {
            if slot.tag != tag || slot.is_pair != (kind == 1) {
                continue;
            }
            if side == 0 && slot.left {
                return Some(slot.first_list);
            }
            if side == 1 && slot.right {
                return Some(slot.first_list + slot.left as usize);
            }
        }
        None
    }

    fn apply_dependent(
        &mut self,
        channel: usize,
        target_ics: &Ics,
        target: (u8, u8, u8),
        couplings: usize,
        point: u8,
    ) {
        let span = self.span(&target_ics.info);
        for index in 0..couplings {
            let coupling = &self.couplings[index];
            if coupling.point != point {
                continue;
            }
            let Some(list) = Self::gain_list(coupling, target) else {
                continue;
            };
            let swb = self.layout.bands(coupling.info.sequence == WindowSequence::EightShort);
            let source_at = (self.channels + index) * FRAME_SAMPLES;
            let (head, tail) = self.spectrum.split_at_mut(source_at);
            let source = &tail[..FRAME_SAMPLES];
            let destination = &mut head[channel * FRAME_SAMPLES..(channel + 1) * FRAME_SAMPLES];
            let max_sfb = coupling.info.max_sfb;
            let mut window_start = 0;
            for group in 0..coupling.info.groups {
                let length = coupling.info.group_lengths[group] as usize;
                for band in 0..max_sfb {
                    let at = group * max_sfb + band;
                    if coupling.band_types[at] == BAND_ZERO {
                        continue;
                    }
                    let gain = coupling.gains[list.min(MAX_GAIN_LISTS - 1)][at];
                    let (low, high) = (swb[band] as usize, swb[band + 1] as usize);
                    for window in 0..length {
                        let offset = window_start + window * span;
                        for line in offset + low..offset + high {
                            let value = (source[line] as f32 * gain) as i32;
                            destination[line] = destination[line].wrapping_add(value);
                        }
                    }
                }
                window_start += length * span;
            }
        }
    }

    fn apply_independent(
        &mut self,
        channel: usize,
        target: (u8, u8, u8),
        couplings: usize,
        out: &mut [i16],
        position: usize,
    ) {
        for index in 0..couplings {
            let coupling = &self.couplings[index];
            if coupling.point != COUPLE_AFTER_IMDCT {
                continue;
            }
            let Some(list) = Self::gain_list(coupling, target) else {
                continue;
            };
            let gain = coupling.gains[list.min(MAX_GAIN_LISTS - 1)][0];
            for (i, &sample) in self.coupling_time[index][..self.frame].iter().enumerate() {
                if !self.core_pcm.is_empty() {
                    let slot = &mut self.core_pcm[channel * FRAME_SAMPLES + i];
                    *slot = slot.saturating_add((sample as f32 * gain * (1 << PCM_FRACTION) as f32) as i32);
                    continue;
                }
                let slot = &mut out[i * self.channels + position];
                let mixed = *slot as f32 + sample as f32 * gain;
                *slot = mixed.clamp(-32768.0, 32767.0) as i16;
            }
        }
    }
}

fn open_sbr(config: &AudioSpecificConfig) -> Result<SbrDecoder> {
    SbrDecoder::new(
        config.channels(),
        2 * config.sample_rate,
        config.downsampled(),
        config.output_channels() == 2 && config.channels() == 1,
    )
}

/// `2^(numerator / denominator)`, for gains: the denominator is 1, 2, 4 or 8.
fn power_of_two(numerator: i32, denominator: i32) -> f32 {
    const EIGHTHS: [f32; 8] = [
        1.0,
        1.090_507_7,
        1.189_207_1,
        1.296_839_6,
        core::f32::consts::SQRT_2,
        1.542_210_2,
        1.681_792_8,
        1.834_008_1,
    ];
    let eighths = numerator.saturating_mul(8 / denominator.max(1));
    let whole = eighths.div_euclid(8).clamp(-120, 120);
    let part = eighths.rem_euclid(8) as usize;
    EIGHTHS[part] * f32::from_bits(((whole + 127) as u32) << 23)
}

/// What the fold puts at `n` of the first half of a frame's coefficients: `-c_r - d` for a
/// `(a, b, c, d)` of quarters.
fn time_fold_low(time: &[i32], frame: usize, n: usize) -> i32 {
    let three = 3 * frame / 2;
    time[three - 1 - n].wrapping_neg().wrapping_sub(time[three + n])
}

/// And at `n` of the second: `a - b_r`.
fn time_fold_high(time: &[i32], frame: usize, n: usize) -> i32 {
    time[n].wrapping_sub(time[frame - 1 - n])
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
    // Scalable sampling rate, whose gain control this doesn't do.
    if profile == 2 {
        return Err(AudioError::UnsupportedFeature("AAC object type"));
    }
    if blocks > 1 && !protection_absent {
        return Err(AudioError::UnsupportedFeature(
            "AAC ADTS checksum over several blocks",
        ));
    }
    let mut config = if channel_config == 0 {
        AudioSpecificConfig::with_program(rate_index)?
    } else {
        AudioSpecificConfig::new(rate_index, channel_config)?
    };
    config.object_type = profile + 1;
    Ok(Some(AdtsHeader {
        config,
        header_len,
        frame_len,
        blocks,
    }))
}

/// ADTS has nowhere to say a stream has SBR: its first frame shows it, by decoding it.
fn resolve_implicit(config: AudioSpecificConfig, block: Option<&[u8]>) -> Result<AudioSpecificConfig> {
    let Some(block) = block.filter(|_| config.may_be_implicit()) else {
        return Ok(config);
    };
    let mut decoder = AacDecoder::new(config)?;
    let mut out = try_vec(2 * FRAME_SAMPLES * decoder.channels.max(2), 0i16)?;
    let mut reader = MsbReader::new(block);
    // A first frame that won't decode says nothing, and the next will be found out then.
    if decoder.decode_block(&mut reader, &mut out).is_err() {
        return Ok(AudioSpecificConfig { sbr: Some(false), ..config });
    }
    Ok(decoder.config())
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
        let config = header.config.with_program_from(input.get(pos + header.header_len..).unwrap_or(&[]))?;
        let config = resolve_implicit(config, input.get(pos + header.header_len..pos + header.frame_len))?;
        Ok(Self {
            input,
            pos,
            decoder: AacDecoder::new(config)?,
        })
    }

    /// The first frame's parameters; the format can't change mid-stream.
    pub fn config(&self) -> AudioSpecificConfig {
        self.decoder.config()
    }

    /// The most samples, counting every channel, one ADTS frame can produce.
    pub fn max_frame_samples(&self) -> usize {
        4 * self.decoder.config.output_frame() * self.decoder.config.output_channels()
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
        let channels = self.decoder.config.output_channels();
        let samples = self.decoder.config.output_frame();
        if !header.config.matches(&self.decoder.config) {
            return Err(AudioError::UnsupportedFeature(
                "AAC stream changes format mid-stream",
            ));
        }
        let required = header.blocks * samples * channels;
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
            let start = block * samples * channels;
            self.decoder.decode_block(&mut reader, &mut out[start..required])?;
            reader.align();
        }
        Ok(Some(header.blocks * samples))
    }
}

/// Returns the properties of an ADTS stream, by walking its frame headers.
pub fn info(input: &[u8]) -> Result<AudioInfo> {
    let start = id3_len(input);
    let (mut pos, first) =
        next_adts(input, start)?.ok_or(AudioError::InvalidData("no AAC frame found"))?;
    let config = first.config.with_program_from(input.get(pos + first.header_len..).unwrap_or(&[]))?;
    let config = resolve_implicit(config, input.get(pos + first.header_len..pos + first.frame_len))?;
    let mut frames = 0u64;
    while let Some((at, header)) = next_adts(input, pos)? {
        // A cut-off last frame does not count: it will not decode.
        if at + header.frame_len > input.len() {
            break;
        }
        frames += (header.blocks * config.output_frame()) as u64;
        pos = at + header.frame_len;
    }
    Ok(AudioInfo {
        sample_rate: config.output_rate(),
        channels: config.output_channels() as u16,
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
    let channels = config.output_channels();
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
            sample_rate: config.output_rate(),
            channels: channels as u16,
            frames: frames as u64,
            format: AudioFormat::Aac,
        },
        frames,
    })
}

impl AacDecoder {
    /// Channels it decodes to.
    pub(crate) fn channel_count(&self) -> usize {
        self.config.output_channels()
    }
}

/// Decodes the next ADTS frame in `data`, for a caller reading a piece of a file at a
/// time. `Ok(None)` when `data` doesn't reach the end of a frame: the file has ended, or
/// the caller should come back with more. Otherwise `(bytes consumed, sample frames)`.
pub(crate) fn decode_adts(
    decoder: &mut AacDecoder,
    data: &[u8],
    out: &mut [i16],
) -> Result<Option<(usize, usize)>> {
    let Some((pos, header)) = next_adts(data, 0)? else {
        return Ok(None);
    };
    let channels = decoder.config.output_channels();
    let samples = decoder.config.output_frame();
    if !header.config.matches(&decoder.config) {
        return Err(AudioError::UnsupportedFeature(
            "AAC stream changes format mid-stream",
        ));
    }
    let required = header.blocks * samples * channels;
    if out.len() < required {
        return Err(AudioError::BufferTooSmall {
            required,
            available: out.len(),
        });
    }
    let Some(frame) = data.get(pos + header.header_len..pos + header.frame_len) else {
        return Ok(None);
    };
    let mut reader = MsbReader::new(frame);
    for block in 0..header.blocks {
        let start = block * samples * channels;
        decoder.decode_block(&mut reader, &mut out[start..required])?;
        reader.align();
    }
    Ok(Some((pos + header.frame_len, header.blocks * samples)))
}

/// The first frame in `data`, and from its frames, samples per byte: how a long file's
/// length is judged without reading it. `(first frame's offset, config, frames, bytes)`.
pub(crate) fn survey_adts(data: &[u8]) -> Result<(usize, AudioSpecificConfig, u64, u64)> {
    let (start, first) =
        next_adts(data, 0)?.ok_or(AudioError::InvalidData("no AAC frame found"))?;
    let (mut pos, mut frames, mut bytes) = (start, 0u64, 0u64);
    while let Ok(Some((at, header))) = next_adts(data, pos) {
        if at + header.frame_len > data.len() {
            break;
        }
        frames += (header.blocks * FRAME_SAMPLES) as u64;
        bytes += (at + header.frame_len - pos) as u64;
        pos = at + header.frame_len;
    }
    let config = first.config.with_program_from(data.get(start + first.header_len..).unwrap_or(&[]))?;
    let config = resolve_implicit(config, data.get(start + first.header_len..start + first.frame_len))?;
    frames = frames / FRAME_SAMPLES as u64 * config.output_frame() as u64;
    Ok((start, config, frames, bytes))
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
        // Channel configuration 0 needs a program config element, which this hasn't.
        assert!(AudioSpecificConfig::parse(&[0x12, 0x00]).is_err());
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
