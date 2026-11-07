//! Spectral band replication, the "HE" of HE-AAC, written from the description in ISO/IEC
//! 14496-3 4.6.18. AAC codes the bottom of the spectrum at half the output rate, and this
//! rebuilds the top from it, guided by a thin side stream.
//!
//! Each frame's core output goes through a 32-band analysis filter bank. Bands above the
//! crossover are copied up from below, flattened by an inverse filter, and shaped to the
//! energies the stream gives, with noise and sines added. A 64-band synthesis bank takes all
//! that to samples at twice the rate.
//!
//! The filter banks and everything done per sample are integers, in Q6 of 16-bit PCM on the
//! standard's scale. Whatever's done once per band of a frame is floating point, as the
//! ranges there are enormous and there's little of it.
//!
//! Damaged side data is dropped, and the frame uses what the last one said.

use alloc::vec::Vec;

use crate::aac::read_code;
use crate::aac_tables::Code;
use crate::bits::{try_vec, MsbReader};
use crate::error::{AudioError, Result};
use crate::mdct::{cmul, cos_turn, q62_to_q31, sin_turn, Imdct};
use crate::ps::Ps;
use crate::sbr_tables::{
    ANALYSIS_PRE, ANALYSIS_ROTATE, F_BAL_1_5, F_BAL_3_0, F_ENV_1_5, F_ENV_3_0, NOISE, QMF_WINDOW, START_MIN, START_OFFSET,
    START_ROW, STOP_MIN, STOP_OFFSET, T_BAL_1_5, T_BAL_3_0, T_ENV_1_5, T_ENV_3_0, T_NOISE_3_0,
    T_NOISE_BAL_3_0,
};

/// QMF slots in a frame.
pub(crate) const SLOTS: usize = 32;
/// Slots of `low` from before this frame, for the high band's filter to look back at.
const HF_GEN: usize = 8;
/// How far the envelopes' time is ahead of the low band's.
const HF_ADJ: usize = 2;
/// Slots of a frame's `low`.
const LOW_SLOTS: usize = SLOTS + HF_GEN;
/// The most slots an envelope can reach: `SLOTS` and 3 time slots of 2.
const HIGH_SLOTS: usize = SLOTS + 6;
const LOW_BANDS: usize = 32;
pub(crate) const BANDS: usize = 64;
const MAX_ENV: usize = 5;
const MAX_NOISE_BANDS: usize = 5;
/// Bands of the high resolution table, and of the master table.
const MAX_HIGH: usize = 48;
/// Fractional bits of a filter bank's subband sample.
const Q: u32 = 6;
/// Fractional bits of a gain.
const GAIN_Q: u32 = 20;
/// The core's output comes in Q13.
const CORE_Q: u32 = 13;

/// Time smoothing of gains, newest first, in Q15.
const SMOOTH: [i64; 5] = [10923, 9880, 7149, 3774, 1043];

const FIX_FIX: u8 = 0;
const FIX_VAR: u8 = 1;
const VAR_FIX: u8 = 2;

pub(crate) type Cx = (i32, i32);

fn log2(x: f64) -> f64 {
    if !(x > 0.0) {
        return 0.0;
    }
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mantissa = f64::from_bits((bits & ((1 << 52) - 1)) | (1023 << 52));
    // ln(m) = 2 atanh((m - 1) / (m + 1)).
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0;
    let mut k = 1.0;
    while k < 40.0 {
        sum += term / k;
        term *= t2;
        k += 2.0;
    }
    exponent as f64 + 2.0 * sum / core::f64::consts::LN_2
}

fn exp2(x: f64) -> f64 {
    let whole = floor(x);
    let part = (x - whole) * core::f64::consts::LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while k < 24.0 {
        term *= part / k;
        sum += term;
        k += 1.0;
    }
    let exponent = (whole as i64).clamp(-1000, 1000);
    sum * f64::from_bits(((exponent + 1023) as u64) << 52)
}

fn floor(x: f64) -> f64 {
    let truncated = x as i64 as f64;
    if truncated > x {
        truncated - 1.0
    } else {
        truncated
    }
}

fn nint(x: f64) -> i32 {
    floor(x + 0.5) as i32
}

/// `2^(n/2)`.
fn pow2_half(n: i32) -> f32 {
    let n = n.clamp(-240, 240);
    let whole = n.div_euclid(2);
    let scale = f32::from_bits(((whole + 127) as u32) << 23);
    if n.rem_euclid(2) != 0 {
        scale * core::f32::consts::SQRT_2
    } else {
        scale
    }
}

/// A square root, near enough to float precision, for what's done once per band.
fn sqrt(x: f32) -> f32 {
    if !(x > 0.0) {
        return 0.0;
    }
    let mut y = f32::from_bits((x.to_bits() >> 1) + 0x1fc0_0000);
    y = 0.5 * (y + x / y);
    y = 0.5 * (y + x / y);
    0.5 * (y + x / y)
}

/// A 64-point inverse complex FFT, without scaling, radix 4.
struct Fft64 {
    twiddle: [Cx; 64],
    reverse: [u8; 64],
}

impl Fft64 {
    fn new() -> Self {
        let mut twiddle = [(0, 0); 64];
        for (k, slot) in twiddle.iter_mut().enumerate() {
            let phase = (k as u64) << 58;
            *slot = (q62_to_q31(cos_turn(phase)), q62_to_q31(sin_turn(phase)));
        }
        let mut reverse = [0u8; 64];
        for (k, slot) in reverse.iter_mut().enumerate() {
            *slot = (((k & 3) << 4) | (k & 12) | (k >> 4)) as u8;
        }
        Self { twiddle, reverse }
    }

    /// Transforms `input`, in order, to `z`.
    fn run(&self, input: &[Cx; 64], z: &mut [Cx; 64]) {
        for (slot, &from) in z.iter_mut().zip(&self.reverse) {
            *slot = input[from as usize];
        }
        // Every value here is a Q31 twiddle's product, so a sum of four can't overflow on
        // input with the headroom the callers leave.
        for start in (0..64).step_by(4) {
            let block = &mut z[start..start + 4];
            let (b, c, d) = (block[1], block[2], block[3]);
            butterfly(block, 0, 1, b, c, d);
        }
        let mut size = 16;
        while size <= 64 {
            let quarter = size / 4;
            let step = 64 / size;
            for start in (0..64).step_by(size) {
                let block = &mut z[start..start + size];
                for j in 0..quarter {
                    let b = cmul(block[j + quarter].0, block[j + quarter].1, self.twiddle[j * step].0, self.twiddle[j * step].1);
                    let c = cmul(
                        block[j + 2 * quarter].0,
                        block[j + 2 * quarter].1,
                        self.twiddle[2 * j * step].0,
                        self.twiddle[2 * j * step].1,
                    );
                    let d = cmul(
                        block[j + 3 * quarter].0,
                        block[j + 3 * quarter].1,
                        self.twiddle[3 * j * step].0,
                        self.twiddle[3 * j * step].1,
                    );
                    butterfly(block, j, quarter, b, c, d);
                }
            }
            size *= 4;
        }
    }
}

/// One radix-4 step: `a` is already at `j`, and the rest have had their twiddles.
#[inline]
fn butterfly(block: &mut [Cx], j: usize, quarter: usize, b: Cx, c: Cx, d: Cx) {
    let a = block[j];
    let t0 = (a.0.wrapping_add(c.0), a.1.wrapping_add(c.1));
    let t1 = (a.0.wrapping_sub(c.0), a.1.wrapping_sub(c.1));
    let t2 = (b.0.wrapping_add(d.0), b.1.wrapping_add(d.1));
    let t3 = (b.0.wrapping_sub(d.0), b.1.wrapping_sub(d.1));
    block[j] = (t0.0.wrapping_add(t2.0), t0.1.wrapping_add(t2.1));
    block[j + 2 * quarter] = (t0.0.wrapping_sub(t2.0), t0.1.wrapping_sub(t2.1));
    // Times +i and -i.
    block[j + quarter] = (t1.0.wrapping_sub(t3.1), t1.1.wrapping_add(t3.0));
    block[j + 3 * quarter] = (t1.0.wrapping_add(t3.1), t1.1.wrapping_sub(t3.0));
}

/// `sbr_header`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Header {
    amp_res: bool,
    start_freq: u8,
    stop_freq: u8,
    xover: u8,
    freq_scale: u8,
    alter_scale: bool,
    noise_bands: u8,
    limiter_bands: u8,
    limiter_gains: u8,
    interpol_freq: bool,
    smoothing: bool,
}

