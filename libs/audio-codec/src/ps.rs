//! Parametric stereo, the "v2" of HE-AAC v2, from ISO/IEC 14496-3 8.6.4 (and with the layout of
//! bands that libxaac's implementation of it, Apache-2.0, uses). A mono signal and a little
//! side data about level differences and correlation become two channels.
//!
//! The lowest QMF bands are split finer by a hybrid filter bank. A decorrelated copy of the
//! signal is made by all-pass filters and delays, and each band of the two is mixed by a matrix
//! the side data picks, moved smoothly from one to the next.
//!
//! All of it is integers but the phase parameters' smoothing: subband samples as in `sbr.rs`,
//! matrices in Q28.

use alloc::vec::Vec;

use crate::aac::read_code;
use crate::aac_tables::Code;
use crate::bits::{try_vec, MsbReader};
use crate::error::{AudioError, Result};
use crate::mdct::cmul;
use crate::sbr::{sqrt, Cx, BANDS, SLOTS};
use crate::sbr_tables::{
    BINS_20, BINS_34, BORDERS_20, BORDERS_34, FRAC_QMF_IM, FRAC_QMF_RE, FRAC_SUB_IM_20,
    FRAC_SUB_IM_34, FRAC_SUB_RE_20, FRAC_SUB_RE_34, ICC_DF, ICC_DT, IID_DF, IID_DF_FINE, IID_DT,
    IID_DT_FINE, IPD_DF, IPD_DT, LINK_DECAY, MIX_A, MIX_A_FINE, MIX_B, MIX_B_FINE, MOD_12, MOD_2, MOD_4, MOD_8, OPD_DF, OPD_DT,
    P12_34, P2_20, P4_34, P8_20, P8_34, SER_QMF_IM, SER_QMF_RE, SER_SUB_IM_20, SER_SUB_IM_34,
    SER_SUB_RE_20, SER_SUB_RE_34,
};

const MAX_ENV: usize = 5;
const MAX_BINS: usize = 34;
/// Bins phases can apply to.
const PHASE_BINS: usize = 17;
const FILTER: usize = 13;
/// Slots of a hybrid band's input: six before the frame, and six past its end.
const WINDOW: usize = 6 + SLOTS + 6;
/// Bands the hybrid filters read.
const HYBRID_INPUTS: usize = 5;
const MAX_HYBRID: usize = 32;
/// QMF bands from here on are delayed, not all-pass filtered.
const ALL_PASS_BANDS: usize = 23;
/// Bands up to here get the long delay, and above it the short one.
const LONG_DELAY_BANDS: usize = 35;
const LONG_DELAY: usize = 14;
const LINKS: usize = 3;
const LINK_LENGTHS: [usize; LINKS] = [3, 4, 5];
const LINK_MAX: usize = 5;
/// Peak decay, in Q31.
const PEAK_DECAY: i64 = 1_644_818_113;

#[derive(Clone, Copy)]
struct Params {
    enable_iid: bool,
    enable_icc: bool,
    iid_mode: u8,
    icc_mode: u8,
    fine: bool,
    mix_b: bool,
    num_env: usize,
    border: [u8; MAX_ENV + 2],
    iid: [[i8; MAX_BINS]; MAX_ENV + 1],
    icc: [[i8; MAX_BINS]; MAX_ENV + 1],
    iid_before: [i8; MAX_BINS],
    icc_before: [i8; MAX_BINS],
    fresh: bool,
    enable_phase: bool,
    ipd: [[u8; PHASE_BINS]; MAX_ENV + 1],
    opd: [[u8; PHASE_BINS]; MAX_ENV + 1],
    ipd_before: [u8; PHASE_BINS],
    opd_before: [u8; PHASE_BINS],
}

/// Phases by index: eighths of a turn.
const PHASES: [(f32, f32); 8] = [
    (1.0, 0.0),
    (core::f32::consts::FRAC_1_SQRT_2, core::f32::consts::FRAC_1_SQRT_2),
    (0.0, 1.0),
    (-core::f32::consts::FRAC_1_SQRT_2, core::f32::consts::FRAC_1_SQRT_2),
    (-1.0, 0.0),
    (-core::f32::consts::FRAC_1_SQRT_2, -core::f32::consts::FRAC_1_SQRT_2),
    (0.0, -1.0),
    (core::f32::consts::FRAC_1_SQRT_2, -core::f32::consts::FRAC_1_SQRT_2),
];

/// The second half of the mapping from 20 bands to 34, in place.
fn map_20_to_34(p: &mut [i8; MAX_BINS]) {
    let f = *p;
    let mean = |a: i8, b: i8| ((a as i32 + b as i32) / 2) as i8;
    *p = [
        f[0],
        mean(f[0], f[1]),
        f[1],
        f[2],
        mean(f[2], f[3]),
        f[3],
        f[4],
        f[4],
        f[5],
        f[5],
        f[6],
        f[7],
        f[8],
        f[8],
        f[9],
        f[9],
        f[10],
        f[11],
        f[12],
        f[13],
        f[14],
        f[14],
        f[15],
        f[15],
        f[16],
        f[16],
        f[17],
        f[17],
        f[18],
        f[18],
        f[18],
        f[18],
        f[19],
        f[19],
    ];
}

