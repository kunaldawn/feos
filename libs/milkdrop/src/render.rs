//! Drawing a preset, a frame at a time, in software.
//!
//! The image is a feedback loop, as in MilkDrop:
//!
//! 1. The per-frame code runs, and sets the motion for this frame.
//! 2. A coarse mesh covers the screen. At each vertex the per-pixel code (if
//!    any) adjusts that motion, which gives the vertex a place in the last
//!    frame to take its colour from: zoomed, rotated, stretched, shifted and
//!    wobbled. Between vertices, the place is interpolated.
//! 3. The last frame is resampled through the mesh, and darkened by `decay`.
//! 4. Motion vectors, custom shapes, custom waves, the main waveform and the
//!    borders are drawn over it. They're part of the image now, so next frame
//!    they move with everything else: that's where the trails come from.
//! 5. What's shown is a composite of that image: a zoomed "video echo" of it
//!    mixed in, then gamma, brighten, darken, solarize and invert. None of
//!    this feeds back.
//!
//! Pixels are `0x00RRGGBB`, the framebuffer's format.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::f32::consts::PI;

use feos_math as m;

use crate::audio::{Analyzer, SAMPLES, SPECTRUM};
use crate::eel::{Env, Globals, Program, VarTable};
use crate::preset::{
    Main, Preset, PresetError, ShapeDef, ShapeVar, WaveDef, WaveVar, FIRST_PARAM, MAIN_NAMES,
    MAIN_Q, Q_COUNT, SHAPE_NAMES, T_COUNT, WAVE_NAMES,
};

/// Mesh cells across.
pub const MESH_W: usize = 40;
/// Mesh cells down.
pub const MESH_H: usize = 30;
/// Most instances of one shape drawn in a frame.
const MAX_INSTANCES: usize = 1024;
/// Most points in a custom wave.
const MAX_WAVE_POINTS: usize = 512;
/// Most pixels drawn for one line, so a wild point can't take forever.
const MAX_LINE_STEPS: usize = 4096;

/// Shown when no preset is loaded, or one won't compile. Written for FeOS.
pub const DEFAULT_PRESET: &str = "[preset00]
fDecay=0.965
fVideoEchoZoom=1.006
fVideoEchoAlpha=0.3
nVideoEchoOrientation=3
nWaveMode=0
bWaveThick=1
bMaximizeWaveColor=1
fWaveAlpha=0.9
fWaveScale=1.3
fWaveSmoothing=0.6
zoom=1.03
warp=0.6
ob_size=0.005
ob_a=0.6
per_frame_1=wave_r = 0.5 + 0.5*sin(time*1.13);
per_frame_2=wave_g = 0.5 + 0.5*sin(time*1.23 + 2);
per_frame_3=wave_b = 0.5 + 0.5*sin(time*1.33 + 4);
per_frame_4=zoom = 1.0 + 0.05*bass_att;
per_frame_5=rot = 0.015*sin(time*0.37);
per_frame_6=ob_r = wave_b; ob_g = wave_r; ob_b = wave_g;
per_pixel_1=rot = rot + 0.02*rad*sin(time*0.71 + rad*3);
per_pixel_2=zoom = zoom + 0.02*sin(ang*3 + time);
";

/// How a preset moves the image at one point.
#[derive(Clone, Copy, Debug)]
struct Motion {
    zoom: f32,
    zoom_exp: f32,
    rot: f32,
    warp: f32,
    cx: f32,
    cy: f32,
    dx: f32,
    dy: f32,
    sx: f32,
    sy: f32,
}

const MOTION_SLOTS: [Main; 10] = [
    Main::Zoom,
    Main::ZoomExp,
    Main::Rot,
    Main::Warp,
    Main::Cx,
    Main::Cy,
    Main::Dx,
    Main::Dy,
    Main::Sx,
    Main::Sy,
];

/// The per-frame code's answers.
#[derive(Clone, Copy, Debug)]
struct Frame {
    motion: Motion,
    decay: f32,
    gamma: f32,
    echo_zoom: f32,
    echo_alpha: f32,
    echo_orient: u32,
    wave_mode: i32,
    wave_x: f32,
    wave_y: f32,
    wave: [f32; 4],
    wave_mystery: f32,
    wave_dots: bool,
    wave_thick: bool,
    wave_additive: bool,
    wave_brighten: bool,
    outer: [f32; 5],
    inner: [f32; 5],
    vectors: [f32; 9],
    darken_center: bool,
    wrap: bool,
    invert: bool,
    brighten: bool,
    darken: bool,
    solarize: bool,
}

/// A value, or `fallback` when the code made it NaN or infinite.
fn value(env: &Env, slot: usize, fallback: f32) -> f32 {
    let value = env.get(slot);
    if value.is_finite() {
        value.clamp(-1.0e6, 1.0e6) as f32
    } else {
        fallback
    }
}

fn flag(env: &Env, slot: usize) -> bool {
    env.get(slot).abs() > 0.00001
}

fn read_motion(env: &Env) -> Motion {
    Motion {
        zoom: value(env, Main::Zoom.at(), 1.0),
        zoom_exp: value(env, Main::ZoomExp.at(), 1.0),
        rot: value(env, Main::Rot.at(), 0.0),
        warp: value(env, Main::Warp.at(), 0.0),
        cx: value(env, Main::Cx.at(), 0.5),
        cy: value(env, Main::Cy.at(), 0.5),
        dx: value(env, Main::Dx.at(), 0.0),
        dy: value(env, Main::Dy.at(), 0.0),
        sx: value(env, Main::Sx.at(), 1.0),
        sy: value(env, Main::Sy.at(), 1.0),
    }
}

fn read_frame(env: &Env) -> Frame {
    let rgba = |slots: [Main; 4]| {
        let mut out = [0.0; 4];
        for (to, slot) in out.iter_mut().zip(slots) {
            *to = value(env, slot.at(), 0.0);
        }
        out
    };
    let five = |slots: [Main; 5]| {
        let mut out = [0.0; 5];
        for (to, slot) in out.iter_mut().zip(slots) {
            *to = value(env, slot.at(), 0.0);
        }
        out
    };
    let mut vectors = [0.0; 9];
    for (to, slot) in vectors.iter_mut().zip([
        Main::MvX,
        Main::MvY,
        Main::MvDx,
        Main::MvDy,
        Main::MvL,
        Main::MvR,
        Main::MvG,
        Main::MvB,
        Main::MvA,
    ]) {
        *to = value(env, slot.at(), 0.0);
    }
    Frame {
        motion: read_motion(env),
        decay: value(env, Main::Decay.at(), 0.98).clamp(0.0, 1.0),
        gamma: value(env, Main::Gamma.at(), 1.0).clamp(0.0, 8.0),
        echo_zoom: value(env, Main::EchoZoom.at(), 1.0),
        echo_alpha: value(env, Main::EchoAlpha.at(), 0.0).clamp(0.0, 1.0),
        echo_orient: (value(env, Main::EchoOrient.at(), 0.0) as i32).rem_euclid(4) as u32,
        wave_mode: value(env, Main::WaveMode.at(), 0.0) as i32,
        wave_x: value(env, Main::WaveX.at(), 0.5),
        wave_y: value(env, Main::WaveY.at(), 0.5),
        wave: rgba([Main::WaveR, Main::WaveG, Main::WaveB, Main::WaveA]),
        wave_mystery: value(env, Main::WaveMystery.at(), 0.0),
        wave_dots: flag(env, Main::WaveDots.at()),
        wave_thick: flag(env, Main::WaveThick.at()),
        wave_additive: flag(env, Main::WaveAdditive.at()),
        wave_brighten: flag(env, Main::WaveBrighten.at()),
        outer: five([Main::ObSize, Main::ObR, Main::ObG, Main::ObB, Main::ObA]),
        inner: five([Main::IbSize, Main::IbR, Main::IbG, Main::IbB, Main::IbA]),
        vectors,
        darken_center: flag(env, Main::DarkenCenter.at()),
        wrap: flag(env, Main::Wrap.at()),
        invert: flag(env, Main::Invert.at()),
        brighten: flag(env, Main::Brighten.at()),
        darken: flag(env, Main::Darken.at()),
        solarize: flag(env, Main::Solarize.at()),
    }
}