impl Header {
    fn read(reader: &mut MsbReader<'_>) -> Result<Self> {
        let mut header = Self {
            amp_res: reader.bit()?,
            start_freq: reader.read(4)? as u8,
            stop_freq: reader.read(4)? as u8,
            xover: reader.read(3)? as u8,
            freq_scale: 2,
            alter_scale: true,
            noise_bands: 2,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: true,
            smoothing: true,
        };
        reader.read(2)?;
        let extra_1 = reader.bit()?;
        let extra_2 = reader.bit()?;
        if extra_1 {
            header.freq_scale = reader.read(2)? as u8;
            header.alter_scale = reader.bit()?;
            header.noise_bands = reader.read(2)? as u8;
        }
        if extra_2 {
            header.limiter_bands = reader.read(2)? as u8;
            header.limiter_gains = reader.read(2)? as u8;
            header.interpol_freq = reader.bit()?;
            header.smoothing = reader.bit()?;
        }
        Ok(header)
    }

    /// Whether the band tables depend on what differs.
    fn same_bands(&self, other: &Self) -> bool {
        self.start_freq == other.start_freq
            && self.stop_freq == other.stop_freq
            && self.xover == other.xover
            && self.freq_scale == other.freq_scale
            && self.alter_scale == other.alter_scale
            && self.noise_bands == other.noise_bands
            && self.limiter_bands == other.limiter_bands
    }
}

/// The frequency tables a header leads to.
#[derive(Clone, Copy)]
struct BandTables {
    kx: usize,
    m: usize,
    n_high: usize,
    n_low: usize,
    n_noise: usize,
    n_limit: usize,
    high: [u8; MAX_HIGH + 1],
    low: [u8; MAX_HIGH + 1],
    noise: [u8; MAX_NOISE_BANDS + 1],
    limit: [u8; MAX_HIGH + 7],
    patches: usize,
    patch_start: [u8; 6],
    patch_size: [u8; 6],
}

fn calc_bands(k0: i32, k1: i32, count: usize, out: &mut [i32]) {
    let base = k1 as f64 / k0 as f64;
    let mut previous = k0;
    for (k, slot) in out.iter_mut().enumerate().take(count) {
        let present = nint(k0 as f64 * exp2(log2(base) * (k + 1) as f64 / count as f64));
        *slot = present - previous;
        previous = present;
    }
}

impl BandTables {
    /// Returns `None` if a header's values make no sense for this rate.
    fn new(header: &Header, rate_row: usize, rate: u32) -> Option<Self> {
        let row = *START_ROW.get(rate_row)? as usize;
        let k0 = START_MIN[rate_row] as i32 + START_OFFSET[row * 16 + header.start_freq as usize] as i32;
        let k2 = match header.stop_freq {
            14 => (2 * k0).min(64),
            15 => (3 * k0).min(64),
            stop => (STOP_MIN[rate_row] as i32 + STOP_OFFSET[rate_row * 14 + stop as usize] as i32).min(64),
        };
        if k0 <= 0 || k0 > 32 || k2 <= k0 {
            return None;
        }
        let limit = if rate <= 32_000 {
            48
        } else if rate <= 44_100 {
            35
        } else {
            32
        };
        if k2 - k0 > limit {
            return None;
        }

        // The master table.
        let mut master = [0i32; 128];
        let n_master;
        if header.freq_scale == 0 {
            let dk = header.alter_scale as i32 + 1;
            let n = ((k2 - k0 + (dk & 2)) >> dk) << 1;
            if n <= 0 || n as usize > MAX_HIGH + 16 {
                return None;
            }
            n_master = n as usize;
            let mut deltas = [dk; 128];
            let difference = k2 - k0 - n * dk;
            if difference < 0 {
                deltas[0] -= 1;
                if difference < -1 {
                    deltas[1] -= 1;
                }
            } else if difference > 0 {
                deltas[n_master - 1] += 1;
            }
            master[0] = k0;
            for k in 0..n_master {
                master[k + 1] = master[k] + deltas[k];
            }
        } else {
            let per_octave = [12.0, 10.0, 8.0][header.freq_scale as usize - 1];
            let warp = if header.alter_scale { 1.3 } else { 1.0 };
            let two_regions = k2 as f64 / k0 as f64 > 2.2449;
            let k1 = if two_regions { 2 * k0 } else { k2 };
            let n0 = 2 * nint(per_octave * log2(k1 as f64 / k0 as f64) / 2.0);
            if n0 <= 0 || n0 > 64 {
                return None;
            }
            let n0 = n0 as usize;
            let mut deltas0 = [0i32; 64];
            calc_bands(k0, k1, n0, &mut deltas0);
            deltas0[..n0].sort_unstable();
            if deltas0[0] <= 0 {
                return None;
            }
            master[0] = k0;
            for k in 0..n0 {
                master[k + 1] = master[k] + deltas0[k];
            }
            let mut count = n0;
            if two_regions {
                let n1 = 2 * nint(per_octave * log2(k2 as f64 / k1 as f64) / (2.0 * warp));
                if n1 <= 0 || n1 > 64 {
                    return None;
                }
                let n1 = n1 as usize;
                let mut deltas1 = [0i32; 64];
                calc_bands(k1, k2, n1, &mut deltas1);
                deltas1[..n1].sort_unstable();
                let (smallest, largest) = (deltas1[0], deltas0[n0 - 1]);
                if smallest < largest {
                    let change = (largest - smallest).min((deltas1[n1 - 1] - smallest) / 2);
                    deltas1[0] += change;
                    deltas1[n1 - 1] -= change;
                    deltas1[..n1].sort_unstable();
                }
                if deltas1[0] <= 0 || n0 + n1 >= 127 {
                    return None;
                }
                for k in 0..n1 {
                    master[n0 + k + 1] = master[n0 + k] + deltas1[k];
                }
                count += n1;
            }
            n_master = count;
        }
        if master[n_master] > 64 {
            return None;
        }

        let xover = header.xover as usize;
        if xover >= n_master {
            return None;
        }
        let n_high = n_master - xover;
        if n_high > MAX_HIGH || n_high == 0 {
            return None;
        }
        let kx = master[xover];
        let top = master[n_master];
        if kx as usize > LOW_BANDS || kx + (top - kx) > 64 {
            return None;
        }
        let mut high = [0u8; MAX_HIGH + 1];
        for k in 0..=n_high {
            high[k] = master[xover + k] as u8;
        }
        let n_low = (n_high + 1) / 2;
        let mut low = [0u8; MAX_HIGH + 1];
        let odd = n_high & 1;
        for k in 0..=n_low {
            low[k] = high[if k == 0 { 0 } else { 2 * k - odd }];
        }
        let n_noise = if header.noise_bands == 0 {
            1
        } else {
            (nint(header.noise_bands as f64 * log2(top as f64 / kx as f64)).max(1)) as usize
        };
        if n_noise > MAX_NOISE_BANDS {
            return None;
        }
        let mut noise = [0u8; MAX_NOISE_BANDS + 1];
        noise[0] = low[0];
        let mut at = 0usize;
        for k in 1..=n_noise {
            at += (n_low - at) / (n_noise + 1 - k);
            noise[k] = low[at];
        }

        // Where to copy the high band from.
        let m = top - kx;
        let goal = nint(2.048e6 / rate as f64);
        let mut k = if goal < kx + m {
            let mut i = 0;
            let mut last = 0;
            while i <= n_master && master[i] < goal {
                last = i + 1;
                i += 1;
            }
            last
        } else {
            n_master
        };
        let mut patch_start = [0u8; 6];
        let mut patch_size = [0u8; 6];
        let mut patches = 0usize;
        let (mut msb, mut usb) = (k0, kx);
        let mut rounds = 0;
        loop {
            // A sensible table is done within a few rounds.
            rounds += 1;
            if rounds > 32 {
                return None;
            }
            let mut j = k + 1;
            let mut sb;
            let mut odd;
            loop {
                if j == 0 {
                    return None;
                }
                j -= 1;
                sb = *master.get(j)?;
                odd = (sb - 2 + k0) % 2;
                if sb <= k0 - 1 + msb - odd {
                    break;
                }
            }
            let size = (sb - usb).max(0);
            if size > 0 {
                if patches >= patch_size.len() {
                    return None;
                }
                let start = k0 - odd - size;
                if start < 0 {
                    return None;
                }
                patch_size[patches] = size as u8;
                patch_start[patches] = start as u8;
                patches += 1;
                usb = sb;
                msb = sb;
            } else {
                msb = kx;
            }
            if master[k.min(n_master)] - sb < 3 {
                k = n_master;
            }
            if sb == kx + m {
                break;
            }
        }
        if patches > 1 && patch_size[patches - 1] < 3 {
            patches -= 1;
        }
        if patches == 0 {
            return None;
        }

        // The limiter's bands.
        let mut limit_table = [0u8; MAX_HIGH + 7];
        let mut n_limit;
        if header.limiter_bands == 0 {
            limit_table[0] = low[0];
            limit_table[1] = low[n_low];
            n_limit = 1;
        } else {
            let per_octave = [0.0, 1.2, 2.0, 3.0][header.limiter_bands as usize];
            let mut borders = [0u8; 7];
            borders[0] = kx as u8;
            for i in 0..patches {
                borders[i + 1] = borders[i] + patch_size[i];
            }
            limit_table[..=n_low].copy_from_slice(&low[..=n_low]);
            n_limit = n_low + patches - 1;
            for i in 1..patches {
                limit_table[n_low + i] = borders[i];
            }
            limit_table[..=n_limit].sort_unstable();
            let is_border = |value: u8| borders[..=patches].contains(&value);
            let mut k = 1;
            while k <= n_limit {
                let octaves = log2(limit_table[k] as f64 / limit_table[k - 1].max(1) as f64);
                if octaves * per_octave < 0.49 {
                    let remove = if limit_table[k] == limit_table[k - 1] || !is_border(limit_table[k]) {
                        Some(k)
                    } else if !is_border(limit_table[k - 1]) {
                        Some(k - 1)
                    } else {
                        None
                    };
                    if let Some(gone) = remove {
                        limit_table.copy_within(gone + 1..=n_limit, gone);
                        n_limit -= 1;
                        continue;
                    }
                }
                k += 1;
            }
        }

        Some(Self {
            kx: kx as usize,
            m: m as usize,
            n_high,
            n_low,
            n_noise,
            n_limit,
            high,
            low,
            noise,
            limit: limit_table,
            patches,
            patch_start,
            patch_size,
        })
    }