fn map_h_20_to_34(p: &mut [[i32; 4]; MAX_BINS]) {
    let f = *p;
    let mean = |a: [i32; 4], b: [i32; 4]| {
        [(a[0] >> 1) + (b[0] >> 1), (a[1] >> 1) + (b[1] >> 1), (a[2] >> 1) + (b[2] >> 1), (a[3] >> 1) + (b[3] >> 1)]
    };
    *p = [
        f[0], mean(f[0], f[1]), f[1], f[2], mean(f[2], f[3]), f[3], f[4], f[4], f[5], f[5], f[6], f[7], f[8], f[8],
        f[9], f[9], f[10], f[11], f[12], f[13], f[14], f[14], f[15], f[15], f[16], f[16], f[17], f[17], f[18], f[18],
        f[18], f[18], f[19], f[19],
    ];
}

fn map_h_34_to_20(p: &mut [[i32; 4]; MAX_BINS]) {
    let f = *p;
    let mix = |parts: &[(i32, [i32; 4])], div: i32| {
        let mut out = [0i32; 4];
        for (slot, value) in out.iter_mut().enumerate() {
            let total: i64 = parts.iter().map(|(w, a)| *w as i64 * a[slot] as i64).sum();
            *value = (total / div as i64) as i32;
        }
        out
    };
    let two = |a: [i32; 4], b: [i32; 4]| mix(&[(1, a), (1, b)], 2);
    p[0] = mix(&[(2, f[0]), (1, f[1])], 3);
    p[1] = mix(&[(1, f[1]), (2, f[2])], 3);
    p[2] = mix(&[(2, f[3]), (1, f[4])], 3);
    p[3] = mix(&[(1, f[4]), (2, f[5])], 3);
    p[4] = two(f[6], f[7]);
    p[5] = two(f[8], f[9]);
    p[6] = f[10];
    p[7] = f[11];
    p[8] = two(f[12], f[13]);
    p[9] = two(f[14], f[15]);
    p[10] = f[16];
    p[11] = f[17];
    p[12] = f[18];
    p[13] = f[19];
    p[14] = two(f[20], f[21]);
    p[15] = two(f[22], f[23]);
    p[16] = two(f[24], f[25]);
    p[17] = two(f[26], f[27]);
    p[18] = mix(&[(1, f[28]), (1, f[29]), (1, f[30]), (1, f[31])], 4);
    p[19] = two(f[32], f[33]);
}

/// Parametric stereo's state for a mono channel.
pub(crate) struct Ps {
    params: Params,
    /// Whether any parametric stereo data has come.
    started: bool,
    use_34: bool,
    next_34: bool,
    /// The header's word on whether frames have an extension.
    extension: bool,
    /// What each hybrid filter sees, one band after another.
    window: Vec<Cx>,
    /// A filter's coefficients: bands, then taps, for each resolution.
    filter_8_20: Vec<Cx>,
    filter_2_20: Vec<Cx>,
    filter_12_34: Vec<Cx>,
    filter_8_34: Vec<Cx>,
    filter_4_34: Vec<Cx>,
    /// Hybrid bands' delay lines, and QMF bands'.
    sub_delay: Vec<Cx>,
    sub_links: Vec<Cx>,
    qmf_delay: Vec<Cx>,
    qmf_links: Vec<Cx>,
    delay_at: usize,
    long_at: usize,
    link_at: [usize; LINKS],
    peak: [i64; MAX_BINS],
    peak_difference: [i64; MAX_BINS],
    energy: [i64; MAX_BINS],
    h: [[i32; 4]; MAX_BINS],
    h_delta: [[i32; 4]; MAX_BINS],
    /// The matrices' imaginary parts, while phases are in use.
    h_im: [[i32; 4]; MAX_BINS],
    h_im_delta: [[i32; 4]; MAX_BINS],
    complex: bool,
    /// The last two envelopes' phases, which each one's are smoothed with.
    ipd_history: [[u8; PHASE_BINS]; 2],
    opd_history: [[u8; PHASE_BINS]; 2],
    env: usize,
    top: usize,
    /// The second synthesis bank.
    bank: Vec<i32>,
    bank_at: usize,
    decay: [[i32; LINKS]; BANDS],
}

fn make_filter(proto: &[i32; FILTER], modulation: &[i32], bands: usize, complex: bool) -> Result<Vec<Cx>> {
    let mut out = try_vec(bands * FILTER, (0, 0))?;
    for q in 0..bands {
        for n in 0..FILTER {
            let (c, s) = if complex {
                (modulation[(q * FILTER + n) * 2], modulation[(q * FILTER + n) * 2 + 1])
            } else {
                (modulation[q * FILTER + n], 0)
            };
            let scale = |v: i32| ((proto[n] as i64 * v as i64 + (1 << 30)) >> 31) as i32;
            out[q * FILTER + n] = (scale(c), scale(s));
        }
    }
    Ok(out)
}

