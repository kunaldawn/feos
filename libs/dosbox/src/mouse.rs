//! A Microsoft-compatible mouse driver (`INT 33h`).
//!
//! The desktop pointer and the serial shell feed absolute positions and button
//! transitions; DOS programs read them through the standard `INT 33h` calls.
//! Coordinates are the driver's virtual screen (640x200 in most modes), not the
//! video mode's pixels, and a program's range survives until the next mode set.
//! Handlers installed with `AX=0Ch` run from `IRQ 12` through a ROM stub.

use crate::cpu::{CpuRegs, InterruptAction};

/// Mouse state and the `INT 33h` API.
pub struct Mouse {
    pub installed: bool,
    pub x: i32,
    pub y: i32,
    /// Bit 0 left, bit 1 right, bit 2 middle.
    pub buttons: u8,
    /// Zero means the cursor is shown; each hide takes it further negative.
    pub hide_counter: i32,
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
    /// The current mode's virtual-screen size.
    pub virtual_width: i32,
    pub virtual_height: i32,
    pub threshold: u16,
    pub handler_mask: u16,
    pub handler: (u16, u16),
    pub press_count: [u16; 3],
    pub release_count: [u16; 3],
    pub press_pos: [(i32, i32); 3],
    pub release_pos: [(i32, i32); 3],
    pub motion_x: i32,
    pub motion_y: i32,
    pub text_cursor: (u16, u16),
    pub graphics_cursor: (u16, u16),
    pub cursor_hot: (i32, i32),
    pub double_speed: u16,
    pub irq: u8,
    pub version: u16,
    /// The events waiting for the handler, in `AX=0Ch` mask bits.
    pub pending_events: u16,
    /// The handler is running; events wait until it returns.
    pub in_handler: bool,
}

impl Default for Mouse {
    fn default() -> Self {
        Self::new()
    }
}

impl Mouse {
    pub fn new() -> Self {
        Self {
            installed: true,
            x: 0,
            y: 0,
            buttons: 0,
            hide_counter: -1,
            min_x: 0,
            max_x: 639,
            min_y: 0,
            max_y: 199,
            virtual_width: 640,
            virtual_height: 200,
            threshold: 64,
            handler_mask: 0,
            handler: (0, 0),
            press_count: [0; 3],
            release_count: [0; 3],
            press_pos: [(0, 0); 3],
            release_pos: [(0, 0); 3],
            motion_x: 0,
            motion_y: 0,
            text_cursor: (0x07, 0x07),
            graphics_cursor: (0x77, 0x07),
            cursor_hot: (0, 0),
            double_speed: 64,
            irq: 12,
            version: 0x0805,
            pending_events: 0,
            in_handler: false,
        }
    }

    /// Whether the cursor is visible.
    pub fn visible(&self) -> bool {
        self.hide_counter >= 0
    }

    /// `AX=0`: the software reset, centred in the current virtual screen.
    pub fn reset(&mut self) {
        self.min_x = 0;
        self.max_x = self.virtual_width - 1;
        self.min_y = 0;
        self.max_y = self.virtual_height - 1;
        self.x = self.virtual_width / 2;
        self.y = self.virtual_height / 2;
        self.buttons = 0;
        self.hide_counter = -1;
        self.press_count = [0; 3];
        self.release_count = [0; 3];
        self.motion_x = 0;
        self.motion_y = 0;
        self.handler_mask = 0;
        self.pending_events = 0;
    }

    /// Sets the virtual screen and the range after a video mode change.
    pub fn set_range(&mut self, width: usize, height: usize) {
        let width = width.clamp(1, 4096) as i32;
        let height = height.clamp(1, 4096) as i32;
        self.virtual_width = width;
        self.virtual_height = height;
        self.min_x = 0;
        self.max_x = width - 1;
        self.min_y = 0;
        self.max_y = height - 1;
        self.x = self.x.clamp(self.min_x, self.max_x);
        self.y = self.y.clamp(self.min_y, self.max_y);
    }

    fn set_range_x(&mut self, a: i32, b: i32) {
        // Drivers accept the bounds in either order.
        self.min_x = a.min(b);
        self.max_x = a.max(b);
        self.x = self.x.clamp(self.min_x, self.max_x);
    }

    fn set_range_y(&mut self, a: i32, b: i32) {
        self.min_y = a.min(b);
        self.max_y = a.max(b);
        self.y = self.y.clamp(self.min_y, self.max_y);
    }

    /// Moves to a position given as a fraction of the screen, from the host.
    pub fn move_fraction(&mut self, x_num: i32, x_den: i32, y_num: i32, y_den: i32) {
        let x = x_num.saturating_mul(self.virtual_width) / x_den.max(1);
        let y = y_num.saturating_mul(self.virtual_height) / y_den.max(1);
        self.move_to(x, y);
    }