struct CompiledWave {
    def: WaveDef,
    env: Env,
    frame: Program,
    point: Program,
    t_init: [f64; T_COUNT],
}

struct CompiledShape {
    def: ShapeDef,
    env: Env,
    frame: Program,
    t_init: [f64; T_COUNT],
}

/// A preset compiled and ready to run.
struct Compiled {
    preset: Preset,
    env: Env,
    frame: Program,
    pixel: Program,
    q_init: [f64; Q_COUNT],
    waves: Vec<CompiledWave>,
    shapes: Vec<CompiledShape>,
}

/// How a preset went when it was loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LoadReport {
    /// Blocks of code that didn't compile, and were left out.
    pub errors: u32,
    /// It has pixel shaders, which aren't run.
    pub has_shaders: bool,
}

/// A table with `names` first, then `q1`..`q32`, then (if `t`) `t1`..`t8`.
fn table(names: &[&str], t: bool) -> Option<VarTable> {
    let mut table = VarTable::with_builtins(names);
    for index in 1..=Q_COUNT {
        table.slot(&format!("q{index}"))?;
    }
    if t {
        for index in 1..=T_COUNT {
            table.slot(&format!("t{index}"))?;
        }
    }
    Some(table)
}

fn compile(source: &str, table: &mut VarTable, errors: &mut u32) -> Program {
    match Program::compile(source, table) {
        Ok(program) => program,
        Err(_) => {
            *errors += 1;
            Program::default()
        }
    }
}

/// One point of a line strip, in pixels, with its colour.
#[derive(Clone, Copy, Debug)]
struct Point {
    x: f32,
    y: f32,
    rgba: [f32; 4],
}

/// Runs presets and draws them.
pub struct Visualizer {
    width: usize,
    height: usize,
    /// The image: last frame's until the warp, this frame's after.
    front: Vec<u32>,
    /// The other image: the warp's source, and after it last frame's.
    back: Vec<u32>,
    /// What's shown.
    out: Vec<u32>,
    /// Where each mesh vertex samples from, in source pixels.
    uv: Vec<[f32; 2]>,
    analyzer: Analyzer,
    globals: Globals,
    compiled: Option<Box<Compiled>>,
    time: f64,
    frame: u64,
    fps: f32,
    preset_start: f64,
    /// How long a preset is meant to show, for its `progress` variable.
    pub preset_seconds: f64,
    /// MilkDrop's aspect factors: 1 on the long side, less on the short.
    aspect_x: f32,
    aspect_y: f32,
    points: Vec<Point>,
    left: Vec<f32>,
    right: Vec<f32>,
}

fn zeroed<T: Clone>(len: usize, value: T) -> Option<Vec<T>> {
    let mut vector = Vec::new();
    vector.try_reserve_exact(len).ok()?;
    vector.resize(len, value);
    Some(vector)
}

impl Visualizer {
    /// A visualiser drawing at `width` by `height`, for audio at `sample_rate`.
    pub fn new(width: usize, height: usize, sample_rate: u32) -> Option<Self> {
        if width < 16 || height < 16 || width > 4096 || height > 4096 {
            return None;
        }
        let pixels = width * height;
        let mut visualizer = Self {
            width,
            height,
            front: zeroed(pixels, 0)?,
            back: zeroed(pixels, 0)?,
            out: zeroed(pixels, 0)?,
            uv: zeroed((MESH_W + 1) * (MESH_H + 1), [0.0; 2])?,
            analyzer: Analyzer::new(sample_rate)?,
            globals: Globals::new(0x5eed_f00d_cafe_d00d),
            compiled: None,
            time: 0.0,
            frame: 0,
            fps: 30.0,
            preset_start: 0.0,
            preset_seconds: 20.0,
            aspect_x: if height > width { width as f32 / height as f32 } else { 1.0 },
            aspect_y: if width > height { height as f32 / width as f32 } else { 1.0 },
            points: Vec::new(),
            left: zeroed(SAMPLES, 0.0)?,
            right: zeroed(SAMPLES, 0.0)?,
        };
        visualizer.points.try_reserve(SAMPLES * 2).ok()?;
        let _ = visualizer.load_text(DEFAULT_PRESET);
        Some(visualizer)
    }

    /// The image's width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The image's height.
    pub fn height(&self) -> usize {
        self.height
    }

    /// The last frame drawn, `width * height` pixels.
    pub fn pixels(&self) -> &[u32] {
        &self.out
    }

    /// The analysis of the last frame's audio.
    pub fn analyzer(&self) -> &Analyzer {
        &self.analyzer
    }

    /// Frames drawn per second, smoothed.
    pub fn fps(&self) -> f32 {
        self.fps
    }

    /// Seconds since the current preset started.
    pub fn preset_age(&self) -> f64 {
        self.time - self.preset_start
    }

    /// Reads, compiles and switches to a preset. The image carries over, so
    /// the new preset takes it from where the last one left it.
    pub fn load_text(&mut self, text: &str) -> Result<LoadReport, PresetError> {
        let preset = Preset::parse(text)?;
        self.load(preset)
    }

    /// Compiles and switches to `preset`.
    pub fn load(&mut self, preset: Preset) -> Result<LoadReport, PresetError> {
        let mut errors = 0;
        let mut main = table(MAIN_NAMES, false).ok_or(PresetError::OutOfMemory)?;
        let init = compile(&preset.init, &mut main, &mut errors);
        let frame = compile(&preset.per_frame, &mut main, &mut errors);
        let pixel = compile(&preset.per_pixel, &mut main, &mut errors);
        let mut env = Env::for_table(&main).ok_or(PresetError::OutOfMemory)?;
        env.vars[FIRST_PARAM..MAIN_NAMES.len()].copy_from_slice(&preset.values[FIRST_PARAM..]);
        self.preset_start = self.time;
        self.set_main_inputs(&mut env);
        init.run(&mut env, &mut self.globals);
        let mut q_init = [0.0; Q_COUNT];
        q_init.copy_from_slice(&env.vars[MAIN_Q..MAIN_Q + Q_COUNT]);

        let mut waves = Vec::new();
        waves.try_reserve(preset.waves.len()).map_err(|_| PresetError::OutOfMemory)?;
        for def in &preset.waves {
            let mut names = table(WAVE_NAMES, true).ok_or(PresetError::OutOfMemory)?;
            let init = compile(&def.init, &mut names, &mut errors);
            let frame = compile(&def.per_frame, &mut names, &mut errors);
            let point = compile(&def.per_point, &mut names, &mut errors);
            let mut env = Env::for_table(&names).ok_or(PresetError::OutOfMemory)?;
            let q = WAVE_NAMES.len();
            env.vars[q..q + Q_COUNT].copy_from_slice(&q_init);
            init.run(&mut env, &mut self.globals);
            let mut t_init = [0.0; T_COUNT];
            t_init.copy_from_slice(&env.vars[q + Q_COUNT..q + Q_COUNT + T_COUNT]);
            waves.push(CompiledWave {
                def: def.clone(),
                env,
                frame,
                point,
                t_init,
            });
        }
        let mut shapes = Vec::new();
        shapes.try_reserve(preset.shapes.len()).map_err(|_| PresetError::OutOfMemory)?;
        for def in &preset.shapes {
            let mut names = table(SHAPE_NAMES, true).ok_or(PresetError::OutOfMemory)?;
            let init = compile(&def.init, &mut names, &mut errors);
            let frame = compile(&def.per_frame, &mut names, &mut errors);
            let mut env = Env::for_table(&names).ok_or(PresetError::OutOfMemory)?;
            env.vars[..SHAPE_NAMES.len()].copy_from_slice(&def.values);
            let q = SHAPE_NAMES.len();
            env.vars[q..q + Q_COUNT].copy_from_slice(&q_init);
            init.run(&mut env, &mut self.globals);
            let mut t_init = [0.0; T_COUNT];
            t_init.copy_from_slice(&env.vars[q + Q_COUNT..q + Q_COUNT + T_COUNT]);
            shapes.push(CompiledShape {
                def: def.clone(),
                env,
                frame,
                t_init,
            });
        }
        let report = LoadReport {
            errors,
            has_shaders: preset.has_shaders,
        };
        self.compiled = Some(Box::new(Compiled {
            preset,
            env,
            frame,
            pixel,
            q_init,
            waves,
            shapes,
        }));
        Ok(report)
    }