impl Ps {
    pub(crate) fn new() -> Result<Self> {
        let bank_len = (10 * 2 + 2 * SLOTS) * BANDS;
        Ok(Self {
            params: Params {
                enable_iid: false,
                enable_icc: false,
                iid_mode: 0,
                icc_mode: 0,
                fine: false,
                mix_b: false,
                num_env: 1,
                border: [0, SLOTS as u8, 0, 0, 0, 0, 0],
                iid: [[0; MAX_BINS]; MAX_ENV + 1],
                icc: [[0; MAX_BINS]; MAX_ENV + 1],
                iid_before: [0; MAX_BINS],
                icc_before: [0; MAX_BINS],
                fresh: false,
                enable_phase: false,
                ipd: [[0; PHASE_BINS]; MAX_ENV + 1],
                opd: [[0; PHASE_BINS]; MAX_ENV + 1],
                ipd_before: [0; PHASE_BINS],
                opd_before: [0; PHASE_BINS],
            },
            started: false,
            use_34: false,
            next_34: false,
            extension: false,
            window: try_vec(HYBRID_INPUTS * WINDOW, (0, 0))?,
            filter_8_20: make_filter(&P8_20, &MOD_8, 8, true)?,
            filter_2_20: make_filter(&P2_20, &MOD_2, 2, false)?,
            filter_12_34: make_filter(&P12_34, &MOD_12, 12, true)?,
            filter_8_34: make_filter(&P8_34, &MOD_8, 8, true)?,
            filter_4_34: make_filter(&P4_34, &MOD_4, 4, true)?,
            sub_delay: try_vec(2 * MAX_HYBRID, (0, 0))?,
            sub_links: try_vec(LINKS * LINK_MAX * MAX_HYBRID, (0, 0))?,
            qmf_delay: try_vec(LONG_DELAY * BANDS, (0, 0))?,
            qmf_links: try_vec(LINKS * LINK_MAX * BANDS, (0, 0))?,
            delay_at: 0,
            long_at: 0,
            link_at: [0; LINKS],
            peak: [0; MAX_BINS],
            peak_difference: [0; MAX_BINS],
            energy: [0; MAX_BINS],
            h: [[0; 4]; MAX_BINS],
            h_delta: [[0; 4]; MAX_BINS],
            h_im: [[0; 4]; MAX_BINS],
            h_im_delta: [[0; 4]; MAX_BINS],
            complex: false,
            ipd_history: [[0; PHASE_BINS]; 2],
            opd_history: [[0; PHASE_BINS]; 2],
            env: 0,
            top: BANDS,
            bank: try_vec(bank_len, 0)?,
            bank_at: bank_len - 9 * 2 * BANDS,
            decay: [[0; LINKS]; BANDS],
        })
    }

    pub(crate) fn reset(&mut self) {
        self.started = false;
        self.params.fresh = false;
        self.params.iid_before = [0; MAX_BINS];
        self.params.icc_before = [0; MAX_BINS];
        self.window.fill((0, 0));
        self.clear_delays();
        self.peak = [0; MAX_BINS];
        self.peak_difference = [0; MAX_BINS];
        self.energy = [0; MAX_BINS];
        self.h = [[0; 4]; MAX_BINS];
        self.h_im = [[0; 4]; MAX_BINS];
        self.complex = false;
        self.ipd_history = [[0; PHASE_BINS]; 2];
        self.opd_history = [[0; PHASE_BINS]; 2];
        self.params.ipd_before = [0; PHASE_BINS];
        self.params.opd_before = [0; PHASE_BINS];
        self.bank.fill(0);
    }

    fn clear_delays(&mut self) {
        self.sub_delay.fill((0, 0));
        self.sub_links.fill((0, 0));
        self.qmf_delay.fill((0, 0));
        self.qmf_links.fill((0, 0));
    }

    pub(crate) fn started(&self) -> bool {
        self.started
    }

