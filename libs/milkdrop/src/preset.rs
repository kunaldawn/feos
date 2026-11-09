//! The `.milk` preset format.
//!
//! A preset is an INI-style list of `key=value` lines under a `[preset00]`
//! heading. Most keys set a starting value for a per-frame variable, under a
//! name of its own (`fDecay` is `decay`, `nWaveMode` is `wave_mode`). Code
//! comes in numbered lines: `per_frame_init_1=`, `per_frame_1=`,
//! `per_pixel_1=`, and for MilkDrop 2's custom waves and shapes,
//! `wave_0_per_point1=`, `shape_3_per_frame2=` and so on, with their settings
//! as `wavecode_0_samples=` and `shapecode_3_sides=`.
//!
//! Lines this doesn't know are ignored, as MilkDrop ignored them, except that
//! pixel shader code (`warp_1=`, `comp_1=`) is noted: see [`Preset::has_shaders`].

use alloc::string::String;
use alloc::vec::Vec;

/// Most custom waves and shapes a preset can have, as in MilkDrop 2.
pub const MAX_WAVES: usize = 4;
/// See [`MAX_WAVES`].
pub const MAX_SHAPES: usize = 4;
/// The biggest preset file read.
pub const MAX_PRESET_BYTES: usize = 256 * 1024;

/// Why a preset can't be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresetError {
    /// It's bigger than [`MAX_PRESET_BYTES`].
    TooLarge,
    /// No memory for it.
    OutOfMemory,
    /// Nothing in it is a preset setting.
    NotAPreset,
}