    fn table(&self, high: bool) -> (&[u8], usize) {
        if high {
            (&self.high, self.n_high)
        } else {
            (&self.low, self.n_low)
        }
    }
}

/// Everything a frame's side data says about one channel, and what those were coded against.
#[derive(Clone, Copy)]
struct Frame {
    class: u8,
    amp_res: bool,
    num_env: usize,
    t_e: [u8; MAX_ENV + 1],
    freq_res: [bool; MAX_ENV],
    pointer: u8,
    num_noise: usize,
    t_q: [u8; 3],
    df_env: [bool; MAX_ENV],
    df_noise: [bool; 2],
    invf: [u8; MAX_NOISE_BANDS],
    env: [[i16; MAX_HIGH]; MAX_ENV],
    noise: [[i16; MAX_NOISE_BANDS]; 2],
    harmonic: [bool; MAX_HIGH],
    /// The envelope this frame's transient is at, or -1, and last frame's in this one's terms.
    l_a: i8,
    l_a_previous: i8,
}

impl Frame {
    const fn new() -> Self {
        Self {
            class: 0,
            amp_res: false,
            num_env: 0,
            t_e: [0; MAX_ENV + 1],
            freq_res: [false; MAX_ENV],
            pointer: 0,
            num_noise: 0,
            t_q: [0; 3],
            df_env: [false; MAX_ENV],
            df_noise: [false; 2],
            invf: [0; MAX_NOISE_BANDS],
            env: [[0; MAX_HIGH]; MAX_ENV],
            noise: [[0; MAX_NOISE_BANDS]; 2],
            harmonic: [false; MAX_HIGH],
            l_a: -1,
            l_a_previous: -1,
        }
    }

    /// The time and frequency layout of its envelopes: `sbr_grid`.
    fn read_grid(&mut self, reader: &mut MsbReader<'_>, header: &Header) -> Result<()> {
        const BAD: AudioError = AudioError::InvalidData("bad SBR grid");
        let old_l_a = self.l_a;
        let old_env = self.num_env;
        self.class = reader.read(2)? as u8;
        self.amp_res = header.amp_res;
        let mut rel_0 = [0u8; 4];
        let mut rel_1 = [0u8; 4];
        let (mut count_0, mut count_1) = (0usize, 0usize);
        let (abs_0, abs_1);
        match self.class {
            FIX_FIX => {
                let log = reader.read(2)?;
                if log == 3 {
                    return Err(BAD);
                }
                self.num_env = 1 << log;
                if self.num_env == 1 {
                    self.amp_res = false;
                }
                let res = reader.bit()?;
                self.freq_res = [res; MAX_ENV];
                self.pointer = 0;
                abs_0 = 0;
                abs_1 = 16;
            }
            FIX_VAR => {
                abs_0 = 0;
                abs_1 = reader.read(2)? as usize + 16;
                count_1 = reader.read(2)? as usize;
                self.num_env = count_1 + 1;
                for slot in rel_1.iter_mut().take(count_1) {
                    *slot = 2 * reader.read(2)? as u8 + 2;
                }
                self.pointer = reader.read(pointer_bits(self.num_env))? as u8;
                for env in 0..self.num_env {
                    self.freq_res[self.num_env - 1 - env] = reader.bit()?;
                }
            }
            VAR_FIX => {
                abs_0 = reader.read(2)? as usize;
                abs_1 = 16;
                count_0 = reader.read(2)? as usize;
                self.num_env = count_0 + 1;
                for slot in rel_0.iter_mut().take(count_0) {
                    *slot = 2 * reader.read(2)? as u8 + 2;
                }
                self.pointer = reader.read(pointer_bits(self.num_env))? as u8;
                for env in 0..self.num_env {
                    self.freq_res[env] = reader.bit()?;
                }
            }
            _ => {
                abs_0 = reader.read(2)? as usize;
                abs_1 = reader.read(2)? as usize + 16;
                count_0 = reader.read(2)? as usize;
                count_1 = reader.read(2)? as usize;
                self.num_env = count_0 + count_1 + 1;
                if self.num_env > MAX_ENV {
                    return Err(BAD);
                }
                for slot in rel_0.iter_mut().take(count_0) {
                    *slot = 2 * reader.read(2)? as u8 + 2;
                }
                for slot in rel_1.iter_mut().take(count_1) {
                    *slot = 2 * reader.read(2)? as u8 + 2;
                }
                self.pointer = reader.read(pointer_bits(self.num_env))? as u8;
                for env in 0..self.num_env {
                    self.freq_res[env] = reader.bit()?;
                }
            }
        }
        let n = self.num_env;
        if self.pointer as usize > n {
            return Err(BAD);
        }

        // The borders, in time slots of two QMF slots.
        let mut borders = [0i32; MAX_ENV + 1];
        if self.class == FIX_FIX {
            for (l, slot) in borders.iter_mut().enumerate().take(n + 1) {
                *slot = (l * 16 / n) as i32;
            }
        } else {
            borders[0] = abs_0 as i32;
            borders[n] = abs_1 as i32;
            for l in 0..count_0 {
                borders[l + 1] = borders[l] + rel_0[l] as i32;
            }
            for l in 0..count_1 {
                borders[n - 1 - l] = borders[n - l] - rel_1[l] as i32;
            }
        }
        for l in 0..n {
            if borders[l] >= borders[l + 1] || borders[l] < 0 {
                return Err(BAD);
            }
        }
        if borders[n] > 19 {
            return Err(BAD);
        }
        for l in 0..=n {
            self.t_e[l] = borders[l] as u8;
        }

        // Noise floors: one, or two either side of a chosen border.
        self.num_noise = if n > 1 { 2 } else { 1 };
        let pointer = self.pointer as usize;
        self.t_q[0] = self.t_e[0];
        if self.num_noise == 1 {
            self.t_q[1] = self.t_e[n];
        } else {
            let middle = match self.class {
                FIX_FIX => n / 2,
                VAR_FIX => match pointer {
                    0 => 1,
                    1 => n - 1,
                    _ => pointer - 1,
                },
                _ if pointer > 1 => n + 1 - pointer,
                _ => n - 1,
            };
            self.t_q[1] = self.t_e[middle.min(n)];
            self.t_q[2] = self.t_e[n];
        }

        // Which envelope follows a transient, and so isn't smoothed.
        self.l_a_previous = if old_l_a >= 0 && old_l_a as usize == old_env { 0 } else { -1 };
        self.l_a = match self.class {
            FIX_FIX => -1,
            VAR_FIX if pointer > 1 => (pointer - 1) as i8,
            VAR_FIX => -1,
            _ if pointer > 0 => (n + 1 - pointer) as i8,
            _ => -1,
        };
        Ok(())
    }

    fn read_dtdf(&mut self, reader: &mut MsbReader<'_>) -> Result<()> {
        for env in 0..self.num_env {
            self.df_env[env] = reader.bit()?;
        }
        for noise in 0..self.num_noise {
            self.df_noise[noise] = reader.bit()?;
        }
        Ok(())
    }

    fn read_invf(&mut self, reader: &mut MsbReader<'_>, bands: &BandTables) -> Result<()> {
        for slot in self.invf.iter_mut().take(bands.n_noise) {
            *slot = reader.read(2)? as u8;
        }
        Ok(())
    }