    /// Reads `ps_data`. Nothing changes if it turns out to be damaged.
    pub(crate) fn read(&mut self, reader: &mut MsbReader<'_>, _bits: usize) -> Result<()> {
        let mut p = self.params;
        let mut extension = self.extension;
        if reader.bit()? {
            p.enable_iid = reader.bit()?;
            if p.enable_iid {
                let mode = reader.read(3)? as u8;
                if mode > 5 {
                    return Err(AudioError::InvalidData("reserved parametric stereo mode"));
                }
                p.fine = mode > 2;
                p.iid_mode = mode % 3;
            }
            p.enable_icc = reader.bit()?;
            if p.enable_icc {
                let mode = reader.read(3)? as u8;
                if mode > 5 {
                    return Err(AudioError::InvalidData("reserved parametric stereo mode"));
                }
                p.mix_b = mode > 2;
                p.icc_mode = mode % 3;
            }
            extension = reader.bit()?;
        }
        let class = reader.bit()?;
        let index = reader.read(2)? as usize;
        let coded = if class { index + 1 } else { [0, 1, 2, 4][index] };
        let mut border = [0u8; MAX_ENV + 2];
        if class {
            for slot in border[1..=coded].iter_mut() {
                *slot = reader.read(5)? as u8 + 1;
            }
        }
        if p.enable_iid {
            let (dt, df): (&[Code], &[Code]) = if p.fine {
                (&IID_DT_FINE, &IID_DF_FINE)
            } else {
                (&IID_DT, &IID_DF)
            };
            let limit = if p.fine { 15 } else { 7 };
            let before = p.iid_before;
            read_parameters(reader, &mut p.iid, &before, coded, p.iid_mode, dt, df, -limit, limit)?;
        } else {
            p.iid = [[0; MAX_BINS]; MAX_ENV + 1];
        }
        if p.enable_icc {
            let before = p.icc_before;
            read_parameters(reader, &mut p.icc, &before, coded, p.icc_mode, &ICC_DT, &ICC_DF, 0, 7)?;
        } else {
            p.icc = [[0; MAX_BINS]; MAX_ENV + 1];
        }
        if extension {
            let mut count = reader.read(4)? as usize;
            if count == 15 {
                count += reader.read(8)? as usize;
            }
            let mut left = 8 * count;
            p.enable_phase = false;
            while left > 7 {
                let start = reader.bits_left();
                if reader.read(2)? == 0 {
                    p.enable_phase = reader.bit()?;
                    if p.enable_phase {
                        let (ipd_before, opd_before) = (p.ipd_before, p.opd_before);
                        for e in 0..coded {
                            read_phases(reader, &mut p.ipd, &ipd_before, e, p.iid_mode, &IPD_DT, &IPD_DF)?;
                            read_phases(reader, &mut p.opd, &opd_before, e, p.iid_mode, &OPD_DT, &OPD_DF)?;
                        }
                    }
                    reader.bit()?;
                    let used = start - reader.bits_left();
                    if used > left {
                        return Err(AudioError::InvalidData("parametric stereo extension overruns"));
                    }
                    left -= used;
                } else {
                    let used = start - reader.bits_left();
                    reader.skip_long(left - used)?;
                    left = 0;
                }
            }
            reader.skip_long(left)?;
        } else {
            p.enable_phase = false;
        }
        if !p.enable_phase {
            p.ipd = [[0; PHASE_BINS]; MAX_ENV + 1];
            p.opd = [[0; PHASE_BINS]; MAX_ENV + 1];
        }

        let mut envelopes = coded;
        if envelopes == 0 {
            envelopes = 1;
            p.iid[0] = if p.enable_iid { p.iid_before } else { [0; MAX_BINS] };
            p.icc[0] = if p.enable_icc { p.icc_before } else { [0; MAX_BINS] };
            p.ipd[0] = p.ipd_before;
            p.opd[0] = p.opd_before;
        }
        p.iid_before = p.iid[envelopes - 1];
        p.icc_before = p.icc[envelopes - 1];
        p.ipd_before = p.ipd[envelopes - 1];
        p.opd_before = p.opd[envelopes - 1];

        let slots = SLOTS as i32;
        if class {
            border[0] = 0;
            if border[envelopes] as i32 > slots {
                return Err(AudioError::InvalidData("parametric stereo border past the frame"));
            }
            if (border[envelopes] as i32) < slots {
                envelopes += 1;
                border[envelopes] = SLOTS as u8;
                p.iid[envelopes - 1] = p.iid[envelopes - 2];
                p.icc[envelopes - 1] = p.icc[envelopes - 2];
                p.ipd[envelopes - 1] = p.ipd[envelopes - 2];
                p.opd[envelopes - 1] = p.opd[envelopes - 2];
            }
            for e in 1..envelopes {
                let most = slots - (envelopes - e) as i32;
                let least = border[e - 1] as i32 + 1;
                border[e] = (border[e] as i32).min(most).max(least) as u8;
            }
        } else {
            for e in 0..=envelopes {
                border[e] = (e * SLOTS / envelopes) as u8;
            }
        }
        p.num_env = envelopes;
        p.border = border;
        // Everything in the frame is on one of the two grids.
        let use_34 = (p.enable_iid && p.iid_mode == 2) || (p.enable_icc && p.icc_mode == 2);
        if use_34 {
            for e in 0..envelopes {
                if p.iid_mode != 2 || !p.enable_iid {
                    map_20_to_34(&mut p.iid[e]);
                }
                if p.icc_mode != 2 || !p.enable_icc {
                    map_20_to_34(&mut p.icc[e]);
                }
            }
        }
        self.extension = extension;
        self.next_34 = use_34;
        p.fresh = true;
        self.params = p;
        self.started = true;
        Ok(())
    }

