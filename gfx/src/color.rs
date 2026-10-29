//! RGB colours for a 32-bit XRGB8888 framebuffer.

/// A 24-bit RGB colour, packed as `0x00RRGGBB`.
///
/// In an XRGB8888 little-endian framebuffer this corresponds to the bytes
/// `B, G, R, 0x00`, which is exactly what QEMU's `ramfb` device expects for
/// the `XR24` fourcc.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Color(u32);

impl Color {
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const RED: Self = Self::rgb(255, 0, 0);
    pub const GREEN: Self = Self::rgb(0, 200, 0);
    pub const BLUE: Self = Self::rgb(0, 0, 255);
    pub const YELLOW: Self = Self::rgb(255, 255, 0);
    pub const CYAN: Self = Self::rgb(0, 255, 255);
    pub const MAGENTA: Self = Self::rgb(255, 0, 255);
    pub const ORANGE: Self = Self::rgb(255, 128, 0);
    pub const GRAY: Self = Self::rgb(128, 128, 128);
    pub const LIGHT_GRAY: Self = Self::rgb(200, 200, 200);
    pub const DARK_GRAY: Self = Self::rgb(32, 32, 32);
    pub const NAVY: Self = Self::rgb(16, 24, 64);
    pub const TEAL: Self = Self::rgb(0, 128, 128);
    pub const PURPLE: Self = Self::rgb(128, 0, 128);

    /// Builds a colour from 8-bit red, green and blue components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    /// Builds a colour from a packed `0x00RRGGBB` value (the high byte is
    /// ignored).
    pub const fn from_u32(value: u32) -> Self {
        Self(value & 0x00ff_ffff)
    }

    /// Returns the packed `0x00RRGGBB` value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn r(self) -> u8 {
        ((self.0 >> 16) & 0xff) as u8
    }

    pub const fn g(self) -> u8 {
        ((self.0 >> 8) & 0xff) as u8
    }

    pub const fn b(self) -> u8 {
        (self.0 & 0xff) as u8
    }

    /// Linearly interpolates towards `other`; `t = 0` gives `self`, `t = 255`
    /// gives `other`.
    pub fn lerp(self, other: Self, t: u8) -> Self {
        let t = t as u32;
        let inv = 255 - t;
        let mix = |a: u8, b: u8| -> u8 { ((a as u32 * inv + b as u32 * t) / 255) as u8 };
        Self::rgb(
            mix(self.r(), other.r()),
            mix(self.g(), other.g()),
            mix(self.b(), other.b()),
        )
    }
}

impl From<u32> for Color {
    fn from(value: u32) -> Self {
        Self::from_u32(value)
    }
}

impl From<Color> for u32 {
    fn from(color: Color) -> Self {
        color.raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks_channels() {
        let color = Color::rgb(0x12, 0x34, 0x56);
        assert_eq!(color.raw(), 0x0012_3456);
        assert_eq!((color.r(), color.g(), color.b()), (0x12, 0x34, 0x56));
        assert_eq!(Color::from_u32(0xff12_3456), color);
    }

    #[test]
    fn lerp_hits_both_ends() {
        let a = Color::rgb(0, 0, 0);
        let b = Color::rgb(255, 100, 50);
        assert_eq!(a.lerp(b, 0), a);
        assert_eq!(a.lerp(b, 255), b);
        let middle = a.lerp(b, 128);
        assert!((127..=128).contains(&middle.r()));
    }
}
