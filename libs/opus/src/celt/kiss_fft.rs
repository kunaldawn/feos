#![allow(dead_code)]

//! Minimal CELT FFT primitives for decoder path.
//!
//! This module intentionally implements only the inverse transform behavior
//! needed by the decoder. It follows the libopus scaling convention where the
//! inverse FFT is unscaled and the forward path applies `1/N`.

#[allow(unused_imports)]
use crate::prelude::*;
use core::f32::consts::PI;

/// Complex number used by CELT FFT/MDCT primitives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Complex32 {
    /// Real component.
    pub re: f32,
    /// Imaginary component.
    pub im: f32,
}

impl Complex32 {
    /// Create a complex value from real and imaginary parts.
    ///
    /// Params: `re` is the real component, `im` is the imaginary component.
    /// Returns: a new `Complex32`.
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
}

/// Inverse-only FFT plan for CELT decoder usage.
#[derive(Debug, Clone)]
pub(crate) struct KissFft {
    nfft: usize,
    inv_twiddles: Vec<Complex32>,
}

impl KissFft {
    /// Build an inverse FFT plan with runtime twiddle generation.
    ///
    /// Params: `nfft` is the FFT size.
    /// Returns: a plan containing precomputed inverse twiddles.
    pub fn new(nfft: usize) -> Self {
        let mut inv_twiddles = Vec::with_capacity(nfft);
        for k in 0..nfft {
            let phase = 2.0 * PI * (k as f32) / (nfft as f32);
            inv_twiddles.push(Complex32::new(phase.cos(), phase.sin()));
        }
        Self { nfft, inv_twiddles }
    }

    /// Return FFT size configured in this plan.
    ///
    /// Params: none.
    /// Returns: number of points in the transform.
    pub fn len(&self) -> usize {
        self.nfft
    }

    /// Compute unscaled inverse FFT.
    ///
    /// Params: `input` is frequency-domain complex spectrum, `output` is
    /// destination time-domain buffer and must have length `self.len()`.
    /// Returns: `Ok(())` on success, otherwise a static validation error.
    pub fn ifft(&self, input: &[Complex32], output: &mut [Complex32]) -> Result<(), &'static str> {
        if input.len() != self.nfft || output.len() != self.nfft {
            return Err("kiss_fft length mismatch");
        }
        for out in output.iter_mut() {
            *out = Complex32::new(0.0, 0.0);
        }
        for (n, out) in output.iter_mut().enumerate() {
            let mut acc_re = 0.0f32;
            let mut acc_im = 0.0f32;
            for (k, xk) in input.iter().enumerate() {
                let tw = self.inv_twiddles[(k * n) % self.nfft];
                acc_re += xk.re * tw.re - xk.im * tw.im;
                acc_im += xk.re * tw.im + xk.im * tw.re;
            }
            *out = Complex32::new(acc_re, acc_im);
        }
        Ok(())
    }

    /// Compute unscaled forward FFT.
    ///
    /// Params: `input` is time-domain complex vector, `output` is destination
    /// frequency-domain buffer and must have length `self.len()`.
    /// Returns: `Ok(())` on success, otherwise a static validation error.
    pub fn fft(&self, input: &[Complex32], output: &mut [Complex32]) -> Result<(), &'static str> {
        if input.len() != self.nfft || output.len() != self.nfft {
            return Err("kiss_fft length mismatch");
        }
        for out in output.iter_mut() {
            *out = Complex32::new(0.0, 0.0);
        }
        for (n, out) in output.iter_mut().enumerate() {
            let mut acc_re = 0.0f32;
            let mut acc_im = 0.0f32;
            for (k, xk) in input.iter().enumerate() {
                let tw = self.inv_twiddles[(k * n) % self.nfft];
                // Conjugate inverse twiddle gives forward phase.
                acc_re += xk.re * tw.re + xk.im * tw.im;
                acc_im += -xk.re * tw.im + xk.im * tw.re;
            }
            *out = Complex32::new(acc_re, acc_im);
        }
        Ok(())
    }
}

/// A forward DFT of interleaved `[re0, im0, re1, im1, ...]`, by Stockham's FFT, one factor at a
/// time. It works in double precision, as the direct sum it replaces did: it's that sum's results
/// the conformance numbers were measured with, and a sum for each of 480 bins, with a sine and a
/// cosine for every term, took 1.2 times real time at 256 kbit/s on a fast machine.
#[derive(Debug, Clone)]
pub(crate) struct FlatFft {
    n: usize,
    /// A whole turn, in `n` steps.
    twiddle: Vec<(f64, f64)>,
}