    /// What's needed before a frame's slots: this frame's input to the hybrid filters, its
    /// parameters, and where they apply.
    pub(crate) fn begin_frame(&mut self, low: &[Cx], low_bands: usize, adjust: usize, kx: usize, top: usize) {
        for band in 0..HYBRID_INPUTS {
            let line = &mut self.window[band * WINDOW..(band + 1) * WINDOW];
            line.copy_within(SLOTS.., 0);
            for slot in 0..SLOTS + 6 {
                line[6 + slot] = if band < kx {
                    low[(slot + adjust) * low_bands + band]
                } else {
                    (0, 0)
                };
            }
        }
        if !self.params.fresh {
            // Nothing new: stay at the last matrix.
            let p = &mut self.params;
            let last = p.num_env.saturating_sub(1);
            p.iid[0] = p.iid[last];
            p.icc[0] = p.icc[last];
            p.ipd[0] = p.ipd[last];
            p.opd[0] = p.opd[last];
            p.num_env = 1;
            p.border = [0, SLOTS as u8, 0, 0, 0, 0, 0];
        }
        if self.next_34 != self.use_34 {
            self.use_34 = self.next_34;
            if self.use_34 {
                map_h_20_to_34(&mut self.h);
            } else {
                map_h_34_to_20(&mut self.h);
            }
            self.clear_delays();
            self.peak = [0; MAX_BINS];
            self.peak_difference = [0; MAX_BINS];
            self.energy = [0; MAX_BINS];
        }
        // How the all-pass filters fade up the spectrum.
        let (cutoff, slope) = (if self.use_34 { 5 } else { 3 }, 107_374_182i64);
        for sb in 0..BANDS {
            let factor = if sb <= cutoff {
                1i64 << 31
            } else {
                ((1i64 << 31) - slope * (sb as i64 - cutoff as i64)).max(0)
            };
            for link in 0..LINKS {
                self.decay[sb][link] = ((factor * LINK_DECAY[link] as i64) >> 31) as i32;
            }
        }
        // Above the top of the signal, the delay lines have nothing in them.
        self.top = top.min(BANDS);
        for sb in self.top..BANDS {
            for depth in 0..LONG_DELAY {
                self.qmf_delay[depth * BANDS + sb] = (0, 0);
            }
            for row in 0..LINKS * LINK_MAX {
                self.qmf_links[row * BANDS + sb] = (0, 0);
            }
        }
        self.env = 0;
        self.start_envelope();
    }

    /// Picks the matrices the envelope ends on, and how far each moves per slot.
    fn start_envelope(&mut self) {
        let p = self.params;
        let bins = if self.use_34 { 34 } else { 20 };
        let length = (p.border[self.env + 1] as i32 - p.border[self.env] as i32).max(1);
        let (table, steps): (&[i32], usize) = match (p.mix_b, p.fine) {
            (false, false) => (&MIX_A, 7),
            (false, true) => (&MIX_A_FINE, 15),
            (true, false) => (&MIX_B, 7),
            (true, true) => (&MIX_B_FINE, 15),
        };
        let mut complex = false;
        for bin in 0..bins {
            let level = (p.iid[self.env][bin] as i32).clamp(-(steps as i32), steps as i32) + steps as i32;
            let correlation = (p.icc[self.env][bin] as i32).clamp(0, 7);
            let at = (level as usize * 8 + correlation as usize) * 4;
            let mut target = [table[at], table[at + 1], table[at + 2], table[at + 3]];
            let mut imaginary = [0i32; 4];
            if p.enable_phase && bin < PHASE_BINS {
                // The phases, smoothed with the last two envelopes', as unit vectors.
                let phase = |now: u8, one: u8, two: u8| {
                    let (a, b, c) = (PHASES[now as usize & 7], PHASES[one as usize & 7], PHASES[two as usize & 7]);
                    let re = a.0 + 0.5 * b.0 + 0.25 * c.0;
                    let im = a.1 + 0.5 * b.1 + 0.25 * c.1;
                    let size = sqrt(re * re + im * im);
                    if size > 0.0 { (re / size, im / size) } else { (1.0, 0.0) }
                };
                let ipd = phase(p.ipd[self.env][bin], self.ipd_history[0][bin], self.ipd_history[1][bin]);
                let opd = phase(p.opd[self.env][bin], self.opd_history[0][bin], self.opd_history[1][bin]);
                // The left channel turns by OPD, and the right by OPD less IPD.
                let left = opd;
                let right = (opd.0 * ipd.0 + opd.1 * ipd.1, opd.1 * ipd.0 - opd.0 * ipd.1);
                let turn = |value: i32, by: f32| (value as f32 * by) as i32;
                imaginary = [
                    turn(target[0], left.1),
                    turn(target[1], right.1),
                    turn(target[2], left.1),
                    turn(target[3], right.1),
                ];
                target = [
                    turn(target[0], left.0),
                    turn(target[1], right.0),
                    turn(target[2], left.0),
                    turn(target[3], right.0),
                ];
            }
            for m in 0..4 {
                self.h_delta[bin][m] = (target[m] - self.h[bin][m]) / length;
                self.h_im_delta[bin][m] = (imaginary[m] - self.h_im[bin][m]) / length;
                if imaginary[m] != 0 || self.h_im[bin][m] != 0 {
                    complex = true;
                }
            }
        }
        self.complex = complex;
        self.ipd_history[1] = self.ipd_history[0];
        self.opd_history[1] = self.opd_history[0];
        self.ipd_history[0] = p.ipd[self.env];
        self.opd_history[0] = p.opd[self.env];
    }

