//! SILK packet loss concealment and comfort noise, after libopus's `PLC.c` and `CNG.c`: a
//! lost frame continues the pitch and spectrum of the last good one, on its own excitation.

#[allow(unused_imports)]
use crate::prelude::*;
use super::decode_core::{SilkChannelState, inverse32_var_q, lpc_analysis_filter};
use super::lpc;
use super::tables::{LTP_ORDER, MAX_FRAME_LENGTH, MAX_LPC_ORDER, MAX_NB_SUBFR};

const NB_ATT: usize = 2;
const HARM_ATT_Q15: [i32; NB_ATT] = [32440, 31130];
const PLC_RAND_ATTENUATE_V_Q15: [i32; NB_ATT] = [31130, 26214];
const PLC_RAND_ATTENUATE_UV_Q15: [i32; NB_ATT] = [32440, 29491];
const BWE_COEF_Q16: i32 = 64881; // 0.99 in Q16, rounded as SILK_FIX_CONST does
const V_PITCH_GAIN_START_MIN_Q14: i32 = 11469;
const V_PITCH_GAIN_START_MAX_Q14: i32 = 15565;
const MAX_PITCH_LAG_MS: i32 = 18;
const RAND_BUF_SIZE: usize = 128;
const RAND_BUF_MASK: i32 = RAND_BUF_SIZE as i32 - 1;
const LOG2_INV_LPC_GAIN_HIGH_THRES: u32 = 3;
const LOG2_INV_LPC_GAIN_LOW_THRES: u32 = 8;
const PITCH_DRIFT_FAC_Q16: i32 = 655;
const CNG_BUF_MASK_MAX: i32 = 255;
const CNG_GAIN_SMTH_Q16: i32 = 4634;
const CNG_GAIN_SMTH_THRESHOLD_Q16: i32 = 46396;
const CNG_NLSF_SMTH_Q16: i32 = 16348;
const TYPE_NO_VOICE_ACTIVITY: i32 = 0;
const TYPE_VOICED: i32 = 2;
const RAND_MULTIPLIER: i32 = 196_314_165;
const RAND_INCREMENT: i32 = 907_633_515;

/// What the last good frame's parameters were, kept by the core for concealment.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct LastFrame {
    pub gains_q16: [i32; MAX_NB_SUBFR],
    pub ltp_coeffs_q14: [[i16; LTP_ORDER]; MAX_NB_SUBFR],
    pub ltp_scale_q14: i32,
    pub pitch_lags: [i32; MAX_NB_SUBFR],
}

/// State of concealment (`silk_PLC_struct`).
#[derive(Debug, Clone)]
pub(super) struct SilkPlcState {
    pub pitch_l_q8: i32,
    pub ltp_coef_q14: [i16; LTP_ORDER],
    pub prev_lpc_q12: [i16; MAX_LPC_ORDER],
    pub last_frame_lost: bool,
    pub rand_seed: i32,
    pub rand_scale_q14: i16,
    pub conc_energy: i32,
    pub conc_energy_shift: i32,
    pub prev_ltp_scale_q14: i32,
    pub prev_gain_q16: [i32; 2],
    pub fs_khz: i32,
    pub nb_subfr: usize,
    pub subfr_length: usize,
    /// Consecutive frames concealed.
    pub loss_cnt: usize,
    /// The channel hasn't decoded a frame since a reset.
    pub first_frame_after_reset: bool,
    pub lpc_order: usize,
    cng: Cng,
}

/// State of comfort noise (`silk_CNG_struct`).
#[derive(Debug, Clone)]
struct Cng {
    exc_buf_q14: Vec<i32>,
    smth_nlsf_q15: [i16; MAX_LPC_ORDER],
    synth_state: [i32; MAX_LPC_ORDER],
    smth_gain_q16: i32,
    rand_seed: i32,
    fs_khz: i32,
}

impl Default for SilkPlcState {
    fn default() -> Self {
        Self {
            pitch_l_q8: 0,
            ltp_coef_q14: [0; LTP_ORDER],
            prev_lpc_q12: [0; MAX_LPC_ORDER],
            last_frame_lost: false,
            rand_seed: 0,
            rand_scale_q14: 0,
            conc_energy: 0,
            conc_energy_shift: 0,
            prev_ltp_scale_q14: 0,
            prev_gain_q16: [1 << 16; 2],
            fs_khz: 0,
            nb_subfr: 0,
            subfr_length: 0,
            loss_cnt: 0,
            first_frame_after_reset: true,
            lpc_order: 0,
            cng: Cng {
                exc_buf_q14: vec![0; MAX_FRAME_LENGTH],
                smth_nlsf_q15: [0; MAX_LPC_ORDER],
                synth_state: [0; MAX_LPC_ORDER],
                smth_gain_q16: 0,
                rand_seed: 0,
                fs_khz: 0,
            },
        }
    }
}

