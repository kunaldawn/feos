//! Polled PS/2 keyboard and mouse on the i8042 controller.
//!
//! QEMU's q35 machine always exposes a PS/2 keyboard and an auxiliary mouse,
//! so the x86_64 port does not need a virtio-input transport. The driver reads
//! both devices from the shared controller in [`I8042::poll_key`] and
//! [`I8042::poll_pointer`]; every call drains whatever bytes are waiting and
//! then returns one decoded event.
//!
//! The keyboard uses scancode set 1, which is QEMU's default. Key codes are
//! Linux evdev numbers, so `feos-keys` decodes them exactly as it does the
//! virtio keyboard.

#![no_std]

use core::arch::asm;

use feos_hal::{Keyboard, KeyboardEvent, Pointer, PointerEvent};
use feos_sync::SpinLock;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const COMMAND_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const STATUS_AUX_DATA: u8 = 1 << 5;

const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
const CMD_DISABLE_KEYBOARD: u8 = 0xad;
const CMD_ENABLE_KEYBOARD: u8 = 0xae;
const CMD_DISABLE_AUX: u8 = 0xa7;
const CMD_ENABLE_AUX: u8 = 0xa8;
const CMD_WRITE_AUX: u8 = 0xd4;

const MOUSE_SET_DEFAULTS: u8 = 0xf6;
const MOUSE_ENABLE_REPORTING: u8 = 0xf4;
const ACK: u8 = 0xfa;

const KEY_QUEUE: usize = 32;

/// Which kind of pointer a board initialised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    /// PS/2 auxiliary device (relative movement).
    Ps2,
}

struct State {
    initialized: bool,
    width: u16,
    height: u16,
    x: u16,
    y: u16,

    keys: [KeyboardEvent; KEY_QUEUE],
    key_head: usize,
    key_count: usize,
    extended: bool,

    mouse_packet: [u8; 3],
    mouse_len: usize,
    left: bool,
    right: bool,
    middle: bool,
    pending: Option<PointerEvent>,
}

impl State {
    const EMPTY: Self = Self {
        initialized: false,
        width: 1,
        height: 1,
        x: 0,
        y: 0,
        keys: [KeyboardEvent {
            code: 0,
            pressed: false,
        }; KEY_QUEUE],
        key_head: 0,
        key_count: 0,
        extended: false,
        mouse_packet: [0; 3],
        mouse_len: 0,
        left: false,
        right: false,
        middle: false,
        pending: None,
    };

    fn push_key(&mut self, event: KeyboardEvent) {
        if self.key_count == KEY_QUEUE {
            self.key_head = (self.key_head + 1) % KEY_QUEUE;
            self.key_count -= 1;
        }
        let index = (self.key_head + self.key_count) % KEY_QUEUE;
        self.keys[index] = event;
        self.key_count += 1;
    }

    fn pop_key(&mut self) -> Option<KeyboardEvent> {
        if self.key_count == 0 {
            return None;
        }
        let event = self.keys[self.key_head];
        self.key_head = (self.key_head + 1) % KEY_QUEUE;
        self.key_count -= 1;
        Some(event)
    }