    fn progress(&self) -> f64 {
        ((self.time - self.preset_start) / self.preset_seconds.max(0.1)).clamp(0.0, 1.0)
    }

    fn set_main_inputs(&self, env: &mut Env) {
        let a = &self.analyzer;
        env.set(Main::Time.at(), self.time);
        env.set(Main::Fps.at(), self.fps as f64);
        env.set(Main::Frame.at(), self.frame as f64);
        env.set(Main::Progress.at(), self.progress());
        env.set(Main::Bass.at(), a.bass as f64);
        env.set(Main::Mid.at(), a.mid as f64);
        env.set(Main::Treb.at(), a.treb as f64);
        env.set(Main::BassAtt.at(), a.bass_att as f64);
        env.set(Main::MidAtt.at(), a.mid_att as f64);
        env.set(Main::TrebAtt.at(), a.treb_att as f64);
        env.set(Main::MeshX.at(), MESH_W as f64);
        env.set(Main::MeshY.at(), MESH_H as f64);
        env.set(Main::AspectX.at(), 1.0 / self.aspect_x as f64);
        env.set(Main::AspectY.at(), 1.0 / self.aspect_y as f64);
        env.set(Main::PixelsX.at(), self.width as f64);
        env.set(Main::PixelsY.at(), self.height as f64);
    }

    /// The common inputs of a wave or shape context, in its first ten slots.
    fn set_shared_inputs(&self, env: &mut Env) {
        let a = &self.analyzer;
        let values = [
            self.time,
            self.fps as f64,
            self.frame as f64,
            self.progress(),
            a.bass as f64,
            a.mid as f64,
            a.treb as f64,
            a.bass_att as f64,
            a.mid_att as f64,
            a.treb_att as f64,
        ];
        env.vars[..values.len()].copy_from_slice(&values);
    }

    /// Draws a frame. `pcm` is the newest audio, interleaved stereo (576
    /// frames is what MilkDrop took), and `seconds` how long since the last.
    pub fn render(&mut self, pcm: &[i16], seconds: f32) {
        let seconds = seconds.clamp(0.001, 0.25);
        self.fps = self.fps * 0.9 + 0.1 / seconds;
        self.time += seconds as f64;
        self.frame += 1;
        self.analyzer.update(pcm, self.fps);

        let Some(mut compiled) = self.compiled.take() else {
            return;
        };
        let frame = self.run_frame(&mut compiled);
        self.build_mesh(&mut compiled, &frame);
        self.warp(&frame);
        self.draw_motion_vectors(&frame);
        self.draw_shapes(&mut compiled);
        self.draw_custom_waves(&mut compiled);
        self.draw_main_wave(&compiled.preset, &frame);
        if frame.darken_center {
            self.darken_center();
        }
        self.draw_borders(&frame);
        self.composite(&frame);
        self.compiled = Some(compiled);
    }

    fn run_frame(&mut self, compiled: &mut Compiled) -> Frame {
        let env = &mut compiled.env;
        env.vars[FIRST_PARAM..MAIN_NAMES.len()].copy_from_slice(&compiled.preset.values[FIRST_PARAM..]);
        self.set_main_inputs(env);
        env.vars[MAIN_Q..MAIN_Q + Q_COUNT].copy_from_slice(&compiled.q_init);
        compiled.frame.run(env, &mut self.globals);
        read_frame(env)
    }

    fn build_mesh(&mut self, compiled: &mut Compiled, frame: &Frame) {
        let per_pixel = !compiled.pixel.is_empty();
        let (ax, ay) = (self.aspect_x, self.aspect_y);
        let time = self.time;
        let warp_time = (time * compiled.preset.warp_speed) as f32;
        let warp_scale = compiled.preset.warp_scale as f32;
        let warp_scale_inv = if warp_scale.abs() > 0.0001 { 1.0 / warp_scale } else { 1.0 };
        let f = [
            11.68 + 4.0 * m::cos(time * 1.413 + 10.0) as f32,
            8.77 + 3.0 * m::cos(time * 1.113 + 7.0) as f32,
            10.54 + 3.0 * m::cos(time * 1.233 + 3.0) as f32,
            11.49 + 4.0 * m::cos(time * 0.933 + 5.0) as f32,
        ];
        let mut motion_values = [0.0f64; MOTION_SLOTS.len()];
        for (to, slot) in motion_values.iter_mut().zip(MOTION_SLOTS) {
            *to = compiled.env.get(slot.at());
        }
        let corner = m::f32::sqrt(ax * ax + ay * ay);
        let (w, h) = (self.width as f32, self.height as f32);
        let mut shared = (frame.motion, f32::NAN, 0.0f32, 0.0f32);
        for j in 0..=MESH_H {
            for i in 0..=MESH_W {
                let vx = i as f32 / MESH_W as f32 * 2.0 - 1.0;
                let vy = j as f32 / MESH_H as f32 * 2.0 - 1.0;
                let x = vx * 0.5 * ax + 0.5;
                let y = vy * 0.5 * ay + 0.5;
                let rad = m::f32::sqrt(vx * vx * ax * ax + vy * vy * ay * ay) / corner;
                let motion = if per_pixel {
                    let mut ang = m::f32::atan2(-vy * ay, vx * ax);
                    if ang < 0.0 {
                        ang += 2.0 * PI;
                    }
                    let env = &mut compiled.env;
                    for (slot, value) in MOTION_SLOTS.iter().zip(motion_values) {
                        env.vars[slot.at()] = value;
                    }
                    env.set(Main::X.at(), x as f64);
                    env.set(Main::Y.at(), y as f64);
                    env.set(Main::Rad.at(), rad as f64);
                    env.set(Main::Ang.at(), ang as f64);
                    compiled.pixel.run(env, &mut self.globals);
                    read_motion(env)
                } else {
                    frame.motion
                };
                // The rotation's sine and cosine, cached while it's unchanged.
                if motion.rot != shared.1 {
                    let (s, c) = m::f32::sin_cos(motion.rot);
                    shared = (motion, motion.rot, s, c);
                }
                let (sin_rot, cos_rot) = (shared.2, shared.3);
                let zoom = m::f32::pow(motion.zoom, m::f32::pow(motion.zoom_exp, rad * 2.0 - 1.0));
                let inverse = if zoom.abs() > 0.0001 { 1.0 / zoom } else { 1.0 };
                let mut u = vx * ax * 0.5 * inverse + 0.5;
                let mut v = vy * ay * 0.5 * inverse + 0.5;
                if motion.sx.abs() > 0.0001 {
                    u = (u - motion.cx) / motion.sx + motion.cx;
                }
                if motion.sy.abs() > 0.0001 {
                    v = (v - motion.cy) / motion.sy + motion.cy;
                }
                if motion.warp != 0.0 {
                    let amount = motion.warp * 0.0035;
                    u += amount * m::f32::sin(warp_time * 0.333 + warp_scale_inv * (x * f[0] - y * f[3]));
                    v += amount * m::f32::cos(warp_time * 0.375 - warp_scale_inv * (x * f[2] + y * f[1]));
                    u += amount * m::f32::cos(warp_time * 0.753 - warp_scale_inv * (x * f[1] - y * f[2]));
                    v += amount * m::f32::sin(warp_time * 0.825 + warp_scale_inv * (x * f[0] + y * f[3]));
                }
                let (u2, v2) = (u - motion.cx, v - motion.cy);
                u = u2 * cos_rot - v2 * sin_rot + motion.cx;
                v = u2 * sin_rot + v2 * cos_rot + motion.cy;
                u -= motion.dx;
                v -= motion.dy;
                u = (u - 0.5) / ax + 0.5;
                v = (v - 0.5) / ay + 0.5;
                let limit = 8.0;
                let u = if u.is_finite() { u.clamp(-limit, limit) } else { 0.5 };
                let v = if v.is_finite() { v.clamp(-limit, limit) } else { 0.5 };
                self.uv[j * (MESH_W + 1) + i] = [u * w - 0.5, v * h - 0.5];
            }
        }
    }

