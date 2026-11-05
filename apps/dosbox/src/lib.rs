//! `dosbox`: the FEOS-DOS desktop application.
//!
//! The virtual machine runs on a task of its own, and the window only ever looks
//! at what that task has published. A guest can burn all the CPU it likes, or
//! the machine can wedge in a way nobody thought of: the pointer still moves, and
//! Close still works.
//!
//! - The task owns the [`DosBox`], so nothing has to lock it, and it holds no
//!   lock while the guest runs. A kill can therefore always find a safe point.
//! - Input goes to the task through a queue, and finished pictures come back in
//!   a buffer. The task only ever `try_lock`s it, so a window that's drawing
//!   can't hold the guest up, and the window waits at most for one copy.
//! - The task stamps a heartbeat after every frame. If that stops for
//!   [`WATCHDOG_MS`], the window kills the task and says so.
//! - Closing the window sends the task away and also kills it: if it's in the
//!   middle of a frame, it polls [`kill_requested`] when that ends and frees the
//!   machine itself.
//!
//! Launching `dosbox` from the serial shell runs the same machine full-screen.
//!
//! [`kill_requested`]: feos_app::Processes::kill_requested

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use feos_app::{App, Context, Platform, Processes, WindowContext, WindowFrame};
use feos_audio::AudioFormat;
use feos_console::println;
use feos_dosbox::keyboard::{ascii_for_char, scan_for_char, Key as DosKey};
use feos_dosbox::vga::{blit_screen, SCREEN_HEIGHT, SCREEN_WIDTH};
use feos_dosbox::machine::draw_software_cursor;
use feos_dosbox::DosBox;
use feos_gfx::Color;
use feos_gui::{Event, Input, Key as GuiKey, Modifiers, Painter};
use feos_keys::SerialKeys;
use feos_mixer::Stream;
use feos_sync::Mutex;
use feos_text::Text;

/// How long the machine may go without finishing a frame before it's killed. A
/// frame runs a bounded number of guest instructions, so this takes a bug.
pub const WATCHDOG_MS: u64 = 5_000;
/// The machine's frame period.
const FRAME_MS: u64 = 16;
/// Input events that can wait for the machine's next frame.
const INPUT_QUEUE: usize = 128;
/// The rate the machine makes audio at: the mixer's own, so nothing resamples.
const AUDIO_RATE: u32 = 48_000;

/// One input event on its way to the machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmInput {
    /// A typed character.
    Char(char),
    /// A key with no character.
    Key(DosKey),
    /// An ASCII code and scan code, as Ctrl+letter makes.
    Code(u8, u8),
    /// A key with this scan code was released.
    Up(u8),
    /// The state of Shift, Ctrl and Alt.
    Modifiers {
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    /// A pointer report in canvas coordinates, with that canvas's size.
    Pointer {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        left: Option<bool>,
        right: Option<bool>,
        middle: Option<bool>,
    },
}

/// What the window and the task share. Nothing in here takes long to touch,
/// except copying a picture.
struct Shared {
    /// The task that may run. Any other leaves when it next looks.
    owner: u32,
    input: [VmInput; INPUT_QUEUE],
    input_len: usize,
    /// The latest picture, `SCREEN_WIDTH` by `SCREEN_HEIGHT`, and what's used.
    screen: Vec<u32>,
    size: (usize, usize),
    cursor: Option<(i32, i32)>,
    /// A guest program is running or AUTOEXEC.BAT is, so frames matter.
    running: bool,
    /// The guest asked to leave.
    exit: bool,
    /// The machine couldn't start.
    failed: bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    owner: 0,
    input: [VmInput::Char(' '); INPUT_QUEUE],
    input_len: 0,
    screen: Vec::new(),
    size: (0, 0),
    cursor: None,
    running: false,
    exit: false,
    failed: false,
});
/// When the task last finished a frame.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
/// A guest is running, so the window animates.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// A picture was published since the window last drew.
static FRESH: AtomicBool = AtomicBool::new(false);