    fn push_mouse(&mut self, byte: u8) {
        if self.mouse_len == 0 && byte & 0x08 == 0 {
            // Not a valid first packet byte; resynchronise.
            return;
        }

        self.mouse_packet[self.mouse_len] = byte;
        self.mouse_len += 1;
        if self.mouse_len < 3 {
            return;
        }
        self.mouse_len = 0;

        let flags = self.mouse_packet[0];
        let mut dx = self.mouse_packet[1] as i32;
        let mut dy = self.mouse_packet[2] as i32;
        if flags & 0x10 != 0 {
            dx -= 256;
        }
        if flags & 0x20 != 0 {
            dy -= 256;
        }

        let max_x = self.width.saturating_sub(1) as i32;
        let max_y = self.height.saturating_sub(1) as i32;
        // PS/2 reports positive Y upward; the framebuffer grows downward.
        let x = (self.x as i32 + dx).clamp(0, max_x) as u16;
        let y = (self.y as i32 - dy).clamp(0, max_y) as u16;

        let left = flags & 0x01 != 0;
        let right = flags & 0x02 != 0;
        let middle = flags & 0x04 != 0;

        let mut event = PointerEvent {
            x,
            y,
            left: None,
            right: None,
            middle: None,
            wheel: 0,
        };
        if left != self.left {
            event.left = Some(left);
            self.left = left;
        }
        if right != self.right {
            event.right = Some(right);
            self.right = right;
        }
        if middle != self.middle {
            event.middle = Some(middle);
            self.middle = middle;
        }

        self.x = x;
        self.y = y;
        self.pending = Some(event);
    }
}

/// The i8042 controller and its decoded state.
pub struct I8042 {
    state: SpinLock<State>,
}

impl I8042 {
    /// Creates an uninitialised controller.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(State::EMPTY),
        }
    }

    /// Initialises the controller and both devices.
    ///
    /// Idempotent: both the keyboard and pointer setup paths call it.
    pub fn init(&self, width: u16, height: u16) -> bool {
        let mut state = self.state.lock();
        state.width = width.max(1);
        state.height = height.max(1);
        if !state.initialized {
            if !self.configure() {
                return false;
            }
            state.x = width / 2;
            state.y = height / 2;
            state.initialized = true;
        }
        true
    }

    /// Returns `true` once the controller has been configured.
    pub fn is_ready(&self) -> bool {
        self.state.lock().initialized
    }

    /// Returns the next key transition.
    pub fn poll_key(&self) -> Option<KeyboardEvent> {
        let mut state = self.state.lock();
        self.drain(&mut state);
        state.pop_key()
    }

    /// Returns the next pointer report.
    pub fn poll_pointer(&self) -> Option<PointerEvent> {
        let mut state = self.state.lock();
        self.drain(&mut state);
        state.pending.take()
    }

    /// Current absolute cursor position.
    pub fn position(&self) -> (u16, u16) {
        let state = self.state.lock();
        (state.x, state.y)
    }

    fn configure(&self) -> bool {
        // Disable both devices while reconfiguring.
        wait_input_clear();
        write_command(CMD_DISABLE_KEYBOARD);
        wait_input_clear();
        write_command(CMD_DISABLE_AUX);
        flush_output();

        // Read the configuration byte and mask both IRQ lines: the kernel
        // polls the controller from the desktop/shell loops.
        wait_input_clear();
        write_command(CMD_READ_CONFIG);
        let Some(mut config) = read_output() else {
            return false;
        };
        config &= !0x03;
        // Enable set-2 -> set-1 translation so the scancode table below works
        // regardless of the keyboard's power-on default.
        config |= 0x40;
        wait_input_clear();
        write_command(CMD_WRITE_CONFIG);
        wait_input_clear();
        write_data(config);

        // Enable both ports.
        wait_input_clear();
        write_command(CMD_ENABLE_KEYBOARD);
        wait_input_clear();
        write_command(CMD_ENABLE_AUX);

        // Put the mouse in a known state and start reporting. ACK bytes are
        // allowed to be missing on some emulated controllers.
        let _ = self.mouse_command(MOUSE_SET_DEFAULTS);
        let _ = self.mouse_command(MOUSE_ENABLE_REPORTING);

        flush_output();
        true
    }

    fn mouse_command(&self, command: u8) -> bool {
        wait_input_clear();
        write_command(CMD_WRITE_AUX);
        wait_input_clear();
        write_data(command);
        matches!(read_output(), Some(ACK))
    }

    fn drain(&self, state: &mut State) {
        // Bounded so a stuck controller cannot lock the desktop forever.
        //
        // The controller's AUX bit describes the byte currently sitting in
        // the output buffer. Sample the status *before* reading the data:
        // reading clears the buffer, so a second status read would describe
        // the next byte (or an empty buffer) and misclassify the byte we just
        // consumed. That turns the last byte of a PS/2 mouse packet into a
        // keyboard scancode and makes the pointer drift or jump.
        for _ in 0..256 {
            let status = status();
            if status & STATUS_OUTPUT_FULL == 0 {
                break;
            }
            let byte = read_data();
            if status & STATUS_AUX_DATA != 0 {
                state.push_mouse(byte);
            } else {
                decode_keyboard(state, byte);
            }
        }
    }
}