    fn warp(&mut self, frame: &Frame) {
        let (w, h) = (self.width, self.height);
        let decay = (frame.decay * 256.0) as u32;
        let wrap = frame.wrap;
        let source = &self.front;
        let target = &mut self.back;
        let row = MESH_W + 1;
        for py in 0..h {
            let fy = (py as f32 + 0.5) / h as f32 * MESH_H as f32;
            let j = (fy as usize).min(MESH_H - 1);
            let ty = fy - j as f32;
            let line = &mut target[py * w..(py + 1) * w];
            for i in 0..MESH_W {
                let x0 = i * w / MESH_W;
                let x1 = (i + 1) * w / MESH_W;
                if x0 == x1 {
                    continue;
                }
                let a = self.uv[j * row + i];
                let b = self.uv[(j + 1) * row + i];
                let c = self.uv[j * row + i + 1];
                let d = self.uv[(j + 1) * row + i + 1];
                let left = [a[0] + (b[0] - a[0]) * ty, a[1] + (b[1] - a[1]) * ty];
                let right = [c[0] + (d[0] - c[0]) * ty, c[1] + (d[1] - c[1]) * ty];
                let per_pixel = MESH_W as f32 / w as f32;
                let t0 = (x0 as f32 + 0.5) * per_pixel - i as f32;
                let step = [(right[0] - left[0]) * per_pixel, (right[1] - left[1]) * per_pixel];
                let mut su = ((left[0] + (right[0] - left[0]) * t0) * 65536.0) as i32;
                let mut sv = ((left[1] + (right[1] - left[1]) * t0) * 65536.0) as i32;
                let du = (step[0] * 65536.0) as i32;
                let dv = (step[1] * 65536.0) as i32;
                for pixel in &mut line[x0..x1] {
                    *pixel = scale(sample(source, w, h, su, sv, wrap), decay);
                    su = su.wrapping_add(du);
                    sv = sv.wrapping_add(dv);
                }
            }
        }
        core::mem::swap(&mut self.front, &mut self.back);
    }