    /// The hybrid analysis of one slot, into `out`.
    fn hybrid(&self, slot: usize, out: &mut [Cx; MAX_HYBRID]) {
        let window = &self.window;
        let run = |band: usize, filter: &[Cx], count: usize, into: &mut [Cx], complex: bool| {
            let x = &window[band * WINDOW + slot..][..FILTER];
            for q in 0..count {
                let row = &filter[q * FILTER..][..FILTER];
                let (mut re, mut im) = (0i64, 0i64);
                for (value, &(c, s)) in x.iter().zip(row) {
                    if complex {
                        re += value.0 as i64 * c as i64 - value.1 as i64 * s as i64;
                        im += value.1 as i64 * c as i64 + value.0 as i64 * s as i64;
                    } else {
                        re += value.0 as i64 * c as i64;
                        im += value.1 as i64 * c as i64;
                    }
                }
                into[q] = (((re + (1 << 30)) >> 31) as i32, ((im + (1 << 30)) >> 31) as i32);
            }
        };
        if self.use_34 {
            run(0, &self.filter_12_34, 12, &mut out[0..], true);
            run(1, &self.filter_8_34, 8, &mut out[12..], true);
            run(2, &self.filter_4_34, 4, &mut out[20..], true);
            run(3, &self.filter_4_34, 4, &mut out[24..], true);
            run(4, &self.filter_4_34, 4, &mut out[28..], true);
        } else {
            run(0, &self.filter_8_20, 8, &mut out[0..], true);
            run(1, &self.filter_2_20, 2, &mut out[8..], false);
            run(2, &self.filter_2_20, 2, &mut out[10..], false);
            out[3] = (out[3].0.wrapping_add(out[4].0), out[3].1.wrapping_add(out[4].1));
            out[2] = (out[2].0.wrapping_add(out[5].0), out[2].1.wrapping_add(out[5].1));
            out[4] = (0, 0);
            out[5] = (0, 0);
        }
    }