    pub fn move_to(&mut self, x: i32, y: i32) {
        let new_x = x.clamp(self.min_x, self.max_x);
        let new_y = y.clamp(self.min_y, self.max_y);
        if new_x != self.x || new_y != self.y {
            self.pending_events |= 1;
        }
        self.motion_x = self.motion_x.saturating_add(new_x - self.x);
        self.motion_y = self.motion_y.saturating_add(new_y - self.y);
        self.x = new_x;
        self.y = new_y;
    }

    pub fn set_button(&mut self, button: usize, down: bool) {
        if button >= 3 {
            return;
        }
        let mask = 1 << button;
        // Mask bits: left press 2, left release 4, right 8/10h, middle 20h/40h.
        let press_bit = 2u16 << (button * 2);
        if down {
            if self.buttons & mask == 0 {
                self.press_count[button] = self.press_count[button].saturating_add(1);
                self.press_pos[button] = (self.x, self.y);
                self.pending_events |= press_bit;
            }
            self.buttons |= mask;
        } else {
            if self.buttons & mask != 0 {
                self.release_count[button] = self.release_count[button].saturating_add(1);
                self.release_pos[button] = (self.x, self.y);
                self.pending_events |= press_bit << 1;
            }
            self.buttons &= !mask;
        }
    }

    /// Whether a handler call should be raised now.
    pub fn handler_due(&self) -> bool {
        !self.in_handler
            && self.handler_mask != 0
            && self.handler != (0, 0)
            && self.pending_events & self.handler_mask != 0
    }

    /// `INT 74h`'s service: loads the handler's registers and takes the events.
    pub fn enter_handler(&mut self, regs: &mut CpuRegs<'_>) -> bool {
        let events = self.pending_events & self.handler_mask;
        self.pending_events = 0;
        if events == 0 || self.handler == (0, 0) {
            return false;
        }
        self.in_handler = true;
        regs.set_ax(events);
        regs.set_bx(self.buttons as u16);
        regs.set_cx(self.x as u16);
        regs.set_dx(self.y as u16);
        regs.set_si(self.motion_x as u16);
        regs.set_di(self.motion_y as u16);
        true
    }