    /// The mesh's source position at (`x`, `y`) in 0..1, as a fraction.
    fn uv_at(&self, x: f32, y: f32) -> (f32, f32) {
        let fx = (x * MESH_W as f32).clamp(0.0, MESH_W as f32 - 0.001);
        let fy = (y * MESH_H as f32).clamp(0.0, MESH_H as f32 - 0.001);
        let (i, j) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - i as f32, fy - j as f32);
        let row = MESH_W + 1;
        let at = |i: usize, j: usize| self.uv[j * row + i];
        let lerp = |a: [f32; 2], b: [f32; 2], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        let top = lerp(at(i, j), at(i + 1, j), tx);
        let bottom = lerp(at(i, j + 1), at(i + 1, j + 1), tx);
        let p = lerp(top, bottom, ty);
        (
            (p[0] + 0.5) / self.width as f32,
            (p[1] + 0.5) / self.height as f32,
        )
    }

    fn draw_motion_vectors(&mut self, frame: &Frame) {
        let [nx, ny, mdx, mdy, length, r, g, b, a] = frame.vectors;
        if a <= 0.001 || nx < 1.0 || ny < 1.0 {
            return;
        }
        let nx = (nx as usize).min(64);
        let ny = (ny as usize).min(48);
        let rgba = [r, g, b, a];
        let (w, h) = (self.width as f32, self.height as f32);
        for gy in 0..ny {
            for gx in 0..nx {
                let x = (gx as f32 + 0.25) / nx as f32 + mdx;
                let y = (gy as f32 + 0.25) / ny as f32 + mdy;
                if !(0.0001..0.9999).contains(&x) || !(0.0001..0.9999).contains(&y) {
                    continue;
                }
                // Screen y is down, as the mesh's is.
                let (u, v) = self.uv_at(x, 1.0 - y);
                let (sx, sy) = (x * w, (1.0 - y) * h);
                let mut ex = sx - (u * w - sx) * length;
                let mut ey = sy - (v * h - sy) * length;
                if (ex - sx).abs() < 1.0 && (ey - sy).abs() < 1.0 {
                    ex = sx + 1.0;
                    ey = sy;
                }
                self.line(sx, sy, ex, ey, rgba, rgba, false, false);
            }
        }
    }

    fn draw_shapes(&mut self, compiled: &mut Compiled) {
        let mut q = [0.0; Q_COUNT];
        q.copy_from_slice(&compiled.env.vars[MAIN_Q..MAIN_Q + Q_COUNT]);
        let mut shapes = core::mem::take(&mut compiled.shapes);
        for shape in &mut shapes {
            let instances = (shape.def.instances.clamp(1.0, MAX_INSTANCES as f64)) as usize;
            let base = SHAPE_NAMES.len();
            for instance in 0..instances {
                let env = &mut shape.env;
                env.vars[..base].copy_from_slice(&shape.def.values);
                self.set_shared_inputs(env);
                env.set(ShapeVar::Instance.at(), instance as f64);
                env.set(ShapeVar::NumInst.at(), instances as f64);
                env.vars[base..base + Q_COUNT].copy_from_slice(&q);
                env.vars[base + Q_COUNT..base + Q_COUNT + T_COUNT].copy_from_slice(&shape.t_init);
                shape.frame.run(env, &mut self.globals);
                let env = &shape.env;
                let get = |slot: ShapeVar, fallback| value(env, slot.at(), fallback);
                let color = |slots: [ShapeVar; 4]| {
                    let mut out = [0.0; 4];
                    for (to, slot) in out.iter_mut().zip(slots) {
                        *to = value(env, slot.at(), 0.0);
                    }
                    out
                };
                let spec = ShapeSpec {
                    sides: (get(ShapeVar::Sides, 4.0) as i32).clamp(3, 100) as usize,
                    x: get(ShapeVar::X, 0.5),
                    y: get(ShapeVar::Y, 0.5),
                    rad: get(ShapeVar::Rad, 0.1),
                    ang: get(ShapeVar::Ang, 0.0),
                    tex_ang: get(ShapeVar::TexAng, 0.0),
                    tex_zoom: get(ShapeVar::TexZoom, 1.0),
                    textured: flag(env, ShapeVar::Textured.at()),
                    additive: flag(env, ShapeVar::Additive.at()),
                    thick: flag(env, ShapeVar::Thick.at()),
                    center: color([ShapeVar::R, ShapeVar::G, ShapeVar::B, ShapeVar::A]),
                    edge: color([ShapeVar::R2, ShapeVar::G2, ShapeVar::B2, ShapeVar::A2]),
                    border: color([ShapeVar::BorderR, ShapeVar::BorderG, ShapeVar::BorderB, ShapeVar::BorderA]),
                };
                self.draw_shape(&spec);
            }
        }
        compiled.shapes = shapes;
    }

    fn draw_shape(&mut self, shape: &ShapeSpec) {
        let (w, h) = (self.width as f32, self.height as f32);
        let radius = shape.rad * w.min(h) * 0.5;
        if !(radius > 0.25) || radius > 8.0 * w.max(h) {
            return;
        }
        let (cx, cy) = (shape.x * w, (1.0 - shape.y) * h);
        let sides = shape.sides;
        let start = shape.ang + PI * 0.25;
        let fill = shape.center[3] > 0.001 || shape.edge[3] > 0.001;
        if fill {
            let mut normals = [[0.0f32; 2]; 16];
            let round = sides >= normals.len();
            let apothem = radius * m::f32::cos(PI / sides as f32);
            if !round {
                for (k, normal) in normals.iter_mut().take(sides).enumerate() {
                    let (s, c) = m::f32::sin_cos(start + 2.0 * PI * (k as f32 + 0.5) / sides as f32);
                    *normal = [c / apothem, s / apothem];
                }
            }
            let (tex_s, tex_c) = m::f32::sin_cos(shape.tex_ang);
            let tex_zoom = if shape.tex_zoom.abs() > 0.0001 { shape.tex_zoom } else { 1.0 };
            let x0 = m::f32::floor(cx - radius).max(0.0) as usize;
            let x1 = (m::f32::ceil(cx + radius).max(0.0) as usize).min(self.width);
            let y0 = m::f32::floor(cy - radius).max(0.0) as usize;
            let y1 = (m::f32::ceil(cy + radius).max(0.0) as usize).min(self.height);
            let inverse_radius = 1.0 / radius;
            for py in y0..y1 {
                let dy = cy - (py as f32 + 0.5);
                for px in x0..x1 {
                    let dx = px as f32 + 0.5 - cx;
                    let t = if round {
                        m::f32::sqrt(dx * dx + dy * dy) * inverse_radius
                    } else {
                        let mut most = f32::MIN;
                        for normal in &normals[..sides] {
                            most = most.max(dx * normal[0] + dy * normal[1]);
                        }
                        most
                    };
                    if t > 1.0 {
                        continue;
                    }
                    let t = t.max(0.0);
                    let mut rgba = [0.0; 4];
                    for (channel, out) in rgba.iter_mut().enumerate() {
                        *out = shape.center[channel] + (shape.edge[channel] - shape.center[channel]) * t;
                    }
                    if shape.textured {
                        let (nx, ny) = (dx * inverse_radius, dy * inverse_radius);
                        let rx = nx * tex_c - ny * tex_s;
                        let ry = nx * tex_s + ny * tex_c;
                        let u = 0.5 + 0.5 * rx / tex_zoom * self.aspect_y;
                        let v = 0.5 - 0.5 * ry / tex_zoom * self.aspect_x;
                        let texel = sample(
                            &self.back,
                            self.width,
                            self.height,
                            ((u * w - 0.5) * 65536.0) as i32,
                            ((v * h - 0.5) * 65536.0) as i32,
                            true,
                        );
                        rgba[0] *= ((texel >> 16) & 0xff) as f32 / 255.0;
                        rgba[1] *= ((texel >> 8) & 0xff) as f32 / 255.0;
                        rgba[2] *= (texel & 0xff) as f32 / 255.0;
                    }
                    blend(&mut self.front[py * self.width + px], rgba, shape.additive);
                }
            }
        }
        if shape.border[3] > 0.001 {
            let corner = |k: usize| {
                let (s, c) = m::f32::sin_cos(start + 2.0 * PI * k as f32 / sides as f32);
                (cx + radius * c, cy - radius * s)
            };
            for k in 0..sides {
                let (ax, ay) = corner(k);
                let (bx, by) = corner(k + 1);
                self.line(ax, ay, bx, by, shape.border, shape.border, shape.thick, false);
            }
        }
    }

    fn draw_custom_waves(&mut self, compiled: &mut Compiled) {
        let mut q = [0.0; Q_COUNT];
        q.copy_from_slice(&compiled.env.vars[MAIN_Q..MAIN_Q + Q_COUNT]);
        let mut waves = core::mem::take(&mut compiled.waves);
        for wave in &mut waves {
            let base = WAVE_NAMES.len();
            let env = &mut wave.env;
            self.set_shared_inputs(env);
            env.vars[base..base + Q_COUNT].copy_from_slice(&q);
            env.vars[base + Q_COUNT..base + Q_COUNT + T_COUNT].copy_from_slice(&wave.t_init);
            for (slot, value) in [WaveVar::R, WaveVar::G, WaveVar::B, WaveVar::A].iter().zip(wave.def.rgba) {
                env.set(slot.at(), value);
            }
            env.set(WaveVar::Samples.at(), wave.def.samples);
            wave.frame.run(env, &mut self.globals);
            let samples = value(env, WaveVar::Samples.at(), 0.0).clamp(0.0, MAX_WAVE_POINTS as f32) as usize;
            if samples < 2 {
                continue;
            }
            let color = [
                env.get(WaveVar::R.at()),
                env.get(WaveVar::G.at()),
                env.get(WaveVar::B.at()),
                env.get(WaveVar::A.at()),
            ];
            let (source_left, source_right, mult): (&[f32], &[f32], f32) = if wave.def.spectrum {
                (&self.analyzer.spectrum, &self.analyzer.spectrum, 0.15)
            } else {
                (&self.analyzer.wave_left, &self.analyzer.wave_right, 0.512)
            };
            let mult = mult * wave.def.scaling as f32;
            let length = source_left.len().min(if wave.def.spectrum { SPECTRUM } else { SAMPLES });
            let samples = samples.min(length);
            let sep = (wave.def.sep.max(0.0) as usize).min(length - samples);
            let smoothing = (wave.def.smoothing as f32).clamp(0.0, 0.9);
            let mut left = [0.0f32; MAX_WAVE_POINTS];
            let mut right = [0.0f32; MAX_WAVE_POINTS];
            for j in 0..samples {
                left[j] = source_left[j] * mult;
                right[j] = source_right[j + sep] * mult;
                if j > 0 {
                    left[j] = left[j] * (1.0 - smoothing) + left[j - 1] * smoothing;
                    right[j] = right[j] * (1.0 - smoothing) + right[j - 1] * smoothing;
                }
            }
            self.points.clear();
            let (w, h) = (self.width as f32, self.height as f32);
            for j in 0..samples {
                let env = &mut wave.env;
                env.set(WaveVar::Sample.at(), j as f64 / (samples - 1) as f64);
                env.set(WaveVar::Value1.at(), left[j] as f64);
                env.set(WaveVar::Value2.at(), right[j] as f64);
                env.set(WaveVar::X.at(), 0.5 + left[j] as f64);
                env.set(WaveVar::Y.at(), 0.5 + right[j] as f64);
                for (slot, value) in [WaveVar::R, WaveVar::G, WaveVar::B, WaveVar::A].iter().zip(color) {
                    env.set(slot.at(), value);
                }
                if !wave.point.is_empty() {
                    wave.point.run(env, &mut self.globals);
                }
                let x = value(env, WaveVar::X.at(), 0.5);
                let y = value(env, WaveVar::Y.at(), 0.5);
                self.points.push(Point {
                    x: x * w,
                    y: (1.0 - y) * h,
                    rgba: [
                        value(env, WaveVar::R.at(), 1.0),
                        value(env, WaveVar::G.at(), 1.0),
                        value(env, WaveVar::B.at(), 1.0),
                        value(env, WaveVar::A.at(), 1.0),
                    ],
                });
            }
            self.draw_points(wave.def.dots, wave.def.thick, wave.def.additive);
        }
        compiled.waves = waves;
    }

    fn draw_main_wave(&mut self, preset: &Preset, frame: &Frame) {
        let a = &self.analyzer;
        let mut alpha = frame.wave[3];
        if let Some((start, end)) = preset.wave_alpha_by_volume {
            let span = (end - start) as f32;
            let t = if span.abs() > 0.0001 {
                (a.volume() - start as f32) / span
            } else {
                1.0
            };
            alpha *= t.clamp(0.0, 1.0);
        }
        let mode = frame.wave_mode.rem_euclid(8);
        if mode == 3 {
            alpha *= (1.3 * a.treb * a.treb).min(1.0);
        }
        let alpha = alpha.clamp(0.0, 1.0);
        if alpha <= 0.001 {
            return;
        }
        let mut rgb = [
            frame.wave[0].clamp(0.0, 1.0),
            frame.wave[1].clamp(0.0, 1.0),
            frame.wave[2].clamp(0.0, 1.0),
        ];
        if frame.wave_brighten {
            let most = rgb[0].max(rgb[1]).max(rgb[2]);
            if most > 0.01 {
                for channel in &mut rgb {
                    *channel /= most;
                }
            }
        }
        let rgba = [rgb[0], rgb[1], rgb[2], alpha];

        // The waveform, scaled and smoothed.
        let scale = preset.wave_scale as f32;
        let smoothing = (preset.wave_smoothing as f32).clamp(0.0, 0.98);
        for index in 0..SAMPLES {
            let (l, r) = (a.wave_left[index] * scale, a.wave_right[index] * scale);
            if index == 0 {
                self.left[0] = l;
                self.right[0] = r;
            } else {
                self.left[index] = l * (1.0 - smoothing) + self.left[index - 1] * smoothing;
                self.right[index] = r * (1.0 - smoothing) + self.right[index - 1] * smoothing;
            }
        }
        let (left, right) = (&self.left, &self.right);
        let wx = frame.wave_x * 2.0 - 1.0;
        let wy = frame.wave_y * 2.0 - 1.0;
        let (ax, ay) = (self.aspect_x, self.aspect_y);
        let mystery = frame.wave_mystery;
        let time = self.time as f32;
        let (w, h) = (self.width as f32, self.height as f32);
        let to_screen = |x: f32, y: f32| Point {
            x: (x * 0.5 + 0.5) * w,
            y: (0.5 - y * 0.5) * h,
            rgba,
        };
        self.points.clear();
        let points = &mut self.points;
        // A second strip, for mode 7's other channel.
        let mut split = None;
        match mode {
            0 => {
                let n = SAMPLES / 2;
                for i in 0..n {
                    let mut rad = 0.5 + 0.4 * right[i] + mystery;
                    if i < n / 10 {
                        let mix = i as f32 / (n as f32 * 0.1);
                        let mix = 0.5 - 0.5 * m::f32::cos(mix * PI);
                        let other = 0.5 + 0.4 * right[i + n] + mystery;
                        rad = other * (1.0 - mix) + rad * mix;
                    }
                    let ang = i as f32 / (n - 1) as f32 * 2.0 * PI + time * 0.2;
                    let (s, c) = m::f32::sin_cos(ang);
                    points.push(to_screen(wx + rad * c * ay, wy + rad * s * ax));
                }
                if let Some(&first) = points.first() {
                    points.push(first);
                }
            }
            1 => {
                for i in 0..SAMPLES / 2 {
                    let rad = 0.53 + 0.43 * right[i] + mystery;
                    let ang = left[i + 32] * 1.57 + time * 2.3;
                    let (s, c) = m::f32::sin_cos(ang);
                    points.push(to_screen(wx + rad * c * ay, wy + rad * s * ax));
                }
            }
            2 | 3 => {
                for i in 0..SAMPLES - 32 {
                    points.push(to_screen(wx + right[i] * ay, wy + left[i + 32] * ax));
                }
            }
            4 => {
                let w1 = 0.45 + 0.5 * (mystery * 0.5 + 0.5);
                let w2 = 1.0 - w1;
                let mut previous = [(0.0f32, 0.0f32); 2];
                for i in 0..SAMPLES {
                    let mut x = -1.0 + 2.0 * i as f32 / SAMPLES as f32 + wx;
                    let mut y = left[i] * 0.47 + wy;
                    if i >= 2 {
                        x = x * w2 + w1 * (previous[1].0 * 2.0 - previous[0].0);
                        y = y * w2 + w1 * (previous[1].1 * 2.0 - previous[0].1);
                    }
                    previous = [previous[1], (x, y)];
                    points.push(to_screen(x, y));
                }
            }
            5 => {
                let (s, c) = m::f32::sin_cos(time * 0.3);
                for i in 0..SAMPLES - 32 {
                    let x0 = right[i] * left[i + 32] + left[i] * right[i + 32];
                    let y0 = right[i] * right[i] - left[i + 32] * left[i + 32];
                    points.push(to_screen(wx + (x0 * c - y0 * s) * ay, wy + (x0 * s + y0 * c) * ax));
                }
            }
            _ => {
                let ang = mystery * PI * 0.5;
                let (s, c) = m::f32::sin_cos(ang);
                let n = SAMPLES / 2;
                let lines: &[(&[f32], f32)] = if mode == 6 {
                    &[(left, 0.0)]
                } else {
                    &[(left, 0.25), (right, -0.25)]
                };
                for (index, (data, offset)) in lines.iter().enumerate() {
                    if index == 1 {
                        split = Some(points.len());
                    }
                    for i in 0..n {
                        let t = i as f32 / (n - 1) as f32 * 2.0 - 1.0;
                        let along = t * 1.5;
                        let across = data[i] * 0.5 + offset;
                        points.push(to_screen(wx + c * along - s * across, wy + s * along + c * across));
                    }
                }
            }
        }
        match split {
            Some(at) => {
                let second = self.points.split_off(at);
                self.draw_points(frame.wave_dots, frame.wave_thick, frame.wave_additive);
                self.points = second;
                self.draw_points(frame.wave_dots, frame.wave_thick, frame.wave_additive);
            }
            None => self.draw_points(frame.wave_dots, frame.wave_thick, frame.wave_additive),
        }
    }

    fn draw_points(&mut self, dots: bool, thick: bool, additive: bool) {
        let points = core::mem::take(&mut self.points);
        if dots {
            for point in &points {
                self.dot(point.x, point.y, point.rgba, thick, additive);
            }
        } else {
            for pair in points.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                self.line(a.x, a.y, b.x, b.y, a.rgba, b.rgba, thick, additive);
            }
        }
        self.points = points;
    }

    fn darken_center(&mut self) {
        let (w, h) = (self.width as f32, self.height as f32);
        let radius = 0.08 * w.min(h);
        let (cx, cy) = (w * 0.5, h * 0.5);
        let x0 = (cx - radius) as usize;
        let y0 = (cy - radius) as usize;
        let x1 = ((cx + radius) as usize).min(self.width);
        let y1 = ((cy + radius) as usize).min(self.height);
        for py in y0..y1 {
            for px in x0..x1 {
                let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
                let d = m::f32::sqrt(dx * dx + dy * dy) / radius;
                if d < 1.0 {
                    blend(&mut self.front[py * self.width + px], [0.0, 0.0, 0.0, (1.0 - d) * 3.0 / 32.0], false);
                }
            }
        }
    }

    fn draw_borders(&mut self, frame: &Frame) {
        let (w, h) = (self.width as f32, self.height as f32);
        let mut inset = 0.0f32;
        for border in [frame.outer, frame.inner] {
            let [size, r, g, b, a] = border;
            let size = size.clamp(0.0, 0.5);
            if a > 0.001 && size > 0.0 {
                let rgba = [r, g, b, a];
                let outer_x = (inset * w * 0.5) as usize;
                let outer_y = (inset * h * 0.5) as usize;
                let inner_x = (m::f32::ceil((inset + size) * w * 0.5) as usize).min(self.width / 2);
                let inner_y = (m::f32::ceil((inset + size) * h * 0.5) as usize).min(self.height / 2);
                for py in outer_y..self.height - outer_y {
                    let in_band_y = py < inner_y || py >= self.height - inner_y;
                    for px in outer_x..self.width - outer_x {
                        if in_band_y || px < inner_x || px >= self.width - inner_x {
                            blend(&mut self.front[py * self.width + px], rgba, false);
                        }
                    }
                }
            }
            inset += size;
        }
    }

    fn composite(&mut self, frame: &Frame) {
        // Gamma and the filters, as one table for all three channels.
        let mut table = [0u8; 256];
        let mut identity = true;
        for (index, entry) in table.iter_mut().enumerate() {
            let mut f = (index as f32 / 255.0 * frame.gamma).min(1.0);
            if frame.brighten {
                f = 1.0 - (1.0 - f) * (1.0 - f);
            }
            if frame.darken {
                f *= f;
            }
            if frame.solarize {
                f = 2.0 * f * (1.0 - f);
            }
            if frame.invert {
                f = 1.0 - f;
            }
            *entry = (f * 255.0 + 0.5) as u8;
            identity &= *entry as usize == index;
        }
        let (w, h) = (self.width, self.height);
        let echo = frame.echo_alpha > 0.001 && frame.echo_zoom.abs() > 0.001;
        if !echo && identity {
            self.out.copy_from_slice(&self.front);
            return;
        }
        let alpha = (frame.echo_alpha * 256.0) as u32;
        let zoom = frame.echo_zoom;
        let flip_x = frame.echo_orient & 1 != 0;
        let flip_y = frame.echo_orient & 2 != 0;
        // Where each output column and row takes its echo from.
        let echo_coord = |index: usize, size: usize, flip: bool| -> usize {
            let mut t = ((index as f32 + 0.5) / size as f32 - 0.5) / zoom + 0.5;
            if flip {
                t = 1.0 - t;
            }
            let at = (t * size as f32) as i32;
            at.rem_euclid(size as i32) as usize
        };
        for py in 0..h {
            let ey = if echo { echo_coord(py, h, flip_y) } else { 0 };
            for px in 0..w {
                let mut pixel = self.front[py * w + px];
                if echo {
                    let ex = echo_coord(px, w, flip_x);
                    pixel = mix(pixel, self.front[ey * w + ex], alpha);
                }
                let r = table[((pixel >> 16) & 0xff) as usize] as u32;
                let g = table[((pixel >> 8) & 0xff) as usize] as u32;
                let b = table[(pixel & 0xff) as usize] as u32;
                self.out[py * w + px] = (r << 16) | (g << 8) | b;
            }
        }
    }

    fn dot(&mut self, x: f32, y: f32, rgba: [f32; 4], thick: bool, additive: bool) {
        let size = if thick { 2 } else { 1 };
        for oy in 0..size {
            for ox in 0..size {
                self.plot(x as i32 + ox, y as i32 + oy, rgba, additive);
            }
        }
    }

    fn plot(&mut self, x: i32, y: i32, rgba: [f32; 4], additive: bool) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            blend(&mut self.front[y as usize * self.width + x as usize], rgba, additive);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, from: [f32; 4], to: [f32; 4], thick: bool, additive: bool) {
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return;
        }
        let (w, h) = (self.width as f32, self.height as f32);
        // Wholly off one side: nothing to draw.
        if (x0 < -2.0 && x1 < -2.0) || (y0 < -2.0 && y1 < -2.0) || (x0 > w + 2.0 && x1 > w + 2.0) || (y0 > h + 2.0 && y1 > h + 2.0) {
            return;
        }
        let steps = (m::f32::ceil((x1 - x0).abs().max((y1 - y0).abs())) as usize).clamp(1, MAX_LINE_STEPS);
        let (dx, dy) = ((x1 - x0) / steps as f32, (y1 - y0) / steps as f32);
        let mut rgba = from;
        let blend_colors = from != to;
        // The last pixel is the next segment's first.
        for step in 0..steps {
            let x = x0 + dx * step as f32;
            let y = y0 + dy * step as f32;
            if blend_colors {
                let t = step as f32 / steps as f32;
                for channel in 0..4 {
                    rgba[channel] = from[channel] + (to[channel] - from[channel]) * t;
                }
            }
            let (ix, iy) = (x as i32, y as i32);
            self.plot(ix, iy, rgba, additive);
            if thick {
                self.plot(ix + 1, iy, rgba, additive);
                self.plot(ix, iy + 1, rgba, additive);
                self.plot(ix + 1, iy + 1, rgba, additive);
            }
        }
    }
}