    /// Takes a slot's QMF bands in `left`, and leaves the two channels' in `left` and `right`.
    pub(crate) fn process_slot(&mut self, slot: usize, left: &mut [Cx; BANDS], right: &mut [Cx; BANDS]) {
        while self.env + 1 < self.params.num_env && slot >= self.params.border[self.env + 1] as usize {
            self.env += 1;
            self.start_envelope();
        }
        let bins = if self.use_34 { 34 } else { 20 };
        for bin in 0..bins {
            for m in 0..4 {
                self.h[bin][m] = self.h[bin][m].wrapping_add(self.h_delta[bin][m]);
                self.h_im[bin][m] = self.h_im[bin][m].wrapping_add(self.h_im_delta[bin][m]);
            }
        }
        let (borders, map, sub_groups, first_qmf): (&[u8], &[u16], usize, usize) = if self.use_34 {
            (&BORDERS_34, &BINS_34, 32, 5)
        } else {
            (&BORDERS_20, &BINS_20, 10, 3)
        };

        let mut hl = [(0i32, 0i32); MAX_HYBRID];
        let mut hr = [(0i32, 0i32); MAX_HYBRID];
        self.hybrid(slot, &mut hl);
        for k in first_qmf..BANDS {
            if k >= self.top {
                left[k] = (0, 0);
            }
        }

        // How transient it is, band by band: a decaying peak against the energy.
        let mut power = [0i64; MAX_BINS];
        let square = |v: Cx| {
            let (re, im) = ((v.0 >> 4) as i64, (v.1 >> 4) as i64);
            re * re + im * im
        };
        for gr in 0..map.len() {
            let bin = (map[gr] & 255) as usize;
            if gr < sub_groups {
                power[bin] += square(hl[borders[gr] as usize]);
            } else {
                for sb in borders[gr] as usize..borders[gr + 1] as usize {
                    power[bin] += square(left[sb]);
                }
            }
        }
        let mut ratio = [1i64 << 15; MAX_BINS];
        for bin in 0..bins {
            let peak = ((self.peak[bin] as i128 * PEAK_DECAY as i128) >> 31) as i64;
            let peak = peak.max(power[bin]);
            self.peak[bin] = peak;
            let difference = self.peak_difference[bin] + ((peak - power[bin] - self.peak_difference[bin]) >> 2);
            self.peak_difference[bin] = difference;
            let energy = self.energy[bin] + ((power[bin] - self.energy[bin]) >> 2);
            self.energy[bin] = energy;
            let weighted = difference + (difference >> 1);
            if weighted > energy && weighted > 0 {
                ratio[bin] = (((energy.max(0) as i128) << 15) / weighted as i128) as i64;
            }
        }

        // The decorrelated signal.
        let (frac_re, frac_im, ser_re, ser_im): (&[i32], &[i32], &[i32], &[i32]) = if self.use_34 {
            (&FRAC_SUB_RE_34, &FRAC_SUB_IM_34, &SER_SUB_RE_34, &SER_SUB_IM_34)
        } else {
            (&FRAC_SUB_RE_20, &FRAC_SUB_IM_20, &SER_SUB_RE_20, &SER_SUB_IM_20)
        };
        for gr in 0..sub_groups {
            let sb = borders[gr] as usize;
            let bin = (map[gr] & 255) as usize;
            let input = hl[sb];
            let delayed = core::mem::replace(&mut self.sub_delay[self.delay_at * MAX_HYBRID + sb], input);
            let mut value = cmul(delayed.0, delayed.1, frac_re[sb], frac_im[sb]);
            for link in 0..LINKS {
                let store = &mut self.sub_links[(link * LINK_MAX + self.link_at[link]) * MAX_HYBRID + sb];
                value = all_pass(store, value, ser_re[sb * LINKS + link], ser_im[sb * LINKS + link], LINK_DECAY[link]);
            }
            hr[sb] = scaled(value, ratio[bin]);
        }
        for gr in sub_groups..map.len() {
            let bin = (map[gr] & 255) as usize;
            for sb in borders[gr] as usize..borders[gr + 1] as usize {
                let input = left[sb];
                let value = if sb >= ALL_PASS_BANDS {
                    if sb < LONG_DELAY_BANDS {
                        core::mem::replace(&mut self.qmf_delay[self.long_at * BANDS + sb], input)
                    } else {
                        core::mem::replace(&mut self.qmf_delay[sb], input)
                    }
                } else {
                    let delayed = core::mem::replace(&mut self.qmf_delay[self.delay_at * BANDS + sb], input);
                    let mut value = cmul(delayed.0, delayed.1, FRAC_QMF_RE[sb], FRAC_QMF_IM[sb]);
                    for link in 0..LINKS {
                        let store = &mut self.qmf_links[(link * LINK_MAX + self.link_at[link]) * BANDS + sb];
                        value = all_pass(
                            store,
                            value,
                            SER_QMF_RE[sb * LINKS + link],
                            SER_QMF_IM[sb * LINKS + link],
                            self.decay[sb][link],
                        );
                    }
                    value
                };
                right[sb] = scaled(value, ratio[bin]);
            }
        }
        self.delay_at = (self.delay_at + 1) % 2;
        self.long_at = (self.long_at + 1) % LONG_DELAY;
        for link in 0..LINKS {
            self.link_at[link] = (self.link_at[link] + 1) % LINK_LENGTHS[link];
        }

        // Mixing.
        for gr in 0..sub_groups {
            let sb = borders[gr] as usize;
            let bin = (map[gr] & 255) as usize;
            let (a, b) = if self.complex {
                mix_complex(&self.h[bin], &self.h_im[bin], map[gr] & 256 != 0, hl[sb], hr[sb])
            } else {
                mix(&self.h[bin], hl[sb], hr[sb])
            };
            hl[sb] = a;
            hr[sb] = b;
        }
        for gr in sub_groups..map.len() {
            let bin = (map[gr] & 255) as usize;
            let h = self.h[bin];
            let h_im = self.h_im[bin];
            let complex = self.complex;
            for sb in borders[gr] as usize..borders[gr + 1] as usize {
                let (a, b) = if complex {
                    mix_complex(&h, &h_im, map[gr] & 256 != 0, left[sb], right[sb])
                } else {
                    mix(&h, left[sb], right[sb])
                };
                left[sb] = a;
                right[sb] = b;
            }
        }

        // Back to QMF bands.
        let resolutions: &[usize] = if self.use_34 { &[12, 8, 4, 4, 4] } else { &[8, 2, 2] };
        let mut at = 0;
        for (band, &count) in resolutions.iter().enumerate() {
            let (mut lr, mut li, mut rr, mut ri) = (0i32, 0i32, 0i32, 0i32);
            for q in at..at + count {
                lr = lr.wrapping_add(hl[q].0);
                li = li.wrapping_add(hl[q].1);
                rr = rr.wrapping_add(hr[q].0);
                ri = ri.wrapping_add(hr[q].1);
            }
            left[band] = (lr, li);
            right[band] = (rr, ri);
            at += count;
        }
    }

    /// Gives a caller the right channel's synthesis bank.
    pub(crate) fn right_synthesis(&mut self, mut with: impl FnMut(&mut [i32], &mut usize)) {
        with(&mut self.bank, &mut self.bank_at);
    }

    pub(crate) fn end_frame(&mut self) {
        // The matrices land where they meant to; rounding is not carried on.
        self.params.fresh = false;
    }
}

