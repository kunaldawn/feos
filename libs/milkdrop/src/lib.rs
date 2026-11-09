//! `feos-milkdrop`: a music visualiser that runs MilkDrop presets.
//!
//! MilkDrop was the visualisation plug-in for Winamp. Its look comes from
//! presets: `.milk` files of parameters and code in a small expression
//! language, which move and colour a feedback image every frame. This crate
//! reads those files, compiles their code, analyses the audio as MilkDrop did,
//! and draws in software:
//!
//! - [`eel`]: the expression language (EEL2's subset that presets use).
//! - [`preset`]: the `.milk` format and its parameter names.
//! - [`audio`]: waveform, spectrum and the `bass`/`mid`/`treb` beat values.
//! - [`render`]: the warp mesh, waves, shapes, borders and the final composite.
//!
//! MilkDrop 2 presets that depend on HLSL pixel shaders can't be drawn without
//! a GPU; they load, and run everything but the shaders. The presets this OS
//! ships are the original pack's shader-free ones.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod audio;
pub mod eel;
pub mod preset;
pub mod render;

pub use audio::{Analyzer, SAMPLES};
pub use preset::{Preset, PresetError};
pub use render::{LoadReport, Visualizer};