macro_rules! slots {
    ($(#[$meta:meta])* $name:ident, $names:ident { $($variant:ident = $text:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[repr(usize)]
        #[allow(missing_docs)]
        pub enum $name {
            $($variant,)*
        }

        /// The names of the slots, in slot order.
        pub const $names: &[&str] = &[$($text,)*];

        impl $name {
            /// Every slot, in slot order.
            pub const ALL: &[Self] = &[$(Self::$variant,)*];

            /// The slot's index.
            pub const fn at(self) -> usize {
                self as usize
            }
        }
    };
}

slots! {
    /// The main context's built-in variables. `q1`..`q32` follow them, at
    /// [`MAIN_Q`].
    Main, MAIN_NAMES {
        Time = "time",
        Fps = "fps",
        Frame = "frame",
        Progress = "progress",
        Bass = "bass",
        Mid = "mid",
        Treb = "treb",
        BassAtt = "bass_att",
        MidAtt = "mid_att",
        TrebAtt = "treb_att",
        MeshX = "meshx",
        MeshY = "meshy",
        AspectX = "aspectx",
        AspectY = "aspecty",
        PixelsX = "pixelsx",
        PixelsY = "pixelsy",
        X = "x",
        Y = "y",
        Rad = "rad",
        Ang = "ang",
        // Everything from here to `Monitor` starts each frame at the preset's value.
        Zoom = "zoom",
        ZoomExp = "zoomexp",
        Rot = "rot",
        Warp = "warp",
        Cx = "cx",
        Cy = "cy",
        Dx = "dx",
        Dy = "dy",
        Sx = "sx",
        Sy = "sy",
        Decay = "decay",
        Gamma = "gamma",
        EchoZoom = "echo_zoom",
        EchoAlpha = "echo_alpha",
        EchoOrient = "echo_orient",
        WaveMode = "wave_mode",
        WaveX = "wave_x",
        WaveY = "wave_y",
        WaveR = "wave_r",
        WaveG = "wave_g",
        WaveB = "wave_b",
        WaveA = "wave_a",
        WaveMystery = "wave_mystery",
        WaveDots = "wave_usedots",
        WaveThick = "wave_thick",
        WaveAdditive = "wave_additive",
        WaveBrighten = "wave_brighten",
        ObSize = "ob_size",
        ObR = "ob_r",
        ObG = "ob_g",
        ObB = "ob_b",
        ObA = "ob_a",
        IbSize = "ib_size",
        IbR = "ib_r",
        IbG = "ib_g",
        IbB = "ib_b",
        IbA = "ib_a",
        MvX = "mv_x",
        MvY = "mv_y",
        MvDx = "mv_dx",
        MvDy = "mv_dy",
        MvL = "mv_l",
        MvR = "mv_r",
        MvG = "mv_g",
        MvB = "mv_b",
        MvA = "mv_a",
        DarkenCenter = "darken_center",
        Wrap = "wrap",
        Invert = "invert",
        Brighten = "brighten",
        Darken = "darken",
        Solarize = "solarize",
        Monitor = "monitor",
    }
}

/// Where `q1` is in the main context; `q32` is 31 after it.
pub const MAIN_Q: usize = MAIN_NAMES.len();
/// The `q` variables, shared from the main context with waves and shapes.
pub const Q_COUNT: usize = 32;
/// The `t` variables, a wave's or shape's own.
pub const T_COUNT: usize = 8;

slots! {
    /// A custom wave's built-in variables. `q1`..`q32` then `t1`..`t8` follow.
    WaveVar, WAVE_NAMES {
        Time = "time",
        Fps = "fps",
        Frame = "frame",
        Progress = "progress",
        Bass = "bass",
        Mid = "mid",
        Treb = "treb",
        BassAtt = "bass_att",
        MidAtt = "mid_att",
        TrebAtt = "treb_att",
        Samples = "samples",
        Sample = "sample",
        Value1 = "value1",
        Value2 = "value2",
        X = "x",
        Y = "y",
        R = "r",
        G = "g",
        B = "b",
        A = "a",
    }
}

slots! {
    /// A custom shape's built-in variables. `q1`..`q32` then `t1`..`t8` follow.
    ShapeVar, SHAPE_NAMES {
        Time = "time",
        Fps = "fps",
        Frame = "frame",
        Progress = "progress",
        Bass = "bass",
        Mid = "mid",
        Treb = "treb",
        BassAtt = "bass_att",
        MidAtt = "mid_att",
        TrebAtt = "treb_att",
        Instance = "instance",
        NumInst = "num_inst",
        X = "x",
        Y = "y",
        Rad = "rad",
        Ang = "ang",
        TexAng = "tex_ang",
        TexZoom = "tex_zoom",
        Sides = "sides",
        Textured = "textured",
        Additive = "additive",
        Thick = "thick",
        R = "r",
        G = "g",
        B = "b",
        A = "a",
        R2 = "r2",
        G2 = "g2",
        B2 = "b2",
        A2 = "a2",
        BorderR = "border_r",
        BorderG = "border_g",
        BorderB = "border_b",
        BorderA = "border_a",
    }
}

/// The first variable a frame resets to the preset's value.
pub const FIRST_PARAM: usize = Main::Zoom as usize;

/// What the main context's parameters start at when a preset doesn't say.
pub fn default_value(slot: Main) -> f64 {
    match slot {
        Main::Zoom | Main::ZoomExp | Main::Sx | Main::Sy | Main::Gamma => 1.0,
        Main::Cx | Main::Cy | Main::WaveX | Main::WaveY => 0.5,
        Main::Warp => 1.0,
        Main::Decay => 0.98,
        Main::EchoZoom => 2.0,
        Main::WaveR | Main::WaveG | Main::WaveB => 1.0,
        Main::WaveA => 0.8,
        Main::ObSize | Main::IbSize => 0.01,
        Main::IbR | Main::IbG | Main::IbB => 0.25,
        Main::MvX => 12.0,
        Main::MvY => 9.0,
        Main::MvL => 0.9,
        Main::MvR | Main::MvG | Main::MvB | Main::MvA => 1.0,
        Main::Wrap => 1.0,
        _ => 0.0,
    }
}

/// Spellings the file uses for main-context parameters.
const FILE_KEYS: &[(&str, Main)] = &[
    ("fdecay", Main::Decay),
    ("fgammaadj", Main::Gamma),
    ("fvideoechozoom", Main::EchoZoom),
    ("fvideoechoalpha", Main::EchoAlpha),
    ("nvideoechoorientation", Main::EchoOrient),
    ("nwavemode", Main::WaveMode),
    ("badditivewaves", Main::WaveAdditive),
    ("bwavedots", Main::WaveDots),
    ("bwavethick", Main::WaveThick),
    ("bmaximizewavecolor", Main::WaveBrighten),
    ("btexwrap", Main::Wrap),
    ("bdarkencenter", Main::DarkenCenter),
    ("bbrighten", Main::Brighten),
    ("bdarken", Main::Darken),
    ("bsolarize", Main::Solarize),
    ("binvert", Main::Invert),
    ("fwavealpha", Main::WaveA),
    ("fwaveparam", Main::WaveMystery),
    ("fzoomexponent", Main::ZoomExp),
    ("nmotionvectorsx", Main::MvX),
    ("nmotionvectorsy", Main::MvY),
];

/// A custom wave's settings and code.
#[derive(Clone, Debug)]
pub struct WaveDef {
    /// Whether it's drawn.
    pub enabled: bool,
    /// Points, up to 512.
    pub samples: f64,
    /// How far apart the two channels' samples are.
    pub sep: f64,
    /// Draw the spectrum rather than the waveform.
    pub spectrum: bool,
    /// Dots, not lines.
    pub dots: bool,
    /// Two pixels wide.
    pub thick: bool,
    /// Added to the image, rather than blended over it.
    pub additive: bool,
    /// Multiplies the data.
    pub scaling: f64,
    /// 0 to 1: how much each sample takes after the last.
    pub smoothing: f64,
    /// Colour and opacity.
    pub rgba: [f64; 4],
    /// Code run once.
    pub init: String,
    /// Code run each frame.
    pub per_frame: String,
    /// Code run for each point.
    pub per_point: String,
}

impl Default for WaveDef {
    fn default() -> Self {
        Self {
            enabled: false,
            samples: 512.0,
            sep: 0.0,
            spectrum: false,
            dots: false,
            thick: false,
            additive: false,
            scaling: 1.0,
            smoothing: 0.5,
            rgba: [1.0; 4],
            init: String::new(),
            per_frame: String::new(),
            per_point: String::new(),
        }
    }
}

/// A custom shape's settings and code.
#[derive(Clone, Debug)]
pub struct ShapeDef {
    /// Whether it's drawn.
    pub enabled: bool,
    /// How many times it's drawn each frame, each with its own `instance`.
    pub instances: f64,
    /// Starting values for the shape's variables, by [`ShapeVar`] slot.
    pub values: [f64; SHAPE_NAMES.len()],
    /// Code run once.
    pub init: String,
    /// Code run each frame, for each instance.
    pub per_frame: String,
}

impl Default for ShapeDef {
    fn default() -> Self {
        let mut values = [0.0; SHAPE_NAMES.len()];
        values[ShapeVar::X.at()] = 0.5;
        values[ShapeVar::Y.at()] = 0.5;
        values[ShapeVar::Rad.at()] = 0.1;
        values[ShapeVar::TexZoom.at()] = 1.0;
        values[ShapeVar::Sides.at()] = 4.0;
        values[ShapeVar::R.at()] = 1.0;
        values[ShapeVar::A.at()] = 1.0;
        values[ShapeVar::G2.at()] = 1.0;
        values[ShapeVar::BorderR.at()] = 1.0;
        values[ShapeVar::BorderG.at()] = 1.0;
        values[ShapeVar::BorderB.at()] = 1.0;
        values[ShapeVar::BorderA.at()] = 0.1;
        values[ShapeVar::NumInst.at()] = 1.0;
        Self {
            enabled: false,
            instances: 1.0,
            values,
            init: String::new(),
            per_frame: String::new(),
        }
    }
}

/// A preset, read but not yet compiled.
#[derive(Clone, Debug)]
pub struct Preset {
    /// Starting values for the main context's built-ins, by [`Main`] slot.
    pub values: [f64; MAIN_NAMES.len()],
    /// How fast the warp's wobble moves.
    pub warp_speed: f64,
    /// How fine the warp's wobble is.
    pub warp_scale: f64,
    /// Multiplies the main waveform.
    pub wave_scale: f64,
    /// 0 to 1: how much the main waveform is smoothed.
    pub wave_smoothing: f64,
    /// Fade the main wave with the volume, between these two loudnesses.
    pub wave_alpha_by_volume: Option<(f64, f64)>,
    /// `per_frame_init` code.
    pub init: String,
    /// `per_frame` code.
    pub per_frame: String,
    /// `per_pixel` code.
    pub per_pixel: String,
    /// Custom waves.
    pub waves: Vec<WaveDef>,
    /// Custom shapes.
    pub shapes: Vec<ShapeDef>,
    /// The preset has pixel shaders, which this can't run.
    pub has_shaders: bool,
}

/// A code line waiting to be put in order.
struct Line<'a> {
    /// 0 is the main context; waves are 1..=4, shapes 5..=8.
    owner: u8,
    /// Which block of the owner's code.
    block: u8,
    number: u32,
    text: &'a str,
}