#[inline(always)]
fn smulwb(a32: i32, b32: i32) -> i32 {
    ((a32 as i64 * (b32 as i16) as i64) >> 16) as i32
}

#[inline(always)]
fn smlawb(a32: i32, b32: i32, c32: i32) -> i32 {
    a32.wrapping_add(smulwb(b32, c32))
}

#[inline(always)]
fn smulww(a32: i32, b32: i32) -> i32 {
    ((a32 as i64 * b32 as i64) >> 16) as i32
}

#[inline(always)]
fn smulbb(a32: i32, b32: i32) -> i32 {
    (a32 as i16 as i32) * (b32 as i16 as i32)
}

#[inline(always)]
fn sat16(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

#[inline(always)]
fn rshift_round(value: i32, shift: u32) -> i32 {
    if shift == 1 {
        (value >> 1) + (value & 1)
    } else {
        ((value >> (shift - 1)) + 1) >> 1
    }
}

#[inline(always)]
fn lshift_sat32(value: i32, shift: u32) -> i32 {
    ((value as i64) << shift).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn silk_rand(seed: i32) -> i32 {
    seed.wrapping_mul(RAND_MULTIPLIER).wrapping_add(RAND_INCREMENT)
}

/// `silk_sum_sqr_shift`.
fn sum_sqr_shift(x: &[i16]) -> (i32, i32) {
    let len = x.len() as u32;
    let run = |shift: u32, start: u32| -> u32 {
        let mut nrg = start;
        let mut i = 0;
        while i + 1 < x.len() {
            let a = x[i] as i32 * x[i] as i32;
            let b = (a as u32).wrapping_add((x[i + 1] as i32 * x[i + 1] as i32) as u32);
            nrg = nrg.wrapping_add(b >> shift);
            i += 2;
        }
        if i < x.len() {
            nrg = nrg.wrapping_add(((x[i] as i32 * x[i] as i32) as u32) >> shift);
        }
        nrg
    };
    let shift = 31 - len.leading_zeros();
    let nrg = run(shift, len);
    let shift = (shift as i32 + 3 - nrg.leading_zeros() as i32).max(0) as u32;
    (run(shift, 0) as i32, shift as i32)
}

/// `silk_SQRT_APPROX`.
fn sqrt_approx(x: i32) -> i32 {
    if x <= 0 {
        return 0;
    }
    let lz = (x as u32).leading_zeros() as i32;
    let frac_q7 = ((x as u32).rotate_right(((24 - lz) & 31) as u32) & 0x7f) as i32;
    let mut y = if lz & 1 != 0 { 32768 } else { 46214 };
    y >>= lz >> 1;
    smlawb(y, y, smulbb(213, frac_q7))
}

fn reset(state: &mut SilkPlcState, frame_length: usize) {
    state.pitch_l_q8 = (frame_length as i32) << 7;
    state.prev_gain_q16 = [1 << 16; 2];
    state.subfr_length = 20;
    state.nb_subfr = 2;
}

/// Everything after a good frame: `silk_PLC_update`, comfort noise's averages and the fade
/// back in from what concealment left. `pcm` is the frame as decoded, before stereo.
pub(super) fn after_good_frame(
    state: &mut SilkChannelState,
    signal_type: i32,
    pred_coef_q12: &[i16; MAX_LPC_ORDER],
    prev_nlsf_q15: &[i16; MAX_LPC_ORDER],
    pcm: &mut [i16],
    fs_khz: i32,
    nb_subfr: usize,
    subfr_length: usize,
    order: usize,
) {
    let frame_length = pcm.len();
    let last = *state.last_frame;
    if state.plc.fs_khz != fs_khz {
        reset(&mut state.plc, frame_length);
        state.plc.fs_khz = fs_khz;
    }
    let plc = &mut state.plc;
    plc.lpc_order = order;
    let mut gain_q14 = 0;
    if signal_type == TYPE_VOICED {
        let mut j = 0;
        while j * (subfr_length as i32) < last.pitch_lags[nb_subfr - 1] {
            if j as usize == nb_subfr {
                break;
            }
            let at = nb_subfr - 1 - j as usize;
            let sum: i32 = last.ltp_coeffs_q14[at].iter().map(|&c| c as i32).sum();
            if sum > gain_q14 {
                gain_q14 = sum;
                plc.ltp_coef_q14 = last.ltp_coeffs_q14[at];
                plc.pitch_l_q8 = last.pitch_lags[at] << 8;
            }
            j += 1;
        }
        plc.ltp_coef_q14 = [0; LTP_ORDER];
        plc.ltp_coef_q14[LTP_ORDER / 2] = gain_q14 as i16;
        if gain_q14 < V_PITCH_GAIN_START_MIN_Q14 {
            let scale_q10 = (V_PITCH_GAIN_START_MIN_Q14 << 10) / gain_q14.max(1);
            for c in plc.ltp_coef_q14.iter_mut() {
                *c = (smulbb(*c as i32, scale_q10) >> 10) as i16;
            }
        } else if gain_q14 > V_PITCH_GAIN_START_MAX_Q14 {
            let scale_q14 = (V_PITCH_GAIN_START_MAX_Q14 << 14) / gain_q14.max(1);
            for c in plc.ltp_coef_q14.iter_mut() {
                *c = (smulbb(*c as i32, scale_q14) >> 14) as i16;
            }
        }
    } else {
        plc.pitch_l_q8 = smulbb(fs_khz, 18) << 8;
        plc.ltp_coef_q14 = [0; LTP_ORDER];
    }
    plc.prev_lpc_q12[..order].copy_from_slice(&pred_coef_q12[..order]);
    plc.prev_ltp_scale_q14 = last.ltp_scale_q14;
    plc.prev_gain_q16 = [last.gains_q16[nb_subfr - 2], last.gains_q16[nb_subfr - 1]];
    plc.subfr_length = subfr_length;
    plc.nb_subfr = nb_subfr;
    plc.loss_cnt = 0;
    plc.first_frame_after_reset = false;
    state.prev_signal_type = signal_type;
    cng(&mut state.plc, &last.gains_q16, &state.exc_q14, Some(prev_nlsf_q15), signal_type, pcm, fs_khz, nb_subfr, subfr_length, order);
    glue_frames(&mut state.plc, pcm);
    if signal_type == TYPE_VOICED {
        state.lag_prev = last.pitch_lags[nb_subfr - 1];
    }
}

/// `silk_PLC_conceal`: one lost frame of `frame.len()` samples into `frame`, with its comfort
/// noise, and the state left as a decoded frame would leave it.
pub(super) fn conceal(state: &mut SilkChannelState, frame: &mut [i16], fs_khz: i32) {
    let frame_length = frame.len();
    let order = if fs_khz >= 16 { 16 } else { 10 };
    if state.plc.fs_khz != fs_khz {
        reset(&mut state.plc, frame_length);
        state.plc.fs_khz = fs_khz;
    }
    // Subframes are 5 ms, whatever was decoded last.
    let plc_subfr = fs_khz as usize * 5;
    let nb_subfr = (frame_length / plc_subfr).clamp(2, MAX_NB_SUBFR);
    let ltp_mem_length = fs_khz as usize * super::tables::LTP_MEM_LENGTH_MS;
    let prev_gain_q10 = [state.plc.prev_gain_q16[0] >> 6, state.plc.prev_gain_q16[1] >> 6];
    if state.plc.first_frame_after_reset {
        state.plc.prev_lpc_q12 = [0; MAX_LPC_ORDER];
    }

    // Which of the last two subframes' excitation is quieter: that's the noise.
    let mut exc_buf = [0i16; 2 * super::tables::MAX_SUB_FRAME_LENGTH];
    for k in 0..2 {
        let from = (k + nb_subfr.max(2) - 2) * plc_subfr;
        for i in 0..plc_subfr {
            exc_buf[k * plc_subfr + i] =
                sat16(smulww(state.exc_q14[i + from], prev_gain_q10[k]) >> 8);
        }
    }
    let (energy1, shift1) = sum_sqr_shift(&exc_buf[..plc_subfr]);
    let (energy2, shift2) = sum_sqr_shift(&exc_buf[plc_subfr..2 * plc_subfr]);
    let rand_from = if (energy1 >> shift2) < (energy2 >> shift1) {
        ((nb_subfr - 1) * plc_subfr).saturating_sub(RAND_BUF_SIZE)
    } else {
        (nb_subfr * plc_subfr).saturating_sub(RAND_BUF_SIZE)
    };

    let plc = &mut state.plc;
    let mut b_q14 = plc.ltp_coef_q14;
    let mut rand_scale_q14 = plc.rand_scale_q14 as i32;
    let loss = plc.loss_cnt.min(NB_ATT - 1);
    let harm_gain_q15 = HARM_ATT_Q15[loss];
    let mut rand_gain_q15 = if state.prev_signal_type == TYPE_VOICED {
        PLC_RAND_ATTENUATE_V_Q15[loss]
    } else {
        PLC_RAND_ATTENUATE_UV_Q15[loss]
    };
    lpc::bw_expand(&mut plc.prev_lpc_q12, order, BWE_COEF_Q16);
    let a_q12 = plc.prev_lpc_q12;
    if plc.loss_cnt == 0 {
        rand_scale_q14 = 1 << 14;
        if state.prev_signal_type == TYPE_VOICED {
            for &b in &b_q14 {
                rand_scale_q14 -= b as i32;
            }
            rand_scale_q14 = rand_scale_q14.max(3277);
            rand_scale_q14 = smulbb(rand_scale_q14, plc.prev_ltp_scale_q14) >> 14;
        } else {
            let inv_gain_q30 = lpc::lpc_inv_pred_gain(&plc.prev_lpc_q12, order);
            let mut down_scale_q30 = (inv_gain_q30).min((1i32 << 30) >> LOG2_INV_LPC_GAIN_HIGH_THRES);
            down_scale_q30 = down_scale_q30.max((1i32 << 30) >> LOG2_INV_LPC_GAIN_LOW_THRES);
            down_scale_q30 <<= LOG2_INV_LPC_GAIN_HIGH_THRES;
            rand_gain_q15 = smulwb(down_scale_q30, rand_gain_q15) >> 14;
        }
    }
    let mut rand_seed = plc.rand_seed;
    let mut lag = rshift_round(plc.pitch_l_q8, 8);
    let mut buf_idx = ltp_mem_length;

    // Whitened history, scaled to the excitation.
    let mut s_ltp = [0i16; MAX_FRAME_LENGTH];
    let mut s_ltp_q14 = [0i32; 2 * MAX_FRAME_LENGTH + MAX_LPC_ORDER];
    // Far enough back for the filter, in a stream that doesn't keep to its limits as well.
    let idx = ltp_mem_length as i32 - lag - order as i32 - LTP_ORDER as i32 / 2;
    let idx = idx.min(ltp_mem_length as i32 - 2 * order as i32);
    let idx = idx.max(0) as usize;
    let len = ltp_mem_length - idx;
    lpc_analysis_filter(&mut s_ltp[idx..ltp_mem_length], &state.out_buf[idx..ltp_mem_length], &a_q12[..order], len, order);
    let inv_gain_q30 = inverse32_var_q(plc.prev_gain_q16[1], 46).min(i32::MAX >> 1);
    for i in (idx + order)..ltp_mem_length {
        s_ltp_q14[i] = smulwb(inv_gain_q30, s_ltp[i] as i32);
    }

    let rand_buf = &state.exc_q14;
    for _ in 0..nb_subfr {
        let mut lag_at = buf_idx as i32 - lag + LTP_ORDER as i32 / 2;
        for _ in 0..plc_subfr {
            let mut pred_q12 = 2;
            for (j, &b) in b_q14.iter().enumerate() {
                let at = lag_at - j as i32;
                let value = if at >= 0 { s_ltp_q14[at as usize] } else { 0 };
                pred_q12 = smlawb(pred_q12, value, b as i32);
            }
            lag_at += 1;
            rand_seed = silk_rand(rand_seed);
            let at = ((rand_seed >> 25) & RAND_BUF_MASK) as usize;
            let noise = rand_buf[(rand_from + at).min(MAX_FRAME_LENGTH - 1)];
            s_ltp_q14[buf_idx] = smlawb(pred_q12, noise, rand_scale_q14).wrapping_shl(2);
            buf_idx += 1;
        }
        for b in b_q14.iter_mut() {
            *b = (smulbb(harm_gain_q15, *b as i32) >> 15) as i16;
        }
        rand_scale_q14 = smulbb(rand_scale_q14, rand_gain_q15) >> 15;
        plc.pitch_l_q8 = smlawb(plc.pitch_l_q8, plc.pitch_l_q8, PITCH_DRIFT_FAC_Q16);
        plc.pitch_l_q8 = plc.pitch_l_q8.min(smulbb(MAX_PITCH_LAG_MS, fs_khz) << 8);
        lag = rshift_round(plc.pitch_l_q8, 8);
    }

    // LPC synthesis, over the same buffer.
    let base = ltp_mem_length - MAX_LPC_ORDER;
    s_ltp_q14[base..ltp_mem_length].copy_from_slice(&state.s_lpc_q14_buf);
    let gain_q10 = prev_gain_q10[1];
    for i in 0..frame_length {
        let mut pred_q10 = order as i32 >> 1;
        for j in 0..order {
            pred_q10 = smlawb(pred_q10, s_ltp_q14[base + MAX_LPC_ORDER + i - j - 1], a_q12[j] as i32);
        }
        let current = s_ltp_q14[base + MAX_LPC_ORDER + i].saturating_add(lshift_sat32(pred_q10, 4));
        s_ltp_q14[base + MAX_LPC_ORDER + i] = current;
        frame[i] = sat16(sat16(rshift_round(smulww(current, gain_q10), 8)) as i32);
    }
    state
        .s_lpc_q14_buf
        .copy_from_slice(&s_ltp_q14[base + frame_length..base + frame_length + MAX_LPC_ORDER]);
    plc.rand_seed = rand_seed;
    plc.rand_scale_q14 = rand_scale_q14 as i16;
    plc.loss_cnt += 1;

    // As a decoded frame would.
    let mv_len = ltp_mem_length.saturating_sub(frame_length);
    if mv_len > 0 {
        state.out_buf.copy_within(frame_length..frame_length + mv_len, 0);
    }
    state.out_buf[mv_len..mv_len + frame_length].copy_from_slice(frame);
    let gains = [0; MAX_NB_SUBFR];
    let signal = state.prev_signal_type;
    cng(&mut state.plc, &gains, &state.exc_q14, None, signal, frame, fs_khz, nb_subfr, plc_subfr, order);
    glue_frames(&mut state.plc, frame);
    state.lag_prev = lag;
    state.last_frame.pitch_lags = [lag; MAX_NB_SUBFR];
}

/// `silk_CNG`. `prev_nlsf_q15` is `None` for a lost frame, where nothing's averaged.
#[allow(clippy::too_many_arguments)]
fn cng(
    plc: &mut SilkPlcState,
    gains_q16: &[i32; MAX_NB_SUBFR],
    exc_q14: &[i32],
    prev_nlsf_q15: Option<&[i16; MAX_LPC_ORDER]>,
    signal_type: i32,
    frame: &mut [i16],
    fs_khz: i32,
    nb_subfr: usize,
    subfr_length: usize,
    order: usize,
) {
    let length = frame.len();
    if plc.cng.fs_khz != fs_khz {
        let step = i16::MAX as i32 / (order as i32 + 1);
        let mut acc = 0;
        for slot in plc.cng.smth_nlsf_q15.iter_mut().take(order) {
            acc += step;
            *slot = acc as i16;
        }
        plc.cng.smth_gain_q16 = 0;
        plc.cng.rand_seed = 3_176_576;
        plc.cng.fs_khz = fs_khz;
    }
    if plc.loss_cnt == 0 && signal_type == TYPE_NO_VOICE_ACTIVITY {
        if let Some(nlsf) = prev_nlsf_q15 {
            for i in 0..order {
                let delta = nlsf[i] as i32 - plc.cng.smth_nlsf_q15[i] as i32;
                plc.cng.smth_nlsf_q15[i] =
                    (plc.cng.smth_nlsf_q15[i] as i32 + smulwb(delta, CNG_NLSF_SMTH_Q16)) as i16;
            }
        }
        let (mut max_gain, mut subfr) = (0, 0);
        for (i, &gain) in gains_q16.iter().enumerate().take(nb_subfr) {
            if gain > max_gain {
                max_gain = gain;
                subfr = i;
            }
        }
        plc.cng.exc_buf_q14.copy_within(0..(nb_subfr - 1) * subfr_length, subfr_length);
        plc.cng.exc_buf_q14[..subfr_length]
            .copy_from_slice(&exc_q14[subfr * subfr_length..(subfr + 1) * subfr_length]);
        for &gain in gains_q16.iter().take(nb_subfr) {
            plc.cng.smth_gain_q16 += smulwb(gain - plc.cng.smth_gain_q16, CNG_GAIN_SMTH_Q16);
            if smulww(plc.cng.smth_gain_q16, CNG_GAIN_SMTH_THRESHOLD_Q16) > gain {
                plc.cng.smth_gain_q16 = gain;
            }
        }
    }
    if plc.loss_cnt != 0 {
        let scale = plc.rand_scale_q14 as i32;
        let mut gain_q16 = smulww(scale, plc.prev_gain_q16[1]);
        if gain_q16 >= (1 << 21) || plc.cng.smth_gain_q16 > (1 << 23) {
            let high = |v: i32| (v >> 16) * (v >> 16);
            gain_q16 = high(gain_q16);
            gain_q16 = high(plc.cng.smth_gain_q16).wrapping_sub(gain_q16 << 5);
            gain_q16 = sqrt_approx(gain_q16) << 16;
        } else {
            gain_q16 = smulww(gain_q16, gain_q16);
            gain_q16 = smulww(plc.cng.smth_gain_q16, plc.cng.smth_gain_q16).wrapping_sub(gain_q16 << 5);
            gain_q16 = sqrt_approx(gain_q16) << 8;
        }
        let gain_q10 = gain_q16 >> 6;
        let mut mask = CNG_BUF_MASK_MAX;
        while mask > length as i32 {
            mask >>= 1;
        }
        let mut sig = [0i32; MAX_FRAME_LENGTH + MAX_LPC_ORDER];
        let mut seed = plc.cng.rand_seed;
        for i in 0..length {
            seed = silk_rand(seed);
            sig[MAX_LPC_ORDER + i] = plc.cng.exc_buf_q14[((seed >> 24) & mask) as usize];
        }
        plc.cng.rand_seed = seed;
        let mut a_q12 = [0i16; MAX_LPC_ORDER];
        lpc::nlsf2a(&mut a_q12[..order], &plc.cng.smth_nlsf_q15[..order], order);
        sig[..MAX_LPC_ORDER].copy_from_slice(&plc.cng.synth_state);
        for i in 0..length {
            let mut pred_q10 = order as i32 >> 1;
            for j in 0..order {
                pred_q10 = smlawb(pred_q10, sig[MAX_LPC_ORDER + i - j - 1], a_q12[j] as i32);
            }
            sig[MAX_LPC_ORDER + i] = sig[MAX_LPC_ORDER + i].saturating_add(lshift_sat32(pred_q10, 4));
            let noise = sat16(rshift_round(smulww(sig[MAX_LPC_ORDER + i], gain_q10), 8));
            frame[i] = sat16(frame[i] as i32 + noise as i32);
        }
        plc.cng.synth_state.copy_from_slice(&sig[length..length + MAX_LPC_ORDER]);
    } else {
        plc.cng.synth_state[..order].fill(0);
    }
}

/// `silk_PLC_glue_frames`: fades back in if the concealment was quieter than what follows.
fn glue_frames(plc: &mut SilkPlcState, frame: &mut [i16]) {
    let length = frame.len();
    if plc.loss_cnt != 0 {
        let (energy, shift) = sum_sqr_shift(frame);
        plc.conc_energy = energy;
        plc.conc_energy_shift = shift;
        plc.last_frame_lost = true;
    } else {
        if plc.last_frame_lost {
            let (mut energy, shift) = sum_sqr_shift(frame);
            if shift > plc.conc_energy_shift {
                plc.conc_energy >>= shift - plc.conc_energy_shift;
            } else if shift < plc.conc_energy_shift {
                energy >>= plc.conc_energy_shift - shift;
            }
            if energy > plc.conc_energy {
                let lz = (plc.conc_energy as u32).leading_zeros() as i32 - 1;
                plc.conc_energy = plc.conc_energy.wrapping_shl(lz.max(0) as u32);
                energy >>= (24 - lz).max(0);
                let frac_q24 = plc.conc_energy / energy.max(1);
                let mut gain_q16 = sqrt_approx(frac_q24) << 4;
                let slope_q16 = (((1 << 16) - gain_q16) / length as i32) << 2;
                for sample in frame.iter_mut() {
                    *sample = smulwb(gain_q16, *sample as i32) as i16;
                    gain_q16 += slope_q16;
                    if gain_q16 > 1 << 16 {
                        break;
                    }
                }
            }
        }
        plc.last_frame_lost = false;
    }
}
