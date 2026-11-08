//! VGA, EGA and CGA adapter.
//!
//! The registers are modelled the way the hardware has them: sequencer,
//! graphics controller, CRTC, attribute controller and DAC. Everything the
//! renderer needs is derived from those registers, so a program that tweaks
//! them (Mode X, 320x400, split screens, page flipping) sees what it would see
//! on a real card.
//!
//! Memory lives in two places:
//!
//! * text and CGA modes keep their buffers in the main `Memory` at `B000:0` or
//!   `B800:0`, where programs write to them directly;
//! * EGA and VGA graphics modes live in four 64 KiB planes behind the graphics
//!   controller. The machine asks `is_vram` for each `A0000-BFFFF` access and
//!   routes the ones that say yes to `vram_read` and `vram_write`. The text
//!   font also lives here, in plane 2, as on the real card.
//!
//! The planes are stored as one `u32` per address, with plane N in byte N. The
//! latches are one such word, which makes the write modes a few lines of
//! word-wide logic and the renderer a table lookup per character clock.
//!
//! The renderer turns the active mode into a XRGB image in the top-left
//! `display_size()` of a `SCREEN_WIDTH` by `SCREEN_HEIGHT` buffer. Register
//! values can never index outside the buffer or the planes: every address is
//! masked and every size is clamped when the registers are decoded.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use feos_gfx::Canvas;

use crate::font;
use crate::memory::{Memory, BDA_BASE, MONO_BASE, VIDEO_BASE};

/// Text columns in the default mode.
pub const COLUMNS: usize = 80;
/// Text rows in the default mode.
pub const ROWS: usize = 25;
/// Pixel width of the rendered screen.
pub const SCREEN_WIDTH: usize = 640;
/// Pixel height of the rendered screen.
pub const SCREEN_HEIGHT: usize = 480;

/// Addresses per plane.
const PLANE_SIZE: usize = 0x1_0000;
/// Size of the banked VESA memory.
pub(crate) const SVGA_SIZE: usize = 0x10_0000;
/// A text mode's buffer wraps at this size.
const TEXT_WINDOW: u32 = 0x8000;
/// Pixels decoded for one line: the screen width and a panning margin.
const LINE_PIXELS: usize = SCREEN_WIDTH + 16;

/// ROM layout, inside the `F1000-F3FFF` range reserved for video.
pub(crate) const ROM_SEGMENT: u16 = 0xF000;
pub(crate) const ROM_FONT8: u32 = 0xF_1000;
pub(crate) const ROM_FONT16: u32 = 0xF_1800;
pub(crate) const ROM_FONT14: u32 = 0xF_2800;
pub(crate) const ROM_STATIC: u32 = 0xF_3600;
pub(crate) const ROM_ALT_FONT: u32 = 0xF_3610;
pub(crate) const ROM_VESA_OEM: u32 = 0xF_3620;
pub(crate) const ROM_VESA_MODES: u32 = 0xF_3630;
pub(crate) const ROM_VESA_WINDOW: u32 = 0xF_3640;
pub(crate) const ROM_VESA_PM: u32 = 0xF_3650;
/// The VBE 2.0 protected-mode interface, as `AX=4F0Ah` hands it out: a table of
/// offsets (window, display start, palette, ports used) and 32-bit code that
/// talks to the private ports `1CEh` to `1D3h` and the DAC. It's position
/// independent, because programs copy it into their own segments.
pub(crate) const VESA_PM_TABLE: [u8; 113] = [
    0x08, 0x00, 0x16, 0x00, 0x2d, 0x00, 0x5d, 0x00, 0x52, 0x50, 0x89, 0xd0,
  0xba, 0xce, 0x01, 0x00, 0x00, 0x66, 0xef, 0x58, 0x5a, 0xc3, 0x52, 0x50,
  0x52, 0x89, 0xc8, 0xba, 0xd0, 0x01, 0x00, 0x00, 0x66, 0xef, 0x58, 0xba,
  0xd2, 0x01, 0x00, 0x00, 0x66, 0xef, 0x58, 0x5a, 0xc3, 0x50, 0x51, 0x52,
  0x57, 0x89, 0xd0, 0xba, 0xc8, 0x03, 0x00, 0x00, 0xee, 0x42, 0xf7, 0xc1,
  0xff, 0xff, 0x00, 0x00, 0x74, 0x16, 0x0f, 0xb7, 0xc9, 0x26, 0x8a, 0x47,
  0x02, 0xee, 0x26, 0x8a, 0x47, 0x01, 0xee, 0x26, 0x8a, 0x07, 0xee, 0x83,
  0xc7, 0x04, 0xe2, 0xed, 0x5f, 0x5a, 0x59, 0x58, 0xc3, 0xce, 0x01, 0xcf,
  0x01, 0xd0, 0x01, 0xd1, 0x01, 0xd2, 0x01, 0xd3, 0x01, 0xc8, 0x03, 0xc9,
  0x03, 0xff, 0xff, 0xff, 0xff,
];

/// The VGA static functionality table returned by `INT 10h AX=1B00h`.
const STATIC_TABLE: [u8; 16] = [
    0xFF, 0xE0, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x07, 0x02, 0x08, 0xE7, 0x0C, 0x00, 0x00, 0x00, 0x00,
];

/// What kind of framebuffer the adapter is displaying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphicsKind {
    Text,
    Cga320,
    Cga640,
    Vga13,
    Planar,
    Vesa,
}

/// The register values and BIOS facts of one standard mode.
pub(crate) struct ModeTable {
    pub(crate) mode: u8,
    misc: u8,
    seq: [u8; 4],
    crtc: [u8; 25],
    ac: [u8; 20],
    gc: [u8; 9],
    dac: u8,
    pub(crate) char_height: u8,
    page_size: u16,
    pages: u8,
    cga_mode: u8,
    cga_palette: u8,
}

const AC_TEXT: [u8; 20] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F,
    0x0C, 0x00, 0x0F, 0x08,
];
const AC_CGA4: [u8; 20] = [
    0x00, 0x13, 0x15, 0x17, 0x02, 0x04, 0x06, 0x07, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
    0x01, 0x00, 0x03, 0x00,
];
const AC_CGA2: [u8; 20] = [
    0x00, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17, 0x17,
    0x01, 0x00, 0x01, 0x00,
];
const AC_MONO_TEXT: [u8; 20] = [
    0x00, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x10, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
    0x0E, 0x00, 0x0F, 0x08,
];
const AC_EGA_LOW: [u8; 20] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
    0x01, 0x00, 0x0F, 0x00,
];
const AC_EGA_MONO: [u8; 20] = [
    0x00, 0x08, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00,
];
const AC_EGA_HIGH: [u8; 20] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F,
    0x01, 0x00, 0x0F, 0x00,
];
const AC_VGA_MONO: [u8; 20] = [
    0x00, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F, 0x3F,
    0x01, 0x00, 0x0F, 0x00,
];
const AC_256: [u8; 20] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x41, 0x00, 0x0F, 0x00,
];

const GC_TEXT: [u8; 9] = [0, 0, 0, 0, 0, 0x10, 0x0E, 0x0F, 0xFF];
const GC_MONO_TEXT: [u8; 9] = [0, 0, 0, 0, 0, 0x10, 0x0A, 0x0F, 0xFF];
const GC_CGA4: [u8; 9] = [0, 0, 0, 0, 0, 0x30, 0x0F, 0x0F, 0xFF];
const GC_CGA2: [u8; 9] = [0, 0, 0, 0, 0, 0x00, 0x0D, 0x0F, 0xFF];
const GC_PLANAR: [u8; 9] = [0, 0, 0, 0, 0, 0x00, 0x05, 0x0F, 0xFF];
const GC_256: [u8; 9] = [0, 0, 0, 0, 0, 0x40, 0x05, 0x0F, 0xFF];

const CRTC_40: [u8; 25] = [
    0x2D, 0x27, 0x28, 0x90, 0x2B, 0xA0, 0xBF, 0x1F, 0x00, 0x4F, 0x0D, 0x0E, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x14, 0x1F, 0x96, 0xB9, 0xA3, 0xFF,
];
const CRTC_80: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x55, 0x81, 0xBF, 0x1F, 0x00, 0x4F, 0x0D, 0x0E, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x28, 0x1F, 0x96, 0xB9, 0xA3, 0xFF,
];
const CRTC_CGA4: [u8; 25] = [
    0x2D, 0x27, 0x28, 0x90, 0x2B, 0x80, 0xBF, 0x1F, 0x00, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x14, 0x00, 0x96, 0xB9, 0xA2, 0xFF,
];
const CRTC_CGA2: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0xBF, 0x1F, 0x00, 0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x28, 0x00, 0x96, 0xB9, 0xC2, 0xFF,
];
const CRTC_MONO: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x55, 0x81, 0xBF, 0x1F, 0x00, 0x4F, 0x0D, 0x0E, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x28, 0x0F, 0x96, 0xB9, 0xA3, 0xFF,
];
const CRTC_0D: [u8; 25] = [
    0x2D, 0x27, 0x28, 0x90, 0x2B, 0x80, 0xBF, 0x1F, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x14, 0x00, 0x96, 0xB9, 0xE3, 0xFF,
];
const CRTC_0E: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0xBF, 0x1F, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x28, 0x00, 0x96, 0xB9, 0xE3, 0xFF,
];
const CRTC_0F: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0xBF, 0x1F, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x83, 0x85, 0x5D, 0x28, 0x0F, 0x63, 0xBA, 0xE3, 0xFF,
];
const CRTC_11: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0x0B, 0x3E, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xEA, 0x8C, 0xDF, 0x28, 0x00, 0xE7, 0x04, 0xC3, 0xFF,
];
const CRTC_12: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0x0B, 0x3E, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xEA, 0x8C, 0xDF, 0x28, 0x00, 0xE7, 0x04, 0xE3, 0xFF,
];
const CRTC_13: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0xBF, 0x1F, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x9C, 0x8E, 0x8F, 0x28, 0x40, 0x96, 0xB9, 0xA3, 0xFF,
];

/// DAC tables loaded by a mode set.
const DAC_MONO: u8 = 0;
const DAC_CGA: u8 = 1;
const DAC_EGA: u8 = 2;
const DAC_VGA: u8 = 3;

macro_rules! mode_table {
    ($mode:expr, $misc:expr, $seq:expr, $crtc:expr, $ac:expr, $gc:expr, $dac:expr, $height:expr,
     $size:expr, $pages:expr, $cga_mode:expr, $cga_palette:expr) => {
        ModeTable {
            mode: $mode,
            misc: $misc,
            seq: $seq,
            crtc: $crtc,
            ac: $ac,
            gc: $gc,
            dac: $dac,
            char_height: $height,
            page_size: $size,
            pages: $pages,
            cga_mode: $cga_mode,
            cga_palette: $cga_palette,
        }
    };
}

const SEQ_TEXT40: [u8; 4] = [0x08, 0x03, 0x00, 0x02];
const SEQ_TEXT80: [u8; 4] = [0x00, 0x03, 0x00, 0x02];
const SEQ_CGA4: [u8; 4] = [0x09, 0x03, 0x00, 0x02];
const SEQ_CGA2: [u8; 4] = [0x01, 0x01, 0x00, 0x06];
const SEQ_0D: [u8; 4] = [0x09, 0x0F, 0x00, 0x06];
const SEQ_EGA: [u8; 4] = [0x01, 0x0F, 0x00, 0x06];
const SEQ_256: [u8; 4] = [0x01, 0x0F, 0x00, 0x0E];

/// The standard modes. A mode that is not here is not a mode.
const MODES: [ModeTable; 15] = [
    mode_table!(
        0x00, 0x67, SEQ_TEXT40, CRTC_40, AC_TEXT, GC_TEXT, DAC_EGA, 16, 0x0800, 8, 0x2C, 0x30
    ),
    mode_table!(
        0x01, 0x67, SEQ_TEXT40, CRTC_40, AC_TEXT, GC_TEXT, DAC_EGA, 16, 0x0800, 8, 0x28, 0x30
    ),
    mode_table!(
        0x02, 0x67, SEQ_TEXT80, CRTC_80, AC_TEXT, GC_TEXT, DAC_EGA, 16, 0x1000, 8, 0x2D, 0x30
    ),
    mode_table!(
        0x03, 0x67, SEQ_TEXT80, CRTC_80, AC_TEXT, GC_TEXT, DAC_EGA, 16, 0x1000, 8, 0x29, 0x30
    ),
    mode_table!(
        0x04, 0x63, SEQ_CGA4, CRTC_CGA4, AC_CGA4, GC_CGA4, DAC_CGA, 8, 0x4000, 1, 0x2A, 0x30
    ),
    mode_table!(
        0x05, 0x63, SEQ_CGA4, CRTC_CGA4, AC_CGA4, GC_CGA4, DAC_CGA, 8, 0x4000, 1, 0x2E, 0x30
    ),
    mode_table!(
        0x06, 0x63, SEQ_CGA2, CRTC_CGA2, AC_CGA2, GC_CGA2, DAC_CGA, 8, 0x4000, 1, 0x1E, 0x3F
    ),
    mode_table!(
        0x07,
        0x66,
        SEQ_TEXT80,
        CRTC_MONO,
        AC_MONO_TEXT,
        GC_MONO_TEXT,
        DAC_MONO,
        16,
        0x1000,
        8,
        0x29,
        0x30
    ),
    mode_table!(
        0x0D, 0x63, SEQ_0D, CRTC_0D, AC_EGA_LOW, GC_PLANAR, DAC_CGA, 8, 0x2000, 8, 0x29, 0x30
    ),
    mode_table!(
        0x0E, 0x63, SEQ_EGA, CRTC_0E, AC_EGA_LOW, GC_PLANAR, DAC_CGA, 8, 0x4000, 4, 0x29, 0x30
    ),
    mode_table!(
        0x0F,
        0xA2,
        SEQ_EGA,
        CRTC_0F,
        AC_EGA_MONO,
        GC_PLANAR,
        DAC_MONO,
        14,
        0x8000,
        2,
        0x29,
        0x30
    ),
    mode_table!(
        0x10,
        0xA3,
        SEQ_EGA,
        CRTC_0F,
        AC_EGA_HIGH,
        GC_PLANAR,
        DAC_EGA,
        14,
        0x8000,
        2,
        0x29,
        0x30
    ),
    mode_table!(
        0x11,
        0xE3,
        SEQ_EGA,
        CRTC_11,
        AC_VGA_MONO,
        GC_PLANAR,
        DAC_EGA,
        16,
        0xA000,
        1,
        0x29,
        0x30
    ),
    mode_table!(
        0x12,
        0xE3,
        SEQ_EGA,
        CRTC_12,
        AC_EGA_HIGH,
        GC_PLANAR,
        DAC_EGA,
        16,
        0xA000,
        1,
        0x29,
        0x30
    ),
    mode_table!(0x13, 0x63, SEQ_256, CRTC_13, AC_256, GC_256, DAC_VGA, 8, 0xFA00, 1, 0x29, 0x30),
];

/// The VBE mode number for the current BIOS mode byte, for the two banked modes.
const VESA_BIOS_MODE_400: u8 = 0x5E;
const VESA_BIOS_MODE_480: u8 = 0x5F;

/// Spreads a 4-bit plane mask to a mask with one byte per plane.
const EXPAND: [u32; 16] = {
    let mut table = [0u32; 16];
    let mut index = 0;
    while index < 16 {
        let mut plane = 0;
        while plane < 4 {
            if index & (1 << plane) != 0 {
                table[index] |= 0xFF << (plane * 8);
            }
            plane += 1;
        }
        index += 1;
    }
    table
};

/// Spreads the bits of a plane byte to one bit per pixel byte, leftmost pixel
/// in the most significant byte.
const PIXEL_BITS: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut index = 0;
    while index < 256 {
        let mut bit = 0;
        while bit < 8 {
            if index & (0x80 >> bit) != 0 {
                table[index] |= 1u64 << (56 - 8 * bit);
            }
            bit += 1;
        }
        index += 1;
    }
    table
};

/// The video adapter state.
pub struct Vga {
    pub mode: u8,
    pub columns: usize,
    pub rows: usize,
    pub page: u8,
    /// Cursor position of the active page as `(row, column)`, in text cells in
    /// every mode.
    pub cursor: (usize, usize),
    /// The cursor shape as set through the BIOS, before scaling to the font.
    pub cursor_start: u8,
    pub cursor_end: u8,
    pub bell: u32,
    /// The blink phase of the last render.
    pub blink: bool,
    pub dirty: bool,
    pub kind: GraphicsKind,
    /// Logical pixel size of the mode.
    pub width: usize,
    pub height: usize,
    /// Set when INT 10h changed the video mode. The machine clears it.
    pub mode_changed: bool,

    pub(crate) page_cursors: [(u8, u8); 8],
    pub(crate) no_clear: bool,
    pub(crate) char_height: u8,
    pub(crate) page_size: u16,
    pub(crate) pages: u8,
    pub(crate) cursor_emulation: bool,
    pub(crate) scanline_select: u8,
    pub(crate) default_palette_off: bool,

    pub(crate) seq: [u8; 5],
    pub(crate) seq_index: u8,
    pub(crate) gc: [u8; 9],
    pub(crate) gc_index: u8,
    pub(crate) crtc: [u8; 25],
    pub(crate) crtc_index: u8,
    pub(crate) ac: [u8; 21],
    pub(crate) ac_index: u8,
    pub(crate) ac_flip: bool,
    pub(crate) misc: u8,
    pub(crate) feature: u8,

    pub(crate) dac: [[u8; 3]; 256],
    pub(crate) dac_write_index: u8,
    pub(crate) dac_write_component: u8,
    pub(crate) dac_read_index: u8,
    pub(crate) dac_read_component: u8,
    pub(crate) dac_reading: bool,
    pub(crate) pel_mask: u8,
    pub(crate) cga_mode: u8,
    pub(crate) cga_palette: u8,

    pub(crate) planes: Box<[u32; PLANE_SIZE]>,
    pub(crate) latch: u32,

    /// DAC index to XRGB, through the PEL mask.
    pub(crate) lut: [u32; 256],
    /// What the VBE protected-mode code wrote to its private ports.
    vesa_pm_latch: [u8; 6],
    /// 4-bit pixel to XRGB, through the attribute controller and the DAC.
    pal16: [u32; 16],
    lut_dirty: bool,
    pal_dirty: bool,
    /// How many times the DAC table was rebuilt.
    pub(crate) lut_builds: u32,

    map_low: u32,
    map_high: u32,
    display_width: usize,
    display_height: usize,
    pixel_scale: usize,
    logical_line: usize,

    /// Graphics font set through `AX=1121h` and friends, as a physical address
    /// and a height. `None` is the ROM font of the mode.
    pub(crate) gfx_font: Option<(u32, u8)>,
    /// What `AX=1130h` reports for the `INT 1Fh` and `INT 43h` pointers.
    pub(crate) int1f: (u16, u16),
    pub(crate) int43: (u16, u16),
    rom_installed: bool,