    fn read_envelope(
        &mut self,
        reader: &mut MsbReader<'_>,
        bands: &BandTables,
        balance: bool,
        before: &Frame,
    ) -> Result<()> {
        let (t_table, f_table, lav, start_bits): (&[Code], &[Code], i32, u32) = match (self.amp_res, balance) {
            (false, false) => (&T_ENV_1_5, &F_ENV_1_5, 60, 7),
            (false, true) => (&T_BAL_1_5, &F_BAL_1_5, 24, 6),
            (true, false) => (&T_ENV_3_0, &F_ENV_3_0, 31, 6),
            (true, true) => (&T_BAL_3_0, &F_BAL_3_0, 12, 5),
        };
        let delta = if balance { 2 } else { 1 };
        for env in 0..self.num_env {
            let (table, n) = bands.table(self.freq_res[env]);
            if self.df_env[env] {
                // Against the envelope before, whichever table that's on.
                let (previous, previous_res) = if env == 0 {
                    let last = before.num_env.saturating_sub(1);
                    (before.env[last], before.freq_res[last])
                } else {
                    (self.env[env - 1], self.freq_res[env - 1])
                };
                let (other, other_n) = bands.table(previous_res);
                let mut line = [0i16; MAX_HIGH];
                for k in 0..n {
                    let value = read_code(reader, t_table)? as i32 - lav;
                    let from = if previous_res == self.freq_res[env] {
                        k
                    } else if self.freq_res[env] {
                        // A high resolution band, within a low resolution one.
                        (0..other_n)
                            .find(|&i| other[i] <= table[k] && table[k] < other[i + 1])
                            .unwrap_or(0)
                    } else {
                        (0..other_n).find(|&i| other[i] == table[k]).unwrap_or(0)
                    };
                    line[k] = clamp_scale(previous[from] as i32 + delta * value);
                }
                self.env[env] = line;
            } else {
                let mut running = delta * reader.read(start_bits)? as i32;
                self.env[env][0] = clamp_scale(running);
                for k in 1..n {
                    running += delta * (read_code(reader, f_table)? as i32 - lav);
                    self.env[env][k] = clamp_scale(running);
                }
            }
        }
        Ok(())
    }

    fn read_noise(
        &mut self,
        reader: &mut MsbReader<'_>,
        bands: &BandTables,
        balance: bool,
        before: &Frame,
    ) -> Result<()> {
        let (t_table, f_table, lav): (&[Code], &[Code], i32) = if balance {
            (&T_NOISE_BAL_3_0, &F_BAL_3_0, 12)
        } else {
            (&T_NOISE_3_0, &F_ENV_3_0, 31)
        };
        let delta = if balance { 2 } else { 1 };
        for noise in 0..self.num_noise {
            if self.df_noise[noise] {
                let previous = if noise == 0 {
                    before.noise[before.num_noise.saturating_sub(1)]
                } else {
                    self.noise[noise - 1]
                };
                for k in 0..bands.n_noise {
                    let value = read_code(reader, t_table)? as i32 - lav;
                    self.noise[noise][k] = clamp_scale(previous[k] as i32 + delta * value);
                }
            } else {
                let mut running = delta * reader.read(5)? as i32;
                self.noise[noise][0] = clamp_scale(running);
                for k in 1..bands.n_noise {
                    running += delta * (read_code(reader, f_table)? as i32 - lav);
                    self.noise[noise][k] = clamp_scale(running);
                }
            }
        }
        Ok(())
    }

    fn read_harmonics(&mut self, reader: &mut MsbReader<'_>, bands: &BandTables) -> Result<()> {
        self.harmonic = [false; MAX_HIGH];
        if reader.bit()? {
            for slot in self.harmonic.iter_mut().take(bands.n_high) {
                *slot = reader.bit()?;
            }
        }
        Ok(())
    }
}

fn clamp_scale(value: i32) -> i16 {
    value.clamp(-512, 512) as i16
}

fn pointer_bits(envelopes: usize) -> u32 {
    match envelopes {
        0 => 0,
        1 => 1,
        2 | 3 => 2,
        _ => 3,
    }
}

/// One channel's filter banks and what it's kept from frame to frame.
struct Channel {
    frame: Frame,
    /// Set when `frame` has news, for `e_orig` and `q_orig`.
    fresh: bool,
    e_orig: [[f32; MAX_HIGH]; MAX_ENV],
    q_orig: [[f32; MAX_NOISE_BANDS]; 2],
    /// Oldest first: what the analysis window can reach, then this frame's input.
    input: Vec<i32>,
    /// `LOW_SLOTS` rows of `LOW_BANDS`.
    low: Vec<Cx>,
    /// `LOW_SLOTS` rows of `BANDS`.
    high: Vec<Cx>,
    /// `HIGH_SLOTS` rows of `BANDS`, of the assembled high band.
    y: Vec<Cx>,
    /// Slots of `y` that the last frame left for this one.
    carried: usize,
    /// The bands those belong to.
    kx_carried: usize,
    m_carried: usize,
    synthesis: Vec<i32>,
    at: usize,
    invf_before: [u8; MAX_NOISE_BANDS],
    bw_before: [f32; MAX_NOISE_BANDS],
    /// Which bands had a sine at the end of the last frame.
    sine_before: [bool; BANDS],
    gain_history: [[i64; BANDS]; 5],
    noise_history: [[i32; BANDS]; 5],
    history_at: usize,
    index_noise: usize,
    index_sine: usize,
}

impl Channel {
    fn new(synthesis_len: usize) -> Result<Self> {
        Ok(Self {
            frame: Frame::new(),
            fresh: false,
            e_orig: [[0.0; MAX_HIGH]; MAX_ENV],
            q_orig: [[0.0; MAX_NOISE_BANDS]; 2],
            input: try_vec(320 + 32 * SLOTS, 0)?,
            low: try_vec(LOW_SLOTS * LOW_BANDS, (0, 0))?,
            high: try_vec(LOW_SLOTS * BANDS, (0, 0))?,
            y: try_vec(HIGH_SLOTS * BANDS, (0, 0))?,
            carried: 0,
            kx_carried: LOW_BANDS,
            m_carried: 0,
            synthesis: try_vec(synthesis_len, 0)?,
            at: synthesis_len - 9 * synthesis_len / 42,
            invf_before: [0; MAX_NOISE_BANDS],
            bw_before: [0.0; MAX_NOISE_BANDS],
            sine_before: [false; BANDS],
            gain_history: [[0; BANDS]; 5],
            noise_history: [[0; BANDS]; 5],
            history_at: 0,
            index_noise: 0,
            index_sine: 0,
        })
    }

    fn reset(&mut self) {
        self.frame = Frame::new();
        self.fresh = false;
        self.input.fill(0);
        self.low.fill((0, 0));
        self.y.fill((0, 0));
        self.carried = 0;
        self.kx_carried = LOW_BANDS;
        self.m_carried = 0;
        self.synthesis.fill(0);
        self.bw_before = [0.0; MAX_NOISE_BANDS];
        self.invf_before = [0; MAX_NOISE_BANDS];
        self.sine_before = [false; BANDS];
        self.index_noise = 0;
        self.index_sine = 0;
    }
}

/// What one element (a channel, or a pair) has heard.
#[derive(Clone, Copy)]
struct Element {
    header: Header,
    bands: Option<BandTables>,
    /// The tables changed: gains have no history to smooth with.
    reset: bool,
    coupled: bool,
}

/// The decoder for a stream's SBR: a channel's state for each channel of the core.
pub(crate) struct SbrDecoder {
    channels: Vec<Channel>,
    elements: [Element; crate::aac::MAX_CHANNELS],
    fft: Fft64,
    /// Bands of the synthesis filter bank: 64, or 32 when output is at the core's rate.
    synthesis_bands: usize,
    transform: Imdct,
    /// Scratch for the synthesis transform: real and imaginary input and its results.
    in_re: Vec<i32>,
    in_im: Vec<i32>,
    out_a: Vec<i32>,
    out_b: Vec<i32>,
    rate: u32,
    rate_row: usize,
    /// Parametric stereo, on a mono core that might have it.
    ps: Option<Ps>,
}

/// The rates the tables go by.
const RATE_ROWS: [u32; 12] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8000,
];

impl SbrDecoder {
    /// `rate` is the rate the high band is reckoned at, twice the core's. With `downsampled`,
    /// output stays at the core's rate, and what would be above that is lost.
    pub(crate) fn new(channels: usize, rate: u32, downsampled: bool, stereo: bool) -> Result<Self> {
        let rate_row = RATE_ROWS
            .iter()
            .position(|&row| row <= rate)
            .unwrap_or(RATE_ROWS.len() - 1);
        let synthesis_bands = if downsampled { BANDS / 2 } else { BANDS };
        // Slots of `2 * bands` go past as the frame goes on, and it moves back at the end.
        let synthesis_len = (10 * 2 + 2 * SLOTS) * synthesis_bands;
        let mut list = Vec::new();
        list.try_reserve_exact(channels).map_err(|_| AudioError::OutOfMemory)?;
        for _ in 0..channels {
            list.push(Channel::new(synthesis_len)?);
        }
        Ok(Self {
            channels: list,
            elements: [Element { header: Header::default(), bands: None, reset: false, coupled: false };
                crate::aac::MAX_CHANNELS],
            fft: Fft64::new(),
            synthesis_bands,
            transform: Imdct::new(2 * synthesis_bands)?,
            in_re: try_vec(BANDS, 0)?,
            in_im: try_vec(BANDS, 0)?,
            out_a: try_vec(2 * BANDS, 0)?,
            out_b: try_vec(2 * BANDS, 0)?,
            rate,
            rate_row,
            ps: if stereo { Some(Ps::new()?) } else { None },
        })
    }

