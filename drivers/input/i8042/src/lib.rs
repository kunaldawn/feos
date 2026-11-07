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

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicUsize, Ordering};

use feos_hal::{Keyboard, KeyboardEvent, Pointer, PointerEvent};
use feos_port_io::{inb, outb};
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
const MOUSE_SET_SAMPLE_RATE: u8 = 0xf3;
const MOUSE_GET_ID: u8 = 0xf2;
const ACK: u8 = 0xfa;
/// Device ID of a mouse in IntelliMouse (wheel) mode.
const MOUSE_ID_WHEEL: u8 = 3;
/// The sample-rate sequence that asks a mouse to enter IntelliMouse mode.
const WHEEL_KNOCK: [u8; 3] = [200, 100, 80];

/// A mouse sends a packet's bytes about a millisecond apart. A partial packet this
/// much older than now lost a byte, and whatever comes next starts a new one.
const STALE_PACKET_MS: u64 = 50;

/// Mouse packet flag bits.
const PACKET_ALWAYS_SET: u8 = 1 << 3;
const PACKET_X_OVERFLOW: u8 = 1 << 6;
const PACKET_Y_OVERFLOW: u8 = 1 << 7;

const KEY_QUEUE: usize = 32;
/// Pointer reports buffered between polls.
const POINTER_QUEUE: usize = 16;
/// Scancode prefix of the Pause key.
const PAUSE_PREFIX: u8 = 0xe1;
/// Linux evdev code for the Pause key.
const KEY_PAUSE: u16 = 119;

/// Which kind of pointer a board initialised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    /// PS/2 auxiliary device (relative movement).
    Ps2,
    /// PS/2 auxiliary device with a wheel (IntelliMouse protocol).
    Ps2Wheel,
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
    /// Bytes left to swallow from a Pause key sequence.
    pause_bytes: u8,

    mouse_packet: [u8; 4],
    /// When the last byte of a partial packet arrived.
    mouse_last_ms: u64,
    mouse_len: usize,
    /// Bytes in a mouse packet: 3, or 4 with a wheel.
    packet_size: usize,
    left: bool,
    right: bool,
    middle: bool,
    pointer: [PointerEvent; POINTER_QUEUE],
    pointer_head: usize,
    pointer_count: usize,
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
        pause_bytes: 0,
        mouse_packet: [0; 4],
        mouse_last_ms: 0,
        mouse_len: 0,
        packet_size: 3,
        left: false,
        right: false,
        middle: false,
        pointer: [PointerEvent {
            x: 0,
            y: 0,
            left: None,
            right: None,
            middle: None,
            wheel: 0,
        }; POINTER_QUEUE],
        pointer_head: 0,
        pointer_count: 0,
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

    /// Queues a pointer report.
    ///
    /// Button and wheel reports must all reach the desktop, or a click is lost
    /// or a drag sticks. Only position matters from a position-only report, so
    /// a new position replaces a queued one. If the queue still fills up, the
    /// oldest report goes.
    fn push_pointer(&mut self, event: PointerEvent) {
        if self.pointer_count > 0 && event.is_position_only() {
            let last = (self.pointer_head + self.pointer_count - 1) % POINTER_QUEUE;
            if self.pointer[last].is_position_only() {
                self.pointer[last] = event;
                return;
            }
        }
        if self.pointer_count == POINTER_QUEUE {
            self.pointer_head = (self.pointer_head + 1) % POINTER_QUEUE;
            self.pointer_count -= 1;
        }
        let index = (self.pointer_head + self.pointer_count) % POINTER_QUEUE;
        self.pointer[index] = event;
        self.pointer_count += 1;
    }

    fn pop_pointer(&mut self) -> Option<PointerEvent> {
        if self.pointer_count == 0 {
            return None;
        }
        let event = self.pointer[self.pointer_head];
        self.pointer_head = (self.pointer_head + 1) % POINTER_QUEUE;
        self.pointer_count -= 1;
        Some(event)
    }

    /// [`push_mouse`](Self::push_mouse), first dropping a partial packet that's
    /// been waiting too long. The marker bit in a packet's first byte only catches
    /// a lost byte by luck: the bytes after one can have that bit set too, and every
    /// packet from there on would decode shifted.
    fn push_mouse_at(&mut self, byte: u8, now_ms: u64) {
        if self.mouse_len != 0 && now_ms.saturating_sub(self.mouse_last_ms) > STALE_PACKET_MS {
            self.mouse_len = 0;
        }
        self.mouse_last_ms = now_ms;
        self.push_mouse(byte);
    }

    fn push_mouse(&mut self, byte: u8) {
        if self.mouse_len == 0 && byte & PACKET_ALWAYS_SET == 0 {
            // Not a valid first packet byte; resynchronise.
            return;
        }

        self.mouse_packet[self.mouse_len] = byte;
        self.mouse_len += 1;
        if self.mouse_len < self.packet_size {
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
        // An overflowed axis has no usable delta, and overflow on a packet that
        // lost a byte is how misalignment usually shows. Buttons still count.
        if flags & PACKET_X_OVERFLOW != 0 {
            dx = 0;
        }
        if flags & PACKET_Y_OVERFLOW != 0 {
            dy = 0;
        }
        // The fourth byte is a signed count where negative is away from the
        // user, the opposite of `PointerEvent::wheel`.
        let wheel = if self.packet_size == 4 {
            -(self.mouse_packet[3] as i8 as i32)
        } else {
            0
        };

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
            wheel,
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
        self.push_pointer(event);
    }
}