/// What the window keeps about its task.
struct Session {
    pid: u32,
    processes: &'static dyn Processes,
    /// The watchdog fired.
    dead: bool,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// The task that runs the virtual machine. Its argument is its identity.
pub struct DosTask;

impl App for DosTask {
    fn name(&self) -> &'static str {
        "dos-vm"
    }

    fn usage(&self) -> &'static str {
        "(started by the FEOS-DOS window)"
    }

    fn description(&self) -> &'static str {
        "run the FEOS-DOS virtual machine for its window"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Ok(identity) = context.args.trim().parse::<u32>() else {
            return 2;
        };
        let platform = context.platform;
        let processes = platform.processes;
        let clock = platform.clock;
        let stream = open_audio(platform);

        let mut machine = DosBox::new(platform.filesystem);
        if stream.is_some() {
            machine.set_audio_rate(AUDIO_RATE);
        }

        let mut inbox = [VmInput::Char(' '); INPUT_QUEUE];
        // Published on the first go, and again after a `try_lock` missed.
        let mut unpublished = true;
        while !processes.kill_requested() {
            let frame_start = clock.uptime_ms();
            HEARTBEAT.store(frame_start, Ordering::Relaxed);

            let received = {
                let mut shared = SHARED.lock();
                if shared.owner != identity {
                    return 0;
                }
                let count = shared.input_len;
                inbox[..count].copy_from_slice(&shared.input[..count]);
                shared.input_len = 0;
                count
            };
            for event in &inbox[..received] {
                apply_input(&mut machine, *event);
            }

            machine.frame_paced(frame_start);
            feed_audio(&mut machine, stream.as_ref());
            unpublished |= machine.refresh_screen();
            let running = machine.is_running() || machine.shell.in_batch();
            RUNNING.store(running, Ordering::Relaxed);

            if machine.exit || unpublished {
                if let Some(mut shared) = SHARED.try_lock() {
                    if shared.owner != identity {
                        return 0;
                    }
                    let (width, height) = machine.display_size();
                    let height = height.min(SCREEN_HEIGHT);
                    let used = (SCREEN_WIDTH * height).min(machine.screen.len());
                    if shared.screen.len() >= used {
                        shared.screen[..used].copy_from_slice(&machine.screen[..used]);
                    }
                    shared.size = (width, height);
                    shared.cursor = machine.software_cursor();
                    shared.running = running;
                    shared.exit = machine.exit;
                    unpublished = false;
                    FRESH.store(true, Ordering::Relaxed);
                }
            }
            if machine.exit {
                return 0;
            }

            let spent = clock.uptime_ms().saturating_sub(frame_start);
            if spent < FRAME_MS {
                processes.sleep_ms(FRAME_MS - spent);
            } else {
                // A guest using all it's allowed: let everyone else have a go.
                processes.yield_now();
            }
        }
        0
    }
}

fn open_audio(platform: &Platform) -> Option<Stream> {
    let format = AudioFormat::stereo(AUDIO_RATE)?;
    Stream::open(platform, format).ok()
}

/// Hands the mixer what it can take now. This never blocks: what doesn't fit
/// waits in the machine for the next frame, and the machine drops the oldest
/// audio if it falls far behind.
fn feed_audio(machine: &mut DosBox, stream: Option<&Stream>) {
    let Some(stream) = stream else {
        return;
    };
    let samples = machine.pending_audio();
    if samples.is_empty() {
        return;
    }
    let taken = stream.write(samples);
    machine.consume_audio(taken);
}

fn apply_input(machine: &mut DosBox, event: VmInput) {
    match event {
        // Every input source reports releases, so a press stays down until its
        // `Up`: a game reading port 60h sees real make and break codes.
        VmInput::Char(ch) => {
            if let Some(ascii) = ascii_for_char(ch) {
                machine.keyboard().key_down(ascii, scan_for_char(ch));
            }
        }
        VmInput::Key(key) => {
            let (ascii, scan) = key.event();
            machine.keyboard().key_down(ascii, scan);
        }
        VmInput::Code(ascii, scan) => machine.keyboard().key_down(ascii, scan),
        VmInput::Up(scan) => machine.keyboard().key_up(scan),
        VmInput::Modifiers { shift, ctrl, alt } => {
            machine
                .keyboard()
                .set_modifiers(shift, ctrl, alt, false, false, false, false)
        }
        VmInput::Pointer {
            x,
            y,
            width,
            height,
            left,
            right,
            middle,
        } => machine.pointer_canvas(x, y, width, height, left, right, middle),
    }
}

