//! `dosbox`: the FEOS-DOS desktop application.
//!
//! The app owns one [`DosBox`] machine while its window is open. The desktop
//! drives [`frame_window`](App::frame_window) once per frame, so the prompt
//! stays responsive while the emulated PC runs a `.COM`/`.EXE` program.
//! Launching `dosbox` from the serial shell runs the same machine full-screen.

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicBool, Ordering};

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::println;
use feos_dosbox::keyboard::{scan_for_char, Key as DosKey};
use feos_dosbox::DosBox;
use feos_gui::{Event, Input, Key as GuiKey, Modifiers};
use feos_keys::SerialKeys;
use feos_sync::SpinLock;

/// The machine hosted by the desktop window.
static MACHINE: SpinLock<Option<DosBox>> = SpinLock::new(None);
/// Whether a guest program is running. The prompt does not need 30 fps
/// compositing; only a running program does.
static RUNNING: AtomicBool = AtomicBool::new(false);

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
        RUNNING.load(Ordering::Relaxed)
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        let mut machine = DosBox::new(context.platform.filesystem);
        if let Some(audio) = context.platform.audio {
            if audio.is_ready() {
                machine.set_audio_rate(audio.sample_rate());
            }
        }
        *MACHINE.lock() = Some(machine);
        // Run frames until AUTOEXEC.BAT has finished and the prompt is shown.
        RUNNING.store(true, Ordering::Relaxed);
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        let mut guard = MACHINE.lock();
        let Some(machine) = guard.as_mut() else {
            return WindowFrame::Close;
        };
        if context.keyboard_active {
            feed_events(machine, context.input);
        }
        if context.pointer_active {
            let width = context.canvas.width() as i32;
            let height = context.canvas.height() as i32;
            context.dispatch_pointer(|report| {
                machine.pointer_canvas(
                    report.x as i32,
                    report.y as i32,
                    width,
                    height,
                    report.left,
                    report.right,
                    report.middle,
                );
            });
        }
        machine.frame_paced(context.now_ms);
        RUNNING.store(
            machine.is_running() || machine.shell.in_batch(),
            Ordering::Relaxed,
        );
        play_audio(machine, context.platform.audio);
        machine.render(context.canvas);
        if machine.exit {
            RUNNING.store(false, Ordering::Relaxed);
            return WindowFrame::Close;
        }
        WindowFrame::Continue
    }

    fn close_window(&self) {
        RUNNING.store(false, Ordering::Relaxed);
        *MACHINE.lock() = None;
    }

    fn run(&self, context: &mut Context<'_>) {
        let Some(display) = context.display.as_mut() else {
            println!("dosbox: no framebuffer; run it from the desktop instead");
            return;
        };
        let platform = context.platform;
        let mut machine = DosBox::new(platform.filesystem);
        if let Some(audio) = platform.audio {
            if audio.is_ready() {
                machine.set_audio_rate(audio.sample_rate());
            }
        }
        let mut input = Input::new();
        let mut keys = SerialKeys::new();

        loop {
            let now = platform.clock.uptime_ms();
            if keys.poll(now, &mut input) {
                break;
            }
            feed_events(&mut machine, &input);
            machine.frame_paced(now);
            play_audio(&mut machine, platform.audio);
            machine.render(display);
            if machine.exit {
                break;
            }
            platform.clock.idle_ms(16);
        }
    }
}

/// Hands the device whatever it can take right now. This never blocks: what
/// doesn't fit waits in the machine for the next frame, and the machine drops the
/// oldest audio if the device falls far behind.
fn play_audio(machine: &mut DosBox, audio: Option<&dyn feos_hal::Audio>) {
    let Some(audio) = audio else {
        return;
    };
    if !audio.is_ready() {
        return;
    }
    let samples = machine.pending_audio();
    if samples.is_empty() {
        return;
    }
    let taken = audio.write(samples);
    machine.consume_audio(taken);
}

fn feed_events(machine: &mut DosBox, input: &Input) {
    let has_text = input.events().any(|event| matches!(event, Event::Text(_)));
    for event in input.events() {
        match event {
            Event::Text(ch) if !ch.is_control() => machine.keyboard().push_char(ch),
            Event::ModifiersChanged(modifiers) => set_modifiers(machine, modifiers),
            Event::KeyDown(key, modifiers) => {
                set_modifiers(machine, modifiers);
                if modifiers.contains(Modifiers::CTRL) {
                    if let GuiKey::Character(ch) = key {
                        if ch.is_ascii_alphabetic() {
                            let upper = ch.to_ascii_uppercase();
                            machine.push_key(upper as u8 & 0x1F, scan_for_char(upper));
                            continue;
                        }
                    }
                }
                // A space arrives both as a key and as text: the text is enough.
                if key == GuiKey::Space && has_text {
                    continue;
                }
                if let Some(key) = map_key(key) {
                    machine.keyboard().push_key(key);
                } else if let GuiKey::Character(ch) = key {
                    if !has_text {
                        machine.keyboard().push_char(ch);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Passes the host's modifier state on, which also sends their scan codes.
fn set_modifiers(machine: &mut DosBox, modifiers: Modifiers) {
    machine.keyboard().set_modifiers(
        modifiers.contains(Modifiers::SHIFT),
        modifiers.contains(Modifiers::CTRL),
        modifiers.contains(Modifiers::ALT),
        false,
        false,
        false,
        false,
    );
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