/// The i8042 controller and its decoded state.
pub struct I8042 {
    state: SpinLock<State>,
    /// Milliseconds since boot, for telling a slow packet from a broken one.
    clock: AtomicUsize,
}

fn no_clock() -> u64 {
    0
}

impl I8042 {
    /// Creates an uninitialised controller.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(State::EMPTY),
            clock: AtomicUsize::new(0),
        }
    }

    /// Gives the driver a clock, in milliseconds. Without one, a lost mouse byte is
    /// only noticed when the marker bit happens to catch it.
    pub fn set_clock(&self, clock: fn() -> u64) {
        self.clock.store(clock as usize, Ordering::Release);
    }

    fn now_ms(&self) -> u64 {
        let clock = match self.clock.load(Ordering::Acquire) {
            0 => no_clock,
            // SAFETY: only `set_clock` writes this, with a valid function pointer,
            // and a function pointer is `usize`-sized.
            address => unsafe { core::mem::transmute::<usize, fn() -> u64>(address) },
        };
        clock()
    }

    /// Initialises the controller and both devices.
    ///
    /// Idempotent: both the keyboard and pointer setup paths call it.
    pub fn init(&self, width: u16, height: u16) -> bool {
        let mut state = self.state.lock();
        state.width = width.max(1);
        state.height = height.max(1);
        if !state.initialized {
            let Some(packet_size) = self.configure() else {
                return false;
            };
            state.packet_size = packet_size;
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
        state.pop_pointer()
    }

    /// Which kind of pointer the controller found.
    pub fn pointer_kind(&self) -> PointerKind {
        if self.state.lock().packet_size == 4 {
            PointerKind::Ps2Wheel
        } else {
            PointerKind::Ps2
        }
    }

    /// Current absolute cursor position.
    pub fn position(&self) -> (u16, u16) {
        let state = self.state.lock();
        (state.x, state.y)
    }

    /// Configures the controller and returns the mouse's packet size.
    fn configure(&self) -> Option<usize> {
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
        let mut config = read_output()?;
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
        let packet_size = if self.enable_wheel() { 4 } else { 3 };
        let _ = self.mouse_command(MOUSE_ENABLE_REPORTING);

        flush_output();
        Some(packet_size)
    }

    /// Asks the mouse to enter IntelliMouse mode and reports if it agreed.
    fn enable_wheel(&self) -> bool {
        for rate in WHEEL_KNOCK {
            let _ = self.mouse_command(MOUSE_SET_SAMPLE_RATE);
            let _ = self.mouse_command(rate);
        }
        self.mouse_command(MOUSE_GET_ID) && read_output() == Some(MOUSE_ID_WHEEL)
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
                state.push_mouse_at(byte, self.now_ms());
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

    fn set_screen(&self, width: u16, height: u16) {
        let mut state = self.state.lock();
        state.width = width.max(1);
        state.height = height.max(1);
        state.x = state.x.min(state.width - 1);
        state.y = state.y.min(state.height - 1);
    }
}

fn decode_keyboard(state: &mut State, byte: u8) {
    // Pause sends E1 1D 45 E1 9D C5, all at once on press and nothing on release.
    // The bytes would otherwise decode as LCtrl and NumLock.
    if state.pause_bytes > 0 {
        state.pause_bytes -= 1;
        if state.pause_bytes == 0 {
            state.push_key(KeyboardEvent {
                code: KEY_PAUSE,
                pressed: true,
            });
            state.push_key(KeyboardEvent {
                code: KEY_PAUSE,
                pressed: false,
            });
        }
        return;
    }
    if byte == PAUSE_PREFIX {
        state.pause_bytes = 5;
        return;
    }

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
        SCANCODE_TO_KEY[code as usize]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut state = State::EMPTY;
        state.width = 1024;
        state.height = 768;
        state.x = 100;
        state.y = 100;
        state
    }

    fn packet(state: &mut State, bytes: &[u8]) {
        for &byte in bytes {
            state.push_mouse(byte);
        }
    }

    #[test]
    fn a_lost_byte_stops_at_the_next_quiet_moment() {
        // DRV-16: every byte here has the marker bit, so it can't catch this. The
        // second packet has lost a byte, and would otherwise take the third's first
        // and shift everything after it.
        let mut state = state();
        for (time, byte) in [(0, 0x08u8), (1, 8), (2, 0), (100, 0x08), (101, 8)] {
            state.push_mouse_at(byte, time);
        }
        assert_eq!(state.mouse_len, 2);
        for (time, byte) in [(200, 0x09u8), (201, 0), (202, 0)] {
            state.push_mouse_at(byte, time);
        }
        let moved = state.pop_pointer().unwrap();
        assert_eq!(moved.x, 108);
        let pressed = state.pop_pointer().unwrap();
        assert_eq!(pressed.left, Some(true));
        assert_eq!(pressed.x, 108);
        assert_eq!(state.pop_pointer(), None);
    }

    #[test]
    fn slow_bytes_within_the_limit_still_make_a_packet() {
        let mut state = state();
        for (time, byte) in [(0, 0x08u8), (STALE_PACKET_MS, 3), (2 * STALE_PACKET_MS, 0)] {
            state.push_mouse_at(byte, time);
        }
        assert_eq!(state.pop_pointer().unwrap().x, 103);
    }

    #[test]
    fn a_press_followed_by_motion_in_one_drain_is_not_lost() {
        // DRV-4: one pending slot was overwritten by the next packet.
        let mut state = state();
        packet(&mut state, &[0x09, 0, 0]); // press
        packet(&mut state, &[0x09, 5, 0]); // move with the button held
        packet(&mut state, &[0x08, 0, 0]); // release

        let press = state.pop_pointer().unwrap();
        assert_eq!(press.left, Some(true));
        let motion = state.pop_pointer().unwrap();
        assert_eq!(motion.left, None);
        assert_eq!(motion.x, 105);
        let release = state.pop_pointer().unwrap();
        assert_eq!(release.left, Some(false));
        assert_eq!(state.pop_pointer(), None);
    }

    #[test]
    fn position_reports_coalesce_but_never_across_a_button() {
        let mut state = state();
        packet(&mut state, &[0x08, 1, 0]);
        packet(&mut state, &[0x08, 1, 0]);
        packet(&mut state, &[0x09, 0, 0]);
        packet(&mut state, &[0x09, 1, 0]);
        packet(&mut state, &[0x09, 1, 0]);

        let first = state.pop_pointer().unwrap();
        assert_eq!((first.x, first.left), (102, None));
        assert_eq!(state.pop_pointer().unwrap().left, Some(true));
        let last = state.pop_pointer().unwrap();
        assert_eq!((last.x, last.left), (104, None));
        assert_eq!(state.pop_pointer(), None);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_report() {
        let mut state = state();
        for index in 0..POINTER_QUEUE * 2 {
            let flags = if index % 2 == 0 { 0x09 } else { 0x08 };
            packet(&mut state, &[flags, 0, 0]);
        }
        let mut seen = 0;
        let mut last = None;
        while let Some(report) = state.pop_pointer() {
            seen += 1;
            last = Some(report);
        }
        assert_eq!(seen, POINTER_QUEUE);
        assert_eq!(last.unwrap().left, Some(false));
    }

    #[test]
    fn deltas_are_signed_and_y_points_down() {
        let mut state = state();
        packet(&mut state, &[0x08 | 0x10 | 0x20, 0xf6, 0xf6]); // -10, -10
        let report = state.pop_pointer().unwrap();
        assert_eq!((report.x, report.y), (90, 110));
    }

    #[test]
    fn overflowed_deltas_are_ignored_but_buttons_still_count() {
        // DRV-16.
        let mut state = state();
        packet(&mut state, &[0x09 | PACKET_X_OVERFLOW | PACKET_Y_OVERFLOW, 0xff, 0xff]);
        let report = state.pop_pointer().unwrap();
        assert_eq!((report.x, report.y), (100, 100));
        assert_eq!(report.left, Some(true));
    }

    #[test]
    fn a_byte_without_the_marker_bit_resynchronises() {
        let mut state = state();
        packet(&mut state, &[0x00, 0x00, 0x09, 0, 0]);
        assert_eq!(state.pop_pointer().unwrap().left, Some(true));
    }

    #[test]
    fn a_wheel_mouse_sends_four_byte_packets() {
        // DRV-5: QEMU sends -1 for wheel-up, which is away from the user.
        let mut state = state();
        state.packet_size = 4;
        packet(&mut state, &[0x08, 0, 0, 0xff]);
        assert_eq!(state.pop_pointer().unwrap().wheel, 1);
        packet(&mut state, &[0x08, 0, 0, 0x01]);
        assert_eq!(state.pop_pointer().unwrap().wheel, -1);
    }

    #[test]
    fn pause_is_one_key_not_lctrl_and_numlock() {
        // DRV-16.
        let mut state = state();
        for byte in [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5] {
            decode_keyboard(&mut state, byte);
        }
        assert_eq!(
            state.pop_key(),
            Some(KeyboardEvent {
                code: KEY_PAUSE,
                pressed: true
            })
        );
        assert_eq!(
            state.pop_key(),
            Some(KeyboardEvent {
                code: KEY_PAUSE,
                pressed: false
            })
        );
        assert_eq!(state.pop_key(), None);

        // The next key decodes normally.
        decode_keyboard(&mut state, 0x1e);
        assert_eq!(state.pop_key().unwrap().code, 30);
    }

    #[test]
    fn extended_and_plain_keys_decode() {
        let mut state = state();
        decode_keyboard(&mut state, 0xe0);
        decode_keyboard(&mut state, 0x48);
        assert_eq!(
            state.pop_key(),
            Some(KeyboardEvent {
                code: 103,
                pressed: true
            })
        );
        decode_keyboard(&mut state, 0x9e);
        assert_eq!(
            state.pop_key(),
            Some(KeyboardEvent {
                code: 30,
                pressed: false
            })
        );
    }
}
