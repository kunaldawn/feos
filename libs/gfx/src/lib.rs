//! `feos-gfx` is a small, dependency-free 2D graphics library for the feos
//! kernel. It targets a caller-provided 32-bit **XRGB8888** pixel buffer and
//! works in `no_std` environments.
//!
//! Coordinates are signed, so shapes can extend past the edges of the canvas
//! and are clipped naturally. The library never allocates.
//!
//! ```
//! use feos_gfx::{Canvas, Color, Rect};
//!
//! let mut pixels = [0u32; 64 * 32];
//! let mut canvas = Canvas::new(&mut pixels, 64, 32, 64).unwrap();
//!
//! canvas.clear(Color::BLACK);
//! canvas.fill_rect(Rect::new(4, 4, 20, 10), Color::RED);
//! canvas.draw_text(4, 20, "hi", Color::WHITE, 1);
//! ```
//!
//! All drawing is immediate: once a method returns, the pixels in the backing
//! buffer have been updated.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod canvas;
pub mod color;
pub mod font;
mod font_extended;
pub mod geometry;

pub use canvas::Canvas;
pub use color::Color;
pub use geometry::{Point, Rect, Size};
