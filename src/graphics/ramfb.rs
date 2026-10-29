//! QEMU `ramfb` framebuffer.
//!
//! `ramfb` is a very small display device: the guest allocates a linear
//! framebuffer in RAM and tells QEMU where it is by writing a 28-byte config
//! block to the writable fw_cfg item `etc/ramfb`. There are no virtqueues and
//! no modesetting ioctls; after the config write, QEMU scans out of guest RAM
//! directly, so drawing is just writing pixels.

use core::ptr::addr_of_mut;

use crate::drivers::fw_cfg;

/// Framebuffer width in pixels.
pub const WIDTH: usize = 1024;
/// Framebuffer height in pixels.
pub const HEIGHT: usize = 768;
/// Number of pixels per scanline (the buffer is tightly packed).
pub const STRIDE: usize = WIDTH;

/// `DRM_FORMAT_XRGB8888`, encoded as the fourcc `'X' 'R' '2' '4'`.
const FOURCC_XRGB8888: u32 = 0x3432_5258;

#[repr(align(4096))]
struct Framebuffer([u32; WIDTH * HEIGHT]);

/// The scanout buffer lives in `.bss`, so it costs nothing in the ELF and is
/// zeroed by the boot code. It is page aligned, which `ramfb` requires.
static mut FRAMEBUFFER: Framebuffer = Framebuffer([0; WIDTH * HEIGHT]);

pub fn buffer_ptr() -> *mut u32 {
    unsafe { addr_of_mut!(FRAMEBUFFER.0).cast::<u32>() }
}

pub const fn buffer_len() -> usize {
    WIDTH * HEIGHT
}

/// Configures QEMU's ramfb device to scan out `FRAMEBUFFER`.
///
/// Returns `false` if the machine has no fw_cfg or no `etc/ramfb` item (i.e.
/// QEMU was not started with `-device ramfb`).
pub fn setup() -> bool {
    let Some(entry) = fw_cfg::find_file("etc/ramfb") else {
        return false;
    };

    // The fw_cfg item is exactly one RAMFBCfg (28 bytes).
    if entry.size as usize != RamfbConfig::SIZE {
        return false;
    }

    let config = RamfbConfig {
        addr: buffer_ptr() as u64,
        fourcc: FOURCC_XRGB8888,
        flags: 0,
        width: WIDTH as u32,
        height: HEIGHT as u32,
        stride: (STRIDE * 4) as u32,
    };

    fw_cfg::dma_write(entry.selector, &config.to_bytes())
}

/// The fw_cfg ramfb configuration block. All fields are stored big-endian.
struct RamfbConfig {
    addr: u64,
    fourcc: u32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
}

impl RamfbConfig {
    const SIZE: usize = 28;

    fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[0..8].copy_from_slice(&self.addr.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.fourcc.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.flags.to_be_bytes());
        bytes[16..20].copy_from_slice(&self.width.to_be_bytes());
        bytes[20..24].copy_from_slice(&self.height.to_be_bytes());
        bytes[24..28].copy_from_slice(&self.stride.to_be_bytes());
        bytes
    }
}