/// Sends the task away and gets rid of it. Doesn't wait.
fn end_session() {
    let session = SESSION.lock().take();
    {
        let mut shared = SHARED.lock();
        shared.owner = shared.owner.wrapping_add(1).max(1);
        shared.input_len = 0;
    }
    RUNNING.store(false, Ordering::Relaxed);
    if let Some(session) = session {
        // It notices either of these between frames and frees the machine.
        let _ = session.processes.try_kill(session.pid);
    }
}

/// The FEOS-DOS application.
pub struct DosBoxApp;

impl App for DosBoxApp {
    fn name(&self) -> &'static str {
        "dosbox"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["dos", "feos-dos", "command", "command.com"]
    }

    fn usage(&self) -> &'static str {
        "dosbox"
    }

    fn description(&self) -> &'static str {
        "run the FEOS-DOS x86 virtual machine and DOS prompt"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn windowed(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        // A picture waiting counts: at the prompt, the echo of a key arrives
        // from the task a little after the key.
        RUNNING.load(Ordering::Relaxed) || FRESH.load(Ordering::Relaxed)
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        end_session();
        let mut screen = Vec::new();
        if screen.try_reserve_exact(SCREEN_WIDTH * SCREEN_HEIGHT).is_err() {
            return false;
        }
        screen.resize(SCREEN_WIDTH * SCREEN_HEIGHT, 0);
        let identity = {
            let mut shared = SHARED.lock();
            shared.screen = screen;
            shared.size = (0, 0);
            shared.cursor = None;
            shared.exit = false;
            shared.failed = false;
            shared.running = true;
            shared.owner
        };
        let processes = context.platform.processes;
        let mut args = Text::<16>::new();
        let _ = write!(args, "{identity}");
        let Ok(pid) = processes.spawn(&DosTask, args.as_str()) else {
            return false;
        };
        HEARTBEAT.store(context.now_ms, Ordering::Relaxed);
        // Run frames until AUTOEXEC.BAT has finished and the prompt is shown.
        RUNNING.store(true, Ordering::Relaxed);
        *SESSION.lock() = Some(Session {
            pid,
            processes,
            dead: false,
        });
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        FRESH.store(false, Ordering::Relaxed);
        let dead = check_watchdog(context.now_ms);
        let mut shared = SHARED.lock();
        let width = context.canvas.width() as i32;
        let height = context.canvas.height() as i32;
        if context.keyboard_active {
            collect_events(context.input, |event| shared.push(event));
        }
        if context.pointer_active {
            context.dispatch_pointer(|report| {
                shared.push(VmInput::Pointer {
                    x: report.x as i32,
                    y: report.y as i32,
                    width,
                    height,
                    left: report.left,
                    right: report.right,
                    middle: report.middle,
                });
            });
        }
        if shared.exit {
            return WindowFrame::Close;
        }
        blit_screen(context.canvas, &shared.screen, shared.size);
        if let Some((x, y)) = shared.cursor {
            draw_software_cursor(context.canvas, x, y);
        }
        drop(shared);
        if dead {
            let mut painter = Painter::new(context.canvas);
            let band = feos_gfx::Rect::new(0, 0, width as u32, 22);
            painter.fill_rect(band, Color::rgb(160, 32, 32));
            painter.draw_text(
                6,
                7,
                "The virtual machine stopped responding and was ended.",
                Color::WHITE,
                1,
            );
        }
        WindowFrame::Continue
    }

    fn close_window(&self) {
        end_session();
        // The window's copy of the picture; the task's goes with the machine.
        let screen = core::mem::take(&mut SHARED.lock().screen);
        drop(screen);
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(display) = context.display.as_mut() else {
            println!("dosbox: no framebuffer; run it from the desktop instead");
            return 1;
        };
        let platform = context.platform;
        let stream = open_audio(platform);
        let mut machine = DosBox::new(platform.filesystem);
        if stream.is_some() {
            machine.set_audio_rate(AUDIO_RATE);
        }
        let mut input = Input::new();
        let mut keys = SerialKeys::new();

        loop {
            let now = platform.clock.uptime_ms();
            if keys.poll(now, &mut input) {
                break;
            }
            collect_events(&input, |event| apply_input(&mut machine, event));
            machine.frame_paced(now);
            feed_audio(&mut machine, stream.as_ref());
            machine.render(display);
            if machine.exit {
                break;
            }
            platform.clock.idle_ms(16);
        }
        0
    }
}