impl FlatFft {
    pub(crate) fn new(n: usize) -> Self {
        let mut twiddle = Vec::with_capacity(n);
        for k in 0..n {
            let angle = -2.0 * PI as f64 * (k as f64) / (n as f64);
            let (sin_a, cos_a) = angle.sin_cos();
            twiddle.push((cos_a, sin_a));
        }
        Self { n, twiddle }
    }

    /// `n` complex values from `input` to `output`.
    pub(crate) fn run(&self, input: &[f32], output: &mut [f32]) {
        let total = self.n;
        assert!(input.len() >= 2 * total && output.len() >= 2 * total);
        let mut from: Vec<(f64, f64)> =
            input[..2 * total].as_chunks::<2>().0.iter().map(|c| (c[0] as f64, c[1] as f64)).collect();
        let mut to = vec![(0.0f64, 0.0f64); total];
        let mut size = total;
        let mut stride = 1;
        while size > 1 {
            let radix = [2usize, 3, 5].into_iter().find(|p| size.is_multiple_of(*p)).unwrap_or(size);
            let m = size / radix;
            for p in 0..m {
                for q in 0..stride {
                    for t in 0..radix {
                        let (mut sum_re, mut sum_im) = (0.0f64, 0.0f64);
                        for i in 0..radix {
                            let (ar, ai) = from[q + stride * (p + i * m)];
                            let (wr, wi) = self.twiddle[(i * t * total / radix) % total];
                            sum_re += ar * wr - ai * wi;
                            sum_im += ar * wi + ai * wr;
                        }
                        let (wr, wi) = self.twiddle[(p * t * stride) % total];
                        to[q + stride * (radix * p + t)] =
                            (sum_re * wr - sum_im * wi, sum_re * wi + sum_im * wr);
                    }
                }
            }
            core::mem::swap(&mut from, &mut to);
            size = m;
            stride *= radix;
        }
        for (slot, value) in output.as_chunks_mut::<2>().0.iter_mut().zip(&from) {
            slot[0] = value.0 as f32;
            slot[1] = value.1 as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Complex32, FlatFft, KissFft};
    use core::f32::consts::PI;

    #[test]
    fn flat_fft_matches_the_sum() {
        for n in [60usize, 120, 240, 480, 7] {
            let input: Vec<f32> = (0..2 * n).map(|i| ((i * i * 37 + 11) % 201) as f32 - 100.0).collect();
            let mut output = vec![0.0f32; 2 * n];
            FlatFft::new(n).run(&input, &mut output);
            for k in 0..n {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for j in 0..n {
                    let angle = -2.0 * core::f64::consts::PI * (k * j) as f64 / n as f64;
                    let (s, c) = (angle.sin(), angle.cos());
                    re += input[2 * j] as f64 * c - input[2 * j + 1] as f64 * s;
                    im += input[2 * j] as f64 * s + input[2 * j + 1] as f64 * c;
                }
                assert!((re as f32 - output[2 * k]).abs() < 1e-2, "{n} {k}");
                assert!((im as f32 - output[2 * k + 1]).abs() < 1e-2, "{n} {k}");
            }
        }
    }

    /// Compute libopus-style forward DFT with `1/N` scaling for tests.
    ///
    /// Params: `input` is time-domain complex vector.
    /// Returns: scaled frequency-domain vector.
    fn forward_scaled(input: &[Complex32]) -> Vec<Complex32> {
        let n = input.len();
        let mut out = vec![Complex32::new(0.0, 0.0); n];
        for (k, yk) in out.iter_mut().enumerate() {
            let mut acc_re = 0.0f32;
            let mut acc_im = 0.0f32;
            for (n_idx, xn) in input.iter().enumerate() {
                let phase = -2.0 * PI * (k as f32) * (n_idx as f32) / (n as f32);
                let c = phase.cos();
                let s = phase.sin();
                acc_re += xn.re * c - xn.im * s;
                acc_im += xn.re * s + xn.im * c;
            }
            *yk = Complex32::new(acc_re / (n as f32), acc_im / (n as f32));
        }
        out
    }

    #[test]
    fn ifft_roundtrip_matches_input() {
        let n = 60usize;
        let fft = KissFft::new(n);
        let mut input = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / n as f32;
            input.push(Complex32::new(
                (2.0 * PI * 3.0 * t).sin(),
                (2.0 * PI * 5.0 * t).cos(),
            ));
        }
        let freq = forward_scaled(&input);
        let mut recon = vec![Complex32::new(0.0, 0.0); n];
        fft.ifft(&freq, &mut recon).expect("ifft must succeed");
        for (a, b) in input.iter().zip(recon.iter()) {
            assert!(
                (a.re - b.re).abs() < 2e-5,
                "re mismatch: {} vs {}",
                a.re,
                b.re
            );
            assert!(
                (a.im - b.im).abs() < 2e-5,
                "im mismatch: {} vs {}",
                a.im,
                b.im
            );
        }
    }
}