/// One shape instance, as its code left it.
struct ShapeSpec {
    sides: usize,
    x: f32,
    y: f32,
    rad: f32,
    ang: f32,
    tex_ang: f32,
    tex_zoom: f32,
    textured: bool,
    additive: bool,
    thick: bool,
    center: [f32; 4],
    edge: [f32; 4],
    border: [f32; 4],
}

/// Linear blend of two pixels, `weight` of `b` out of 256.
#[inline]
fn mix(a: u32, b: u32, weight: u32) -> u32 {
    let keep = 256 - weight.min(256);
    let weight = weight.min(256);
    let rb = (((a & 0xff00ff) * keep + (b & 0xff00ff) * weight) >> 8) & 0xff00ff;
    let g = (((a & 0x00ff00) * keep + (b & 0x00ff00) * weight) >> 8) & 0x00ff00;
    rb | g
}

/// A pixel times `factor` out of 256.
#[inline]
fn scale(pixel: u32, factor: u32) -> u32 {
    let rb = (((pixel & 0xff00ff) * factor) >> 8) & 0xff00ff;
    let g = (((pixel & 0x00ff00) * factor) >> 8) & 0x00ff00;
    rb | g
}

/// Bilinear sample at 16.16 fixed-point pixel coordinates.
#[inline]
fn sample(source: &[u32], w: usize, h: usize, fx: i32, fy: i32, wrap: bool) -> u32 {
    let ix = fx >> 16;
    let iy = fy >> 16;
    let wx = ((fx >> 8) & 0xff) as u32;
    let wy = ((fy >> 8) & 0xff) as u32;
    let (wi, hi) = (w as i32, h as i32);
    let (x0, x1, y0, y1) = if ix >= 0 && iy >= 0 && ix < wi - 1 && iy < hi - 1 {
        (ix as usize, ix as usize + 1, iy as usize, iy as usize + 1)
    } else if wrap {
        (
            ix.rem_euclid(wi) as usize,
            (ix + 1).rem_euclid(wi) as usize,
            iy.rem_euclid(hi) as usize,
            (iy + 1).rem_euclid(hi) as usize,
        )
    } else {
        (
            ix.clamp(0, wi - 1) as usize,
            (ix + 1).clamp(0, wi - 1) as usize,
            iy.clamp(0, hi - 1) as usize,
            (iy + 1).clamp(0, hi - 1) as usize,
        )
    };
    let top = mix(source[y0 * w + x0], source[y0 * w + x1], wx);
    let bottom = mix(source[y1 * w + x0], source[y1 * w + x1], wx);
    mix(top, bottom, wy)
}