    pub(crate) fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.reset();
        }
        for element in &mut self.elements {
            element.bands = None;
            element.header = Header::default();
        }
        if let Some(ps) = &mut self.ps {
            ps.reset();
        }
    }

    /// Reads a fill element's `sbr_extension_data`, `bits` of it, for the element at `first`.
    /// Bad data leaves things as they were, and the caller skips what's there either way.
    pub(crate) fn read(
        &mut self,
        reader: &mut MsbReader<'_>,
        bits: usize,
        crc: bool,
        first: usize,
        pair: bool,
    ) -> Result<()> {
        let available = reader.bits_left();
        if crc {
            reader.read(10)?;
        }
        if reader.bit()? {
            let header = Header::read(reader)?;
            let element = &mut self.elements[first];
            let changed = element.bands.is_none() || !element.header.same_bands(&header);
            element.header = header;
            if changed {
                let new = BandTables::new(&header, self.rate_row, self.rate);
                element.reset = true;
                element.bands = new;
                extern crate std;
                if let Some(b)=&new { std::eprintln!("tables rate {} row {} kx {} m {} nhi {} nlo {} nq {} patches {} start {:?} size {:?} lim {:?} hdr start {} stop {} xo {} scale {} alter {}", self.rate,self.rate_row,b.kx,b.m,b.n_high,b.n_low,b.n_noise,b.patches,&b.patch_start[..b.patches],&b.patch_size[..b.patches],&b.limit[..=b.n_limit],header.start_freq,header.stop_freq,header.xover,header.freq_scale,header.alter_scale); } else { std::eprintln!("invalid header {}",header.stop_freq);}
            }
        }
        let Some(bands) = self.elements[first].bands else {
            return Ok(());
        };
        let header = self.elements[first].header;
        let mut next = [self.channels[first].frame, Frame::new()];
        let before = [
            self.channels[first].frame,
            if pair { self.channels[first + 1].frame } else { Frame::new() },
        ];
        if reader.bit()? {
            reader.read(if pair { 8 } else { 4 })?;
        }
        let mut coupled = false;
        if pair {
            next[1] = before[1];
            coupled = reader.bit()?;
            if coupled {
                next[0].read_grid(reader, &header)?;
                // The second's grid is the first's, with its own memory of last time.
                let (a, b) = next.split_at_mut(1);
                copy_grid(&a[0], &mut b[0]);
                next[0].read_dtdf(reader)?;
                next[1].read_dtdf(reader)?;
                next[0].read_invf(reader, &bands)?;
                let invf = next[0].invf;
                next[1].invf = invf;
                next[0].read_envelope(reader, &bands, false, &before[0])?;
                next[0].read_noise(reader, &bands, false, &before[0])?;
                next[1].read_envelope(reader, &bands, true, &before[1])?;
                next[1].read_noise(reader, &bands, true, &before[1])?;
            } else {
                next[0].read_grid(reader, &header)?;
                next[1].read_grid(reader, &header)?;
                next[0].read_dtdf(reader)?;
                next[1].read_dtdf(reader)?;
                next[0].read_invf(reader, &bands)?;
                next[1].read_invf(reader, &bands)?;
                next[0].read_envelope(reader, &bands, false, &before[0])?;
                next[1].read_envelope(reader, &bands, false, &before[1])?;
                next[0].read_noise(reader, &bands, false, &before[0])?;
                next[1].read_noise(reader, &bands, false, &before[1])?;
            }
            next[0].read_harmonics(reader, &bands)?;
            next[1].read_harmonics(reader, &bands)?;
        } else {
            next[0].read_grid(reader, &header)?;
            next[0].read_dtdf(reader)?;
            next[0].read_invf(reader, &bands)?;
            next[0].read_envelope(reader, &bands, false, &before[0])?;
            next[0].read_noise(reader, &bands, false, &before[0])?;
            next[0].read_harmonics(reader, &bands)?;
        }
        if reader.bit()? {
            let mut count = reader.read(4)? as usize;
            if count == 15 {
                count += reader.read(8)? as usize;
            }
            let mut left = 8 * count;
            while left > 7 {
                let id = reader.read(2)?;
                left -= 2;
                match (id, &mut self.ps) {
                    (2, Some(ps)) if !pair => {
                        let start = reader.bits_left();
                        ps.read(reader, left)?;
                        let used = start - reader.bits_left();
                        if used > left {
                            return Err(AudioError::InvalidData("SBR extension overruns"));
                        }
                        left -= used;
                    }
                    _ => {
                        reader.skip_long(left)?;
                        left = 0;
                    }
                }
            }
            reader.skip_long(left)?;
        }
        if available - reader.bits_left() > bits {
            return Err(AudioError::InvalidData("SBR data overruns its element"));
        }
        self.channels[first].frame = next[0];
        self.channels[first].fresh = true;
        if pair {
            self.channels[first + 1].frame = next[1];
            self.channels[first + 1].fresh = true;
        }
        self.elements[first].coupled = coupled;
        Ok(())
    }

    /// Replaces a frame of core output by its doubled form. `pcm` holds each channel's in Q13,
    /// `CHANNEL_SPAN` apart. Writes `out` at the positions given.
    pub(crate) fn apply(
        &mut self,
        first: usize,
        pair: bool,
        pcm: &[i32],
        span: usize,
        out: &mut [i16],
        stride: usize,
        positions: (usize, usize),
    ) {
        self.dequantize(first, pair);
        let count = if pair { 2 } else { 1 };
        for index in 0..count {
            let channel_index = first + index;
            self.analyse(channel_index, &pcm[channel_index * span..][..1024]);
            self.generate(first, channel_index);
        }
        let bands = self.elements[first].bands;
        let (kx, m) = bands.map_or((LOW_BANDS, 0), |b| (b.kx, b.m));
        let wanted_ps = !pair && self.ps.is_some();
        if wanted_ps {
            let ps_started = self.ps.as_ref().is_some_and(Ps::started);
            if ps_started {
                self.apply_ps(first, kx, m, out, stride, positions);
            } else {
                for slot in 0..SLOTS {
                    self.gather(first, slot, kx, m);
                    self.synthesize(first, out, slot, stride, positions.0);
                }
                let bands = self.synthesis_bands;
                for i in 0..SLOTS * bands {
                    out[i * stride + positions.1] = out[i * stride + positions.0];
                }
            }
        } else {
            for slot in 0..SLOTS {
                self.gather(first, slot, kx, m);
                self.synthesize(first, out, slot, stride, positions.0);
            }
            if pair {
                for slot in 0..SLOTS {
                    self.gather(first + 1, slot, kx, m);
                    self.synthesize(first + 1, out, slot, stride, positions.1);
                }
            }
        }
        for index in 0..count {
            self.finish(first + index, kx, m);
        }
        self.elements[first].reset = false;
    }

    fn apply_ps(&mut self, channel_index: usize, kx: usize, m: usize, out: &mut [i16], stride: usize, positions: (usize, usize)) {
        let Some(mut ps) = self.ps.take() else {
            return;
        };
        let bands = self.synthesis_bands;
        {
            let channel = &self.channels[channel_index];
            ps.begin_frame(&channel.low, LOW_BANDS, HF_ADJ, kx, kx + m);
        }
        let mut left = [(0, 0); BANDS];
        let mut right = [(0, 0); BANDS];
        for slot in 0..SLOTS {
            self.gather(channel_index, slot, kx, m);
            for k in 0..BANDS {
                left[k] = (self.in_re[k], self.in_im[k]);
            }
            ps.process_slot(slot, &mut left, &mut right);
            for k in 0..bands {
                self.in_re[k] = left[k].0;
                self.in_im[k] = left[k].1;
            }
            self.synthesize(channel_index, out, slot, stride, positions.0);
            for k in 0..bands {
                self.in_re[k] = right[k].0;
                self.in_im[k] = right[k].1;
            }
            // The right channel's bank is the second channel's, or this one's spare.
            ps.right_synthesis(|buffer, at| {
                synthesize_into(
                    &mut self.transform,
                    &mut self.in_re,
                    &mut self.in_im,
                    &mut self.out_a,
                    &mut self.out_b,
                    buffer,
                    at,
                    bands,
                    out,
                    slot,
                    stride,
                    positions.1,
                );
            });
        }
        ps.end_frame();
        self.ps = Some(ps);
    }

    /// From what a frame said, the energies and noise floors it means.
    fn dequantize(&mut self, first: usize, pair: bool) {
        let element = self.elements[first];
        let Some(bands) = element.bands else {
            return;
        };
        let count = if pair { 2 } else { 1 };
        if !self.channels[first].fresh {
            return;
        }
        if element.coupled && pair {
            let (a, b) = self.channels.split_at_mut(first + 1);
            let (left, right) = (&mut a[first], &mut b[0]);
            let half = !left.frame.amp_res;
            for env in 0..left.frame.num_env {
                let (_, n) = bands.table(left.frame.freq_res[env]);
                for k in 0..n {
                    let level = left.frame.env[env][k] as i32 * if half { 1 } else { 2 };
                    let pan = right.frame.env[env][k] as i32 * if half { 1 } else { 2 };
                    let power = pow2_half(level + 14);
                    let ratio = pow2_half(24 - pan);
                    left.e_orig[env][k] = power / (1.0 + ratio);
                    right.e_orig[env][k] = power * ratio / (1.0 + ratio);
                }
            }
            for noise in 0..left.frame.num_noise {
                for k in 0..bands.n_noise {
                    let power = pow2_half(2 * (7 - left.frame.noise[noise][k] as i32));
                    let ratio = pow2_half(2 * (12 - right.frame.noise[noise][k] as i32));
                    left.q_orig[noise][k] = power / (1.0 + ratio);
                    right.q_orig[noise][k] = power * ratio / (1.0 + ratio);
                }
            }
        } else {
            for index in 0..count {
                let channel = &mut self.channels[first + index];
                let double = channel.frame.amp_res;
                for env in 0..channel.frame.num_env {
                    let (_, n) = bands.table(channel.frame.freq_res[env]);
                    for k in 0..n {
                        let level = channel.frame.env[env][k] as i32 * if double { 2 } else { 1 };
                        channel.e_orig[env][k] = pow2_half(level + 12);
                    }
                }
                for noise in 0..channel.frame.num_noise {
                    for k in 0..bands.n_noise {
                        channel.q_orig[noise][k] = pow2_half(2 * (6 - channel.frame.noise[noise][k] as i32));
                    }
                }
            }
        }
        for index in 0..count {
            self.channels[first + index].fresh = false;
        }
    }

    /// The core's frame, through the 32-band analysis bank, into `low`.
    fn analyse(&mut self, channel_index: usize, pcm: &[i32]) {
        let fft = &self.fft;
        let channel = &mut self.channels[channel_index];
        channel.low.copy_within(SLOTS * LOW_BANDS.., 0);
        let keep = channel.input.len() - 320;
        channel.input.copy_within(keep.., 0);
        let round = 1 << (CORE_Q - Q - 1);
        let input = &mut channel.input[320..];
        for (slot, &sample) in input.iter_mut().zip(pcm) {
            *slot = (sample >> 1).wrapping_add(round >> 1) >> (CORE_Q - Q - 1);
        }
        let mut u = [0i64; 64];
        let mut z = [(0i32, 0i32); 64];
        let mut w = [(0i32, 0i32); 64];
        let input = &channel.input;
        for slot in 0..SLOTS {
            // Newest first: x[n] is input[newest - n].
            let newest = 320 + 32 * (slot + 1) - 1;
            for (n, sum) in u.iter_mut().enumerate() {
                let mut total = 0i64;
                for j in 0..5 {
                    let at = n + 64 * j;
                    total += input[newest - at] as i64 * QMF_WINDOW[2 * at] as i64;
                }
                *sum = total;
            }
            for n in 0..64 {
                // Times e^(i pi n / 64), a turn over 128.
                let value = ((u[n] + (1 << 30)) >> 31) as i32;
                let (c, s) = ANALYSIS_PRE[n];
                z[n] = (
                    ((value as i64 * c as i64 + (1 << 30)) >> 31) as i32,
                    ((value as i64 * s as i64 + (1 << 30)) >> 31) as i32,
                );
            }
            fft.run(&z, &mut w);
            let row = &mut channel.low[(HF_GEN + slot) * LOW_BANDS..][..LOW_BANDS];
            for (k, out) in row.iter_mut().enumerate() {
                let (c, s) = ANALYSIS_ROTATE[k];
                let (re, im) = cmul(w[k].0, w[k].1, c, s);
                *out = (re.wrapping_mul(2), im.wrapping_mul(2));
            }
        }
    }

    /// Builds the high band for a channel: patches, inverse filtering, envelope adjustment.
    fn generate(&mut self, first: usize, channel_index: usize) {
        let element = self.elements[first];
        let Some(bands) = element.bands else {
            return;
        };
        let header = element.header;
        let channel = &mut self.channels[channel_index];
        let frame = channel.frame;
        if frame.num_env == 0 {
            return;
        }

        // How much of the inverse filter each noise band wants.
        let mut bw = [0f32; MAX_NOISE_BANDS];
        for g in 0..bands.n_noise {
            let previous = channel.invf_before[g];
            let target: f32 = match frame.invf[g] {
                0 => {
                    if previous == 1 { 0.6 } else { 0.0 }
                }
                1 => {
                    if previous == 0 { 0.6 } else { 0.75 }
                }
                2 => 0.9,
                _ => 0.98,
            };
            let before = channel.bw_before[g];
            let mut value = if target < before {
                0.75 * target + 0.25 * before
            } else {
                0.90625 * target + 0.09375 * before
            };
            if value < 0.015625 {
                value = 0.0;
            }
            bw[g] = value;
        }
        channel.bw_before = bw;
        channel.invf_before = frame.invf;

        // Prediction coefficients of each low band used, from the covariance over the frame.
        let mut alpha = [[0f64; 4]; LOW_BANDS];
        let mut needed = [false; LOW_BANDS];
        for i in 0..bands.patches {
            let start = bands.patch_start[i] as usize;
            for p in start..start + bands.patch_size[i] as usize {
                if p < LOW_BANDS {
                    needed[p] = true;
                }
            }
        }
        for p in 0..LOW_BANDS {
            if needed[p] {
                alpha[p] = predict(&channel.low, p);
            }
        }

        // Patches.
        let t0 = 2 * frame.t_e[0] as usize + HF_ADJ;
        let t1 = (2 * frame.t_e[frame.num_env] as usize + HF_ADJ).min(LOW_SLOTS);
        let mut k = bands.kx;
        for i in 0..bands.patches {
            let start = bands.patch_start[i] as usize;
            for x in 0..bands.patch_size[i] as usize {
                let p = start + x;
                if k >= BANDS || p >= LOW_BANDS {
                    break;
                }
                let g = (0..bands.n_noise)
                    .find(|&g| k < bands.noise[g + 1] as usize)
                    .unwrap_or(bands.n_noise - 1);
                let b = bw[g] as f64;
                let [a0r, a0i, a1r, a1i] = alpha[p];
                let to_q = |value: f64| (value * (1i64 << 28) as f64) as i64;
                let (a0r, a0i) = (to_q(a0r * b), to_q(a0i * b));
                let (a1r, a1i) = (to_q(a1r * b * b), to_q(a1i * b * b));
                for t in t0..t1 {
                    let x0 = channel.low[t * LOW_BANDS + p];
                    let value = if b == 0.0 {
                        x0
                    } else {
                        let x1 = if t >= 1 { channel.low[(t - 1) * LOW_BANDS + p] } else { (0, 0) };
                        let x2 = if t >= 2 { channel.low[(t - 2) * LOW_BANDS + p] } else { (0, 0) };
                        let re = a0r * x1.0 as i64 - a0i * x1.1 as i64 + a1r * x2.0 as i64 - a1i * x2.1 as i64;
                        let im = a0r * x1.1 as i64 + a0i * x1.0 as i64 + a1r * x2.1 as i64 + a1i * x2.0 as i64;
                        (
                            x0.0.wrapping_add(((re + (1 << 27)) >> 28) as i32),
                            x0.1.wrapping_add(((im + (1 << 27)) >> 28) as i32),
                        )
                    };
                    channel.high[t * BANDS + k] = value;
                }
                k += 1;
            }
        }

        adjust(channel, &bands, &header, &frame, element.reset);
    }

    /// One slot of the output spectrum, into `in_re` and `in_im`.
    fn gather(&mut self, channel_index: usize, slot: usize, kx: usize, m: usize) {
        let channel = &self.channels[channel_index];
        let (kx, m) = if slot < channel.carried {
            (channel.kx_carried, channel.m_carried)
        } else {
            (kx, m)
        };
        let low = &channel.low[(slot + HF_ADJ) * LOW_BANDS..][..LOW_BANDS];
        let y = &channel.y[slot * BANDS..][..BANDS];
        let top = (kx + m).min(BANDS);
        for k in 0..BANDS {
            let value = if k < kx.min(LOW_BANDS) {
                low[k]
            } else if k >= kx && k < top {
                y[k]
            } else {
                (0, 0)
            };
            self.in_re[k] = value.0;
            self.in_im[k] = value.1;
        }
    }

    fn synthesize(&mut self, channel_index: usize, out: &mut [i16], slot: usize, stride: usize, position: usize) {
        let channel = &mut self.channels[channel_index];
        synthesize_into(
            &mut self.transform,
            &mut self.in_re,
            &mut self.in_im,
            &mut self.out_a,
            &mut self.out_b,
            &mut channel.synthesis,
            &mut channel.at,
            self.synthesis_bands,
            out,
            slot,
            stride,
            position,
        );
    }

    /// Keeps what the next frame needs from `y`.
    fn finish(&mut self, channel_index: usize, kx: usize, m: usize) {
        let channel = &mut self.channels[channel_index];
        let end = if channel.frame.num_env > 0 {
            2 * channel.frame.t_e[channel.frame.num_env] as usize
        } else {
            0
        };
        let carried = end.saturating_sub(SLOTS).min(HIGH_SLOTS - SLOTS);
        channel.y.copy_within(SLOTS * BANDS..(SLOTS + carried) * BANDS, 0);
        channel.carried = carried;
        channel.kx_carried = kx;
        channel.m_carried = m;
    }
}

