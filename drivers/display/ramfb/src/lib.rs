//! QEMU `ramfb` display driver.
//!
//! `ramfb` is a very small display device: the guest allocates a linear
//! framebuffer in RAM and tells QEMU where it is by writing a 28-byte config
//! block to the writable fw_cfg item `etc/ramfb`. There are no virtqueues and
//! no modesetting ioctls; after the config write, QEMU scans out of guest RAM
//! directly, so drawing is just writing pixels.
//!
//! The driver does not own the scanout buffer. The board allocates a
//! page-aligned static buffer and passes its physical address to
//! [`Ramfb::configure`]; `feos-gfx` then wraps the same memory in a `Canvas`.

#![no_std]

use feos_driver_fw_cfg::FwCfg;

/// `DRM_FORMAT_XRGB8888`, encoded as the fourcc `'X' 'R' '2' '4'`.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;

const RAMFB_FILE: &str = "etc/ramfb";
const CONFIG_SIZE: usize = 28;

/// A ramfb device configured through fw_cfg.
#[derive(Clone, Copy, Debug)]
pub struct Ramfb {
    fw_cfg: FwCfg,
}

impl Ramfb {
    /// Creates a driver that talks to fw_cfg at `fw_cfg`.
    pub const fn new(fw_cfg: FwCfg) -> Self {
        Self { fw_cfg }
    }

    /// Returns true if QEMU exposes the `etc/ramfb` item.
    pub fn present(&self) -> bool {
        self.fw_cfg.find_file(RAMFB_FILE).is_some()
    }

    /// Points the display at `address` with the given geometry.
    ///
    /// `stride` is measured in pixels. Returns `false` if QEMU was started
    /// without `-device ramfb` or if the fw_cfg item has an unexpected size.
    pub fn configure(&self, address: u64, width: usize, height: usize, stride: usize) -> bool {
        let Some(entry) = self.fw_cfg.find_file(RAMFB_FILE) else {
            return false;
        };

        // The fw_cfg item is exactly one RAMFBCfg (28 bytes).
        if entry.size as usize != CONFIG_SIZE {
            return false;
        }

        let config = RamfbConfig {
            addr: address,
            fourcc: FOURCC_XRGB8888,
            flags: 0,
            width: width as u32,
            height: height as u32,
            stride: (stride * 4) as u32,
        };

        self.fw_cfg.dma_write(entry.selector, &config.to_bytes())
    }
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
    fn to_bytes(&self) -> [u8; CONFIG_SIZE] {
        let mut bytes = [0u8; CONFIG_SIZE];
        bytes[0..8].copy_from_slice(&self.addr.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.fourcc.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.flags.to_be_bytes());
        bytes[16..20].copy_from_slice(&self.width.to_be_bytes());
        bytes[20..24].copy_from_slice(&self.height.to_be_bytes());
        bytes[24..28].copy_from_slice(&self.stride.to_be_bytes());
        bytes
    }
}