/// Draws `rgba` (0 to 1 each) over `pixel`, blended by its alpha or added.
#[inline]
fn blend(pixel: &mut u32, rgba: [f32; 4], additive: bool) {
    let alpha = rgba[3].clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0) as i32;
    let (r, g, b) = (channel(rgba[0]), channel(rgba[1]), channel(rgba[2]));
    let a = (alpha * 256.0) as i32;
    let old = *pixel;
    let (or, og, ob) = (((old >> 16) & 0xff) as i32, ((old >> 8) & 0xff) as i32, (old & 0xff) as i32);
    let (nr, ng, nb) = if additive {
        (
            (or + ((r * a) >> 8)).min(255),
            (og + ((g * a) >> 8)).min(255),
            (ob + ((b * a) >> 8)).min(255),
        )
    } else {
        (
            or + (((r - or) * a) >> 8),
            og + (((g - og) * a) >> 8),
            ob + (((b - ob) * a) >> 8),
        )
    };
    *pixel = ((nr.clamp(0, 255) as u32) << 16) | ((ng.clamp(0, 255) as u32) << 8) | nb.clamp(0, 255) as u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn music(frames: usize, offset: usize) -> Vec<i16> {
        let mut out = Vec::new();
        for index in 0..frames {
            let t = (offset + index) as f32 / 44_100.0;
            let beat = if (t * 2.0).fract() < 0.1 { 1.0 } else { 0.2 };
            let left = beat * (2.0 * PI * 60.0 * t).sin() * 0.6 + 0.2 * (2.0 * PI * 880.0 * t).sin();
            let right = beat * (2.0 * PI * 62.0 * t).sin() * 0.6 + 0.2 * (2.0 * PI * 660.0 * t).sin();
            out.push((left * 32767.0) as i16);
            out.push((right * 32767.0) as i16);
        }
        out
    }

    fn lit(visualizer: &Visualizer) -> usize {
        visualizer.pixels().iter().filter(|p| **p & 0x00f0f0f0 != 0).count()
    }

    #[test]
    fn the_default_preset_draws_something() {
        let mut visualizer = Visualizer::new(160, 120, 44_100).unwrap();
        let mut offset = 0;
        for _ in 0..30 {
            visualizer.render(&music(1470, offset), 1.0 / 30.0);
            offset += 1470;
        }
        assert!(lit(&visualizer) > 500, "only {} pixels lit", lit(&visualizer));
    }

    #[test]
    fn silence_and_bad_presets_do_not_panic() {
        let mut visualizer = Visualizer::new(64, 48, 48_000).unwrap();
        let wild = "[preset00]\nzoom=0\nrot=1e30\nfDecay=7\nnWaveMode=-3\nwave_x=1e9\nob_size=5\nob_a=1\n\
            per_frame_1=zoom = 1/0; dx = log(-1); warp = sqrt(-1)*1e20;\n\
            per_pixel_1=rot = rot + 1e300 * 1e300;\n\
            shapecode_0_enabled=1\nshapecode_0_rad=1e9\nshapecode_0_num_inst=100000\nshapecode_0_textured=1\n\
            wavecode_0_enabled=1\nwavecode_0_samples=9999\nwavecode_0_sep=-5\n\
            wave_0_per_point1=x = 1e9; y = -1e9;\n";
        let report = visualizer.load_text(wild).unwrap();
        assert_eq!(report.errors, 0);
        for mode in 0..8 {
            visualizer.load_text(&std::format!("[preset00]\nnWaveMode={mode}\nfWaveAlpha=1\nmv_a=1\n")).unwrap();
            visualizer.render(&[], 0.03);
            visualizer.render(&music(576, 0), 0.03);
        }
        visualizer.load_text(wild).unwrap();
        visualizer.render(&music(576, 0), 0.03);
    }

    #[test]
    fn a_shape_fills_its_middle_with_its_center_colour() {
        let mut visualizer = Visualizer::new(100, 100, 44_100).unwrap();
        let preset = "[preset00]\nfDecay=0\nfWaveAlpha=0\nzoom=1\nwarp=0\n\
            shapecode_0_enabled=1\nshapecode_0_sides=4\nshapecode_0_rad=0.5\n\
            shapecode_0_r=0\nshapecode_0_g=0\nshapecode_0_b=1\nshapecode_0_a=1\n\
            shapecode_0_r2=0\nshapecode_0_g2=0\nshapecode_0_b2=1\nshapecode_0_a2=1\nshapecode_0_border_a=0\n";
        visualizer.load_text(preset).unwrap();
        visualizer.render(&[], 0.03);
        let center = visualizer.pixels()[50 * 100 + 50];
        assert_eq!(center, 0x0000ff, "center is {center:06x}");
        assert_eq!(visualizer.pixels()[2 * 100 + 2], 0, "the corner is outside the square");
    }

    #[test]
    fn zoom_moves_the_image_outward() {
        let mut visualizer = Visualizer::new(64, 64, 44_100).unwrap();
        visualizer.load_text("[preset00]\nfDecay=1\nfWaveAlpha=0\nzoom=1.5\nwarp=0\n").unwrap();
        // A dot just off center.
        visualizer.front[32 * 64 + 36] = 0xffffff;
        visualizer.render(&[], 0.03);
        let bright = |v: &Visualizer, x: usize| v.pixels()[32 * 64 + x] & 0xff;
        assert!(bright(&visualizer, 38) > bright(&visualizer, 36), "the dot should move out");
    }

    #[test]
    fn filters_apply_to_what_is_shown_only() {
        let mut visualizer = Visualizer::new(32, 32, 44_100).unwrap();
        visualizer.load_text("[preset00]\nfDecay=1\nfWaveAlpha=0\nwarp=0\nbInvert=1\n").unwrap();
        visualizer.render(&[], 0.03);
        assert_eq!(visualizer.pixels()[0], 0xffffff);
        assert_eq!(visualizer.front[0], 0);
    }
}