/// One slot through the synthesis bank: the transform, then the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn synthesize_into(
    transform: &mut Imdct,
    in_re: &mut [i32],
    in_im: &mut [i32],
    out_a: &mut [i32],
    out_b: &mut [i32],
    buffer: &mut [i32],
    at: &mut usize,
    bands: usize,
    out: &mut [i16],
    slot: usize,
    stride: usize,
    position: usize,
) {
    // The kernel's cosine part is a DCT-IV of the real parts, and its sine part a DST-IV of
    // the imaginary ones, which is a DCT-IV of them reversed with every other result negated.
    let n = 2 * bands;
    transform.run(&in_re[..bands], &mut out_a[..n]);
    in_im[..bands].reverse();
    transform.run(&in_im[..bands], &mut out_b[..n]);
    let block = 2 * bands;
    if *at < block {
        // Back to the end of the buffer, where the filter's memory goes.
        let keep = 10 * block;
        let len = buffer.len();
        buffer.copy_within(0..keep - block, len - keep + block);
        *at = len - keep + block;
    }
    *at -= block;
    let v = &mut buffer[*at..];
    let three = 3 * bands / 2 - 1;
    for l in 0..bands {
        let a = out_a[three - l].wrapping_neg();
        let b = out_b[three - l].wrapping_neg();
        let b = if l & 1 != 0 { b.wrapping_neg() } else { b };
        // l counts down from the top of the block, which runs from 2*bands - 1 to 0.
        v[block - 1 - l] = a.wrapping_add(b);
        v[l] = b.wrapping_sub(a);
    }
    // Twice as far between samples of the window when output is at half the rate.
    let step = 64 / bands;
    let start = slot * bands;
    for j in 0..bands {
        let mut total = 0i64;
        for i in 0..5 {
            total += v[2 * block * i + j] as i64 * QMF_WINDOW[(2 * bands * i + j) * step] as i64;
            total += v[2 * block * i + 3 * bands + j] as i64 * QMF_WINDOW[(2 * bands * i + bands + j) * step] as i64;
        }
        // Q6 and the kernel's missing 1/64 make Q12.
        let value = ((total + (1i64 << (31 + 12 - 1))) >> (31 + 12)).clamp(i16::MIN as i64, i16::MAX as i64);
        out[(start + j) * stride + position] = value as i16;
    }
}

