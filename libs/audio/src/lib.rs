//! `feos-audio` is a small, dependency-free audio library for the feos
//! kernel. It provides integer PCM sample buffers, mixing, fades, and simple
//! waveform synthesis. Like `feos-gfx`, it is `no_std`, never allocates, and
//! works on caller-provided memory.
//!
//! Samples are signed 16-bit mono values. An [`AudioBuffer`] stores
//! interleaved frames for one or more channels, so stereo data is
//! `[left, right, left, right, ...]`.
//!
//! ```
//! use feos_audio::{AudioBuffer, AudioFormat, Oscillator, Sample, Waveform};
//!
//! let format = AudioFormat::mono(48_000).unwrap();
//! let mut samples = [0i16; 480];
//! let mut buffer = AudioBuffer::new(&mut samples, format).unwrap();
//!
//! let mut tone = Oscillator::new(Waveform::Sine, 440, Sample::from_i16(8_000));
//! buffer.render_oscillator(&mut tone, Sample::MAX);
//! buffer.fade_in(48);
//! buffer.fade_out(48);
//!
//! assert_eq!(buffer.frames(), 480);
//! ```
//!
//! All processing is integer-based so it behaves identically on the host and
//! on the target. There is no hardware access here: a future audio driver
//! takes the interleaved buffer and submits it to the device.
//!
//! # Clipping
//!
//! `write_*` replaces samples; `mix_*` adds to them and saturates at
//! [`Sample::MIN`] / [`Sample::MAX`], so mixing several sources cannot wrap
//! around into distortion of the wrong sign.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod buffer;
pub mod format;
pub mod sample;
pub mod wave;

pub use buffer::AudioBuffer;
pub use format::AudioFormat;
pub use sample::Sample;
pub use wave::{Oscillator, Waveform};