impl Default for I8042 {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard for I8042 {
    fn poll(&self) -> Option<KeyboardEvent> {
        self.poll_key()
    }
}

impl Pointer for I8042 {
    fn poll(&self) -> Option<PointerEvent> {
        self.poll_pointer()
    }

    fn position(&self) -> (u16, u16) {
        I8042::position(self)
    }
}

fn decode_keyboard(state: &mut State, byte: u8) {
    if byte == 0xe0 {
        state.extended = true;
        return;
    }

    let extended = state.extended;
    state.extended = false;

    let pressed = byte & 0x80 == 0;
    let code = byte & 0x7f;
    let key = if extended {
        extended_key(code)
    } else {
        let key = SCANCODE_TO_KEY[code as usize];
        if key == 0 {
            0
        } else {
            key
        }
    };

    if key != 0 {
        state.push_key(KeyboardEvent {
            code: key,
            pressed,
        });
    }
}

fn extended_key(code: u8) -> u16 {
    match code {
        0x1c => 96,  // keypad enter
        0x1d => 97,  // right control
        0x35 => 98,  // keypad slash
        0x38 => 100, // right alt
        0x47 => 102, // home
        0x48 => 103, // up
        0x49 => 104, // page up
        0x4b => 105, // left
        0x4d => 106, // right
        0x4f => 107, // end
        0x50 => 108, // down
        0x51 => 109, // page down
        0x52 => 110, // insert
        0x53 => 111, // delete
        0x5b => 125, // left meta
        0x5c => 126, // right meta
        0x5d => 127, // menu
        _ => 0,
    }
}

/// Scancode set 1 to Linux evdev key codes. Zero means "not mapped".
const SCANCODE_TO_KEY: [u16; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, // 0x00-0x0f
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, // 0x10-0x1f
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, // 0x20-0x2f
    48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, // 0x30-0x3f
    64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, // 0x40-0x4f
    80, 81, 82, 83, 0, 0, 86, 87, 88, 0, 0, 0, 0, 0, 0, 0, // 0x50-0x5f
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x60-0x6f
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x70-0x7f
];

#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: callers pass a valid x86 I/O port.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: callers pass a valid x86 I/O port.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack)) };
    value
}

fn status() -> u8 {
    // SAFETY: STATUS_PORT is the i8042 status register.
    unsafe { inb(STATUS_PORT) }
}

fn read_data() -> u8 {
    // SAFETY: DATA_PORT is the i8042 data register.
    unsafe { inb(DATA_PORT) }
}

fn write_data(value: u8) {
    // SAFETY: DATA_PORT is the i8042 data register.
    unsafe { outb(DATA_PORT, value) };
}

fn write_command(value: u8) {
    // SAFETY: COMMAND_PORT is the i8042 command register.
    unsafe { outb(COMMAND_PORT, value) };
}

fn wait_input_clear() {
    for _ in 0..100_000 {
        if status() & STATUS_INPUT_FULL == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn read_output() -> Option<u8> {
    for _ in 0..100_000 {
        if status() & STATUS_OUTPUT_FULL != 0 {
            return Some(read_data());
        }
        core::hint::spin_loop();
    }
    None
}

fn flush_output() {
    for _ in 0..256 {
        if status() & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        let _ = read_data();
    }
}