impl Shared {
    fn push(&mut self, event: VmInput) {
        if self.input_len < INPUT_QUEUE {
            self.input[self.input_len] = event;
            self.input_len += 1;
        }
    }
}

/// Kills a task that has stopped finishing frames. Returns `true` once it has.
fn check_watchdog(now: u64) -> bool {
    let mut session = SESSION.lock();
    let Some(session) = session.as_mut() else {
        return false;
    };
    if !session.dead && watchdog_expired(HEARTBEAT.load(Ordering::Relaxed), now) {
        session.dead = true;
        let _ = session.processes.try_kill(session.pid);
        RUNNING.store(false, Ordering::Relaxed);
    }
    session.dead
}

/// Whether a heartbeat at `beat` is overdue at `now`.
fn watchdog_expired(beat: u64, now: u64) -> bool {
    now.saturating_sub(beat) >= WATCHDOG_MS
}

/// Turns one frame's GUI events into machine input.
fn collect_events(input: &Input, mut push: impl FnMut(VmInput)) {
    let has_text = input.events().any(|event| matches!(event, Event::Text(_)));
    for event in input.events() {
        match event {
            Event::Text(ch) if !ch.is_control() => push(VmInput::Char(ch)),
            Event::ModifiersChanged(modifiers) => push(modifier_state(modifiers)),
            Event::KeyDown(key, modifiers) => {
                push(modifier_state(modifiers));
                if modifiers.contains(Modifiers::CTRL) {
                    if let GuiKey::Character(ch) = key {
                        if ch.is_ascii_alphabetic() {
                            let upper = ch.to_ascii_uppercase();
                            push(VmInput::Code(upper as u8 & 0x1F, scan_for_char(upper)));
                            continue;
                        }
                    }
                }
                // A space arrives both as a key and as text: the text is enough.
                if key == GuiKey::Space && has_text {
                    continue;
                }
                if let Some(key) = map_key(key) {
                    push(VmInput::Key(key));
                } else if let GuiKey::Character(ch) = key {
                    if !has_text {
                        push(VmInput::Char(ch));
                    }
                }
            }
            Event::KeyUp(key, modifiers) => {
                let scan = match key {
                    GuiKey::Character(ch) => Some(scan_for_char(ch)),
                    key => map_key(key).map(|key| key.event().1),
                };
                if let Some(scan) = scan {
                    push(VmInput::Up(scan));
                }
                push(modifier_state(modifiers));
            }
            _ => {}
        }
    }
}

/// The host's modifier state, which also sends their scan codes.
fn modifier_state(modifiers: Modifiers) -> VmInput {
    VmInput::Modifiers {
        shift: modifiers.contains(Modifiers::SHIFT),
        ctrl: modifiers.contains(Modifiers::CTRL),
        alt: modifiers.contains(Modifiers::ALT),
    }
}