    /// `INT 33h`.
    pub fn int33(&mut self, regs: &mut CpuRegs<'_>) -> InterruptAction {
        match regs.ax() {
            0x0000 => {
                self.reset();
                regs.set_ax(0xFFFF);
                regs.set_bx(2);
            }
            0x0021 => {
                self.reset();
                regs.set_ax(0xFFFF);
                regs.set_bx(2);
            }
            0x0001 => {
                if self.hide_counter < 0 {
                    self.hide_counter += 1;
                }
            }
            0x0002 => self.hide_counter = self.hide_counter.saturating_sub(1).max(-32768),
            0x0003 => {
                regs.set_bx(self.buttons as u16);
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0004 => {
                self.x = (regs.cx() as i16 as i32).clamp(self.min_x, self.max_x);
                self.y = (regs.dx() as i16 as i32).clamp(self.min_y, self.max_y);
            }
            0x0005 => {
                let button = (regs.bx() as usize).min(2);
                let count = core::mem::take(&mut self.press_count[button]);
                let (x, y) = self.press_pos[button];
                regs.set_ax(self.buttons as u16);
                regs.set_bx(count);
                regs.set_cx(x as u16);
                regs.set_dx(y as u16);
            }
            0x0006 => {
                let button = (regs.bx() as usize).min(2);
                let count = core::mem::take(&mut self.release_count[button]);
                let (x, y) = self.release_pos[button];
                regs.set_ax(self.buttons as u16);
                regs.set_bx(count);
                regs.set_cx(x as u16);
                regs.set_dx(y as u16);
            }
            0x0007 => self.set_range_x(regs.cx() as i16 as i32, regs.dx() as i16 as i32),
            0x0008 => self.set_range_y(regs.cx() as i16 as i32, regs.dx() as i16 as i32),
            0x0009 => self.graphics_cursor = (regs.bx(), regs.cx()),
            0x000A => self.text_cursor = (regs.cx(), regs.dx()),
            0x000B => {
                regs.set_cx(self.motion_x as u16);
                regs.set_dx(self.motion_y as u16);
                self.motion_x = 0;
                self.motion_y = 0;
            }
            0x000C => {
                self.handler_mask = regs.cx();
                self.handler = (regs.dx(), regs.es());
            }
            0x000F => {}
            0x0013 => self.double_speed = regs.dx(),
            0x0014 => {
                let (old_offset, old_segment, old_mask) =
                    (self.handler.0, self.handler.1, self.handler_mask);
                self.handler_mask = regs.cx();
                self.handler = (regs.dx(), regs.es());
                regs.set_cx(old_mask);
                regs.set_dx(old_offset);
                regs.set_es(old_segment);
            }
            0x0015 => regs.set_bx(0x0100),
            0x0016 | 0x0017 => {}
            0x0018 => regs.set_ax(0xFFFF),
            0x001A | 0x001B => {}
            0x001C => {}
            0x001D => regs.set_bx(0),
            0x001E => regs.set_bx(0),
            0x001F => {
                self.installed = false;
                regs.set_ax(0x001F);
                regs.set_bx(self.handler.0);
                regs.set_es(self.handler.1);
            }
            0x0020 => self.installed = true,
            0x0024 => {
                regs.set_bx(self.version);
                regs.set_ch(4);
                regs.set_cl(self.irq);
            }
            0x0026 => {
                regs.set_bx(0);
                regs.set_cx(self.max_x as u16);
                regs.set_dx(self.max_y as u16);
            }
            0x002A => {
                regs.set_ax(self.hide_counter.max(-32768) as i16 as u16);
                regs.set_bx(self.cursor_hot.0 as u16);
                regs.set_cx(self.cursor_hot.1 as u16);
                regs.set_dx(4);
            }
            _ => {}
        }
        InterruptAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_with_es(
        mouse: &mut Mouse,
        ax: u16,
        bx: u16,
        cx: u16,
        dx: u16,
        es: u16,
    ) -> (u16, u16, u16, u16) {
        let mut r = [0u32; 8];
        let mut sregs = [0u16; 6];
        let mut flags = 0u32;
        r[0] = ax as u32;
        r[3] = bx as u32;
        r[1] = cx as u32;
        r[2] = dx as u32;
        sregs[0] = es;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        mouse.int33(&mut regs);
        (regs.ax(), regs.bx(), regs.cx(), regs.dx())
    }

    fn call(mouse: &mut Mouse, ax: u16, bx: u16, cx: u16, dx: u16) -> (u16, u16, u16, u16) {
        call_with_es(mouse, ax, bx, cx, dx, 0)
    }

    #[test]
    fn position_and_buttons_round_trip() {
        let mut mouse = Mouse::new();
        mouse.set_range(320, 200);
        mouse.move_to(123, 45);
        mouse.set_button(0, true);
        let (_, buttons, x, y) = call(&mut mouse, 3, 0, 0, 0);
        assert_eq!((x, y), (123, 45));
        assert_eq!(buttons & 1, 1);
    }

    #[test]
    fn press_counter_is_consumed() {
        let mut mouse = Mouse::new();
        mouse.set_button(1, true);
        let (_, count, _, _) = call(&mut mouse, 5, 1, 0, 0);
        assert_eq!(count, 1);
        let (_, count, _, _) = call(&mut mouse, 5, 1, 0, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn inverted_ranges_are_swapped_rather_than_panicking() {
        let mut mouse = Mouse::new();
        call(&mut mouse, 7, 0, 100, 50);
        call(&mut mouse, 8, 0, 90, 10);
        assert_eq!((mouse.min_x, mouse.max_x), (50, 100));
        assert_eq!((mouse.min_y, mouse.max_y), (10, 90));
        call(&mut mouse, 4, 0, 500, 500);
        assert_eq!((mouse.x, mouse.y), (100, 90));
    }

    #[test]
    fn reset_reports_two_buttons_and_hides_the_cursor() {
        let mut mouse = Mouse::new();
        let (ax, bx, _, _) = call(&mut mouse, 0, 0, 0, 0);
        assert_eq!((ax, bx), (0xFFFF, 2));
        assert!(!mouse.visible());
        call(&mut mouse, 1, 0, 0, 0);
        assert!(mouse.visible());
    }

    #[test]
    fn hide_counter_nests() {
        let mut mouse = Mouse::new();
        call(&mut mouse, 1, 0, 0, 0);
        call(&mut mouse, 2, 0, 0, 0);
        call(&mut mouse, 2, 0, 0, 0);
        call(&mut mouse, 1, 0, 0, 0);
        assert!(!mouse.visible());
        call(&mut mouse, 1, 0, 0, 0);
        call(&mut mouse, 1, 0, 0, 0);
        assert!(mouse.visible());
    }

    #[test]
    fn handler_gets_masked_events_once() {
        let mut mouse = Mouse::new();
        call_with_es(&mut mouse, 0x0C, 0, 0x02, 0x1234, 0x2000);
        assert_eq!(mouse.handler, (0x1234, 0x2000));
        mouse.move_to(5, 5);
        assert!(!mouse.handler_due(), "motion isn't in the mask");
        mouse.set_button(0, true);
        assert!(mouse.handler_due());
        let mut r = [0u32; 8];
        let mut sregs = [0u16; 6];
        let mut flags = 0u32;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        assert!(mouse.enter_handler(&mut regs));
        assert_eq!(regs.ax(), 2);
        assert_eq!(regs.bx(), 1);
        assert!(mouse.in_handler);
        assert!(!mouse.handler_due());
    }
}