const BLOCK_INIT: u8 = 0;
const BLOCK_FRAME: u8 = 1;
const BLOCK_PIXEL: u8 = 2;

impl Preset {
    /// Reads a preset from the text of a `.milk` file.
    pub fn parse(text: &str) -> Result<Self, PresetError> {
        if text.len() > MAX_PRESET_BYTES {
            return Err(PresetError::TooLarge);
        }
        let mut values = [0.0; MAIN_NAMES.len()];
        for (slot, value) in values.iter_mut().enumerate().skip(FIRST_PARAM) {
            *value = default_value(main_slot(slot));
        }
        let mut preset = Self {
            values,
            warp_speed: 1.0,
            warp_scale: 1.0,
            wave_scale: 1.0,
            wave_smoothing: 0.75,
            wave_alpha_by_volume: None,
            init: String::new(),
            per_frame: String::new(),
            per_pixel: String::new(),
            waves: Vec::new(),
            shapes: Vec::new(),
            has_shaders: false,
        };
        let mut waves: [WaveDef; MAX_WAVES] = Default::default();
        let mut shapes: [ShapeDef; MAX_SHAPES] = Default::default();
        let mut alpha_by_volume = false;
        let mut alpha_range = (0.75, 0.95);
        let mut lines: Vec<Line<'_>> = Vec::new();
        let mut recognised = 0usize;

        for raw in text.lines() {
            let line = raw.trim_start_matches('\u{feff}').trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() || key.len() > 48 {
                continue;
            }
            let mut lower = [0u8; 48];
            for (to, from) in lower.iter_mut().zip(key.bytes()) {
                *to = from.to_ascii_lowercase();
            }
            let Ok(key) = core::str::from_utf8(&lower[..key.len()]) else {
                continue;
            };
            let number = parse_number(value);

            if let Some(code) = code_line(key, value) {
                lines.try_reserve(1).map_err(|_| PresetError::OutOfMemory)?;
                lines.push(code);
                recognised += 1;
                continue;
            }
            if is_shader_line(key) {
                preset.has_shaders = true;
                continue;
            }
            if let Some((index, field)) = indexed(key, "wavecode_") {
                if let (Some(wave), Some(number)) = (waves.get_mut(index), number) {
                    set_wave(wave, field, number);
                    recognised += 1;
                }
                continue;
            }
            if let Some((index, field)) = indexed(key, "shapecode_") {
                if let (Some(shape), Some(number)) = (shapes.get_mut(index), number) {
                    set_shape(shape, field, number);
                    recognised += 1;
                }
                continue;
            }
            let Some(number) = number else {
                continue;
            };
            recognised += 1;
            match key {
                "fwarpanimspeed" => preset.warp_speed = number,
                "fwarpscale" => preset.warp_scale = number,
                "fwavescale" | "wave_scale" => preset.wave_scale = number,
                "fwavesmoothing" | "wave_smoothing" => preset.wave_smoothing = number,
                "bmodwavealphabyvolume" => alpha_by_volume = number != 0.0,
                "fmodwavealphastart" => alpha_range.0 = number,
                "fmodwavealphaend" => alpha_range.1 = number,
                _ => {
                    let slot = FILE_KEYS
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, slot)| slot.at())
                        .or_else(|| {
                            MAIN_NAMES[FIRST_PARAM..]
                                .iter()
                                .position(|name| *name == key)
                                .map(|at| at + FIRST_PARAM)
                        });
                    match slot {
                        Some(slot) => preset.values[slot] = number,
                        None => recognised -= 1,
                    }
                }
            }
        }
        if recognised == 0 {
            return Err(PresetError::NotAPreset);
        }
        if alpha_by_volume {
            preset.wave_alpha_by_volume = Some(alpha_range);
        }

        // Code lines in their numbered order, per owner and block.
        lines.sort_by_key(|line| (line.owner, line.block, line.number));
        for line in &lines {
            let target = match line.owner {
                0 => match line.block {
                    BLOCK_INIT => &mut preset.init,
                    BLOCK_FRAME => &mut preset.per_frame,
                    _ => &mut preset.per_pixel,
                },
                owner @ 1..=4 => {
                    let wave = &mut waves[owner as usize - 1];
                    match line.block {
                        BLOCK_INIT => &mut wave.init,
                        BLOCK_FRAME => &mut wave.per_frame,
                        _ => &mut wave.per_point,
                    }
                }
                owner => {
                    let shape = &mut shapes[(owner - 5) as usize];
                    match line.block {
                        BLOCK_INIT => &mut shape.init,
                        _ => &mut shape.per_frame,
                    }
                }
            };
            target
                .try_reserve(line.text.len() + 1)
                .map_err(|_| PresetError::OutOfMemory)?;
            target.push_str(line.text);
            target.push('\n');
        }
        for wave in waves {
            if wave.enabled {
                preset.waves.try_reserve(1).map_err(|_| PresetError::OutOfMemory)?;
                preset.waves.push(wave);
            }
        }
        for shape in shapes {
            if shape.enabled {
                preset.shapes.try_reserve(1).map_err(|_| PresetError::OutOfMemory)?;
                preset.shapes.push(shape);
            }
        }
        Ok(preset)
    }
}