/// Copies what `sbr_grid` sets, leaving a channel's own memory of its last frame.
fn copy_grid(from: &Frame, to: &mut Frame) {
    to.class = from.class;
    to.amp_res = from.amp_res;
    to.num_env = from.num_env;
    to.t_e = from.t_e;
    to.freq_res = from.freq_res;
    to.pointer = from.pointer;
    to.num_noise = from.num_noise;
    to.t_q = from.t_q;
    to.l_a = from.l_a;
    to.l_a_previous = from.l_a_previous;
}

/// The second order predictor of a low band, as `[a0 re, a0 im, a1 re, a1 im]`.
fn predict(low: &[Cx], p: usize) -> [f64; 4] {
    let mut largest = 0u32;
    for t in 0..LOW_SLOTS {
        let (re, im) = low[t * LOW_BANDS + p];
        largest = largest.max(re.unsigned_abs()).max(im.unsigned_abs());
    }
    if largest == 0 {
        return [0.0; 4];
    }
    // Enough that the sums of 40 products of two fit an i64.
    let shift = (32 - largest.leading_zeros()).saturating_sub(28);
    let at = |t: usize| {
        let (re, im) = low[t * LOW_BANDS + p];
        ((re >> shift) as i64, (im >> shift) as i64)
    };
    // phi(i, j) is the sum over t from 2 to 39 of x[t - i] conj(x[t - j]).
    let (mut r01, mut i01, mut r02, mut i02, mut e1) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut x1 = at(1);
    let mut x2 = at(0);
    for t in 2..LOW_SLOTS {
        let x0 = at(t);
        r01 += x0.0 * x1.0 + x0.1 * x1.1;
        i01 += x0.1 * x1.0 - x0.0 * x1.1;
        r02 += x0.0 * x2.0 + x0.1 * x2.1;
        i02 += x0.1 * x2.0 - x0.0 * x2.1;
        e1 += x1.0 * x1.0 + x1.1 * x1.1;
        x2 = x1;
        x1 = x0;
    }
    let first = at(0);
    let second = at(1);
    let last = at(LOW_SLOTS - 1);
    let before_last = at(LOW_SLOTS - 2);
    // phi(1, 2) and phi(2, 2) are the same sums a slot earlier.
    let r12 = r01 - (last.0 * before_last.0 + last.1 * before_last.1) + (second.0 * first.0 + second.1 * first.1);
    let i12 = i01 - (last.1 * before_last.0 - last.0 * before_last.1) + (second.1 * first.0 - second.0 * first.1);
    let e2 = e1 - (before_last.0 * before_last.0 + before_last.1 * before_last.1) + (first.0 * first.0 + first.1 * first.1);

    let (r01, i01, r02, i02, r12, i12, e1, e2) = (
        r01 as f64, i01 as f64, r02 as f64, i02 as f64, r12 as f64, i12 as f64, e1 as f64, e2 as f64,
    );
    let d = e2 * e1 - (r12 * r12 + i12 * i12) / (1.0 + 1e-6);
    let (mut a1r, mut a1i) = (0.0, 0.0);
    if d != 0.0 {
        a1r = (r01 * r12 - i01 * i12 - r02 * e1) / d;
        a1i = (r01 * i12 + i01 * r12 - i02 * e1) / d;
    }
    let (mut a0r, mut a0i) = (0.0, 0.0);
    if e1 != 0.0 {
        a0r = -(r01 + a1r * r12 + a1i * i12) / e1;
        a0i = -(i01 + a1i * r12 - a1r * i12) / e1;
    }
    if a0r * a0r + a0i * a0i >= 16.0 || a1r * a1r + a1i * a1i >= 16.0 {
        return [0.0; 4];
    }
    [a0r, a0i, a1r, a1i]
}