    /// Banked memory of the VESA modes, allocated when one is first set.
    pub(crate) svga: Vec<u8>,
    pub(crate) vesa_mode: u16,
    pub(crate) vesa_bank: u16,
    /// Bytes per line in banked memory (VBE function 06h).
    pub(crate) vesa_pitch: usize,
    /// The first displayed byte in banked memory (VBE function 07h).
    pub(crate) vesa_start: usize,
    #[cfg(test)]
    pub(crate) fail_alloc: bool,

    /// Memory operations done by BIOS services, for `int10`'s cost.
    pub(crate) work: u32,
}

impl Default for Vga {
    fn default() -> Self {
        Self::new()
    }
}

fn new_planes() -> Box<[u32; PLANE_SIZE]> {
    match vec![0u32; PLANE_SIZE].into_boxed_slice().try_into() {
        Ok(planes) => planes,
        Err(_) => unreachable!(),
    }
}

/// Where a character map starts in plane 2.
pub(crate) fn font_block_offset(block: u8) -> usize {
    match block & 7 {
        0 => 0x0000,
        1 => 0x4000,
        2 => 0x8000,
        3 => 0xC000,
        4 => 0x2000,
        5 => 0x6000,
        6 => 0xA000,
        _ => 0xE000,
    }
}

/// Far pointer into the video ROM.
pub(crate) fn rom_ptr(addr: u32) -> (u16, u16) {
    (ROM_SEGMENT, (addr & 0xFFFF) as u16)
}

/// Looks up a standard mode.
pub(crate) fn mode_table(mode: u8) -> Option<&'static ModeTable> {
    MODES.iter().find(|table| table.mode == mode)
}

/// One DAC entry of the BIOS tables, for an index below 64 (or 256 for
/// `DAC_VGA`).
fn dac_default(table: u8, index: usize) -> [u8; 3] {
    match table {
        DAC_MONO => {
            let value = match index & 0x18 {
                0x00 => 0x00,
                0x18 => 0x3F,
                _ => 0x2A,
            };
            if index < 64 {
                [value; 3]
            } else {
                [0; 3]
            }
        }
        DAC_CGA => {
            if index >= 64 {
                return [0; 3];
            }
            let bright = if index & 0x10 != 0 { 0x15 } else { 0 };
            let level = |bit: usize| {
                if index & bit != 0 {
                    0x2A + bright
                } else {
                    bright
                }
            };
            let green = if index & 7 == 6 && bright == 0 {
                0x15
            } else {
                level(2)
            };
            [level(4), green, level(1)]
        }
        DAC_EGA => {
            if index >= 64 {
                return [0; 3];
            }
            let level = |low: usize, high: usize| {
                (if index & low != 0 { 0x2A } else { 0 })
                    + (if index & high != 0 { 0x15 } else { 0 })
            };
            [level(4, 0x20), level(2, 0x10), level(1, 0x08)]
        }
        _ => vga_default_dac(index),
    }
}

/// The standard 256-colour palette: 16 EGA colours, 16 greys, 216 colours in
/// nine rings of 24 hues (three saturations at three intensities) and 8 blacks.
fn vga_default_dac(index: usize) -> [u8; 3] {
    const GREYS: [u8; 16] = [
        0x00, 0x05, 0x08, 0x0B, 0x0E, 0x11, 0x14, 0x18, 0x1C, 0x20, 0x24, 0x28, 0x2D, 0x32, 0x38,
        0x3F,
    ];
    const LEVELS: [[u8; 5]; 9] = [
        [0x00, 0x10, 0x1F, 0x2F, 0x3F],
        [0x1F, 0x27, 0x2F, 0x37, 0x3F],
        [0x2D, 0x31, 0x36, 0x3A, 0x3F],
        [0x00, 0x07, 0x0E, 0x15, 0x1C],
        [0x0E, 0x11, 0x15, 0x18, 0x1C],
        [0x14, 0x16, 0x18, 0x1A, 0x1C],
        [0x00, 0x04, 0x08, 0x0C, 0x10],
        [0x08, 0x0A, 0x0C, 0x0E, 0x10],
        [0x0B, 0x0C, 0x0D, 0x0F, 0x10],
    ];
    match index {
        0..=15 => {
            let (r, g, b) = palette16(index);
            [r >> 2, g >> 2, b >> 2]
        }
        16..=31 => [GREYS[index - 16]; 3],
        32..=247 => {
            let levels = LEVELS[(index - 32) / 24];
            let hue = (index - 32) % 24;
            // Red rises, green rises and blue falls around the ring, each as a
            // ramp over four steps.
            let ramp = |phase: usize| -> u8 {
                let at = (hue + 24 - phase) % 24;
                match at {
                    0..=4 => levels[at],
                    5..=12 => levels[4],
                    13..=16 => levels[4 - (at - 12)],
                    _ => levels[0],
                }
            };
            [ramp(0), ramp(8), ramp(16)]
        }
        _ => [0; 3],
    }
}

/// How the CRTC turns its character counter into a plane address.
#[derive(Clone, Copy)]
struct CrtcAddressing {
    /// 0 for byte, 1 for word, 2 for doubleword mode.
    shift: u32,
    wrap_bit_15: bool,
    /// Address bit 13 comes from row scan bit 0 (CGA interleave).
    scan_to_13: bool,
    /// Address bit 14 comes from row scan bit 1.
    scan_to_14: bool,
}

impl CrtcAddressing {
    #[inline]
    fn address(&self, counter: u32, scan: u32) -> u32 {
        let counter = counter & 0xFFFF;
        let mut address = match self.shift {
            0 => counter,
            1 => {
                let wrap = if self.wrap_bit_15 { 15 } else { 13 };
                (counter << 1) | ((counter >> wrap) & 1)
            }
            _ => (counter << 2) | ((counter >> 14) & 3),
        };
        if self.scan_to_13 {
            address = (address & !0x2000) | ((scan & 1) << 13);
        }
        if self.scan_to_14 {
            address = (address & !0x4000) | (((scan >> 1) & 1) << 14);
        }
        address & 0xFFFF
    }
}

/// Follows the CRTC down the screen: row scan, double scan, line compare.
struct LineWalker {
    counter: u32,
    scan: u32,
    repeat: u32,
    repeats: u32,
    scans: u32,
    stride: u32,
    compare: u32,
    split: bool,
}

impl LineWalker {
    #[inline]
    fn begin_line(&mut self, y: u32) {
        if y == self.compare.wrapping_add(1) {
            self.counter = 0;
            self.scan = 0;
            self.repeat = 0;
            self.split = true;
        }
    }

    #[inline]
    fn end_line(&mut self) {
        self.repeat += 1;
        if self.repeat >= self.repeats {
            self.repeat = 0;
            self.scan += 1;
            if self.scan >= self.scans {
                self.scan = 0;
                self.counter = self.counter.wrapping_add(self.stride) & 0xFFFF;
            }
        }
    }
}

/// One text cell, decoded once per character row.
#[derive(Clone, Copy, Default)]
struct TextCell {
    glyph: u32,
    foreground: u32,
    background: u32,
    cursor: bool,
    underline: bool,
}

fn dac_to_xrgb(entry: [u8; 3]) -> u32 {
    let expand = |value: u8| {
        let value = (value & 0x3F) as u32;
        (value << 2) | (value >> 4)
    };
    (expand(entry[0]) << 16) | (expand(entry[1]) << 8) | expand(entry[2])
}

impl Vga {
    pub fn new() -> Self {
        let mut vga = Self {
            mode: 3,
            columns: COLUMNS,
            rows: ROWS,
            page: 0,
            cursor: (0, 0),
            cursor_start: 0x06,
            cursor_end: 0x07,
            bell: 0,
            blink: true,
            dirty: true,
            kind: GraphicsKind::Text,
            width: SCREEN_WIDTH,
            height: 400,
            mode_changed: false,
            page_cursors: [(0, 0); 8],
            no_clear: false,
            char_height: 16,
            page_size: 0x1000,
            pages: 8,
            cursor_emulation: true,
            scanline_select: 2,
            default_palette_off: false,
            seq: [0; 5],
            seq_index: 0,
            gc: [0; 9],
            gc_index: 0,
            crtc: [0; 25],
            crtc_index: 0,
            ac: [0; 21],
            ac_index: 0x20,
            ac_flip: false,
            misc: 0,
            feature: 0,
            dac: [[0; 3]; 256],
            dac_write_index: 0,
            dac_write_component: 0,
            dac_read_index: 0,
            dac_read_component: 0,
            dac_reading: false,
            pel_mask: 0xFF,
            cga_mode: 0x29,
            cga_palette: 0x30,
            planes: new_planes(),
            latch: 0,
            lut: [0; 256],
            vesa_pm_latch: [0; 6],
            pal16: [0; 16],
            lut_dirty: true,
            pal_dirty: true,
            lut_builds: 0,
            map_low: 0,
            map_high: 0,
            display_width: SCREEN_WIDTH,
            display_height: 400,
            pixel_scale: 1,
            logical_line: SCREEN_WIDTH,
            gfx_font: None,
            int1f: rom_ptr(ROM_FONT8 + 0x400),
            int43: rom_ptr(ROM_FONT16),
            rom_installed: false,
            svga: Vec::new(),
            vesa_mode: 0,
            vesa_bank: 0,
            vesa_pitch: SCREEN_WIDTH,
            vesa_start: 0,
            #[cfg(test)]
            fail_alloc: false,
            work: 0,
        };
        if let Some(table) = mode_table(3) {
            vga.load_registers(table);
        }
        vga.load_rom_font(16, 0);
        vga.update_geometry();
        vga
    }

    /// Resets to mode 3 and clears the screen.
    pub fn reset(&mut self, mem: &mut Memory) {
        self.bell = 0;
        self.scanline_select = 2;
        self.cursor_emulation = true;
        self.default_palette_off = false;
        self.gfx_font = None;
        self.set_mode(mem, 3);
        self.mode_changed = false;
    }

    /// Installs the fonts, the static functionality table and the VESA data in
    /// the ROM area, and points `INT 1Fh` and `INT 43h` at the fonts.
    pub fn install_rom(&mut self, mem: &mut Memory) {
        for code in 0..=255u8 {
            let index = code as u32;
            mem.copy_from(ROM_FONT8 + index * 8, &font::rom8(code));
            mem.copy_from(ROM_FONT16 + index * 16, &font::rom16(code));
            mem.copy_from(ROM_FONT14 + index * 14, &font::rom14(code));
        }
        mem.copy_from(ROM_STATIC, &STATIC_TABLE);
        // An empty alternate-glyph table for the 9-dot font requests.
        mem.write16(ROM_ALT_FONT, 0);
        mem.copy_from(ROM_VESA_OEM, b"FEOS-DOS\0");
        mem.write16(ROM_VESA_MODES, 0x0100);
        mem.write16(ROM_VESA_MODES + 2, 0x0101);
        mem.write16(ROM_VESA_MODES + 4, 0xFFFF);
        // MOV AX,4F05h; INT 10h; RETF: the far window function of VBE 1.2.
        mem.copy_from(ROM_VESA_WINDOW, &[0xB8, 0x05, 0x4F, 0xCD, 0x10, 0xCB]);
        mem.copy_from(ROM_VESA_PM, &VESA_PM_TABLE);
        self.rom_installed = true;
        self.sync_font_vectors(mem);
    }

    /// Writes the font pointers to the interrupt vector table, where programs
    /// read them. These vectors are data, not code.
    pub(crate) fn sync_font_vectors(&self, mem: &mut Memory) {
        if !self.rom_installed {
            return;
        }
        mem.write16(0x1F * 4, self.int1f.1);
        mem.write16(0x1F * 4 + 2, self.int1f.0);
        mem.write16(0x43 * 4, self.int43.1);
        mem.write16(0x43 * 4 + 2, self.int43.0);
    }