fn main_slot(index: usize) -> Main {
    Main::ALL[index.min(Main::ALL.len() - 1)]
}

fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim();
    let value: f64 = text.parse().ok()?;
    value.is_finite().then_some(value)
}

fn is_shader_line(key: &str) -> bool {
    for prefix in ["warp_", "comp_"] {
        if let Some(rest) = key.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// `prefix` then a digit, an underscore and a field name.
fn indexed<'k>(key: &'k str, prefix: &str) -> Option<(usize, &'k str)> {
    let rest = key.strip_prefix(prefix)?;
    let (index, field) = rest.split_once('_')?;
    Some((index.parse().ok()?, field))
}

/// The digits at the end of `rest`, which must be all that's left.
fn line_number(rest: &str) -> Option<u32> {
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn code_line<'a>(key: &str, value: &'a str) -> Option<Line<'a>> {
    let main = |block, rest: &str| {
        line_number(rest).map(|number| Line {
            owner: 0,
            block,
            number,
            text: value,
        })
    };
    if let Some(rest) = key.strip_prefix("per_frame_init_") {
        return main(BLOCK_INIT, rest);
    }
    if let Some(rest) = key.strip_prefix("per_frame_") {
        return main(BLOCK_FRAME, rest);
    }
    if let Some(rest) = key.strip_prefix("per_pixel_") {
        return main(BLOCK_PIXEL, rest);
    }
    for (prefix, base, count) in [("wave_", 1u8, MAX_WAVES), ("shape_", 5u8, MAX_SHAPES)] {
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        let (index, rest) = rest.split_once('_')?;
        let index: usize = index.parse().ok()?;
        if index >= count {
            return None;
        }
        let blocks: &[(&str, u8)] = if base == 1 {
            &[("init", BLOCK_INIT), ("per_frame", BLOCK_FRAME), ("per_point", BLOCK_PIXEL)]
        } else {
            &[("init", BLOCK_INIT), ("per_frame", BLOCK_FRAME)]
        };
        for (name, block) in blocks {
            if let Some(number) = rest.strip_prefix(name).and_then(line_number) {
                return Some(Line {
                    owner: base + index as u8,
                    block: *block,
                    number,
                    text: value,
                });
            }
        }
        return None;
    }
    None
}