fn map_key(key: GuiKey) -> Option<DosKey> {
    match key {
        GuiKey::Enter => Some(DosKey::Enter),
        GuiKey::Escape => Some(DosKey::Escape),
        GuiKey::Backspace => Some(DosKey::Backspace),
        GuiKey::Tab => Some(DosKey::Tab),
        GuiKey::Space => Some(DosKey::Space),
        GuiKey::Up => Some(DosKey::Up),
        GuiKey::Down => Some(DosKey::Down),
        GuiKey::Left => Some(DosKey::Left),
        GuiKey::Right => Some(DosKey::Right),
        GuiKey::Home => Some(DosKey::Home),
        GuiKey::End => Some(DosKey::End),
        GuiKey::PageUp => Some(DosKey::PageUp),
        GuiKey::PageDown => Some(DosKey::PageDown),
        GuiKey::Insert => Some(DosKey::Insert),
        GuiKey::Delete => Some(DosKey::Delete),
        GuiKey::F1 => Some(DosKey::F1),
        GuiKey::F2 => Some(DosKey::F2),
        GuiKey::F3 => Some(DosKey::F3),
        GuiKey::F4 => Some(DosKey::F4),
        GuiKey::F5 => Some(DosKey::F5),
        GuiKey::F6 => Some(DosKey::F6),
        GuiKey::F7 => Some(DosKey::F7),
        GuiKey::F8 => Some(DosKey::F8),
        GuiKey::F9 => Some(DosKey::F9),
        GuiKey::F10 => Some(DosKey::F10),
        GuiKey::F11 => Some(DosKey::F11),
        GuiKey::F12 => Some(DosKey::F12),
        GuiKey::Character(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_reaches_the_machine_with_its_scan_code() {
        let mut input = Input::new();
        input.push(Event::KeyUp(GuiKey::Character('a'), Modifiers::NONE));
        input.push(Event::KeyUp(GuiKey::Escape, Modifiers::NONE));
        let mut scans = Vec::new();
        collect_events(&input, |event| {
            if let VmInput::Up(scan) = event {
                scans.push(scan);
            }
        });
        assert_eq!(scans, [scan_for_char('a'), DosKey::Escape.event().1]);
    }

    #[test]
    fn the_watchdog_waits_its_full_time() {
        assert!(!watchdog_expired(1000, 1000));
        assert!(!watchdog_expired(1000, 1000 + WATCHDOG_MS - 1));
        assert!(watchdog_expired(1000, 1000 + WATCHDOG_MS));
        // A clock read before the heartbeat was stamped is not overdue.
        assert!(!watchdog_expired(1000, 900));
    }

    #[test]
    fn typed_text_becomes_one_event_each() {
        let mut input = Input::new();
        input.push(Event::KeyDown(GuiKey::Space, Modifiers::NONE));
        input.push(Event::Text(' '));
        let mut events = Vec::new();
        collect_events(&input, |event| events.push(event));
        assert_eq!(events.iter().filter(|e| matches!(e, VmInput::Char(' '))).count(), 1);
        assert!(!events.iter().any(|e| matches!(e, VmInput::Key(DosKey::Space))));
    }

    #[test]
    fn ctrl_letters_become_control_codes() {
        let mut input = Input::new();
        input.push(Event::KeyDown(GuiKey::Character('c'), Modifiers::CTRL));
        let mut events = Vec::new();
        collect_events(&input, |event| events.push(event));
        assert!(events.iter().any(|e| matches!(e, VmInput::Code(3, _))));
    }

    #[test]
    fn a_full_input_queue_drops_rather_than_blocks() {
        let mut shared = SHARED.lock();
        shared.input_len = 0;
        for _ in 0..INPUT_QUEUE + 10 {
            shared.push(VmInput::Char('x'));
        }
        assert_eq!(shared.input_len, INPUT_QUEUE);
        shared.input_len = 0;
    }

    /// Programs a card from outside the CPU, runs the machine's frames at the
    /// prompt and sends what comes out through a mixer, as the task does.
    fn played_through_the_mixer(machine: &mut DosBox, ports: &[(u16, u8)]) -> Vec<i16> {
        machine.set_audio_rate(AUDIO_RATE);
        for &(port, value) in ports {
            feos_dosbox::cpu::Bus::out8(&mut machine.bus, port, value);
        }
        let mut mixer = feos_mixer::Mixer::new();
        let id = mixer
            .open(AudioFormat::stereo(AUDIO_RATE).expect("format"), 1)
            .expect("stream");
        let mut device = vec![0i16; 4096];
        let mut heard = Vec::new();
        for now in (0..128u64).step_by(16) {
            machine.frame(now, 1000);
            let taken = mixer.write(id, machine.pending_audio());
            machine.consume_audio(taken);
            let mixed = mixer.mix(&mut device);
            heard.extend_from_slice(&device[..mixed * 2]);
        }
        heard
    }

    #[test]
    fn sb16_16_bit_stereo_comes_out_of_the_mixer_with_its_sides_apart() {
        let mut machine = DosBox::new(None);
        for frame in 0..0x2000u32 {
            machine.bus.mem.write16(0x3_0000 + frame * 4, 0x3000);
            machine.bus.mem.write16(0x3_0000 + frame * 4 + 2, 0xD000);
        }
        let heard = played_through_the_mixer(
            &mut machine,
            &[
                // DMA 5: word address 8000h on page 2, physical 30000h, auto-init.
                (0xD8, 0),
                (0xC4, 0x00),
                (0xC4, 0x80),
                (0xC6, 0xFF),
                (0xC6, 0x3F),
                (0x8B, 0x02),
                (0xD6, 0x59),
                (0xD4, 1),
                // Volumes full, then 44.1 kHz signed stereo, auto-init.
                (0x224, 0x22),
                (0x225, 0xFF),
                (0x224, 0x04),
                (0x225, 0xFF),
                (0x22C, 0x41),
                (0x22C, 0xAC),
                (0x22C, 0x44),
                (0x22C, 0xB4),
                (0x22C, 0x30),
                (0x22C, 0xFF),
                (0x22C, 0x3F),
            ],
        );
        assert!(!heard.is_empty(), "the mixer gave the device something");
        let pair = heard.chunks(2).rev().find(|pair| pair[0] != 0).expect("sound");
        assert!(pair[0] > 0x1000 && pair[1] < -0x1000, "{pair:?}");
    }

    #[test]
    fn sb_pro_stereo_comes_out_of_the_mixer_with_its_sides_apart() {
        let mut machine = DosBox::new(None);
        machine.bus.sb = feos_dosbox::sb::SoundBlaster::with_model(feos_dosbox::sb::Model::SbPro);
        for at in 0..0x1000u32 {
            machine.bus.mem.write8(0x2_0000 + at, if at % 2 == 0 { 0xE0 } else { 0x20 });
        }
        let heard = played_through_the_mixer(
            &mut machine,
            &[
                (0x0C, 0),
                (0x02, 0),
                (0x02, 0),
                (0x03, 0xFF),
                (0x03, 0x0F),
                (0x83, 0x02),
                (0x0B, 0x59),
                (0x0A, 1),
                (0x224, 0x0E),
                (0x225, 0x02),
                (0x224, 0x22),
                (0x225, 0xFF),
                (0x224, 0x04),
                (0x225, 0xFF),
                (0x22C, 0xD1),
                (0x22C, 0x40),
                (0x22C, 0xD3),
                (0x22C, 0x48),
                (0x22C, 0xFF),
                (0x22C, 0x0F),
                (0x22C, 0x1C),
            ],
        );
        let pair = heard.chunks(2).rev().find(|pair| pair[0] != 0).expect("sound");
        assert!(pair[0] > 0x1000 && pair[1] < -0x1000, "{pair:?}");
    }

    #[test]
    fn a_machine_runs_without_its_window() {
        // What the task does, without a task: the frame loop needs no display,
        // and a guest program counts as running until its frame budget is used.
        let mut machine = DosBox::new(None);
        for now in (0..400u64).step_by(16) {
            machine.frame_paced(now);
            machine.refresh_screen();
        }
        let (width, height) = machine.display_size();
        assert!(width > 0 && height > 0);
        assert!(machine.screen.iter().any(|&pixel| pixel & 0xFF_FFFF != 0), "the prompt shows");
    }
}