    fn base(&self) -> u32 {
        if self.mode == 7 {
            MONO_BASE
        } else {
            VIDEO_BASE
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_text(&self) -> bool {
        self.kind == GraphicsKind::Text
    }

    pub fn is_planar(&self) -> bool {
        self.kind == GraphicsKind::Planar
    }

    /// The current display size in screen pixels.
    pub fn display_size(&self) -> (usize, usize) {
        (self.display_width, self.display_height)
    }

    // ------------------------------------------------------------------
    // Modes
    // ------------------------------------------------------------------

    fn load_registers(&mut self, table: &ModeTable) {
        self.misc = table.misc;
        self.seq[0] = 0x03;
        self.seq[1..5].copy_from_slice(&table.seq);
        self.seq_index = 0;
        self.crtc = table.crtc;
        self.crtc_index = 0;
        self.gc = table.gc;
        self.gc_index = 0;
        self.ac[..20].copy_from_slice(&table.ac);
        self.ac[20] = 0;
        self.ac_index = 0x20;
        self.ac_flip = false;
        self.pel_mask = 0xFF;
        self.latch = 0;
        self.char_height = table.char_height;
        self.page_size = table.page_size;
        self.pages = table.pages;
        self.cga_mode = table.cga_mode;
        self.cga_palette = table.cga_palette;
        self.mode = table.mode;
        if !self.default_palette_off {
            for (index, entry) in self.dac.iter_mut().enumerate() {
                *entry = dac_default(table.dac, index);
            }
            self.dac_write_index = 0;
            self.dac_write_component = 0;
            self.dac_read_index = 0;
            self.dac_read_component = 0;
            self.dac_reading = false;
            self.lut_dirty = true;
        }
        self.pal_dirty = true;
    }

    /// Sets a BIOS video mode. Bit 7 keeps the video memory. An unknown mode
    /// changes nothing and returns false.
    pub fn set_mode(&mut self, mem: &mut Memory, mode: u8) -> bool {
        let Some(table) = mode_table(mode & 0x7F) else {
            return false;
        };
        self.no_clear = mode & 0x80 != 0;
        self.vesa_mode = 0;
        self.vesa_bank = 0;
        self.vesa_pitch = SCREEN_WIDTH;
        self.vesa_start = 0;
        self.gfx_font = None;
        self.load_registers(table);
        self.page = 0;
        self.page_cursors = [(0, 0); 8];
        self.cursor = (0, 0);

        let text = table.mode <= 3 || table.mode == 7;
        if text && table.mode != 7 {
            self.apply_scanline_select();
        }
        if !self.no_clear {
            self.planes.fill(0);
            match table.mode {
                0..=3 | 7 => {
                    let base = self.base();
                    let mut offset = 0;
                    while offset < TEXT_WINDOW {
                        mem.write16(base + offset, 0x0720);
                        offset += 2;
                    }
                }
                4..=6 => mem.fill(VIDEO_BASE, TEXT_WINDOW as usize, 0),
                _ => {}
            }
        }
        if text {
            self.load_rom_font(self.char_height, 0);
        }
        self.int43 = rom_ptr(match table.char_height {
            8 => ROM_FONT8,
            14 => ROM_FONT14,
            _ => ROM_FONT16,
        });
        self.int1f = rom_ptr(ROM_FONT8 + 0x400);
        self.update_geometry();
        if table.mode == 7 {
            self.cursor_start = 0x0B;
            self.cursor_end = 0x0C;
        } else {
            self.cursor_start = 0x06;
            self.cursor_end = 0x07;
        }
        self.program_cursor_shape();
        self.program_cursor_location();
        self.sync_bios(mem);
        self.sync_font_vectors(mem);
        self.mode_changed = true;
        self.dirty = true;
        true
    }

    /// Adjusts a colour text mode for `AH=12h BL=30h`.
    fn apply_scanline_select(&mut self) {
        match self.scanline_select {
            0 => {
                self.crtc[0x09] = 0xC7;
                self.char_height = 8;
            }
            1 => {
                self.misc = 0xA3 | (self.misc & 0x04);
                self.crtc[0x09] = 0x4D;
                self.crtc[0x12] = 0x5D;
                self.char_height = 14;
            }
            _ => {}
        }
    }

    /// Loads the ROM font of a height into a character map of plane 2.
    pub(crate) fn load_rom_font(&mut self, height: u8, block: u8) {
        let base = font_block_offset(block);
        for code in 0..=255u8 {
            let at = base + code as usize * 32;
            let mut lines = [0u8; 32];
            match height {
                8 => lines[..8].copy_from_slice(&font::rom8(code)),
                14 => lines[..14].copy_from_slice(&font::rom14(code)),
                _ => lines[..16].copy_from_slice(&font::rom16(code)),
            }
            for (line, bits) in lines.iter().enumerate() {
                self.set_plane_byte(at + line, 2, *bits);
            }
        }
        self.dirty = true;
    }

    #[inline]
    pub(crate) fn set_plane_byte(&mut self, offset: usize, plane: u32, value: u8) {
        let word = &mut self.planes[offset & (PLANE_SIZE - 1)];
        let shift = (plane & 3) * 8;
        *word = (*word & !(0xFF << shift)) | ((value as u32) << shift);
    }

    #[inline]
    pub(crate) fn plane_byte(&self, offset: usize, plane: u32) -> u8 {
        (self.planes[offset & (PLANE_SIZE - 1)] >> ((plane & 3) * 8)) as u8
    }

    /// The number of scan lines the CRTC displays, unclamped.
    fn scan_lines(&self) -> usize {
        let overflow = self.crtc[0x07] as usize;
        let end = self.crtc[0x12] as usize | ((overflow & 0x02) << 7) | ((overflow & 0x40) << 3);
        end + 1
    }

    pub(crate) fn cell_scans(&self) -> usize {
        (self.crtc[0x09] as usize & 0x1F) + 1
    }

    fn double_scan(&self) -> bool {
        self.crtc[0x09] & 0x80 != 0
    }

    /// Recomputes everything derived from the registers.
    pub(crate) fn update_geometry(&mut self) {
        let graphics = self.ac[0x10] & 0x01 != 0;
        let map = (self.gc[0x06] >> 2) & 3;
        self.kind = if self.vesa_mode != 0 {
            GraphicsKind::Vesa
        } else if !graphics {
            GraphicsKind::Text
        } else if self.gc[0x05] & 0x40 != 0 {
            GraphicsKind::Vga13
        } else if map == 3 {
            if self.gc[0x05] & 0x20 != 0 {
                GraphicsKind::Cga320
            } else {
                GraphicsKind::Cga640
            }
        } else {
            GraphicsKind::Planar
        };

        let (low, high) = match map {
            0 => (0xA_0000, 0xC_0000),
            1 => (0xA_0000, 0xB_0000),
            2 => (0xB_0000, 0xB_8000),
            _ => (0xB_8000, 0xC_0000),
        };
        (self.map_low, self.map_high) = match self.kind {
            GraphicsKind::Vesa => (0xA_0000, 0xB_0000),
            GraphicsKind::Vga13 | GraphicsKind::Planar => (low, high),
            // Text and CGA buffers are in Memory; only a map that reaches the
            // A000 segment, as font loaders program, goes to the planes.
            _ if map <= 1 => (0xA_0000, 0xB_0000),
            _ => (0, 0),
        };

        let clocks = self.crtc[0x01] as usize + 1;
        let half_clock = self.seq[0x01] & 0x08 != 0;
        let scans = self.scan_lines().min(SCREEN_HEIGHT);
        let repeats = if self.double_scan() { 2 } else { 1 };
        let per_row = self.cell_scans();
        self.display_height = scans.max(1);

        let (line, scale) = match self.kind {
            GraphicsKind::Vesa => (SCREEN_WIDTH, 1),
            GraphicsKind::Vga13 => {
                let line = clocks * 4;
                (line, if line * 2 <= SCREEN_WIDTH { 2 } else { 1 })
            }
            _ => (clocks * 8, if half_clock { 2 } else { 1 }),
        };
        self.logical_line = line.clamp(1, SCREEN_WIDTH);
        self.pixel_scale = scale;
        self.display_width = (self.logical_line * scale).clamp(1, SCREEN_WIDTH);
        self.width = self.logical_line;

        match self.kind {
            GraphicsKind::Text => {
                self.columns = clocks.clamp(1, 256);
                self.rows = (scans / repeats / per_row).clamp(1, 255);
                self.height = (self.display_height / scale).max(1);
            }
            GraphicsKind::Vesa => {
                self.height = self.display_height;
                self.columns = SCREEN_WIDTH / 8;
                self.rows = (self.height / self.char_height.max(1) as usize).clamp(1, 255);
            }
            GraphicsKind::Cga320 | GraphicsKind::Cga640 => {
                self.height = (scans / repeats).max(1);
                self.columns = (self.width / 8).max(1);
                self.rows = (self.height / self.char_height.max(1) as usize).clamp(1, 255);
            }
            _ => {
                self.height = (scans / repeats / per_row).max(1);
                self.columns = (self.width / 8).max(1);
                self.rows = (self.height / self.char_height.max(1) as usize).clamp(1, 255);
            }
        }
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // BIOS data area
    // ------------------------------------------------------------------

    /// Writes the whole video part of the BIOS data area.
    pub fn sync_bios(&self, mem: &mut Memory) {
        mem.write8(BDA_BASE + 0x49, self.mode);
        mem.write16(BDA_BASE + 0x4A, self.columns as u16);
        mem.write16(BDA_BASE + 0x4C, self.page_size);
        mem.write16(BDA_BASE + 0x4E, self.page_start(self.page) as u16);
        for page in 0..8u32 {
            let (row, column) = self.page_cursors[page as usize];
            mem.write16(
                BDA_BASE + 0x50 + page * 2,
                ((row as u16) << 8) | column as u16,
            );
        }
        mem.write16(
            BDA_BASE + 0x60,
            ((self.cursor_start as u16) << 8) | self.cursor_end as u16,
        );
        mem.write8(BDA_BASE + 0x62, self.page);
        mem.write16(BDA_BASE + 0x63, self.crtc_port());
        mem.write8(BDA_BASE + 0x65, self.cga_mode);
        mem.write8(BDA_BASE + 0x66, self.cga_palette);
        mem.write8(BDA_BASE + 0x84, self.rows.saturating_sub(1) as u8);
        mem.write16(BDA_BASE + 0x85, self.char_height as u16);
        let mut info = 0x60;
        if self.no_clear {
            info |= 0x80;
        }
        if !self.cursor_emulation {
            info |= 0x01;
        }
        mem.write8(BDA_BASE + 0x87, info);
        mem.write8(BDA_BASE + 0x88, 0xF9);
        let lines = match self.scanline_select {
            0 => 0x80,
            1 => 0x00,
            _ => 0x10,
        };
        let palette = if self.default_palette_off { 0x08 } else { 0x00 };
        mem.write8(BDA_BASE + 0x89, 0x41 | lines | palette);
    }

    pub(crate) fn crtc_port(&self) -> u16 {
        if self.misc & 0x01 != 0 {
            0x3D4
        } else {
            0x3B4
        }
    }

    /// Byte offset of a BIOS page in its buffer.
    pub(crate) fn page_start(&self, page: u8) -> u32 {
        page as u32 * self.page_size as u32
    }

    // ------------------------------------------------------------------
    // Cursor
    // ------------------------------------------------------------------

    /// A page's cursor as `(row, column)`.
    pub(crate) fn cursor_of(&self, page: u8) -> (usize, usize) {
        if page == self.page {
            self.cursor
        } else {
            let (row, column) = self.page_cursors[page as usize & 7];
            (row as usize, column as usize)
        }
    }

    /// Moves a page's cursor. Positions off the screen are legal: programs use
    /// them to hide the cursor.
    pub(crate) fn set_page_cursor(
        &mut self,
        mem: &mut Memory,
        page: u8,
        row: usize,
        column: usize,
    ) {
        let page = page & 7;
        let row = row.min(255);
        let column = column.min(255);
        self.page_cursors[page as usize] = (row as u8, column as u8);
        mem.write16(
            BDA_BASE + 0x50 + page as u32 * 2,
            ((row as u16) << 8) | column as u16,
        );
        if page == self.page {
            self.cursor = (row, column);
            self.program_cursor_location();
            self.dirty = true;
        }
    }

    /// Moves the active page's cursor, kept on the screen.
    pub fn set_cursor(&mut self, mem: &mut Memory, row: usize, column: usize) {
        let row = row.min(self.rows.saturating_sub(1));
        let column = column.min(self.columns.saturating_sub(1));
        self.set_page_cursor(mem, self.page, row, column);
    }

    /// Points the CRTC cursor location at the active page's cursor.
    pub(crate) fn program_cursor_location(&mut self) {
        let (row, column) = self.cursor;
        let location =
            (self.page_start(self.page) / 2).wrapping_add((row * self.columns + column) as u32);
        self.crtc[0x0E] = (location >> 8) as u8;
        self.crtc[0x0F] = location as u8;
    }

    /// Sets the cursor shape as `AH=01h` does: `start` carries the hide bits.
    pub fn set_cursor_shape(&mut self, mem: &mut Memory, start: u8, end: u8) {
        self.cursor_start = start;
        self.cursor_end = end;
        self.program_cursor_shape();
        mem.write16(BDA_BASE + 0x60, ((start as u16) << 8) | end as u16);
        self.dirty = true;
    }

    /// Writes the BIOS shape to the CRTC, scaling the 8-line shapes programs
    /// use to the real character height.
    pub(crate) fn program_cursor_shape(&mut self) {
        let hidden = self.cursor_start & 0x60 == 0x20;
        let mut start = (self.cursor_start & 0x1F) as u32;
        let mut end = (self.cursor_end & 0x1F) as u32;
        let height = self.cell_scans() as u32;
        if self.cursor_emulation && height >= 14 && start < 8 && end < 8 && start <= end {
            if end != start + 1 {
                start = (start + 1) * height / 8 - 1;
            } else {
                start = (end + 1) * height / 8 - 2;
            }
            end = (end + 1) * height / 8 - 1;
        }
        self.crtc[0x0A] = (start as u8 & 0x1F) | if hidden { 0x20 } else { 0 };
        self.crtc[0x0B] = end as u8 & 0x1F;
    }

    // ------------------------------------------------------------------
    // Text cells
    // ------------------------------------------------------------------

    /// Physical address of a linear cell index on a page.
    #[inline]
    fn text_address(&self, page: u8, cell: usize) -> u32 {
        let offset = self
            .page_start(page)
            .wrapping_add((cell as u32).wrapping_mul(2));
        self.base() + (offset & (TEXT_WINDOW - 1))
    }

    pub(crate) fn write_cell_on(
        &mut self,
        mem: &mut Memory,
        page: u8,
        row: usize,
        column: usize,
        ch: u8,
        attr: Option<u8>,
    ) {
        if row < self.rows && column < self.columns {
            let address = self.text_address(page, row * self.columns + column);
            mem.write8(address, ch);
            if let Some(attr) = attr {
                mem.write8(address + 1, attr);
            }
            self.dirty = true;
        }
    }

    pub(crate) fn read_cell_on(
        &self,
        mem: &Memory,
        page: u8,
        row: usize,
        column: usize,
    ) -> (u8, u8) {
        if row < self.rows && column < self.columns {
            let address = self.text_address(page, row * self.columns + column);
            (mem.read8(address), mem.read8(address + 1))
        } else {
            (b' ', 0x07)
        }
    }

    pub fn write_cell(&mut self, mem: &mut Memory, row: usize, column: usize, ch: u8, attr: u8) {
        self.write_cell_on(mem, self.page, row, column, ch, Some(attr));
    }

    pub fn read_cell(&self, mem: &Memory, row: usize, column: usize) -> (u8, u8) {
        self.read_cell_on(mem, self.page, row, column)
    }

    /// Clears the active page and homes its cursor.
    pub fn clear(&mut self, mem: &mut Memory, attr: u8) {
        let rows = self.rows;
        let columns = self.columns;
        let page = self.page;
        let fill = if self.is_text() { attr } else { 0 };
        self.scroll_window(
            mem,
            page,
            0,
            0,
            rows.saturating_sub(1),
            columns.saturating_sub(1),
            0,
            fill,
            true,
        );
        self.set_page_cursor(mem, page, 0, 0);
    }

    // ------------------------------------------------------------------
    // Scrolling
    // ------------------------------------------------------------------

    /// Scrolls a window of text cells in any mode. `lines` of zero, or at
    /// least the window's height, clears the window. In graphics modes `attr`
    /// is the fill colour.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scroll_window(
        &mut self,
        mem: &mut Memory,
        page: u8,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: usize,
        attr: u8,
        up: bool,
    ) {
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let right = right.min(self.columns.saturating_sub(1));
        if top > bottom || left > right {
            return;
        }
        let height = bottom - top + 1;
        let lines = if lines == 0 || lines > height {
            height
        } else {
            lines
        };
        let keep = height - lines;
        let width = right - left + 1;
        self.dirty = true;
        if self.is_text() {
            self.work = self.work.saturating_add((height * width) as u32);
            for step in 0..height {
                let row = if up { top + step } else { bottom - step };
                if step < keep {
                    let source = if up { row + lines } else { row - lines };
                    for column in left..=right {
                        let (ch, cell) = self.read_cell_on(mem, page, source, column);
                        self.write_cell_on(mem, page, row, column, ch, Some(cell));
                    }
                } else {
                    for column in left..=right {
                        self.write_cell_on(mem, page, row, column, b' ', Some(attr));
                    }
                }
            }
            return;
        }
        let cell = self.gfx_cell_height();
        for step in 0..height * cell {
            let y = if up {
                top * cell + step
            } else {
                (bottom + 1) * cell - 1 - step
            };
            if step < keep * cell {
                let source = if up {
                    y + lines * cell
                } else {
                    y - lines * cell
                };
                self.gfx_copy_line(mem, page, source, y, left, width);
            } else {
                self.gfx_fill_line(mem, page, y, left, width, attr);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn scroll_up(
        &mut self,
        mem: &mut Memory,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: usize,
        attr: u8,
    ) {
        self.scroll_window(mem, self.page, top, left, bottom, right, lines, attr, true);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn scroll_down(
        &mut self,
        mem: &mut Memory,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        lines: usize,
        attr: u8,
    ) {
        self.scroll_window(mem, self.page, top, left, bottom, right, lines, attr, false);
    }

    fn gfx_copy_line(
        &mut self,
        mem: &mut Memory,
        page: u8,
        from: usize,
        to: usize,
        left: usize,
        cells: usize,
    ) {
        if self.kind == GraphicsKind::Planar {
            let stride = self.host_stride();
            let start = self.page_start(page) as usize;
            let source = start + from * stride + left;
            let target = start + to * stride + left;
            for offset in 0..cells {
                let word = self.planes[(source + offset) & (PLANE_SIZE - 1)];
                self.planes[(target + offset) & (PLANE_SIZE - 1)] = word;
            }
            self.work = self.work.saturating_add(cells as u32);
            return;
        }
        for x in left * 8..(left + cells) * 8 {
            let color = self.pixel_on(mem, page, x, from);
            self.plot(mem, page, x, to, color, false);
        }
        self.work = self.work.saturating_add(cells as u32 * 8);
    }

    fn gfx_fill_line(
        &mut self,
        mem: &mut Memory,
        page: u8,
        y: usize,
        left: usize,
        cells: usize,
        color: u8,
    ) {
        if self.kind == GraphicsKind::Planar {
            let stride = self.host_stride();
            let target = self.page_start(page) as usize + y * stride + left;
            let fill = EXPAND[(color & 0x0F) as usize];
            for offset in 0..cells {
                self.planes[(target + offset) & (PLANE_SIZE - 1)] = fill;
            }
            self.work = self.work.saturating_add(cells as u32);
            return;
        }
        for x in left * 8..(left + cells) * 8 {
            self.plot(mem, page, x, y, color, false);
        }
        self.work = self.work.saturating_add(cells as u32 * 4);
    }

    // ------------------------------------------------------------------
    // Teletype
    // ------------------------------------------------------------------

    /// BIOS teletype output (INT 10h function 0Eh).
    pub fn teletype(&mut self, mem: &mut Memory, ch: u8) {
        self.teletype_on(mem, self.page, ch, None);
    }

    /// Teletype on a page. In text modes `color` replaces the cell's attribute
    /// and `None` keeps it; in graphics modes it is the foreground.
    pub(crate) fn teletype_on(&mut self, mem: &mut Memory, page: u8, ch: u8, color: Option<u8>) {
        let (mut row, mut column) = self.cursor_of(page);
        let rows = self.rows.max(1);
        let columns = self.columns.max(1);
        match ch {
            0x07 => self.bell = self.bell.wrapping_add(1),
            0x08 => column = column.saturating_sub(1),
            0x09 => column = ((column + 8) & !7).min(columns - 1),
            0x0A => row += 1,
            0x0D => column = 0,
            _ => {
                self.write_glyph(mem, page, row, column, ch, color);
                column += 1;
                if column >= columns {
                    column = 0;
                    row += 1;
                }
            }
        }
        if row >= rows {
            let attr = if self.is_text() {
                self.read_cell_on(mem, page, rows - 1, column.min(columns - 1))
                    .1
            } else {
                0
            };
            self.scroll_window(mem, page, 0, 0, rows - 1, columns - 1, 1, attr, true);
            row = rows - 1;
        }
        self.set_page_cursor(mem, page, row, column);
        self.dirty = true;
    }

    /// Puts one character at a cell, in the way the mode draws characters.
    pub(crate) fn write_glyph(
        &mut self,
        mem: &mut Memory,
        page: u8,
        row: usize,
        column: usize,
        ch: u8,
        color: Option<u8>,
    ) {
        if self.is_text() {
            self.write_cell_on(mem, page, row, column, ch, color);
        } else {
            self.gfx_char(mem, page, row, column, ch, color.unwrap_or(0x0F));
        }
    }

    pub fn write_str(&mut self, mem: &mut Memory, text: &[u8]) {
        for byte in text {
            self.teletype(mem, *byte);
        }
    }

    pub fn write_line(&mut self, mem: &mut Memory, text: &[u8]) {
        self.write_str(mem, text);
        self.teletype(mem, b'\r');
        self.teletype(mem, b'\n');
    }

    // ------------------------------------------------------------------
    // Graphics characters
    // ------------------------------------------------------------------

    pub(crate) fn gfx_cell_height(&self) -> usize {
        match self.gfx_font {
            Some((_, height)) => (height as usize).clamp(1, 32),
            None => (self.char_height as usize).clamp(1, 32),
        }
    }

    /// The bitmap of a character in the current graphics font, bit 7 on the
    /// left, and its height.
    fn gfx_glyph(&self, mem: &Memory, ch: u8) -> ([u8; 32], usize) {
        let mut lines = [0u8; 32];
        let height = self.gfx_cell_height();
        if let Some((address, font_height)) = self.gfx_font {
            let start = address.wrapping_add(ch as u32 * font_height as u32);
            for (line, slot) in lines.iter_mut().take(height).enumerate() {
                *slot = mem.read8(start.wrapping_add(line as u32));
            }
        } else {
            match height {
                8 => lines[..8].copy_from_slice(&font::rom8(ch)),
                14 => lines[..14].copy_from_slice(&font::rom14(ch)),
                16 => lines[..16].copy_from_slice(&font::rom16(ch)),
                _ => {
                    let source = font::rom16(ch);
                    let count = height.min(16);
                    lines[..count].copy_from_slice(&source[..count]);
                }
            }
        }
        (lines, height)
    }

    /// Draws a character at a text cell. Bit 7 of `color` XORs the set bits;
    /// otherwise the cell is drawn opaque over colour 0.
    pub(crate) fn gfx_char(
        &mut self,
        mem: &mut Memory,
        page: u8,
        row: usize,
        column: usize,
        ch: u8,
        color: u8,
    ) {
        let (lines, height) = self.gfx_glyph(mem, ch);
        let xor = color & 0x80 != 0
            && self.kind != GraphicsKind::Vga13
            && self.kind != GraphicsKind::Vesa;
        let x0 = column * 8;
        let y0 = row * height;
        for (line, bits) in lines.iter().take(height).enumerate() {
            for bit in 0..8 {
                let set = bits & (0x80 >> bit) != 0;
                if xor {
                    if set {
                        self.plot(mem, page, x0 + bit, y0 + line, color & 0x7F, true);
                    }
                } else {
                    self.plot(
                        mem,
                        page,
                        x0 + bit,
                        y0 + line,
                        if set { color } else { 0 },
                        false,
                    );
                }
            }
        }
        self.work = self.work.saturating_add(height as u32 * 2);
        self.dirty = true;
    }

    /// Reads the character at a graphics cell by comparing it with the font.
    pub(crate) fn gfx_read_char(&self, mem: &Memory, page: u8, row: usize, column: usize) -> u8 {
        let height = self.gfx_cell_height();
        let mut cell = [0u8; 32];
        for (line, slot) in cell.iter_mut().take(height).enumerate() {
            for bit in 0..8 {
                if self.pixel_on(mem, page, column * 8 + bit, row * height + line) != 0 {
                    *slot |= 0x80 >> bit;
                }
            }
        }
        for code in 0..=255u8 {
            if self.gfx_glyph(mem, code).0 == cell {
                return code;
            }
        }
        0
    }

    /// Draws a character at pixel coordinates in the current colour, leaving
    /// the background alone.
    pub fn draw_char(&mut self, mem: &mut Memory, x: usize, y: usize, ch: u8, color: u8) {
        let (lines, height) = self.gfx_glyph(mem, ch);
        for (line, bits) in lines.iter().take(height).enumerate() {
            for bit in 0..8 {
                if bits & (0x80 >> bit) != 0 {
                    self.plot(mem, self.page, x + bit, y + line, color, false);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Pixels
    // ------------------------------------------------------------------

    /// Plane addresses from one displayed line to the next, in the host's view.
    fn host_stride(&self) -> usize {
        let stride = (self.crtc[0x13] as usize * 2) << self.addressing().shift;
        if stride == 0 {
            match self.kind {
                GraphicsKind::Vga13 => self.width,
                _ => (self.width / 8).max(1),
            }
        } else {
            stride
        }
    }

    /// Where a 256-colour pixel lives, as an address and a plane.
    fn pixel_256(&self, x: usize, y: usize) -> (usize, u32) {
        let stride = self.host_stride();
        if self.seq[0x04] & 0x08 != 0 {
            let linear = (y * stride + x) & (PLANE_SIZE - 1);
            (linear & !3, (linear & 3) as u32)
        } else {
            ((y * stride + x / 4) & (PLANE_SIZE - 1), (x & 3) as u32)
        }
    }

    /// Writes one pixel, or XORs it.
    pub(crate) fn plot(
        &mut self,
        mem: &mut Memory,
        page: u8,
        x: usize,
        y: usize,
        color: u8,
        xor: bool,
    ) {
        if x >= self.width || y >= self.height {
            return;
        }
        match self.kind {
            GraphicsKind::Text => {}
            GraphicsKind::Vesa => {
                let index = self.vesa_start + y * self.vesa_pitch + x;
                if let Some(slot) = self.svga.get_mut(index) {
                    *slot = if xor { *slot ^ color } else { color };
                }
            }
            GraphicsKind::Vga13 => {
                let (offset, plane) = self.pixel_256(x, y);
                let old = self.plane_byte(offset, plane);
                self.set_plane_byte(offset, plane, if xor { old ^ color } else { color });
            }
            GraphicsKind::Cga320 | GraphicsKind::Cga640 => {
                let wide = self.kind == GraphicsKind::Cga640;
                let (offset, shift, mask) = cga_pixel(x, y, wide);
                let address = VIDEO_BASE + offset;
                let old = mem.read8(address);
                let bits = (color & mask) << shift;
                let value = if xor {
                    old ^ bits
                } else {
                    (old & !(mask << shift)) | bits
                };
                mem.write8(address, value);
            }
            GraphicsKind::Planar => {
                let offset = self.page_start(page) as usize + y * self.host_stride() + x / 8;
                let mask = (0x80u32 >> (x & 7)) * 0x0101_0101;
                let bits = EXPAND[(color & 0x0F) as usize] & mask;
                let word = &mut self.planes[offset & (PLANE_SIZE - 1)];
                *word = if xor {
                    *word ^ bits
                } else {
                    (*word & !mask) | bits
                };
            }
        }
        self.dirty = true;
    }

    /// Reads one pixel of a page.
    pub(crate) fn pixel_on(&self, mem: &Memory, page: u8, x: usize, y: usize) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        match self.kind {
            GraphicsKind::Text => 0,
            GraphicsKind::Vesa => self
                .svga
                .get(self.vesa_start + y * self.vesa_pitch + x)
                .copied()
                .unwrap_or(0),
            GraphicsKind::Vga13 => {
                let (offset, plane) = self.pixel_256(x, y);
                self.plane_byte(offset, plane)
            }
            GraphicsKind::Cga320 | GraphicsKind::Cga640 => {
                let (offset, shift, mask) = cga_pixel(x, y, self.kind == GraphicsKind::Cga640);
                (mem.read8(VIDEO_BASE + offset) >> shift) & mask
            }
            GraphicsKind::Planar => {
                let offset = self.page_start(page) as usize + y * self.host_stride() + x / 8;
                let word = self.planes[offset & (PLANE_SIZE - 1)];
                let bit = 7 - (x & 7) as u32;
                let mut color = 0;
                for plane in 0..4 {
                    if word >> (plane * 8 + bit) & 1 != 0 {
                        color |= 1 << plane;
                    }
                }
                color
            }
        }
    }

    /// Writes one pixel in the current graphics mode.
    pub fn put_pixel(&mut self, mem: &mut Memory, x: usize, y: usize, color: u8) {
        self.plot(mem, self.page, x, y, color, false);
    }

    pub fn read_pixel(&self, mem: &Memory, x: usize, y: usize) -> u8 {
        self.pixel_on(mem, self.page, x, y)
    }

    // ------------------------------------------------------------------
    // The memory pipeline
    // ------------------------------------------------------------------

    /// Whether this physical address goes through the VGA memory pipeline
    /// rather than `Memory`.
    #[inline]
    pub fn is_vram(&self, addr: u32) -> bool {
        addr >= self.map_low && addr < self.map_high
    }

    /// The address inside the CPU's window, wrapped to the plane size.
    #[inline]
    fn window_offset(&self, addr: u32) -> usize {
        (addr.wrapping_sub(self.map_low) as usize) & (PLANE_SIZE - 1)
    }

    /// A read through the graphics controller: loads the latches and applies
    /// the read mode.
    pub fn vram_read(&mut self, addr: u32) -> u8 {
        if self.vesa_mode != 0 {
            let index = self.vesa_bank as usize * PLANE_SIZE + self.window_offset(addr);
            return self.svga.get(index).copied().unwrap_or(0xFF);
        }
        if !self.is_vram(addr) {
            return 0xFF;
        }
        let offset = self.window_offset(addr);
        let (at, plane) = if self.seq[0x04] & 0x08 != 0 {
            (offset & !3, (offset & 3) as u32)
        } else if self.gc[0x05] & 0x10 != 0 {
            (
                offset & !1,
                ((self.gc[0x04] as u32) & 2) | (offset & 1) as u32,
            )
        } else {
            (offset, (self.gc[0x04] & 3) as u32)
        };
        self.latch = self.planes[at];
        if self.gc[0x05] & 0x08 == 0 {
            (self.latch >> (plane * 8)) as u8
        } else {
            let compare = EXPAND[(self.gc[0x02] & 0x0F) as usize];
            let care = EXPAND[(self.gc[0x07] & 0x0F) as usize];
            let differ = (self.latch ^ compare) & care;
            !((differ | (differ >> 8) | (differ >> 16) | (differ >> 24)) as u8)
        }
    }

    /// A write through the graphics controller: applies the write mode, the
    /// logic function, the bit mask and the plane selection.
    pub fn vram_write(&mut self, addr: u32, value: u8) {
        if self.vesa_mode != 0 {
            let index = self.vesa_bank as usize * PLANE_SIZE + self.window_offset(addr);
            if let Some(slot) = self.svga.get_mut(index) {
                *slot = value;
                self.dirty = true;
            }
            return;
        }
        if !self.is_vram(addr) {
            return;
        }
        let offset = self.window_offset(addr);
        let map_mask = (self.seq[0x02] & 0x0F) as usize;
        let (at, planes) = if self.seq[0x04] & 0x08 != 0 {
            (offset & !3, map_mask & (1 << (offset & 3)))
        } else if self.seq[0x04] & 0x04 == 0 {
            (offset & !1, map_mask & (0b0101 << (offset & 1)))
        } else {
            (offset, map_mask)
        };
        let result = self.write_pipeline(value);
        let mask = EXPAND[planes];
        let word = &mut self.planes[at];
        *word = (*word & !mask) | (result & mask);
        self.dirty = true;
    }

    #[inline]
    fn write_pipeline(&self, value: u8) -> u32 {
        let mode = self.gc[0x05] & 3;
        if mode == 1 {
            return self.latch;
        }
        let rotate = self.gc[0x03] & 7;
        let rotated = value.rotate_right(rotate as u32);
        let set_reset = EXPAND[(self.gc[0x00] & 0x0F) as usize];
        let mut bit_mask = self.gc[0x08];
        let data = match mode {
            0 => {
                let enable = EXPAND[(self.gc[0x01] & 0x0F) as usize];
                (((rotated as u32) * 0x0101_0101) & !enable) | (set_reset & enable)
            }
            2 => EXPAND[(value & 0x0F) as usize],
            _ => {
                bit_mask &= rotated;
                set_reset
            }
        };
        let latch = self.latch;
        let logic = match (self.gc[0x03] >> 3) & 3 {
            0 => data,
            1 => data & latch,
            2 => data | latch,
            _ => data ^ latch,
        };
        let spread = (bit_mask as u32) * 0x0101_0101;
        (logic & spread) | (latch & !spread)
    }

    // ------------------------------------------------------------------
    // Ports
    // ------------------------------------------------------------------

    /// A read from a VGA, EGA or CGA port. `time_us` is guest time, for the
    /// retrace bits.
    pub fn port_in(&mut self, port: u16, _mem: &Memory, time_us: u64) -> u8 {
        match port {
            0x3C0 => self.ac_index,
            0x3C1 => {
                let index = (self.ac_index & 0x1F) as usize;
                self.ac.get(index).copied().unwrap_or(0)
            }
            0x3C2 => 0x70,
            0x3C3 => 0x01,
            0x3C4 => self.seq_index,
            0x3C5 => self.seq.get(self.seq_index as usize).copied().unwrap_or(0),
            0x3C6 => self.pel_mask,
            0x3C7 => {
                if self.dac_reading {
                    0x03
                } else {
                    0x00
                }
            }
            0x3C8 => self.dac_write_index,
            0x3C9 => {
                let entry = self.dac[self.dac_read_index as usize];
                let value = entry[(self.dac_read_component as usize).min(2)];
                self.dac_read_component += 1;
                if self.dac_read_component >= 3 {
                    self.dac_read_component = 0;
                    self.dac_read_index = self.dac_read_index.wrapping_add(1);
                }
                value
            }
            0x3CA => self.feature,
            0x3CC => self.misc,
            0x3CE => self.gc_index,
            0x3CF => self.gc.get(self.gc_index as usize).copied().unwrap_or(0),
            0x3B4 | 0x3D4 if port == self.crtc_port() => self.crtc_index,
            0x3B5 | 0x3D5 if port == self.crtc_port() + 1 => self
                .crtc
                .get(self.crtc_index as usize)
                .copied()
                .unwrap_or(0),
            0x3BA | 0x3DA => {
                self.ac_flip = false;
                status_bits(time_us)
            }
            0x3D8 => self.cga_mode,
            0x3D9 => self.cga_palette,
            _ => 0xFF,
        }
    }

    /// A write to a VGA, EGA or CGA port.
    pub fn port_out(&mut self, port: u16, value: u8, mem: &mut Memory) {
        match port {
            0x3C0 => self.attribute_write(value),
            0x3C2 => {
                self.misc = value;
                self.update_geometry();
            }
            0x3C4 => self.seq_index = value & 0x07,
            0x3C5 => self.sequencer_write(value),
            0x3C6 => {
                if self.pel_mask != value {
                    self.pel_mask = value;
                    self.lut_dirty = true;
                    self.pal_dirty = true;
                    self.dirty = true;
                }
            }
            0x3C7 => {
                self.dac_read_index = value;
                self.dac_read_component = 0;
                self.dac_reading = true;
            }
            0x3C8 => {
                self.dac_write_index = value;
                self.dac_write_component = 0;
                self.dac_reading = false;
            }
            0x3C9 => self.dac_port_write(value),
            0x3CE => self.gc_index = value & 0x0F,
            0x3CF => self.graphics_write(value),
            0x3B4 | 0x3D4 if port == self.crtc_port() => self.crtc_index = value & 0x1F,
            0x3B5 | 0x3D5 if port == self.crtc_port() + 1 => self.crtc_write(mem, value),
            0x3BA | 0x3DA => self.feature = value,
            0x3D8 => self.cga_mode_write(mem, value),
            0x3D9 => {
                self.set_cga_palette(value);
                mem.write8(BDA_BASE + 0x66, value);
            }
            _ => {}
        }
    }

    fn attribute_write(&mut self, value: u8) {
        if !self.ac_flip {
            let was = self.ac_index & 0x20;
            self.ac_index = value & 0x3F;
            self.ac_flip = true;
            if was != value & 0x20 {
                self.dirty = true;
            }
            return;
        }
        self.ac_flip = false;
        let index = (self.ac_index & 0x1F) as usize;
        if index < 16 {
            // The palette can only be written with the display disconnected.
            if self.ac_index & 0x20 == 0 {
                self.ac[index] = value & 0x3F;
            }
        } else if let Some(slot) = self.ac.get_mut(index) {
            *slot = value;
        }
        self.pal_dirty = true;
        self.dirty = true;
        if index == 0x10 {
            self.update_geometry();
        }
    }

    fn sequencer_write(&mut self, value: u8) {
        let index = self.seq_index as usize;
        if let Some(slot) = self.seq.get_mut(index) {
            *slot = value;
        }
        if index == 0x01 {
            self.update_geometry();
        }
        self.dirty = true;
    }

    fn graphics_write(&mut self, value: u8) {
        let index = self.gc_index as usize;
        if let Some(slot) = self.gc.get_mut(index) {
            *slot = value;
        }
        if index == 0x05 || index == 0x06 {
            self.update_geometry();
        }
    }

    fn crtc_write(&mut self, mem: &mut Memory, value: u8) {
        let index = self.crtc_index as usize;
        if index >= self.crtc.len() {
            return;
        }
        if self.crtc[0x11] & 0x80 != 0 && index <= 7 {
            // Registers 0-7 are write-protected, except the line compare bit.
            if index == 7 {
                self.crtc[7] = (self.crtc[7] & !0x10) | (value & 0x10);
                self.dirty = true;
            }
            return;
        }
        self.crtc[index] = value;
        self.dirty = true;
        match index {
            0x01 | 0x07 | 0x09 | 0x12 | 0x13 | 0x14 | 0x17 => self.update_geometry(),
            0x0E | 0x0F if self.is_text() => {
                let location = ((self.crtc[0x0E] as u32) << 8) | self.crtc[0x0F] as u32;
                let columns = self.columns.max(1);
                let position = location.wrapping_sub(self.start_counter()) as usize & 0x3FFF;
                self.cursor = ((position / columns).min(255), (position % columns).min(255));
                self.page_cursors[self.page as usize & 7] =
                    (self.cursor.0 as u8, self.cursor.1 as u8);
                mem.write16(
                    BDA_BASE + 0x50 + (self.page as u32 & 7) * 2,
                    ((self.cursor.0 as u16) << 8) | self.cursor.1 as u16,
                );
            }
            _ => {}
        }
    }

    fn cga_mode_write(&mut self, mem: &mut Memory, value: u8) {
        self.cga_mode = value;
        if value & 0x20 != 0 {
            self.ac[0x10] |= 0x08;
        } else {
            self.ac[0x10] &= !0x08;
        }
        if self.kind == GraphicsKind::Cga320 {
            let bright = self.ac[0x01] & 0x10;
            if value & 0x04 != 0 {
                self.ac[0x01] = 0x03 | bright;
                self.ac[0x02] = 0x04 | bright;
                self.ac[0x03] = 0x07 | bright;
            } else {
                self.set_cga_palette(self.cga_palette);
            }
        }
        self.pal_dirty = true;
        self.dirty = true;
        mem.write8(BDA_BASE + 0x65, value);
    }

    /// The first displayed character counter.
    pub(crate) fn start_counter(&self) -> u32 {
        ((self.crtc[0x0C] as u32) << 8) | self.crtc[0x0D] as u32
    }

    /// Writes the DAC through the 3C9 port's component sequence.
    fn dac_port_write(&mut self, value: u8) {
        let entry = &mut self.dac[self.dac_write_index as usize];
        entry[(self.dac_write_component as usize).min(2)] = value & 0x3F;
        self.dac_write_component += 1;
        if self.dac_write_component >= 3 {
            self.dac_write_component = 0;
            self.dac_write_index = self.dac_write_index.wrapping_add(1);
        }
        self.lut_dirty = true;
        self.pal_dirty = true;
        self.dirty = true;
    }

    /// Takes a byte on the ports behind the VBE protected-mode interface: `1CEh`
    /// and `1CFh` hold the bank, and `1D0h` to `1D3h` the display start in dwords.
    pub fn vesa_pm_port(&mut self, port: u16, value: u8) {
        let index = (port - 0x1CE) as usize;
        self.vesa_pm_latch[index] = value;
        if self.vesa_mode == 0 {
            return;
        }
        match port {
            0x1CF => {
                let bank = self.vesa_pm_latch[0] as usize | ((value as usize) << 8);
                if bank < SVGA_SIZE / 0x1_0000 {
                    self.vesa_bank = bank as u16;
                }
            }
            0x1D3 => {
                let [a, b, c, d] = [
                    self.vesa_pm_latch[2],
                    self.vesa_pm_latch[3],
                    self.vesa_pm_latch[4],
                    value,
                ];
                let start = u32::from_le_bytes([a, b, c, d]) as usize;
                self.vesa_start = start.saturating_mul(4).min(SVGA_SIZE - 1);
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Sets one DAC entry, for the BIOS.
    pub(crate) fn set_dac(&mut self, index: u8, red: u8, green: u8, blue: u8) {
        self.dac[index as usize] = [red & 0x3F, green & 0x3F, blue & 0x3F];
        self.lut_dirty = true;
        self.pal_dirty = true;
        self.dirty = true;
    }

    /// Sets one attribute controller register, for the BIOS.
    pub(crate) fn set_ac(&mut self, index: usize, value: u8) {
        if let Some(slot) = self.ac.get_mut(index) {
            *slot = if index < 16 { value & 0x3F } else { value };
        }
        self.pal_dirty = true;
        self.dirty = true;
        if index == 0x10 {
            self.update_geometry();
        }
    }

    pub(crate) fn invalidate_luts(&mut self) {
        self.lut_dirty = true;
        self.pal_dirty = true;
        self.dirty = true;
    }

    /// The colour-select value CGA software writes to port 3D9 and `AH=0Bh`
    /// builds: bits 0-3 background (foreground at 640 wide), bit 4 intensity,
    /// bit 5 palette. Applied to the attribute palette, as a VGA BIOS does.
    pub fn set_cga_palette(&mut self, value: u8) {
        self.cga_palette = value;
        let mapped = {
            let low = value & 0x0F;
            if low & 0x08 != 0 {
                (low & 7) | 0x10
            } else {
                low
            }
        };
        match self.kind {
            GraphicsKind::Cga320 => {
                let bright = value & 0x10;
                self.ac[0x00] = mapped;
                self.ac[0x11] = mapped;
                let base: [u8; 3] = if value & 0x20 != 0 {
                    [3, 5, 7]
                } else {
                    [2, 4, 6]
                };
                for (slot, color) in self.ac[1..4].iter_mut().zip(base) {
                    *slot = color | bright;
                }
            }
            GraphicsKind::Cga640 => {
                self.ac[0x01] = mapped;
            }
            _ => self.ac[0x11] = mapped,
        }
        self.pal_dirty = true;
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    fn refresh_luts(&mut self) {
        if self.lut_dirty {
            for (index, slot) in self.lut.iter_mut().enumerate() {
                *slot = dac_to_xrgb(self.dac[index & self.pel_mask as usize]);
            }
            self.lut_dirty = false;
            self.lut_builds = self.lut_builds.wrapping_add(1);
            self.pal_dirty = true;
        }
        if self.pal_dirty {
            let enable = self.ac[0x12] & 0x0F;
            let control = self.ac[0x10];
            let select = self.ac[0x14];
            for index in 0..16 {
                let register = self.ac[(index as u8 & enable) as usize];
                let mut dac = register & 0x3F;
                if control & 0x80 != 0 {
                    dac = (dac & 0x0F) | ((select & 0x03) << 4);
                }
                dac |= (select & 0x0C) << 4;
                self.pal16[index] = self.lut[dac as usize];
            }
            self.pal_dirty = false;
        }
    }

    fn addressing(&self) -> CrtcAddressing {
        let shift = if self.crtc[0x14] & 0x40 != 0 {
            2
        } else if self.crtc[0x17] & 0x40 == 0 {
            1
        } else {
            0
        };
        CrtcAddressing {
            shift,
            wrap_bit_15: self.crtc[0x17] & 0x20 != 0,
            scan_to_13: self.crtc[0x17] & 0x01 == 0,
            scan_to_14: self.crtc[0x17] & 0x02 == 0,
        }
    }

    fn line_compare(&self) -> u32 {
        self.crtc[0x18] as u32
            | (((self.crtc[0x07] as u32) & 0x10) << 4)
            | (((self.crtc[0x09] as u32) & 0x40) << 3)
    }

    fn walker(&self) -> LineWalker {
        LineWalker {
            counter: self.start_counter(),
            scan: (self.crtc[0x08] & 0x1F) as u32,
            repeat: 0,
            repeats: if self.double_scan() { 2 } else { 1 },
            scans: self.cell_scans() as u32,
            stride: self.crtc[0x13] as u32 * 2,
            compare: self.line_compare(),
            split: false,
        }
    }

    /// Whether the display is turned off: the sequencer's screen-off bit, or
    /// the attribute controller's index left with the palette disconnected.
    fn blanked(&self) -> bool {
        self.seq[0x01] & 0x20 != 0 || self.ac_index & 0x20 == 0
    }

    /// Renders the current mode into the top-left `display_size()` of `screen`.
    pub fn render(&mut self, mem: &Memory, screen: &mut [u32], blink: bool) {
        self.dirty = false;
        self.blink = blink;
        if screen.len() < SCREEN_WIDTH * SCREEN_HEIGHT {
            return;
        }
        self.refresh_luts();
        if self.blanked() {
            screen[..SCREEN_WIDTH * self.display_height.min(SCREEN_HEIGHT)].fill(0);
            return;
        }
        match self.kind {
            GraphicsKind::Text => self.render_text(mem, screen, blink),
            GraphicsKind::Vesa => self.render_vesa(screen),
            _ => self.render_graphics(mem, screen),
        }
    }

    fn render_vesa(&self, screen: &mut [u32]) {
        for (y, row) in screen
            .as_chunks_mut::<SCREEN_WIDTH>().0.iter_mut()
            .take(self.display_height)
            .enumerate()
        {
            let at = self.vesa_start + y * self.vesa_pitch;
            match self.svga.get(at..at + SCREEN_WIDTH) {
                Some(source) => {
                    for (out, index) in row.iter_mut().zip(source) {
                        *out = self.lut[*index as usize];
                    }
                }
                None => row.fill(0),
            }
        }
    }

    /// The per-line decoder for every pixel graphics kind.
    fn render_graphics(&self, mem: &Memory, screen: &mut [u32]) {
        let addressing = self.addressing();
        let mut walker = self.walker();
        let width = self.display_width;
        let scale = self.pixel_scale.max(1);
        let logical = self.logical_line;
        let byte_pan = ((self.crtc[0x08] >> 5) & 3) as u32;
        let pel = self.ac[0x13] & 0x0F;
        let pan_full = match self.kind {
            GraphicsKind::Vga13 => (pel as usize & 7) >> 1,
            GraphicsKind::Planar => pel as usize & 7,
            _ => 0,
        };
        let split_pan_off = self.ac[0x10] & 0x20 != 0;
        let scan_matters = addressing.scan_to_13 || addressing.scan_to_14;
        let wide = self.kind == GraphicsKind::Vga13;

        let mut line = [0u8; LINE_PIXELS];
        let mut last = None;
        for y in 0..self.display_height.min(SCREEN_HEIGHT) {
            walker.begin_line(y as u32);
            let pan = if walker.split && split_pan_off {
                0
            } else {
                pan_full
            };
            let key = (
                walker.counter,
                if scan_matters { walker.scan } else { 0 },
                pan,
            );
            let at = y * SCREEN_WIDTH;
            if last == Some(key) {
                screen.copy_within(at - SCREEN_WIDTH..at - SCREEN_WIDTH + width, at);
            } else {
                let counter = walker.counter.wrapping_add(byte_pan);
                let needed = logical + pan;
                match self.kind {
                    GraphicsKind::Vga13 => {
                        self.decode_256(&addressing, counter, walker.scan, needed, &mut line)
                    }
                    GraphicsKind::Planar => {
                        self.decode_planar(&addressing, counter, walker.scan, needed, &mut line)
                    }
                    _ => self.decode_cga(mem, &addressing, counter, walker.scan, needed, &mut line),
                }
                let row = &mut screen[at..at + width];
                let pixels = line.get(pan..).unwrap_or(&[]);
                if wide {
                    for (out, index) in row.chunks_mut(scale).zip(pixels) {
                        out.fill(self.lut[*index as usize]);
                    }
                } else {
                    for (out, index) in row.chunks_mut(scale).zip(pixels) {
                        out.fill(self.pal16[(*index & 0x0F) as usize]);
                    }
                }
                last = Some(key);
            }
            walker.end_line();
        }
    }

    fn decode_256(
        &self,
        addressing: &CrtcAddressing,
        counter: u32,
        scan: u32,
        needed: usize,
        line: &mut [u8],
    ) {
        let clocks = needed / 4 + 1;
        for (clock, slot) in line.as_chunks_mut::<4>().0.iter_mut().take(clocks).enumerate() {
            let at = addressing.address(counter.wrapping_add(clock as u32), scan) as usize;
            slot.copy_from_slice(&self.planes[at & (PLANE_SIZE - 1)].to_le_bytes());
        }
    }

    fn decode_planar(
        &self,
        addressing: &CrtcAddressing,
        counter: u32,
        scan: u32,
        needed: usize,
        line: &mut [u8],
    ) {
        let clocks = needed / 8 + 1;
        for (clock, slot) in line.as_chunks_mut::<8>().0.iter_mut().take(clocks).enumerate() {
            let at = addressing.address(counter.wrapping_add(clock as u32), scan) as usize;
            let word = self.planes[at & (PLANE_SIZE - 1)];
            let pixels = PIXEL_BITS[(word & 0xFF) as usize]
                | (PIXEL_BITS[((word >> 8) & 0xFF) as usize] << 1)
                | (PIXEL_BITS[((word >> 16) & 0xFF) as usize] << 2)
                | (PIXEL_BITS[((word >> 24) & 0xFF) as usize] << 3);
            slot.copy_from_slice(&pixels.to_be_bytes());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_cga(
        &self,
        mem: &Memory,
        addressing: &CrtcAddressing,
        counter: u32,
        scan: u32,
        needed: usize,
        line: &mut [u8],
    ) {
        let bytes_per_clock = 1u32 << addressing.shift;
        let wide = self.kind == GraphicsKind::Cga640;
        let mut x = 0;
        let mut clock = 0u32;
        while x < needed {
            let at = addressing.address(counter.wrapping_add(clock), scan);
            for byte in 0..bytes_per_clock {
                let value = mem.read8(VIDEO_BASE + (at.wrapping_add(byte) & (TEXT_WINDOW - 1)));
                if wide {
                    let pixels = PIXEL_BITS[value as usize].to_be_bytes();
                    if let Some(slot) = line.get_mut(x..x + 8) {
                        slot.copy_from_slice(&pixels);
                    }
                    x += 8;
                } else {
                    let pixels = [value >> 6, (value >> 4) & 3, (value >> 2) & 3, value & 3];
                    if let Some(slot) = line.get_mut(x..x + 4) {
                        slot.copy_from_slice(&pixels);
                    }
                    x += 4;
                }
            }
            clock += 1;
        }
    }

    fn font_maps(&self) -> (usize, usize) {
        let select = self.seq[0x03];
        let map_a = ((select >> 2) & 3) | ((select >> 3) & 4);
        let map_b = (select & 3) | ((select >> 2) & 4);
        (font_block_offset(map_a), font_block_offset(map_b))
    }

    fn render_text(&self, mem: &Memory, screen: &mut [u32], blink: bool) {
        const MAX_CELLS: usize = SCREEN_WIDTH / 8 + 1;
        let scale = self.pixel_scale.max(1);
        let width = self.display_width;
        let base = self.base();
        let cells =
            ((self.crtc[0x01] as usize + 1).min(SCREEN_WIDTH / (8 * scale)) + 1).min(MAX_CELLS);
        let (font_a, font_b) = self.font_maps();
        let control = self.ac[0x10];
        let blink_enabled = control & 0x08 != 0;
        let mono = control & 0x02 != 0;
        let underline_scan = (self.crtc[0x14] & 0x1F) as u32;
        let cursor_location = ((self.crtc[0x0E] as u32) << 8) | self.crtc[0x0F] as u32;
        let cursor_shown = blink && self.crtc[0x0A] & 0x20 == 0;
        let cursor_first = (self.crtc[0x0A] & 0x1F) as u32;
        let cursor_last = (self.crtc[0x0B] & 0x1F) as u32;
        let pan_full = self.ac[0x13] as usize & 7;
        let split_pan_off = control & 0x20 != 0;

        let mut walker = self.walker();
        let mut row_cells = [TextCell::default(); MAX_CELLS];
        let mut fetched = None;
        let mut pixels = [0u32; MAX_CELLS * 8];
        let byte_pan = ((self.crtc[0x08] >> 5) & 3) as u32;

        for y in 0..self.display_height.min(SCREEN_HEIGHT) {
            walker.begin_line(y as u32);
            if fetched != Some(walker.counter) {
                fetched = Some(walker.counter);
                for (column, cell) in row_cells.iter_mut().take(cells).enumerate() {
                    let counter = (walker.counter + byte_pan + column as u32) & 0x3FFF;
                    let address = base + counter * 2;
                    let ch = mem.read8(address);
                    let attr = mem.read8(address + 1);
                    let foreground = self.pal16[(attr & 0x0F) as usize];
                    let (background, blinking) = if blink_enabled {
                        (self.pal16[((attr >> 4) & 0x07) as usize], attr & 0x80 != 0)
                    } else {
                        (self.pal16[((attr >> 4) & 0x0F) as usize], false)
                    };
                    cell.foreground = if blinking && !blink {
                        background
                    } else {
                        foreground
                    };
                    cell.background = background;
                    let map = if attr & 0x08 != 0 { font_a } else { font_b };
                    cell.glyph = (map + ch as usize * 32) as u32;
                    cell.cursor = counter == (cursor_location & 0x3FFF);
                    cell.underline = mono && attr & 0x77 == 0x01;
                }
            }
            let scan = walker.scan;
            for (cell, slot) in row_cells.iter().take(cells).zip(pixels.chunks_exact_mut(8)) {
                let mut bits = self.plane_byte(cell.glyph as usize + scan as usize, 2);
                if cell.underline && scan == underline_scan {
                    bits = 0xFF;
                }
                if cell.cursor && cursor_shown && scan >= cursor_first && scan <= cursor_last {
                    bits = 0xFF;
                }
                for (bit, pixel) in slot.iter_mut().enumerate() {
                    *pixel = if bits & (0x80 >> bit) != 0 {
                        cell.foreground
                    } else {
                        cell.background
                    };
                }
            }
            let pan = if walker.split && split_pan_off {
                0
            } else {
                pan_full
            };
            let at = y * SCREEN_WIDTH;
            let row = &mut screen[at..at + width];
            for (out, pixel) in row.chunks_mut(scale).zip(pixels[pan..].iter()) {
                out.fill(*pixel);
            }
            walker.end_line();
        }
    }

    /// Centres the displayed area in the canvas with an integer scale.
    pub fn blit(&self, canvas: &mut Canvas<'_>, screen: &[u32]) {
        blit_screen(canvas, screen, self.display_size());
    }

    pub fn text(&self, mem: &Memory) -> String {
        let mut out = String::new();
        if !self.is_text() {
            return out;
        }
        for row in 0..self.rows {
            let mut line = String::new();
            for column in 0..self.columns {
                let (ch, _) = self.read_cell(mem, row, column);
                line.push(if ch >= 0x20 { ch as char } else { ' ' });
            }
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if row + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }

    // ------------------------------------------------------------------
    // VESA
    // ------------------------------------------------------------------

    /// Sets a banked VESA mode: 100h is 640x400, 101h is 640x480, both with 256
    /// colours. The memory is allocated on first use. Returns false if the mode
    /// is unknown or the memory cannot be allocated; nothing changes then.
    pub(crate) fn set_vesa_mode(&mut self, mem: &mut Memory, mode: u16, clear: bool) -> bool {
        let (end, bios_mode, rows) = match mode {
            0x100 => (399u32, VESA_BIOS_MODE_400, 25),
            0x101 => (479u32, VESA_BIOS_MODE_480, 30),
            _ => return false,
        };
        if self.svga.len() < SVGA_SIZE {
            #[cfg(test)]
            if self.fail_alloc {
                return false;
            }
            let more = SVGA_SIZE - self.svga.len();
            if self.svga.try_reserve_exact(more).is_err() {
                return false;
            }
            self.svga.resize(SVGA_SIZE, 0);
        } else if clear {
            self.svga.fill(0);
        }
        if !self.set_mode(mem, 0x13 | if clear { 0 } else { 0x80 }) {
            return false;
        }
        self.mode = bios_mode;
        self.vesa_mode = mode;
        self.vesa_bank = 0;
        self.vesa_pitch = SCREEN_WIDTH;
        self.vesa_start = 0;
        self.char_height = 16;
        self.pages = 1;
        self.page_size = 0;
        self.crtc[0x01] = 0x4F;
        self.crtc[0x07] = (self.crtc[0x07] & !0x42)
            | if end & 0x100 != 0 { 0x02 } else { 0 }
            | if end & 0x200 != 0 { 0x40 } else { 0 };
        self.crtc[0x09] &= 0xE0;
        self.crtc[0x12] = end as u8;
        self.crtc[0x13] = 0x50;
        self.int43 = rom_ptr(ROM_FONT16);
        self.update_geometry();
        self.rows = rows;
        self.sync_bios(mem);
        self.sync_font_vectors(mem);
        self.dirty = true;
        true
    }

    // ------------------------------------------------------------------
    // Save states
    // ------------------------------------------------------------------

    /// Appends the adapter state in a versioned format.
    pub fn save_state(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"FVGA");
        out.push(STATE_VERSION);
        out.push(self.mode);
        out.push(self.no_clear as u8);
        out.extend_from_slice(&self.vesa_mode.to_le_bytes());
        out.extend_from_slice(&self.vesa_bank.to_le_bytes());
        out.push(self.page);
        out.push(self.pages);
        out.push(self.char_height);
        out.extend_from_slice(&self.page_size.to_le_bytes());
        out.push(self.cursor_start);
        out.push(self.cursor_end);
        out.push(self.cursor_emulation as u8);
        out.push(self.scanline_select);
        out.push(self.default_palette_off as u8);
        out.push(self.cursor.0.min(255) as u8);
        out.push(self.cursor.1.min(255) as u8);
        for (row, column) in self.page_cursors {
            out.push(row);
            out.push(column);
        }
        out.push(self.seq_index);
        out.extend_from_slice(&self.seq);
        out.push(self.gc_index);
        out.extend_from_slice(&self.gc);
        out.push(self.crtc_index);
        out.extend_from_slice(&self.crtc);
        out.push(self.ac_index);
        out.push(self.ac_flip as u8);
        out.extend_from_slice(&self.ac);
        out.push(self.misc);
        out.push(self.feature);
        out.push(self.dac_write_index);
        out.push(self.dac_write_component);
        out.push(self.dac_read_index);
        out.push(self.dac_read_component);
        out.push(self.dac_reading as u8);
        out.push(self.pel_mask);
        out.push(self.cga_mode);
        out.push(self.cga_palette);
        out.extend_from_slice(&self.bell.to_le_bytes());
        out.extend_from_slice(&self.latch.to_le_bytes());
        match self.gfx_font {
            Some((address, height)) => {
                out.push(1);
                out.extend_from_slice(&address.to_le_bytes());
                out.push(height);
            }
            None => {
                out.push(0);
                out.extend_from_slice(&0u32.to_le_bytes());
                out.push(0);
            }
        }
        for pointer in [self.int1f, self.int43] {
            out.extend_from_slice(&pointer.0.to_le_bytes());
            out.extend_from_slice(&pointer.1.to_le_bytes());
        }
        for entry in &self.dac {
            out.extend_from_slice(entry);
        }
        out.reserve(PLANE_SIZE * 4 + self.svga.len() + 4);
        for word in self.planes.iter() {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&(self.svga.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.svga);
    }

    /// Restores a state written by `save_state`, reading at `*at`. Every field
    /// is checked. On malformed data it returns false and neither the adapter
    /// nor `*at` changes.
    pub fn load_state(&mut self, data: &[u8], at: &mut usize) -> bool {
        let mut reader = StateReader { data, at: *at };
        match Self::parse_state(&mut reader) {
            Some(mut state) => {
                state.rom_installed = self.rom_installed;
                *self = state;
                *at = reader.at;
                true
            }
            None => false,
        }
    }

    fn parse_state(reader: &mut StateReader<'_>) -> Option<Vga> {
        if reader.bytes(4)? != b"FVGA" || reader.byte()? != STATE_VERSION {
            return None;
        }
        let mut vga = Vga::new();
        vga.mode = reader.byte()?;
        vga.no_clear = reader.flag()?;
        vga.vesa_mode = reader.word()?;
        vga.vesa_bank = reader.word()?;
        match vga.vesa_mode {
            0 => mode_table(vga.mode)?,
            0x100 if vga.mode == VESA_BIOS_MODE_400 => mode_table(0x13)?,
            0x101 if vga.mode == VESA_BIOS_MODE_480 => mode_table(0x13)?,
            _ => return None,
        };
        if vga.vesa_bank >= (SVGA_SIZE / PLANE_SIZE) as u16 {
            return None;
        }
        vga.page = reader.byte()?;
        vga.pages = reader.byte()?;
        if vga.pages == 0 || vga.pages > 8 || vga.page >= vga.pages {
            return None;
        }
        vga.char_height = reader.byte()?;
        if vga.char_height == 0 || vga.char_height > 32 {
            return None;
        }
        vga.page_size = reader.word()?;
        vga.cursor_start = reader.byte()?;
        vga.cursor_end = reader.byte()?;
        vga.cursor_emulation = reader.flag()?;
        vga.scanline_select = reader.byte()?;
        if vga.scanline_select > 2 {
            return None;
        }
        vga.default_palette_off = reader.flag()?;
        vga.cursor = (reader.byte()? as usize, reader.byte()? as usize);
        for slot in vga.page_cursors.iter_mut() {
            *slot = (reader.byte()?, reader.byte()?);
        }
        vga.seq_index = reader.byte()?;
        if vga.seq_index > 7 {
            return None;
        }
        vga.seq.copy_from_slice(reader.bytes(5)?);
        vga.gc_index = reader.byte()?;
        if vga.gc_index > 15 {
            return None;
        }
        vga.gc.copy_from_slice(reader.bytes(9)?);
        vga.crtc_index = reader.byte()?;
        if vga.crtc_index > 31 {
            return None;
        }
        vga.crtc.copy_from_slice(reader.bytes(25)?);
        vga.ac_index = reader.byte()?;
        if vga.ac_index > 0x3F {
            return None;
        }
        vga.ac_flip = reader.flag()?;
        vga.ac.copy_from_slice(reader.bytes(21)?);
        vga.misc = reader.byte()?;
        vga.feature = reader.byte()?;
        vga.dac_write_index = reader.byte()?;
        vga.dac_write_component = reader.byte()?;
        vga.dac_read_index = reader.byte()?;
        vga.dac_read_component = reader.byte()?;
        if vga.dac_write_component > 2 || vga.dac_read_component > 2 {
            return None;
        }
        vga.dac_reading = reader.flag()?;
        vga.pel_mask = reader.byte()?;
        vga.cga_mode = reader.byte()?;
        vga.cga_palette = reader.byte()?;
        vga.bell = reader.dword()?;
        vga.latch = reader.dword()?;
        let has_font = reader.flag()?;
        let address = reader.dword()?;
        let height = reader.byte()?;
        vga.gfx_font = if has_font {
            if height == 0 || height > 32 {
                return None;
            }
            Some((address, height))
        } else {
            None
        };
        vga.int1f = (reader.word()?, reader.word()?);
        vga.int43 = (reader.word()?, reader.word()?);
        for entry in vga.dac.iter_mut() {
            let bytes = reader.bytes(3)?;
            if bytes.iter().any(|value| *value > 0x3F) {
                return None;
            }
            entry.copy_from_slice(bytes);
        }
        let planes = reader.bytes(PLANE_SIZE * 4)?;
        for (word, bytes) in vga.planes.iter_mut().zip(planes.chunks_exact(4)) {
            *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        let svga_len = reader.dword()? as usize;
        if svga_len != 0 && svga_len != SVGA_SIZE {
            return None;
        }
        if vga.vesa_mode != 0 && svga_len == 0 {
            return None;
        }
        if svga_len != 0 {
            let bytes = reader.bytes(svga_len)?;
            if vga.svga.try_reserve_exact(svga_len).is_err() {
                return None;
            }
            vga.svga.extend_from_slice(bytes);
        }
        vga.lut_dirty = true;
        vga.pal_dirty = true;
        vga.dirty = true;
        vga.update_geometry();
        Some(vga)
    }
}

/// Bumped when the save format changes.
const STATE_VERSION: u8 = 1;

/// Reads a save state, refusing to go past the end of the data.
struct StateReader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> StateReader<'a> {
    fn bytes(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.data.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn flag(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn word(&mut self) -> Option<u16> {
        let bytes = self.bytes(2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn dword(&mut self) -> Option<u32> {
        let bytes = self.bytes(4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// Where a CGA pixel lives: offset from `B800:0`, shift and value mask.
fn cga_pixel(x: usize, y: usize, wide: bool) -> (u32, u32, u8) {
    let bank = if y & 1 == 0 { 0 } else { 0x2000 };
    let row = bank + (y / 2) * 80;
    let (offset, shift, mask) = if wide {
        (row + x / 8, 7 - (x & 7) as u32, 1)
    } else {
        (row + x / 4, (3 - (x & 3) as u32) * 2, 3)
    };
    ((offset as u32) & (TEXT_WINDOW - 1), shift, mask)
}

/// Input status 1, from guest time, for a 400-line frame at 70 Hz. Display enable
/// toggles at the 31.5 kHz line rate during the 400 visible lines and stays off
/// through vertical blanking, with the retrace a few lines into that. Wolfenstein
/// 3D finds the bottom of the screen by that run of blank without retrace.
fn status_bits(time_us: u64) -> u8 {
    const FRAME_US: u64 = 14_286;
    const LINE_US: u64 = 32;
    const VISIBLE_US: u64 = 400 * LINE_US;
    const RETRACE_START_US: u64 = 412 * LINE_US;
    const RETRACE_END_US: u64 = 416 * LINE_US;
    const HBLANK_US: u64 = 5;
    let at = time_us % FRAME_US;
    let mut value = 0;
    if (RETRACE_START_US..RETRACE_END_US).contains(&at) {
        value |= 0x08;
    }
    if at >= VISIBLE_US || at % LINE_US >= LINE_US - HBLANK_US {
        value |= 0x01;
    }
    value
}

/// The standard 16-colour VGA/EGA palette.
pub fn palette16(index: usize) -> (u8, u8, u8) {
    const COLORS: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (0, 0, 170),
        (0, 170, 0),
        (0, 170, 170),
        (170, 0, 0),
        (170, 0, 170),
        (170, 85, 0),
        (170, 170, 170),
        (85, 85, 85),
        (85, 85, 255),
        (85, 255, 85),
        (85, 255, 255),
        (255, 85, 85),
        (255, 85, 255),
        (255, 255, 85),
        (255, 255, 255),
    ];
    COLORS[index & 0x0F]
}

/// Draws a [`SCREEN_WIDTH`] by [`SCREEN_HEIGHT`] buffer holding a picture of
/// `size`, centred at the largest integer scale that fits.
pub fn blit_screen(canvas: &mut Canvas<'_>, screen: &[u32], size: (usize, usize)) {
    canvas.clear(feos_gfx::Color::BLACK);
    let canvas_width = canvas.width();
    let canvas_height = canvas.height();
    if canvas_width == 0 || canvas_height == 0 || screen.len() < SCREEN_WIDTH * SCREEN_HEIGHT {
        return;
    }
    let (width, height) = size;
    let width = width.clamp(1, SCREEN_WIDTH);
    let height = height.clamp(1, SCREEN_HEIGHT);
    let scale = (canvas_width / width).min(canvas_height / height).max(1);
    let draw_width = (width * scale).min(canvas_width);
    let draw_height = (height * scale).min(canvas_height);
    let offset_x = (canvas_width - draw_width) / 2;
    let offset_y = (canvas_height - draw_height) / 2;

    if scale == 1 {
        for y in 0..draw_height {
            let at = y * SCREEN_WIDTH;
            canvas.blit(
                offset_x as i32,
                (offset_y + y) as i32,
                draw_width as u32,
                1,
                &screen[at..at + draw_width],
            );
        }
        return;
    }

    let mut row = vec![0u32; draw_width];
    for y in 0..draw_height.div_ceil(scale) {
        let at = y * SCREEN_WIDTH;
        for (slot, pixel) in row.chunks_mut(scale).zip(&screen[at..at + width]) {
            slot.fill(*pixel);
        }
        for repeat in 0..scale {
            let target = y * scale + repeat;
            if target < draw_height {
                canvas.blit(
                    offset_x as i32,
                    (offset_y + target) as i32,
                    draw_width as u32,
                    1,
                    &row,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(mode: u8) -> (Vga, Memory, Vec<u32>) {
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        vga.reset(&mut mem);
        assert!(vga.set_mode(&mut mem, mode));
        (vga, mem, vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT])
    }

    fn out(vga: &mut Vga, mem: &mut Memory, port: u16, value: u8) {
        vga.port_out(port, value, mem);
    }

    fn gc(vga: &mut Vga, mem: &mut Memory, index: u8, value: u8) {
        out(vga, mem, 0x3CE, index);
        out(vga, mem, 0x3CF, value);
    }

    fn seq(vga: &mut Vga, mem: &mut Memory, index: u8, value: u8) {
        out(vga, mem, 0x3C4, index);
        out(vga, mem, 0x3C5, value);
    }

    fn crtc(vga: &mut Vga, mem: &mut Memory, index: u8, value: u8) {
        let port = vga.crtc_port();
        out(vga, mem, port, index);
        out(vga, mem, port + 1, value);
    }

    fn ac(vga: &mut Vga, mem: &mut Memory, index: u8, value: u8) {
        vga.port_in(0x3DA, &Memory::new(), 0);
        out(vga, mem, 0x3C0, index);
        out(vga, mem, 0x3C0, value);
        out(vga, mem, 0x3C0, 0x20);
    }

    fn rgb(index: usize) -> u32 {
        let (r, g, b) = palette16(index);
        ((r as u32) << 16) | ((g as u32) << 8) | b as u32
    }

    fn at(screen: &[u32], x: usize, y: usize) -> u32 {
        screen[y * SCREEN_WIDTH + x]
    }

    fn set_dac_port(vga: &mut Vga, mem: &mut Memory, index: u8, color: [u8; 3]) {
        out(vga, mem, 0x3C8, index);
        for component in color {
            out(vga, mem, 0x3C9, component);
        }
    }

    const A: u32 = 0xA_0000;

    // ------------------------------------------------------------------
    // Text and teletype
    // ------------------------------------------------------------------

    #[test]
    fn teletype_wraps_and_scrolls() {
        let (mut vga, mut mem, _) = setup(3);
        for _ in 0..26 {
            vga.write_line(&mut mem, b"hello");
        }
        let text = vga.text(&mem);
        assert!(text.starts_with("hello"));
        assert_eq!(vga.cursor, (24, 0));
    }

    #[test]
    fn crtc_cursor_round_trips() {
        let (mut vga, mut mem, _) = setup(3);
        crtc(&mut vga, &mut mem, 0x0E, 0x01);
        crtc(&mut vga, &mut mem, 0x0F, 0x40);
        assert_eq!(vga.cursor, (4, 0));
        assert_eq!(mem.read16(BDA_BASE + 0x50), 0x0400);
    }

    #[test]
    fn teletype_keeps_attribute_and_scrolls_with_cursor_cell_attribute() {
        let (mut vga, mut mem, _) = setup(3);
        vga.write_cell(&mut mem, 0, 0, b'x', 0x1E);
        vga.teletype(&mut mem, b'A');
        assert_eq!(vga.read_cell(&mem, 0, 0), (b'A', 0x1E));

        vga.write_cell(&mut mem, 24, 0, b' ', 0x4F);
        vga.set_cursor(&mut mem, 24, 0);
        vga.teletype(&mut mem, b'\n');
        assert_eq!(vga.cursor, (24, 0));
        assert_eq!(vga.read_cell(&mem, 24, 0), (b' ', 0x4F));
        assert_eq!(vga.read_cell(&mem, 24, 79), (b' ', 0x4F));
        assert_eq!(vga.read_cell(&mem, 23, 0).1, 0x4F);
    }

    #[test]
    fn half_block_glyphs_are_on_the_right_sides() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 0, 0, 0xDD, 0x0F);
        vga.write_cell(&mut mem, 0, 1, 0xDE, 0x0F);
        vga.render(&mem, &mut screen, false);
        let white = rgb(15);
        assert_eq!(at(&screen, 0, 5), white);
        assert_eq!(at(&screen, 3, 5), white);
        assert_eq!(at(&screen, 4, 5), 0);
        assert_eq!(at(&screen, 8, 5), 0);
        assert_eq!(at(&screen, 11, 5), 0);
        assert_eq!(at(&screen, 12, 5), white);
        assert_eq!(font::rom8(0xDD)[0], 0xF0);
        assert_eq!(font::rom8(0xDE)[0], 0x0F);
    }

    #[test]
    fn rom_font_layouts_match_the_base_font() {
        let glyph = font::glyph(b'A');
        let low = font::rom8(b'A');
        let tall = font::rom16(b'A');
        let mid = font::rom14(b'A');
        for line in 0..8 {
            assert_eq!(low[line], glyph[line].reverse_bits());
            assert_eq!(tall[line * 2], low[line]);
            assert_eq!(tall[line * 2 + 1], low[line]);
        }
        assert_eq!(mid[0], low[0]);
        assert_eq!(mid[1], low[1]);
        assert_eq!(mid[2], low[1]);
        assert_eq!(mid[12], low[6]);
        assert_eq!(mid[13], low[7]);
    }

    #[test]
    fn text_colors_go_through_attribute_palette_and_dac() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 0, 0, 0xDB, 0x06);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(6));

        ac(&mut vga, &mut mem, 6, 0x3F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0xFF_FFFF);

        set_dac_port(&mut vga, &mut mem, 0x3F, [0x3F, 0, 0]);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0xFF_0000);
    }

    #[test]
    fn blink_and_intensity_bit() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 0, 2, 0xDB, 0x8F);
        vga.render(&mem, &mut screen, true);
        assert_eq!(at(&screen, 16, 0), rgb(15));
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 16, 0), 0);

        // Mode control bit 3 clear: bit 7 is a bright background.
        ac(&mut vga, &mut mem, 0x10, 0x04);
        vga.write_cell(&mut mem, 0, 3, b' ', 0xC0);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 16, 0), rgb(15));
        assert_eq!(at(&screen, 24, 0), rgb(12));
    }

    #[test]
    fn text_start_address_and_cursor_shape() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 1, 0, 0xDB, 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 16), rgb(15));
        assert_eq!(at(&screen, 0, 0), 0);
        crtc(&mut vga, &mut mem, 0x0C, 0);
        crtc(&mut vga, &mut mem, 0x0D, 80);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(15));

        let (mut vga, mut mem, mut screen) = setup(3);
        crtc(&mut vga, &mut mem, 0x0A, 2);
        crtc(&mut vga, &mut mem, 0x0B, 3);
        vga.render(&mem, &mut screen, true);
        assert_eq!(at(&screen, 0, 1), 0);
        assert_eq!(at(&screen, 0, 2), rgb(7));
        assert_eq!(at(&screen, 0, 3), rgb(7));
        assert_eq!(at(&screen, 0, 4), 0);
        // Hidden through bit 5, and off in the dark half of the blink.
        crtc(&mut vga, &mut mem, 0x0A, 0x22);
        vga.render(&mem, &mut screen, true);
        assert_eq!(at(&screen, 0, 2), 0);
    }

    #[test]
    fn forty_column_text_fills_the_width() {
        let (mut vga, mut mem, mut screen) = setup(1);
        assert_eq!((vga.columns, vga.rows), (40, 25));
        assert_eq!(vga.display_size(), (640, 400));
        vga.write_cell(&mut mem, 0, 39, 0xDB, 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 639, 0), rgb(15));
        assert_eq!(at(&screen, 39 * 16 - 1, 0), 0);
    }

    #[test]
    fn mono_mode_uses_b000_and_underlines() {
        let (mut vga, mut mem, mut screen) = setup(7);
        assert_eq!(vga.crtc_port(), 0x3B4);
        assert_eq!(mem.read16(BDA_BASE + 0x63), 0x3B4);
        mem.write8(MONO_BASE, 0xDB);
        mem.write8(MONO_BASE + 1, 0x0F);
        mem.write8(MONO_BASE + 3, 0x01);
        vga.render(&mem, &mut screen, false);
        assert_ne!(at(&screen, 0, 0), 0);
        assert_eq!(at(&screen, 8, 0), 0);
        assert_ne!(at(&screen, 8, 15), 0);
    }

    #[test]
    fn font_in_plane_two_drives_text_rendering() {
        let (mut vga, mut mem, mut screen) = setup(3);
        // The classic sequence to reach the font.
        seq(&mut vga, &mut mem, 2, 4);
        seq(&mut vga, &mut mem, 4, 7);
        gc(&mut vga, &mut mem, 4, 2);
        gc(&mut vga, &mut mem, 5, 0);
        gc(&mut vga, &mut mem, 6, 4);
        assert!(vga.is_vram(A));
        assert!(!vga.is_vram(VIDEO_BASE));
        vga.vram_write(A + b'Z' as u32 * 32, 0xFF);
        assert_eq!(vga.vram_read(A + b'Z' as u32 * 32), 0xFF);
        seq(&mut vga, &mut mem, 2, 3);
        seq(&mut vga, &mut mem, 4, 3);
        gc(&mut vga, &mut mem, 4, 0);
        gc(&mut vga, &mut mem, 5, 0x10);
        gc(&mut vga, &mut mem, 6, 0x0E);
        assert!(!vga.is_vram(A));

        vga.write_cell(&mut mem, 0, 0, b'Z', 0x0F);
        vga.render(&mem, &mut screen, false);
        for x in 0..8 {
            assert_eq!(at(&screen, x, 0), rgb(15));
        }
    }

    // ------------------------------------------------------------------
    // Graphics controller
    // ------------------------------------------------------------------

    #[test]
    fn gc6_does_not_touch_bit_mask() {
        let (mut vga, mut mem, _) = setup(0x12);
        gc(&mut vga, &mut mem, 8, 0x0F);
        gc(&mut vga, &mut mem, 6, 0x05);
        assert_eq!(vga.gc[8], 0x0F);
        assert_eq!(vga.gc[6], 0x05);
    }

    #[test]
    fn logic_op_and_rotate_decode_the_right_bits() {
        let (mut vga, mut mem, _) = setup(0x12);
        vga.vram_write(A, 0b1100_1100);
        vga.vram_read(A);
        // Rotate 1, OR.
        gc(&mut vga, &mut mem, 3, 0b10_001);
        vga.vram_write(A, 0b0000_0011);
        assert_eq!(vga.vram_read(A), 0b1100_1100 | 0b1000_0001);
        // XOR against the fresh latch.
        vga.vram_read(A);
        gc(&mut vga, &mut mem, 3, 0b11_000);
        vga.vram_write(A, 0xFF);
        assert_eq!(vga.vram_read(A), !(0b1100_1100u8 | 0b1000_0001));
        // AND.
        vga.vram_read(A);
        gc(&mut vga, &mut mem, 3, 0b01_000);
        vga.vram_write(A, 0x0F);
        assert_eq!(vga.vram_read(A), !(0b1100_1100u8 | 0b1000_0001) & 0x0F);
    }

    #[test]
    fn write_mode_0_bit_mask_and_set_reset() {
        let (mut vga, mut mem, _) = setup(0x12);
        vga.vram_write(A, 0xAA);
        vga.vram_read(A);
        gc(&mut vga, &mut mem, 8, 0x0F);
        vga.vram_write(A, 0x55);
        assert_eq!(vga.vram_read(A), 0xA5);
        gc(&mut vga, &mut mem, 8, 0xFF);

        gc(&mut vga, &mut mem, 0, 0b0101);
        gc(&mut vga, &mut mem, 1, 0b0011);
        vga.vram_write(A + 1, 0x3C);
        assert_eq!(vga.planes[1], 0x003C_00FF | 0x3C00_0000);
    }

    #[test]
    fn write_mode_1_copies_latches() {
        let (mut vga, mut mem, _) = setup(0x12);
        vga.planes[10] = 0x1234_5678;
        vga.vram_read(A + 10);
        gc(&mut vga, &mut mem, 5, 1);
        vga.vram_write(A + 20, 0xFF);
        assert_eq!(vga.planes[20], 0x1234_5678);
        // The map mask still applies; the bit mask does not.
        seq(&mut vga, &mut mem, 2, 0b0001);
        gc(&mut vga, &mut mem, 8, 0);
        vga.vram_write(A + 21, 0);
        assert_eq!(vga.planes[21], 0x0000_0078);
    }

    #[test]
    fn write_mode_2_spreads_color() {
        let (mut vga, mut mem, _) = setup(0x12);
        gc(&mut vga, &mut mem, 5, 2);
        gc(&mut vga, &mut mem, 8, 0x80);
        vga.vram_read(A);
        vga.vram_write(A, 0x0B);
        assert_eq!(vga.read_pixel(&mem, 0, 0), 0x0B);
        assert_eq!(vga.read_pixel(&mem, 1, 0), 0);
    }

    #[test]
    fn write_mode_3_uses_data_as_mask() {
        let (mut vga, mut mem, _) = setup(0x12);
        gc(&mut vga, &mut mem, 5, 3);
        gc(&mut vga, &mut mem, 0, 0x06);
        gc(&mut vga, &mut mem, 8, 0xF0);
        vga.vram_read(A);
        vga.vram_write(A, 0xCC);
        // Mask = 0xCC & 0xF0 = 0xC0.
        assert_eq!(vga.read_pixel(&mem, 0, 0), 6);
        assert_eq!(vga.read_pixel(&mem, 1, 0), 6);
        assert_eq!(vga.read_pixel(&mem, 2, 0), 0);
    }

    #[test]
    fn read_modes_and_map_select() {
        let (mut vga, mut mem, _) = setup(0x12);
        vga.planes[5] = 0x4433_2211;
        for plane in 0..4 {
            gc(&mut vga, &mut mem, 4, plane);
            assert_eq!(vga.vram_read(A + 5), 0x11 * (plane + 1));
        }
        gc(&mut vga, &mut mem, 4, 0);

        // Colour compare: pixel 0 is colour 0b0101, the rest 0.
        vga.planes[6] = 0x0080_0080;
        gc(&mut vga, &mut mem, 5, 0x08);
        gc(&mut vga, &mut mem, 2, 0b0101);
        gc(&mut vga, &mut mem, 7, 0x0F);
        assert_eq!(vga.vram_read(A + 6), 0x80);
        // Colour 0 matches the others.
        gc(&mut vga, &mut mem, 2, 0);
        assert_eq!(vga.vram_read(A + 6), 0x7F);
        // Don't care on planes 0 and 2 makes everything match.
        gc(&mut vga, &mut mem, 7, 0b1010);
        assert_eq!(vga.vram_read(A + 6), 0xFF);
    }

    #[test]
    fn memory_map_select_controls_is_vram() {
        let (mut vga, mut mem, _) = setup(0x12);
        assert!(vga.is_vram(0xA_0000));
        assert!(vga.is_vram(0xA_FFFF));
        assert!(!vga.is_vram(0xB_0000));
        assert!(!vga.is_vram(0x9_FFFF));
        gc(&mut vga, &mut mem, 6, 0x01);
        assert!(vga.is_vram(0xB_FFFF));
        assert!(!vga.is_vram(0xC_0000));
        gc(&mut vga, &mut mem, 6, 0x09);
        assert!(!vga.is_vram(0xA_0000));
        assert!(vga.is_vram(0xB_7FFF));
        assert!(!vga.is_vram(0xB_8000));
        let (vga, _, _) = setup(3);
        assert!(!vga.is_vram(VIDEO_BASE));
        assert!(!vga.is_vram(A));
        let (vga, _, _) = setup(4);
        assert!(!vga.is_vram(VIDEO_BASE));
    }

    #[test]
    fn odd_even_addressing_splits_planes() {
        let (mut vga, mut mem, _) = setup(0x12);
        seq(&mut vga, &mut mem, 4, 0x02);
        gc(&mut vga, &mut mem, 5, 0x10);
        vga.vram_write(A + 8, 0x11);
        vga.vram_write(A + 9, 0x22);
        assert_eq!(vga.planes[8], 0x2211_2211);
        assert_eq!(vga.vram_read(A + 9), 0x22);
        assert_eq!(vga.vram_read(A + 8), 0x11);
    }

    // ------------------------------------------------------------------
    // Chain-4 and tweaked modes
    // ------------------------------------------------------------------

    #[test]
    fn mode_13h_pixels_round_trip_through_the_pipeline() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        vga.put_pixel(&mut mem, 319, 199, 42);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 42);
        assert_eq!(vga.vram_read(A + 199 * 320 + 319), 42);
        vga.vram_write(A + 321, 9);
        assert_eq!(vga.read_pixel(&mem, 1, 1), 9);
        vga.render(&mem, &mut screen, false);
        assert_eq!(vga.display_size(), (640, 400));
        assert_eq!(at(&screen, 2, 2), rgb(9));
        assert_eq!(at(&screen, 3, 3), rgb(9));
        assert_eq!(at(&screen, 638, 398), vga.lut[42]);
        assert_eq!(at(&screen, 639, 399), vga.lut[42]);
        assert_eq!(at(&screen, 0, 0), 0);
    }

    #[test]
    fn chain_4_maps_address_to_plane_and_offset() {
        let (mut vga, mut mem, _) = setup(0x13);
        vga.vram_write(A + 0x1006, 0xAB);
        assert_eq!(vga.planes[0x1004], 0x00AB_0000);
        // Turning chain-4 off shows the same memory on the plane.
        seq(&mut vga, &mut mem, 4, 0x06);
        gc(&mut vga, &mut mem, 4, 2);
        gc(&mut vga, &mut mem, 5, 0x40);
        assert_eq!(vga.vram_read(A + 0x1004), 0xAB);
    }

    /// The standard register list for 320x240 Mode X.
    fn mode_x(vga: &mut Vga, mem: &mut Memory) {
        seq(vga, mem, 4, 0x06);
        out(vga, mem, 0x3C2, 0xE3);
        crtc(vga, mem, 0x11, 0x2C);
        for (index, value) in [
            (0x06, 0x0D),
            (0x07, 0x3E),
            (0x09, 0x41),
            (0x10, 0xEA),
            (0x11, 0xAC),
            (0x12, 0xDF),
            (0x14, 0x00),
            (0x15, 0xE7),
            (0x16, 0x06),
            (0x17, 0xE3),
        ] {
            crtc(vga, mem, index, value);
        }
    }

    #[test]
    fn mode_x_renders_320x240_from_unchained_planes() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        mode_x(&mut vga, &mut mem);
        assert_eq!(vga.display_size(), (640, 480));
        assert_eq!((vga.width, vga.height), (320, 240));

        // Pixel (5, 239): plane 1, offset 80*239+1.
        seq(&mut vga, &mut mem, 2, 1 << 1);
        vga.vram_write(A + 80 * 239 + 1, 14);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 10, 478), rgb(14));
        assert_eq!(at(&screen, 11, 479), rgb(14));
        assert_eq!(at(&screen, 8, 478), 0);
        assert_eq!(at(&screen, 10, 476), 0);

        // A write to all planes makes four pixels.
        seq(&mut vga, &mut mem, 2, 0x0F);
        vga.vram_write(A, 2);
        vga.render(&mem, &mut screen, false);
        for x in 0..8 {
            assert_eq!(at(&screen, x, 0), rgb(2));
        }
        assert_eq!(at(&screen, 8, 0), 0);
    }

    #[test]
    fn mode_y_320x400_turns_off_line_doubling() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        seq(&mut vga, &mut mem, 4, 0x06);
        crtc(&mut vga, &mut mem, 0x09, 0x40);
        crtc(&mut vga, &mut mem, 0x14, 0x00);
        crtc(&mut vga, &mut mem, 0x17, 0xE3);
        assert_eq!((vga.width, vga.height), (320, 400));
        assert_eq!(vga.display_size(), (640, 400));
        seq(&mut vga, &mut mem, 2, 1);
        vga.vram_write(A + 80, 1);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
        assert_eq!(at(&screen, 0, 1), rgb(1));
        assert_eq!(at(&screen, 0, 2), 0);
    }

    #[test]
    fn crtc_address_modes() {
        // Byte mode, the default of mode 12h.
        let (mut vga, mut mem, mut screen) = setup(0x12);
        vga.planes[80] = 0xFF;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 1), rgb(1));

        // Word mode: the counter is doubled, per clock and per line.
        crtc(&mut vga, &mut mem, 0x17, 0xA3);
        vga.planes.fill(0);
        vga.planes[2] = 0xFF;
        vga.planes[160] = 0xFF00;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 8, 0), rgb(1));
        assert_eq!(at(&screen, 0, 1), rgb(2));

        // Doubleword mode.
        crtc(&mut vga, &mut mem, 0x14, 0x40);
        vga.planes.fill(0);
        vga.planes[4] = 0xFF;
        vga.planes[320] = 0xFF00;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 8, 0), rgb(1));
        assert_eq!(at(&screen, 0, 1), rgb(2));
    }

    #[test]
    fn zero_and_extreme_crtc_values_do_not_panic() {
        for mode in [3, 4, 6, 0x0D, 0x12, 0x13] {
            for fill in [0u8, 0xFF] {
                let (mut vga, mut mem, mut screen) = setup(mode);
                crtc(&mut vga, &mut mem, 0x11, 0);
                for index in 0..25 {
                    crtc(&mut vga, &mut mem, index, fill);
                }
                seq(&mut vga, &mut mem, 1, fill & 0x1F);
                vga.render(&mem, &mut screen, true);
                let (width, height) = vga.display_size();
                assert!(width >= 1 && width <= SCREEN_WIDTH);
                assert!(height >= 1 && height <= SCREEN_HEIGHT);
                assert!(vga.rows >= 1 && vga.columns >= 1);
                vga.teletype(&mut mem, b'x');
                vga.teletype(&mut mem, b'\n');
            }
        }
    }

    // ------------------------------------------------------------------
    // Display geometry, paging and panning
    // ------------------------------------------------------------------

    #[test]
    fn standard_modes_have_standard_geometry() {
        for (mode, kind, size, logical, cells) in [
            (0x00, GraphicsKind::Text, (640, 400), (320, 200), (40, 25)),
            (0x03, GraphicsKind::Text, (640, 400), (640, 400), (80, 25)),
            (0x04, GraphicsKind::Cga320, (640, 400), (320, 200), (40, 25)),
            (0x06, GraphicsKind::Cga640, (640, 400), (640, 200), (80, 25)),
            (0x07, GraphicsKind::Text, (640, 400), (640, 400), (80, 25)),
            (0x0D, GraphicsKind::Planar, (640, 400), (320, 200), (40, 25)),
            (0x0E, GraphicsKind::Planar, (640, 400), (640, 200), (80, 25)),
            (0x0F, GraphicsKind::Planar, (640, 350), (640, 350), (80, 25)),
            (0x10, GraphicsKind::Planar, (640, 350), (640, 350), (80, 25)),
            (0x11, GraphicsKind::Planar, (640, 480), (640, 480), (80, 30)),
            (0x12, GraphicsKind::Planar, (640, 480), (640, 480), (80, 30)),
            (0x13, GraphicsKind::Vga13, (640, 400), (320, 200), (40, 25)),
        ] {
            let (vga, _, _) = setup(mode);
            assert_eq!(vga.kind, kind, "mode {mode:x}");
            assert_eq!(vga.display_size(), size, "mode {mode:x}");
            assert_eq!((vga.width, vga.height), logical, "mode {mode:x}");
            assert_eq!((vga.columns, vga.rows), cells, "mode {mode:x}");
        }
    }

    #[test]
    fn mode_12h_shows_all_480_lines() {
        let (mut vga, mut mem, mut screen) = setup(0x12);
        vga.put_pixel(&mut mem, 639, 479, 0x0C);
        assert_eq!(vga.read_pixel(&mem, 639, 479), 0x0C);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 639, 479), rgb(0x0C));
    }

    #[test]
    fn low_resolution_planar_doubles_both_axes() {
        let (mut vga, mut mem, mut screen) = setup(0x0D);
        vga.put_pixel(&mut mem, 319, 199, 0x0A);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 0x0A);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 638, 398), rgb(0x0A));
        assert_eq!(at(&screen, 639, 399), rgb(0x0A));
        assert_eq!(at(&screen, 637, 398), 0);
    }

    #[test]
    fn cga_modes_render_from_memory_with_banks() {
        let (mut vga, mut mem, mut screen) = setup(4);
        vga.put_pixel(&mut mem, 319, 199, 3);
        assert_eq!(vga.read_pixel(&mem, 319, 199), 3);
        assert_eq!(mem.read8(VIDEO_BASE + 0x2000 + 99 * 80 + 79) & 3, 3);
        // Pixel values 1, 2 and 3 in the first bank.
        mem.write8(VIDEO_BASE, 0b00_01_10_11);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
        assert_eq!(at(&screen, 2, 0), rgb(11));
        assert_eq!(at(&screen, 4, 1), rgb(13));
        assert_eq!(at(&screen, 6, 0), rgb(15));
        assert_eq!(at(&screen, 639, 399), rgb(15));
        assert_eq!(at(&screen, 639, 395), 0);

        let (mut vga, mut mem, mut screen) = setup(6);
        mem.write8(VIDEO_BASE + 0x2000, 0x80);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 2), rgb(15));
        assert_eq!(at(&screen, 0, 3), rgb(15));
        assert_eq!(at(&screen, 0, 0), 0);
        assert_eq!(at(&screen, 1, 2), 0);
    }

    #[test]
    fn cga_port_3d9_selects_palette() {
        let (mut vga, mut mem, mut screen) = setup(4);
        mem.write8(VIDEO_BASE, 0b01_00_00_00);
        out(&mut vga, &mut mem, 0x3D9, 0x00);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(2));
        out(&mut vga, &mut mem, 0x3D9, 0x31);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(11));
        assert_eq!(at(&screen, 2, 0), rgb(1));
        assert_eq!(mem.read8(BDA_BASE + 0x66), 0x31);
    }

    #[test]
    fn graphics_honours_start_address() {
        let (mut vga, mut mem, mut screen) = setup(0x0D);
        // Page 1 begins at 0x2000 in mode 0Dh.
        vga.planes[0x2000] = 0xFF00;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
        crtc(&mut vga, &mut mem, 0x0C, 0x20);
        crtc(&mut vga, &mut mem, 0x0D, 0x00);
        assert!(vga.dirty);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(2));
    }

    #[test]
    fn pel_panning_shifts_pixels() {
        let (mut vga, mut mem, mut screen) = setup(0x12);
        vga.planes[0] = 0x01;
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 7, 0), rgb(1));
        ac(&mut vga, &mut mem, 0x13, 3);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 4, 0), rgb(1));
        assert_eq!(at(&screen, 7, 0), 0);

        let (mut vga, mut mem, mut screen) = setup(0x13);
        vga.vram_write(A + 3, 1);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 6, 0), rgb(1));
        ac(&mut vga, &mut mem, 0x13, 4);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 2, 0), rgb(1));
    }

    #[test]
    fn line_compare_splits_the_screen() {
        let (mut vga, mut mem, mut screen) = setup(0x12);
        vga.planes[0] = 0xFF;
        crtc(&mut vga, &mut mem, 0x0C, 0x10);
        crtc(&mut vga, &mut mem, 0x18, 99);
        crtc(&mut vga, &mut mem, 0x07, vga_overflow_without_compare());
        crtc(&mut vga, &mut mem, 0x09, 0x00);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 99), 0);
        assert_eq!(at(&screen, 0, 100), rgb(1));
        assert_eq!(at(&screen, 0, 101), 0);
    }

    fn vga_overflow_without_compare() -> u8 {
        0x3E & !0x10
    }

    #[test]
    fn attribute_mode_p54s_and_color_select() {
        let (mut vga, mut mem, mut screen) = setup(0x12);
        vga.planes[0] = 0xFF;
        set_dac_port(&mut vga, &mut mem, 0x21, [0, 0x3F, 0]);
        set_dac_port(&mut vga, &mut mem, 0x81, [0x3F, 0, 0]);
        ac(&mut vga, &mut mem, 0x10, 0x81);
        ac(&mut vga, &mut mem, 0x14, 0x02);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0x00_FF00);
        ac(&mut vga, &mut mem, 0x10, 0x01);
        ac(&mut vga, &mut mem, 0x14, 0x08);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0xFF_0000);
        // Colour plane enable masks the pixel.
        ac(&mut vga, &mut mem, 0x14, 0x00);
        ac(&mut vga, &mut mem, 0x12, 0x0E);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
    }

    // ------------------------------------------------------------------
    // Ports
    // ------------------------------------------------------------------

    #[test]
    fn dac_read_and_write_states_are_separate() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        out(&mut vga, &mut mem, 0x3C8, 20);
        out(&mut vga, &mut mem, 0x3C9, 1);
        out(&mut vga, &mut mem, 0x3C7, 40);
        assert_eq!(vga.port_in(0x3C7, &mem, 0), 3);
        let first = vga.port_in(0x3C9, &mem, 0);
        assert_eq!(first, vga.dac[40][0]);
        // Writes carry on with the green of entry 20.
        out(&mut vga, &mut mem, 0x3C9, 2);
        out(&mut vga, &mut mem, 0x3C9, 3);
        assert_eq!(vga.dac[20], [1, 2, 3]);
        assert_eq!(vga.port_in(0x3C8, &mem, 0), 21);
        assert_eq!(vga.port_in(0x3C9, &mem, 0), vga.dac[40][1]);
        assert_eq!(vga.port_in(0x3C9, &mem, 0), vga.dac[40][2]);
        assert_eq!(vga.port_in(0x3C9, &mem, 0), vga.dac[41][0]);

        // The index wraps and values are 6 bits.
        set_dac_port(&mut vga, &mut mem, 255, [0xFF, 0x40, 0x3F]);
        assert_eq!(vga.dac[255], [0x3F, 0x00, 0x3F]);
        assert_eq!(vga.port_in(0x3C8, &mem, 0), 0);

        vga.vram_write(A, 200);
        set_dac_port(&mut vga, &mut mem, 200, [0x3F, 0, 0]);
        set_dac_port(&mut vga, &mut mem, 8, [0, 0x3F, 0]);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0xFF_0000);
        // 200 & 0x0F = 8.
        out(&mut vga, &mut mem, 0x3C6, 0x0F);
        assert_eq!(vga.port_in(0x3C6, &mem, 0), 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0x00_FF00);
    }

    #[test]
    fn attribute_flip_flop_index_and_status_read() {
        let (mut vga, mut mem, _) = setup(3);
        out(&mut vga, &mut mem, 0x3C0, 0x25);
        assert_eq!(vga.port_in(0x3C0, &mem, 0), 0x25);
        assert_eq!(vga.port_in(0x3C1, &mem, 0), vga.ac[5]);
        // A status read means the next write is an index again.
        vga.port_in(0x3DA, &mem, 0);
        out(&mut vga, &mut mem, 0x3C0, 0x31);
        out(&mut vga, &mut mem, 0x3C0, 0x2A);
        assert_eq!(vga.ac[0x11], 0x2A);
        // With PAS set the palette registers do not change.
        let old = vga.ac[3];
        out(&mut vga, &mut mem, 0x3C0, 0x23);
        out(&mut vga, &mut mem, 0x3C0, 0x3F);
        assert_eq!(vga.ac[3], old);
        out(&mut vga, &mut mem, 0x3C0, 0x03);
        out(&mut vga, &mut mem, 0x3C0, 0x3E);
        assert_eq!(vga.ac[3], 0x3E);
    }

    #[test]
    fn clearing_pas_or_screen_off_blanks_the_display() {
        let (mut vga, mut mem, mut screen) = setup(3);
        vga.write_cell(&mut mem, 0, 0, 0xDB, 0x0F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(15));
        vga.port_in(0x3DA, &mem, 0);
        out(&mut vga, &mut mem, 0x3C0, 0x00);
        assert!(vga.dirty);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
        out(&mut vga, &mut mem, 0x3C0, 0x00);
        out(&mut vga, &mut mem, 0x3C0, 0x20);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), rgb(15));
        seq(&mut vga, &mut mem, 1, 0x20);
        vga.render(&mem, &mut screen, false);
        assert_eq!(at(&screen, 0, 0), 0);
    }

    #[test]
    fn registers_read_back() {
        let (mut vga, mut mem, _) = setup(0x12);
        seq(&mut vga, &mut mem, 2, 0x05);
        assert_eq!(vga.port_in(0x3C4, &mem, 0), 2);
        assert_eq!(vga.port_in(0x3C5, &mem, 0), 0x05);
        gc(&mut vga, &mut mem, 3, 0x12);
        assert_eq!(vga.port_in(0x3CE, &mem, 0), 3);
        assert_eq!(vga.port_in(0x3CF, &mem, 0), 0x12);
        crtc(&mut vga, &mut mem, 0x13, 0x33);
        assert_eq!(vga.port_in(0x3D4, &mem, 0), 0x13);
        assert_eq!(vga.port_in(0x3D5, &mem, 0), 0x33);
        assert_eq!(vga.port_in(0x3CC, &mem, 0), 0xE3);
        // Out-of-range indexes read as 0 and swallow writes.
        out(&mut vga, &mut mem, 0x3C4, 7);
        out(&mut vga, &mut mem, 0x3C5, 1);
        assert_eq!(vga.port_in(0x3C5, &mem, 0), 0);
        out(&mut vga, &mut mem, 0x3CE, 15);
        out(&mut vga, &mut mem, 0x3CF, 1);
        assert_eq!(vga.port_in(0x3CF, &mem, 0), 0);
        out(&mut vga, &mut mem, 0x3D4, 31);
        out(&mut vga, &mut mem, 0x3D5, 1);
        assert_eq!(vga.port_in(0x3D5, &mem, 0), 0);
    }

    #[test]
    fn misc_output_selects_the_crtc_base() {
        let (mut vga, mut mem, _) = setup(3);
        assert_eq!(vga.port_in(0x3B5, &mem, 0), 0xFF);
        out(&mut vga, &mut mem, 0x3B4, 0x13);
        assert_ne!(vga.crtc_index, 0x13);
        out(&mut vga, &mut mem, 0x3C2, 0x66);
        out(&mut vga, &mut mem, 0x3B4, 0x13);
        assert_eq!(vga.crtc_index, 0x13);
        assert_eq!(vga.port_in(0x3B5, &mem, 0), 0x28);
        assert_eq!(vga.port_in(0x3D4, &mem, 0), 0xFF);
    }

    #[test]
    fn crtc_protect_bit_guards_timing_registers() {
        let (mut vga, mut mem, _) = setup(0x13);
        crtc(&mut vga, &mut mem, 0x11, 0x8E);
        crtc(&mut vga, &mut mem, 0x01, 0x27);
        assert_eq!(vga.crtc[1], 0x4F);
        crtc(&mut vga, &mut mem, 0x07, 0x00);
        assert_eq!(vga.crtc[7], 0x0F);
        crtc(&mut vga, &mut mem, 0x11, 0x0E);
        crtc(&mut vga, &mut mem, 0x01, 0x27);
        assert_eq!(vga.crtc[1], 0x27);
    }

    #[test]
    fn retrace_comes_from_guest_time() {
        let (mut vga, mem, _) = setup(3);
        let mut retrace = 0u32;
        let mut samples = 0u32;
        let mut toggles = 0u32;
        let mut last_enable = 0;
        let mut time = 0u64;
        while time < 14_286 * 3 {
            let status = vga.port_in(0x3DA, &mem, time);
            assert_eq!(vga.port_in(0x3BA, &mem, time), status);
            if status & 0x08 != 0 {
                retrace += 1;
                assert_eq!(status & 1, 1);
            }
            if status & 1 != last_enable {
                toggles += 1;
                last_enable = status & 1;
            }
            samples += 1;
            time += 3;
        }
        let share = retrace * 1000 / samples;
        assert!((8..=10).contains(&share), "retrace share {share}");
        assert!(toggles > 1000, "toggles {toggles}");
        // A wait-for-retrace loop sees both edges.
        assert_eq!(vga.port_in(0x3DA, &mem, 0) & 8, 0);
        assert_eq!(vga.port_in(0x3DA, &mem, 13_200) & 8, 8);
        assert_eq!(vga.port_in(0x3DA, &mem, 14_285) & 8, 0);
        // Vertical blanking before the retrace: blank, with no retrace yet.
        assert_eq!(vga.port_in(0x3DA, &mem, 12_900) & 9, 1);
    }

    // ------------------------------------------------------------------
    // Mode sets
    // ------------------------------------------------------------------

    #[test]
    fn unknown_modes_change_nothing() {
        let (mut vga, mut mem, _) = setup(0x13);
        vga.mode_changed = false;
        vga.vram_write(A, 5);
        for mode in [0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x14, 0x40, 0x7F] {
            assert!(!vga.set_mode(&mut mem, mode));
            assert!(!vga.set_mode(&mut mem, mode | 0x80));
        }
        assert_eq!(vga.mode, 0x13);
        assert!(!vga.mode_changed);
        assert_eq!(vga.vram_read(A), 5);
    }

    #[test]
    fn bit_7_keeps_memory() {
        let (mut vga, mut mem, _) = setup(0x13);
        vga.vram_write(A, 5);
        assert!(vga.set_mode(&mut mem, 0x93));
        assert_eq!(vga.mode, 0x13);
        assert_eq!(vga.vram_read(A), 5);
        assert!(mem.read8(BDA_BASE + 0x87) & 0x80 != 0);
        assert!(vga.set_mode(&mut mem, 0x13));
        assert_eq!(vga.vram_read(A), 0);
        assert!(mem.read8(BDA_BASE + 0x87) & 0x80 == 0);

        let (mut vga, mut mem, _) = setup(3);
        vga.write_cell(&mut mem, 0, 0, b'K', 0x07);
        vga.set_mode(&mut mem, 0x83);
        assert_eq!(vga.read_cell(&mem, 0, 0).0, b'K');
        vga.set_mode(&mut mem, 3);
        assert_eq!(vga.read_cell(&mem, 0, 0), (b' ', 0x07));
    }

    #[test]
    fn mode_set_loads_standard_registers() {
        let (vga, _, _) = setup(0x13);
        assert_eq!(vga.seq, [0x03, 0x01, 0x0F, 0x00, 0x0E]);
        assert_eq!(vga.gc[5], 0x40);
        assert_eq!(vga.crtc[0x09], 0x41);
        assert_eq!(vga.crtc[0x14], 0x40);
        assert_eq!(vga.ac[0x10], 0x41);
        let (vga, _, _) = setup(0x12);
        assert_eq!(vga.misc, 0xE3);
        assert_eq!(vga.seq[4], 0x06);
        assert_eq!(vga.crtc[0x12], 0xDF);
        assert_eq!(vga.crtc[0x17], 0xE3);
        let (vga, _, _) = setup(3);
        assert_eq!(vga.gc[6], 0x0E);
        assert_eq!(vga.ac[6], 0x14);
        assert_eq!(vga.dac[0x14], [0x2A, 0x15, 0x00]);
    }

    #[test]
    fn default_dac_is_the_standard_table() {
        let (vga, _, _) = setup(0x13);
        assert_eq!(vga.dac[1], [0x00, 0x00, 0x2A]);
        assert_eq!(vga.dac[6], [0x2A, 0x15, 0x00]);
        assert_eq!(vga.dac[16], [0, 0, 0]);
        assert_eq!(vga.dac[31], [0x3F, 0x3F, 0x3F]);
        assert_eq!(vga.dac[32], [0x00, 0x00, 0x3F]);
        assert_eq!(vga.dac[36], [0x3F, 0x00, 0x3F]);
        assert_eq!(vga.dac[40], [0x3F, 0x00, 0x00]);
        assert_eq!(vga.dac[44], [0x3F, 0x3F, 0x00]);
        assert_eq!(vga.dac[48], [0x00, 0x3F, 0x00]);
        assert_eq!(vga.dac[52], [0x00, 0x3F, 0x3F]);
        assert_eq!(vga.dac[55], [0x00, 0x10, 0x3F]);
        assert_eq!(vga.dac[56], [0x1F, 0x1F, 0x3F]);
        assert_eq!(vga.dac[104], [0x00, 0x00, 0x1C]);
        assert_eq!(vga.dac[176], [0x00, 0x00, 0x10]);
        assert_eq!(vga.dac[247], [0x0B, 0x0C, 0x10]);
        assert_eq!(vga.dac[248], [0, 0, 0]);
    }

    #[test]
    fn bios_data_area_follows_the_mode() {
        let (_, mem, _) = setup(0x12);
        assert_eq!(mem.read8(BDA_BASE + 0x49), 0x12);
        assert_eq!(mem.read16(BDA_BASE + 0x4A), 80);
        assert_eq!(mem.read16(BDA_BASE + 0x4C), 0xA000);
        assert_eq!(mem.read16(BDA_BASE + 0x4E), 0);
        assert_eq!(mem.read16(BDA_BASE + 0x60), 0x0607);
        assert_eq!(mem.read8(BDA_BASE + 0x62), 0);
        assert_eq!(mem.read16(BDA_BASE + 0x63), 0x3D4);
        assert_eq!(mem.read8(BDA_BASE + 0x84), 29);
        assert_eq!(mem.read16(BDA_BASE + 0x85), 16);
        assert_eq!(mem.read8(BDA_BASE + 0x87), 0x60);
        assert_eq!(mem.read8(BDA_BASE + 0x88), 0xF9);
        assert_eq!(mem.read8(BDA_BASE + 0x89), 0x51);

        let (mut vga, mut mem, _) = setup(3);
        assert_eq!(mem.read8(BDA_BASE + 0x65), 0x29);
        assert_eq!(mem.read8(BDA_BASE + 0x66), 0x30);
        vga.set_cursor(&mut mem, 3, 4);
        assert_eq!(mem.read16(BDA_BASE + 0x50), 0x0304);
        assert_eq!(
            vga.crtc[0x0F] as usize + ((vga.crtc[0x0E] as usize) << 8),
            3 * 80 + 4
        );
        vga.teletype(&mut mem, b'A');
        assert_eq!(mem.read16(BDA_BASE + 0x50), 0x0305);
    }

    // ------------------------------------------------------------------
    // The renderer's economy
    // ------------------------------------------------------------------

    #[test]
    fn palette_lut_is_rebuilt_only_on_change() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        vga.render(&mem, &mut screen, false);
        let builds = vga.lut_builds;
        vga.render(&mem, &mut screen, false);
        vga.vram_write(A, 3);
        vga.render(&mem, &mut screen, false);
        ac(&mut vga, &mut mem, 0x11, 3);
        vga.render(&mem, &mut screen, false);
        assert_eq!(vga.lut_builds, builds);
        out(&mut vga, &mut mem, 0x3C6, 0xFF);
        vga.render(&mem, &mut screen, false);
        assert_eq!(vga.lut_builds, builds);
        out(&mut vga, &mut mem, 0x3C6, 0x7F);
        vga.render(&mem, &mut screen, false);
        assert_eq!(vga.lut_builds, builds + 1);
        set_dac_port(&mut vga, &mut mem, 1, [1, 2, 3]);
        vga.render(&mem, &mut screen, false);
        assert_eq!(vga.lut_builds, builds + 2);
        assert_eq!(vga.lut[1], dac_to_xrgb([1, 2, 3]));
    }

    #[test]
    fn dirty_follows_the_pipeline() {
        let (mut vga, mut mem, mut screen) = setup(0x13);
        vga.render(&mem, &mut screen, false);
        assert!(!vga.dirty);
        vga.vram_read(A);
        assert!(!vga.dirty);
        vga.vram_write(A, 1);
        assert!(vga.dirty);
        vga.render(&mem, &mut screen, false);
        assert!(!vga.dirty);
        vga.mark_dirty();
        assert!(vga.dirty);
        vga.render(&mem, &mut screen, false);
        out(&mut vga, &mut mem, 0x3C8, 0);
        out(&mut vga, &mut mem, 0x3C9, 0);
        assert!(vga.dirty);
    }

    #[test]
    fn clear_homes_the_cursor_and_blanks_graphics_to_black() {
        let (mut vga, mut mem, _) = setup(3);
        vga.write_cell(&mut mem, 24, 79, b'x', 0x1F);
        vga.set_cursor(&mut mem, 9, 9);
        vga.clear(&mut mem, 0x2A);
        assert_eq!(vga.read_cell(&mem, 24, 79), (b' ', 0x2A));
        assert_eq!(vga.cursor, (0, 0));

        let (mut vga, mut mem, _) = setup(0x12);
        vga.put_pixel(&mut mem, 639, 479, 5);
        vga.clear(&mut mem, 0x07);
        assert_eq!(vga.read_pixel(&mem, 639, 479), 0);
    }

    #[test]
    fn render_rejects_a_short_buffer() {
        let (mut vga, mem, _) = setup(0x13);
        let mut small = vec![0u32; 16];
        vga.render(&mem, &mut small, false);
        assert!(small.iter().all(|pixel| *pixel == 0));
    }

    #[test]
    fn blit_centres_with_integer_scale() {
        let (mut vga, mem, mut screen) = setup(0x13);
        for pixel in 0..4 {
            vga.vram_write(A + pixel, 15);
        }
        vga.render(&mem, &mut screen, false);

        // 1280x1000 fits 640x400 at scale 2 with margins.
        let mut pixels = vec![0x55u32; 1280 * 1000];
        let mut canvas = Canvas::new(&mut pixels, 1280, 1000, 1280).unwrap();
        vga.blit(&mut canvas, &screen);
        let y0 = (1000 - 800) / 2;
        assert_eq!(pixels[y0 * 1280 + 15] & 0xFF_FFFF, 0xFF_FFFF);
        assert_eq!(pixels[y0 * 1280 + 16] & 0xFF_FFFF, 0);
        assert_eq!(pixels[(y0 - 1) * 1280] & 0xFF_FFFF, 0);

        // Smaller than the display: clipped, no panic.
        let mut pixels = vec![0u32; 100 * 100];
        let mut canvas = Canvas::new(&mut pixels, 100, 100, 100).unwrap();
        vga.blit(&mut canvas, &screen);
        assert_eq!(pixels[0] & 0xFF_FFFF, 0xFF_FFFF);

        // Narrow display: the margins are centred.
        let (mut vga, mut mem, mut screen) = setup(0x0D);
        seq(&mut vga, &mut mem, 1, 0x01);
        assert_eq!(vga.display_size(), (320, 400));
        vga.planes[0] = 0xFFFF_FFFF;
        vga.render(&mem, &mut screen, false);
        let mut pixels = vec![0x55u32; 640 * 400];
        let mut canvas = Canvas::new(&mut pixels, 640, 400, 640).unwrap();
        vga.blit(&mut canvas, &screen);
        assert_eq!(pixels[160] & 0xFF_FFFF, 0xFF_FFFF);
        assert_eq!(pixels[159] & 0xFF_FFFF, 0);
    }

    // ------------------------------------------------------------------
    // Save states
    // ------------------------------------------------------------------

    fn busy_state() -> (Vga, Memory, Vec<u32>) {
        let (mut vga, mut mem, screen) = setup(0x13);
        vga.vram_write(A + 100, 44);
        set_dac_port(&mut vga, &mut mem, 44, [1, 2, 3]);
        out(&mut vga, &mut mem, 0x3C7, 9);
        vga.port_in(0x3C9, &mem, 0);
        vga.vram_read(A + 100);
        mode_x(&mut vga, &mut mem);
        (vga, mem, screen)
    }

    #[test]
    fn save_state_round_trips() {
        let (mut vga, mem, mut screen) = busy_state();
        let mut data = vec![0xEE, 0xEE];
        vga.save_state(&mut data);
        vga.render(&mem, &mut screen, false);

        let mut other = Vga::new();
        let mut at_pos = 2;
        assert!(other.load_state(&data, &mut at_pos));
        assert_eq!(at_pos, data.len());
        assert_eq!(other.mode, 0x13);
        assert_eq!(other.kind, vga.kind);
        assert_eq!(other.display_size(), vga.display_size());
        assert_eq!(other.latch, vga.latch);
        assert_eq!(other.dac_read_component, 1);
        assert!(other.dirty);
        let mut restored = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
        other.render(&mem, &mut restored, false);
        assert_eq!(screen, restored);
    }

    #[test]
    fn truncated_or_bad_states_are_refused_and_change_nothing() {
        let (vga, _, _) = busy_state();
        let mut data = Vec::new();
        vga.save_state(&mut data);
        let fixed_part = data.len() - (PLANE_SIZE * 4) - 4;

        let mut target = Vga::new();
        target.bell = 77;
        for len in (0..fixed_part).chain([fixed_part + 100, data.len() - 1]) {
            let mut at_pos = 0;
            assert!(
                !target.load_state(&data[..len], &mut at_pos),
                "length {len}"
            );
            assert_eq!(at_pos, 0);
            assert_eq!(target.bell, 77);
        }
        let mut at_pos = data.len() + 5;
        assert!(!target.load_state(&data, &mut at_pos));
        let mut at_pos = usize::MAX;
        assert!(!target.load_state(&data, &mut at_pos));

        for (offset, value) in [
            (0usize, 0u8), // magic
            (4, 99),       // version
            (5, 0x7F),     // mode
            (6, 2),        // flag
            (7, 1),        // VESA mode without its memory
            (11, 9),       // page beyond the pages
            (12, 0),       // pages
            (13, 0),       // character height
            (13, 33),      // character height
        ] {
            let mut bad = data.clone();
            bad[offset] = value;
            let mut at_pos = 0;
            assert!(!target.load_state(&bad, &mut at_pos), "offset {offset}");
            assert_eq!(target.bell, 77);
        }
    }

    #[test]
    fn corrupted_register_bytes_cannot_panic_render() {
        let (vga, mem, _) = busy_state();
        let mut data = Vec::new();
        vga.save_state(&mut data);
        let fixed_part = data.len() - (PLANE_SIZE * 4) - 4;
        let mut screen = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];
        let mut seed = 0x1234_5678u32;
        for _ in 0..60 {
            let mut bad = data.clone();
            for _ in 0..24 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let offset = seed as usize % fixed_part;
                bad[offset] = (seed >> 24) as u8;
            }
            let mut target = Vga::new();
            let mut at_pos = 0;
            target.load_state(&bad, &mut at_pos);
            target.render(&mem, &mut screen, true);
            assert!(target.display_size().0 <= SCREEN_WIDTH);
        }
    }

    #[test]
    fn vesa_state_round_trips() {
        let (mut vga, mut mem, mut screen) = setup(3);
        assert!(vga.set_vesa_mode(&mut mem, 0x101, true));
        vga.vesa_bank = 3;
        vga.vram_write(A + 7, 200);
        vga.render(&mem, &mut screen, false);
        let mut data = Vec::new();
        vga.save_state(&mut data);
        let mut other = Vga::new();
        let mut at_pos = 0;
        assert!(other.load_state(&data, &mut at_pos));
        assert_eq!(other.vesa_mode, 0x101);
        assert_eq!(other.vesa_bank, 3);
        assert_eq!(other.vram_read(A + 7), 200);
        assert_eq!(other.kind, GraphicsKind::Vesa);
        // A wrong VESA memory size is refused.
        let mut bad = data.clone();
        let len_at = bad.len() - SVGA_SIZE - 4;
        bad[len_at] = 1;
        let mut at_pos = 0;
        assert!(!other.load_state(&bad, &mut at_pos));
    }
}
