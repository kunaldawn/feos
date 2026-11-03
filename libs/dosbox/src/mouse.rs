//! A Microsoft-compatible mouse driver (`INT 33h`).
//!
//! The desktop pointer and the serial shell feed absolute positions and button
//! transitions; DOS programs read them through the standard `INT 33h` calls.
//! The driver keeps the position, button counters, ranges, visibility and the
//! user handler fields a program expects.

use crate::cpu::{CpuRegs, InterruptAction};

/// Mouse state and the `INT 33h` API.
pub struct Mouse {
    pub installed: bool,
    pub x: i32,
    pub y: i32,
    /// Bit 0 left, bit 1 right, bit 2 middle.
    pub buttons: u8,
    pub visible: bool,
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
    pub threshold: u16,
    pub handler_mask: u16,
    pub handler: (u16, u16),
    pub press_count: [u16; 3],
    pub release_count: [u16; 3],
    pub motion_x: i32,
    pub motion_y: i32,
    pub text_cursor: (u8, u8),
    pub graphics_cursor: (u8, u8),
    pub cursor_hot: (i32, i32),
    pub double_speed: u16,
    pub irq: u8,
    pub version: u16,
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
            visible: true,
            min_x: 0,
            max_x: 639,
            min_y: 0,
            max_y: 199,
            threshold: 64,
            handler_mask: 0,
            handler: (0, 0),
            press_count: [0; 3],
            release_count: [0; 3],
            motion_x: 0,
            motion_y: 0,
            text_cursor: (0x07, 0x07),
            graphics_cursor: (0x77, 0x07),
            cursor_hot: (0, 0),
            double_speed: 64,
            irq: 0,
            version: 0x0300,
        }
    }

    pub fn reset(&mut self) {
        self.x = (self.min_x + self.max_x) / 2;
        self.y = (self.min_y + self.max_y) / 2;
        self.buttons = 0;
        self.visible = true;
        self.press_count = [0; 3];
        self.release_count = [0; 3];
        self.motion_x = 0;
        self.motion_y = 0;
        self.handler_mask = 0;
    }

    /// Updates the coordinate range when the video mode changes.
    pub fn set_range(&mut self, width: usize, height: usize) {
        self.max_x = width.saturating_sub(1) as i32;
        self.max_y = height.saturating_sub(1) as i32;
        self.x = self.x.clamp(self.min_x, self.max_x);
        self.y = self.y.clamp(self.min_y, self.max_y);
    }

    pub fn move_to(&mut self, x: i32, y: i32) {
        let new_x = x.clamp(self.min_x, self.max_x);
        let new_y = y.clamp(self.min_y, self.max_y);
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
        if down {
            if self.buttons & mask == 0 {
                self.press_count[button] = self.press_count[button].saturating_add(1);
            }
            self.buttons |= mask;
        } else {
            if self.buttons & mask != 0 {
                self.release_count[button] = self.release_count[button].saturating_add(1);
            }
            self.buttons &= !mask;
        }
    }

    /// `INT 33h`.
    pub fn int33(&mut self, regs: &mut CpuRegs<'_>) -> InterruptAction {
        match regs.ax() {
            0x0000 | 0x0021 => {
                self.reset();
                regs.set_ax(0xFFFF);
                regs.set_bx(self.buttons as u16);
            }
            0x0001 => {
                self.visible = true;
                regs.set_ax(0);
            }
            0x0002 => {
                self.visible = false;
                regs.set_ax(0);
            }
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
                let button = (regs.bx() as usize) & 3;
                let count = if button < 3 {
                    core::mem::take(&mut self.press_count[button])
                } else {
                    0
                };
                regs.set_ax(self.buttons as u16);
                regs.set_bx(count);
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0006 => {
                let button = (regs.bx() as usize) & 3;
                let count = if button < 3 {
                    core::mem::take(&mut self.release_count[button])
                } else {
                    0
                };
                regs.set_ax(self.buttons as u16);
                regs.set_bx(count);
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0007 => {
                self.min_x = regs.cx() as i16 as i32;
                self.max_x = regs.dx() as i16 as i32;
            }
            0x0008 => {
                self.min_y = regs.cx() as i16 as i32;
                self.max_y = regs.dx() as i16 as i32;
            }
            0x0009 => self.graphics_cursor = (regs.bx() as u8, regs.cx() as u8),
            0x000A => self.text_cursor = (regs.bx() as u8, regs.cx() as u8),
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
            0x0015 => {
                regs.set_ax(0xFFFF);
                regs.set_bx(0xFFFF);
                regs.set_cx(0);
            }
            0x0016 | 0x0017 => {}
            0x0018 => {
                self.handler_mask = regs.cx();
                self.handler = (regs.dx(), regs.es());
            }
            0x0019 => {
                regs.set_bx(self.handler.0);
                regs.set_es(self.handler.1);
            }
            0x001A | 0x001B => {}
            0x001C => self.irq = regs.bx() as u8,
            0x001D => regs.set_bx(self.irq as u16),
            0x001E => regs.set_bx(0),
            0x001F => self.installed = false,
            0x0020 => self.installed = true,
            0x0024 => {
                regs.set_bx(self.version);
                regs.set_cx(0x000A);
            }
            0x0026 => {
                regs.set_cx(self.max_x as u16);
                regs.set_dx(self.max_y as u16);
            }
            0x002B => {
                regs.set_cx(self.motion_x as u16);
                regs.set_dx(self.motion_y as u16);
                self.motion_x = 0;
                self.motion_y = 0;
            }
            0x002C => {
                self.handler_mask = regs.cx();
                self.handler = (regs.dx(), regs.es());
            }
            0x002D => {
                regs.set_bx(self.handler.0);
                regs.set_es(self.handler.1);
            }
            0x002E => self.graphics_cursor = (regs.bx() as u8, regs.cx() as u8),
            0x002F => {
                regs.set_bx(self.graphics_cursor.0 as u16);
                regs.set_cx(self.graphics_cursor.1 as u16);
            }
            0x0030 => {
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0031..=0x0034 => {
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0036 => {
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            0x0037..=0x0039 => {
                regs.set_cx(self.x as u16);
                regs.set_dx(self.y as u16);
            }
            _ => {
                regs.set_ax(0);
            }
        }
        InterruptAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(mouse: &mut Mouse, ax: u16, bx: u16, cx: u16, dx: u16) -> (u16, u16, u16, u16) {
        let mut r = [0u16; 8];
        let mut sregs = [0u16; 4];
        let mut flags = 0u16;
        r[0] = ax;
        r[3] = bx;
        r[1] = cx;
        r[2] = dx;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        mouse.int33(&mut regs);
        (regs.ax(), regs.bx(), regs.cx(), regs.dx())
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
}