/// Scales the patches to the energies the stream asked for: the mapping, the gains and
/// their limits, and the assembly, envelope by envelope.
fn adjust(channel: &mut Channel, bands: &BandTables, header: &Header, frame: &Frame, reset: bool) {
    const LIMITS: [f32; 4] = [0.70795, 1.0, 1.41254, 1e10];
    let (kx, m) = (bands.kx, bands.m);
    let scale = 1.0 / (1u64 << (2 * Q)) as f32;
    let mut sine_now = channel.sine_before;
    let mut first_slot = reset;
    for l in 0..frame.num_env {
        let (table, n) = bands.table(frame.freq_res[l]);
        let (s0, s1) = (2 * frame.t_e[l] as usize, 2 * frame.t_e[l + 1] as usize);
        let transient = l as i8 == frame.l_a || l as i8 == frame.l_a_previous;
        let noise_env = usize::from(frame.num_noise > 1 && frame.t_e[l] >= frame.t_q[1]);

        let mut e_orig = [0f32; BANDS];
        let mut q_m = [0f32; BANDS];
        let mut e_curr = [0f32; BANDS];
        let mut s_index = [false; BANDS];
        let mut s_band = [false; BANDS];
        for i in 0..n {
            for k in table[i] as usize..table[i + 1] as usize {
                if k >= kx && k - kx < BANDS {
                    e_orig[k - kx] = channel.e_orig[l][i];
                }
            }
        }
        for i in 0..bands.n_noise {
            for k in bands.noise[i] as usize..bands.noise[i + 1] as usize {
                if k >= kx && k - kx < BANDS {
                    q_m[k - kx] = channel.q_orig[noise_env][i];
                }
            }
        }
        for i in 0..bands.n_high {
            let middle = (bands.high[i] as usize + bands.high[i + 1] as usize) >> 1;
            if middle >= kx && middle - kx < BANDS {
                let before = channel.sine_before[middle - kx];
                s_index[middle - kx] = frame.harmonic[i] && ((l as i8) >= frame.l_a || before);
            }
        }
        for i in 0..n {
            let (low, high) = (table[i] as usize, table[i + 1] as usize);
            let any = (low..high).any(|k| k >= kx && k - kx < BANDS && s_index[k - kx]);
            for k in low..high {
                if k >= kx && k - kx < BANDS {
                    s_band[k - kx] = any;
                }
            }
        }

        // The energies there are now.
        let t0 = s0 + HF_ADJ;
        let t1 = (s1 + HF_ADJ).min(LOW_SLOTS);
        let span = (t1.saturating_sub(t0)).max(1) as f32;
        let energy = |high: &[Cx], k: usize| -> f32 {
            let mut sum = 0u64;
            for t in t0..t1 {
                let (re, im) = high[t * BANDS + k];
                let (re, im) = (re as i64, im as i64);
                sum = sum.wrapping_add((re * re + im * im) as u64 >> 4);
            }
            sum as f32 * 16.0 * scale
        };
        if header.interpol_freq {
            for k in 0..m {
                e_curr[k] = energy(&channel.high, kx + k) / span;
            }
        } else {
            for i in 0..n {
                let (low, high) = (table[i] as usize, table[i + 1] as usize);
                let mut sum = 0f32;
                for k in low..high {
                    sum += energy(&channel.high, k);
                }
                let mean = sum / (span * (high - low).max(1) as f32);
                for k in low..high {
                    if k >= kx && k - kx < BANDS {
                        e_curr[k - kx] = mean;
                    }
                }
            }
        }

        // Gains.
        let mut gain = [0f32; BANDS];
        let mut noise = [0f32; BANDS];
        let mut sine = [0f32; BANDS];
        for k in 0..m {
            let (e, q) = (e_orig[k], q_m[k]);
            let curr = 1.0 + e_curr[k];
            if s_band[k] {
                gain[k] = sqrt(e * q / (curr * (1.0 + q)));
            } else if transient {
                gain[k] = sqrt(e / curr);
            } else {
                gain[k] = sqrt(e / (curr * (1.0 + q)));
            }
            noise[k] = sqrt(e * q / (1.0 + q));
            sine[k] = if s_index[k] { sqrt(e / (1.0 + q)) } else { 0.0 };
        }
        // Limits and compensation, band by band.
        let limit_gain = LIMITS[header.limiter_gains as usize & 3];
        for band in 0..bands.n_limit {
            let low = (bands.limit[band] as usize).saturating_sub(kx).min(m);
            let high = (bands.limit[band + 1] as usize).saturating_sub(kx).min(m);
            let (mut sum_orig, mut sum_curr) = (0f32, 0f32);
            for k in low..high {
                sum_orig += e_orig[k];
                sum_curr += e_curr[k];
            }
            let most = (limit_gain * sqrt((1e-12 + sum_orig) / (1e-12 + sum_curr))).min(1e5);
            for k in low..high {
                if gain[k] > most {
                    noise[k] *= most / gain[k];
                    gain[k] = most;
                }
            }
            let mut sum = 0f32;
            for k in low..high {
                sum += e_curr[k] * gain[k] * gain[k] + sine[k] * sine[k];
                if !transient && sine[k] == 0.0 {
                    sum += noise[k] * noise[k];
                }
            }
            let boost = sqrt((1e-12 + sum_orig) / (1e-12 + sum)).min(1.584_893_2);
            for k in low..high {
                gain[k] *= boost;
                noise[k] *= boost;
                sine[k] *= boost;
            }
        }

        let mut gain_q = [0i64; BANDS];
        let mut noise_q = [0i32; BANDS];
        let mut sine_q = [0i32; BANDS];
        for k in 0..m {
            gain_q[k] = (gain[k] * (1u64 << GAIN_Q) as f32) as i64;
            noise_q[k] = (noise[k] * (1u32 << Q) as f32) as i32;
            sine_q[k] = (sine[k] * (1u32 << Q) as f32) as i32;
        }
        for k in 0..BANDS {
            if k < m {
                sine_now[k] = s_index[k];
            }
        }

        // Slot by slot, smoothing the gains over the last few unless this follows a transient.
        let smooth = !header.smoothing;
        for slot in s0..s1.min(HIGH_SLOTS) {
            channel.history_at = (channel.history_at + 1) % 5;
            channel.gain_history[channel.history_at][..m].copy_from_slice(&gain_q[..m]);
            channel.noise_history[channel.history_at][..m].copy_from_slice(&noise_q[..m]);
            if first_slot {
                for age in 0..5 {
                    channel.gain_history[age][..m].copy_from_slice(&gain_q[..m]);
                    channel.noise_history[age][..m].copy_from_slice(&noise_q[..m]);
                }
                first_slot = false;
            }
            let t = slot + HF_ADJ;
            let row = t.min(LOW_SLOTS - 1) * BANDS + kx;
            let filtered = smooth && !transient;
            let sign_odd = kx & 1 != 0;
            for k in 0..m {
                let (g, q) = if filtered {
                    let (mut g, mut q) = (0i64, 0i64);
                    for (age, &weight) in SMOOTH.iter().enumerate() {
                        let from = (channel.history_at + 5 - age) % 5;
                        g += channel.gain_history[from][k].wrapping_mul(weight);
                        q += channel.noise_history[from][k] as i64 * weight;
                    }
                    (g >> 15, (q >> 15) as i32)
                } else {
                    (gain_q[k], noise_q[k])
                };
                let (re, im) = channel.high[row + k];
                let mut y = (
                    ((re as i64).wrapping_mul(g) >> GAIN_Q) as i32,
                    ((im as i64).wrapping_mul(g) >> GAIN_Q) as i32,
                );
                let index = (channel.index_noise + k + 1) & 511;
                if sine_q[k] != 0 {
                    let s = sine_q[k];
                    let odd = sign_odd ^ (k & 1 != 0);
                    match channel.index_sine {
                        0 => y.0 = y.0.wrapping_add(s),
                        1 => y.1 = y.1.wrapping_add(if odd { -s } else { s }),
                        2 => y.0 = y.0.wrapping_sub(s),
                        _ => y.1 = y.1.wrapping_add(if odd { s } else { -s }),
                    }
                } else if !transient {
                    let (nr, ni) = (NOISE[2 * index], NOISE[2 * index + 1]);
                    y.0 = y.0.wrapping_add(((q as i64 * nr as i64) >> 31) as i32);
                    y.1 = y.1.wrapping_add(((q as i64 * ni as i64) >> 31) as i32);
                }
                channel.y[slot * BANDS + kx + k] = y;
            }
            channel.index_noise = (channel.index_noise + m) & 511;
            channel.index_sine = (channel.index_sine + 1) & 3;
        }
    }
    channel.sine_before = sine_now;
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fft_matches_the_sum() {
        let fft = Fft64::new();
        let mut input = [(0, 0); 64];
        let mut seed = 0x1234_5678u32;
        for slot in input.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let re = (seed >> 8) as i32 - (1 << 23);
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let im = (seed >> 8) as i32 - (1 << 23);
            *slot = (re / 8, im / 8);
        }
        let mut out = [(0, 0); 64];
        fft.run(&input, &mut out);
        for (k, &(re, im)) in out.iter().enumerate() {
            let (mut sum_re, mut sum_im) = (0f64, 0f64);
            for (n, &(a, b)) in input.iter().enumerate() {
                let phase = (((k * n) % 64) as u64) << 58;
                let c = cos_turn(phase) as f64 / (1u64 << 62) as f64;
                let s = sin_turn(phase) as f64 / (1u64 << 62) as f64;
                sum_re += a as f64 * c - b as f64 * s;
                sum_im += a as f64 * s + b as f64 * c;
            }
            assert!((sum_re - re as f64).abs() < 32.0 && (sum_im - im as f64).abs() < 32.0, "{k}");
        }
    }

    #[test]
    fn logs_and_powers() {
        assert!((log2(8.0) - 3.0).abs() < 1e-12);
        assert!((exp2(log2(3.5)) - 3.5).abs() < 1e-10);
        assert_eq!(pow2_half(4), 4.0);
        assert!((sqrt(2.0) - core::f32::consts::SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn hostile_headers_give_tables_in_bounds_or_none() {
        for row in 0..12usize {
            for start in 0..16 {
                for stop in 0..16 {
                    for xover in 0..8 {
                        for scale in 0..4 {
                            for alter in [false, true] {
                                for noise in 0..4 {
                                    for limiter in 0..4 {
                                        let header = Header {
                                            start_freq: start,
                                            stop_freq: stop,
                                            xover,
                                            freq_scale: scale,
                                            alter_scale: alter,
                                            noise_bands: noise,
                                            limiter_bands: limiter,
                                            ..Header::default()
                                        };
                                        if let Some(b) = BandTables::new(&header, row, RATE_ROWS[row]) {
                                            assert!(b.kx + b.m <= 64 && b.kx <= LOW_BANDS);
                                            assert!(b.n_noise >= 1 && b.n_low >= 1);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