fn set_wave(wave: &mut WaveDef, field: &str, value: f64) {
    match field {
        "enabled" => wave.enabled = value != 0.0,
        "samples" => wave.samples = value,
        "sep" => wave.sep = value,
        "bspectrum" => wave.spectrum = value != 0.0,
        "busedots" => wave.dots = value != 0.0,
        "bdrawthick" => wave.thick = value != 0.0,
        "badditive" => wave.additive = value != 0.0,
        "scaling" => wave.scaling = value,
        "smoothing" => wave.smoothing = value,
        "r" => wave.rgba[0] = value,
        "g" => wave.rgba[1] = value,
        "b" => wave.rgba[2] = value,
        "a" => wave.rgba[3] = value,
        _ => {}
    }
}

fn set_shape(shape: &mut ShapeDef, field: &str, value: f64) {
    let slot = match field {
        "enabled" => {
            shape.enabled = value != 0.0;
            return;
        }
        "num_inst" => {
            shape.instances = value;
            ShapeVar::NumInst
        }
        "sides" => ShapeVar::Sides,
        "additive" => ShapeVar::Additive,
        "thickoutline" => ShapeVar::Thick,
        "textured" => ShapeVar::Textured,
        "x" => ShapeVar::X,
        "y" => ShapeVar::Y,
        "rad" => ShapeVar::Rad,
        "ang" => ShapeVar::Ang,
        "tex_ang" => ShapeVar::TexAng,
        "tex_zoom" => ShapeVar::TexZoom,
        "r" => ShapeVar::R,
        "g" => ShapeVar::G,
        "b" => ShapeVar::B,
        "a" => ShapeVar::A,
        "r2" => ShapeVar::R2,
        "g2" => ShapeVar::G2,
        "b2" => ShapeVar::B2,
        "a2" => ShapeVar::A2,
        "border_r" => ShapeVar::BorderR,
        "border_g" => ShapeVar::BorderG,
        "border_b" => ShapeVar::BorderB,
        "border_a" => ShapeVar::BorderA,
        _ => return,
    };
    shape.values[slot.at()] = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[preset00]\r\n\
        fRating=3.0\r\n\
        fDecay=0.930000\r\n\
        nWaveMode=6\r\n\
        zoom=1.01\r\n\
        bModWaveAlphaByVolume=1\r\n\
        fModWaveAlphaStart=0.5\r\n\
        wavecode_0_enabled=1\r\n\
        wavecode_0_samples=256\r\n\
        wave_0_per_point2=y = y + 0.1;\r\n\
        wave_0_per_point1=x = sample;\r\n\
        shapecode_1_enabled=1\r\n\
        shapecode_1_sides=6\r\n\
        shape_1_per_frame1=ang = time;\r\n\
        per_frame_2=rot = 0.1;\r\n\
        per_frame_1=zoom = zoom + 0.01*bass;\r\n\
        per_frame_10=dx = 0;\r\n\
        per_pixel_1=zoom = zoom + rad*0.1;\r\n\
        per_frame_init_1=q1 = 4;\r\n\
        warp_1=`shader_body {\r\n";

    #[test]
    fn reads_settings_under_their_file_names() {
        let preset = Preset::parse(SAMPLE).unwrap();
        assert_eq!(preset.values[Main::Decay.at()], 0.93);
        assert_eq!(preset.values[Main::WaveMode.at()], 6.0);
        assert_eq!(preset.values[Main::Zoom.at()], 1.01);
        assert_eq!(preset.values[Main::Cx.at()], 0.5);
        assert_eq!(preset.wave_alpha_by_volume, Some((0.5, 0.95)));
        assert!(preset.has_shaders);
    }

    #[test]
    fn code_comes_out_in_line_number_order() {
        let preset = Preset::parse(SAMPLE).unwrap();
        assert_eq!(preset.per_frame, "zoom = zoom + 0.01*bass;\nrot = 0.1;\ndx = 0;\n");
        assert_eq!(preset.per_pixel, "zoom = zoom + rad*0.1;\n");
        assert_eq!(preset.init, "q1 = 4;\n");
        assert_eq!(preset.waves.len(), 1);
        assert_eq!(preset.waves[0].samples, 256.0);
        assert_eq!(preset.waves[0].per_point, "x = sample;\ny = y + 0.1;\n");
        assert_eq!(preset.shapes.len(), 1);
        assert_eq!(preset.shapes[0].values[ShapeVar::Sides.at()], 6.0);
        assert_eq!(preset.shapes[0].per_frame, "ang = time;\n");
    }

    #[test]
    fn rejects_what_is_not_a_preset() {
        assert_eq!(Preset::parse("hello world").unwrap_err(), PresetError::NotAPreset);
        assert_eq!(Preset::parse("").unwrap_err(), PresetError::NotAPreset);
        let huge = "a".repeat(MAX_PRESET_BYTES + 1);
        assert_eq!(Preset::parse(&huge).unwrap_err(), PresetError::TooLarge);
        // Odd lines are skipped, not fatal.
        assert!(Preset::parse("zoom=1\n=\n==\nwavecode_99_enabled=1\nshape_9_init1=x\nper_frame_x=1\n").is_ok());
    }

    #[test]
    fn slot_tables_agree_with_their_enums() {
        assert_eq!(MAIN_NAMES[Main::Monitor.at()], "monitor");
        assert_eq!(main_slot(Main::Solarize.at()), Main::Solarize);
        assert_eq!(WAVE_NAMES[WaveVar::A.at()], "a");
        assert_eq!(SHAPE_NAMES[ShapeVar::BorderA.at()], "border_a");
    }
}
