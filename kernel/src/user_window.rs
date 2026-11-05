//! A window for a user program.
//!
//! The desktop only knows how to host an app linked into the kernel. This is one, and
//! all it does is stand between the desktop and a program: it starts the program,
//! copies what the program drew from its surface to the window, and passes input the
//! other way. The program can't touch the screen or the desktop, and if it ends for
//! any reason, the window closes.
//!
//! Because it's an ordinary hosted app, the boot test also uses it to show what happens
//! when one fails: `--test-host-fault`, `--test-host-panic` and `--test-host-hang` make
//! this code, not the program, fail on a later frame. As with `ktest`, nothing but a test should ask for them.

use core::sync::atomic::{AtomicU32, Ordering};

use feos_abi::{event, Event};
use feos_app::{App, Context, Processes, SpawnIo, WaitStatus, WindowContext, WindowFrame};
use feos_console::println;
use feos_gfx::Color;

use crate::surface;

/// The program this hosts, the one demo there is so far.
const PROGRAM: &str = "/bin/wdemo";

pub struct UserWindow {
    /// The running program's PID, or zero.
    pid: AtomicU32,
    /// The surface, plus one, once the program has made it.
    surface: AtomicU32,
    /// Frames since the window opened, for the failure tests.
    frames: AtomicU32,
}

/// The window the Start menu opens.
pub static WDEMO: UserWindow = UserWindow {
    pid: AtomicU32::new(0),
    surface: AtomicU32::new(0),
    frames: AtomicU32::new(0),
};

/// The frame the failure tests fail on.
const TEST_FAILURE_FRAME: u32 = 5;

/// Most words of arguments a window passes on.
const MAX_WORDS: usize = 8;

impl UserWindow {
    fn release_surface(&self) {
        let surface = self.surface.swap(0, Ordering::AcqRel);
        if surface != 0 {
            surface::release(surface as usize - 1);
        }
    }

    fn surface_index(&self, pid: u32) -> Option<usize> {
        match self.surface.load(Ordering::Acquire) {
            0 => {
                let index = surface::attach(pid)?;
                self.surface.store(index as u32 + 1, Ordering::Release);
                Some(index)
            }
            surface => Some(surface as usize - 1),
        }
    }

    fn pass_input(&self, index: usize, context: &WindowContext<'_, '_>) {
        context.dispatch_pointer(|report| {
            surface::push_event(
                index,
                Event {
                    kind: event::POINTER,
                    a: i32::from(report.x),
                    b: i32::from(report.y),
                    c: i32::from(report.left == Some(true)),
                },
            );
        });
        context.dispatch_keys(|input| {
            if let feos_gui::Event::Text(character) = input {
                surface::push_event(
                    index,
                    Event {
                        kind: event::KEY,
                        a: character as i32,
                        b: 1,
                        c: 0,
                    },
                );
            }
        });
    }
}

impl App for UserWindow {
    fn name(&self) -> &'static str {
        "wdemo"
    }

    fn usage(&self) -> &'static str {
        "wdemo [--crash] [--frames n]"
    }

    fn description(&self) -> &'static str {
        "a user program in a window"
    }

    fn windowed(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        true
    }

    fn animation_period_ms(&self) -> u32 {
        33
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        self.release_surface();
        self.frames.store(0, Ordering::Relaxed);
        let mut words = [""; MAX_WORDS];
        let mut count = 0;
        for word in context.args.split_whitespace().take(MAX_WORDS) {
            words[count] = word;
            count += 1;
        }
        let io = SpawnIo {
            background: true,
            ..SpawnIo::default()
        };
        match crate::sched::PROCESSES.spawn_user(PROGRAM, &words[..count], "/", io) {
            Ok(pid) => {
                self.pid.store(pid, Ordering::Release);
                true
            }
            Err(error) => {
                println!("[desktop] {PROGRAM} couldn't start: {error:?}");
                false
            }
        }
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        if self.frames.fetch_add(1, Ordering::Relaxed) == TEST_FAILURE_FRAME {
            if context.args.contains("--test-host-panic") {
                println!("[desktop] a hosted app is about to panic, for a test");
                panic!("a window's panic");
            }
            if context.args.contains("--test-host-hang") {
                println!("[desktop] a hosted app is about to hang, for a test");
                loop {
                    core::hint::spin_loop();
                }
            }
            if context.args.contains("--test-host-fault") {
                println!("[desktop] a hosted app is about to fault, for a test");
                // SAFETY: not at all, and that's the point. Low addresses are mapped for
                // the kernel, and this one isn't, by either port.
                unsafe { core::ptr::write_volatile(core::hint::black_box(0x70_0000_0000usize) as *mut u32, 1) };
            }
        }
        let pid = self.pid.load(Ordering::Acquire);
        let gone = match crate::sched::PROCESSES.try_wait(pid) {
            WaitStatus::Running => None,
            WaitStatus::Exited(status) => Some(status),
            // Gone so long ago that its status isn't remembered.
            WaitStatus::Unknown => Some(feos_app::EXIT_KILLED),
        };
        if let Some(status) = gone {
            println!("[desktop] {PROGRAM} ended with status {status}; closing its window");
            self.pid.store(0, Ordering::Release);
            self.release_surface();
            return WindowFrame::Close;
        }
        let Some(index) = self.surface_index(pid) else {
            context.canvas.clear(Color::rgb(0, 0, 0));
            return WindowFrame::Continue;
        };
        self.pass_input(index, context);

        let (pixels, width, height, _presented) = surface::pixels(index);
        let (shown_width, shown_height) = (
            width.min(context.canvas.width()),
            height.min(context.canvas.height()),
        );
        for y in 0..shown_height {
            for x in 0..shown_width {
                // SAFETY: the window holds a use of the surface, which keeps its frames
                // from going, and this is inside its size. The program can be writing it
                // at the same moment: that's why this is volatile, and all that can
                // happen is a torn picture.
                let pixel = unsafe { pixels.add(y * width + x).read_volatile() };
                context.canvas.put_pixel(x as i32, y as i32, Color::from_u32(pixel));
            }
        }
        WindowFrame::Continue
    }

    fn close_window(&self) {
        println!("[desktop] wdemo: closing");
        let pid = self.pid.swap(0, Ordering::AcqRel);
        if pid != 0 {
            if let Some(index) = self.surface_index(pid) {
                surface::push_event(index, Event { kind: event::CLOSE, a: 0, b: 0, c: 0 });
            }
            // It gets no longer than this to go by itself.
            crate::sched::sleep_ms(50);
            let _ = crate::sched::PROCESSES.try_kill(pid);
        }
        self.release_surface();
    }

    /// From a shell with the display to itself: the program, for a moment, with nobody
    /// looking at its surface.
    fn run(&self, context: &mut Context<'_>) -> i32 {
        let processes = &crate::sched::PROCESSES;
        match processes.spawn_user(PROGRAM, context.argv, "/", SpawnIo::default()) {
            Ok(pid) => processes.wait(pid).unwrap_or(feos_app::EXIT_KILLED),
            Err(_) => 126,
        }
    }
}