#[inline]
fn all_pass(store: &mut Cx, input: Cx, re: i32, im: i32, gain: i32) -> Cx {
    let delayed = cmul(store.0, store.1, re, im);
    let output = (
        delayed.0.wrapping_sub(((gain as i64 * input.0 as i64) >> 31) as i32),
        delayed.1.wrapping_sub(((gain as i64 * input.1 as i64) >> 31) as i32),
    );
    *store = (
        input.0.wrapping_add(((gain as i64 * output.0 as i64) >> 31) as i32),
        input.1.wrapping_add(((gain as i64 * output.1 as i64) >> 31) as i32),
    );
    output
}

#[inline]
fn scaled(value: Cx, ratio: i64) -> Cx {
    if ratio >= 1 << 15 {
        return value;
    }
    (((value.0 as i64 * ratio) >> 15) as i32, ((value.1 as i64 * ratio) >> 15) as i32)
}

#[inline]
fn mix(h: &[i32; 4], l: Cx, r: Cx) -> (Cx, Cx) {
    let (h11, h12, h21, h22) = (h[0] as i64, h[1] as i64, h[2] as i64, h[3] as i64);
    let round = 1i64 << 27;
    (
        (
            ((h11 * l.0 as i64 + h21 * r.0 as i64 + round) >> 28) as i32,
            ((h11 * l.1 as i64 + h21 * r.1 as i64 + round) >> 28) as i32,
        ),
        (
            ((h12 * l.0 as i64 + h22 * r.0 as i64 + round) >> 28) as i32,
            ((h12 * l.1 as i64 + h22 * r.1 as i64 + round) >> 28) as i32,
        ),
    )
}

/// Mixing with matrices that have imaginary parts, which some groups see negated.
#[inline]
fn mix_complex(h: &[i32; 4], im: &[i32; 4], negate: bool, l: Cx, r: Cx) -> (Cx, Cx) {
    let round = 1i64 << 27;
    let one = |re_a: i32, im_a: i32, re_b: i32, im_b: i32| -> Cx {
        let (im_a, im_b) = if negate { (-(im_a as i64), -(im_b as i64)) } else { (im_a as i64, im_b as i64) };
        let (re_a, re_b) = (re_a as i64, re_b as i64);
        (
            ((re_a * l.0 as i64 - im_a * l.1 as i64 + re_b * r.0 as i64 - im_b * r.1 as i64 + round) >> 28) as i32,
            ((im_a * l.0 as i64 + re_a * l.1 as i64 + im_b * r.0 as i64 + re_b * r.1 as i64 + round) >> 28) as i32,
        )
    };
    (one(h[0], im[0], h[2], im[2]), one(h[1], im[1], h[3], im[3]))
}

/// Reads an envelope's phases, which are modulo 8.
fn read_phases(
    reader: &mut MsbReader<'_>,
    table: &mut [[u8; PHASE_BINS]; MAX_ENV + 1],
    before: &[u8; PHASE_BINS],
    e: usize,
    mode: u8,
    dt: &[Code],
    df: &[Code],
) -> Result<()> {
    let bins = [5, 11, 17][mode as usize];
    let stride = if mode == 0 { 2 } else { 1 };
    let time = reader.bit()?;
    let previous = if e == 0 { *before } else { table[e - 1] };
    let mut line = [0u8; PHASE_BINS];
    let mut running = 0u16;
    for b in 0..bins {
        let code = read_code(reader, if time { dt } else { df })?;
        line[b] = if time {
            ((previous[b * stride] as u16 + code) & 7) as u8
        } else {
            running = (running + code) & 7;
            running as u8
        };
    }
    if mode == 0 {
        for b in (0..10).rev() {
            line[b] = line[b / 2];
        }
    }
    table[e] = line;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_parameters(
    reader: &mut MsbReader<'_>,
    table: &mut [[i8; MAX_BINS]; MAX_ENV + 1],
    before: &[i8; MAX_BINS],
    envelopes: usize,
    mode: u8,
    dt: &[Code],
    df: &[Code],
    low: i32,
    high: i32,
) -> Result<()> {
    let bins = [10, 20, 34][mode as usize];
    // The coarsest grid is kept doubled up, so it can be read against the next one.
    let stride = if mode == 0 { 2 } else { 1 };
    for e in 0..envelopes {
        let time = reader.bit()?;
        let previous = if e == 0 { *before } else { table[e - 1] };
        let mut line = [0i8; MAX_BINS];
        let mut running = 0i32;
        for b in 0..bins {
            let code = read_code(reader, if time { dt } else { df })? as i32 - 128;
            let value = if time {
                (previous[b * stride] as i32 + code).clamp(low, high)
            } else {
                running = (running + code).clamp(low, high);
                running
            };
            line[b] = value as i8;
        }
        if mode == 0 {
            for b in (0..20).rev() {
                line[b] = line[b / 2];
            }
        }
        table[e] = line;
    }
    Ok(())
}
