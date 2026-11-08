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

#![cfg_attr(not(test), no_std)]

use feos_driver_fw_cfg::{FileEntry, FwCfg};

/// `DRM_FORMAT_XRGB8888`, encoded as the fourcc `'X' 'R' '2' '4'`.
pub const FOURCC_XRGB8888: u32 = 0x3432_5258;

const RAMFB_FILE: &str = "etc/ramfb";
const CONFIG_SIZE: usize = 28;

/// What ramfb needs of fw_cfg: [`FwCfg`] on a machine, a fake in the tests.
pub trait ConfigFiles {
    /// Looks up a named item in the file directory.
    fn find_file(&self, name: &str) -> Option<FileEntry>;
    /// Writes `data` to the item `selector`, returning whether the device took it.
    fn dma_write(&self, selector: u16, data: &[u8]) -> bool;
}

impl ConfigFiles for FwCfg {
    fn find_file(&self, name: &str) -> Option<FileEntry> {
        FwCfg::find_file(self, name)
    }

    fn dma_write(&self, selector: u16, data: &[u8]) -> bool {
        FwCfg::dma_write(self, selector, data)
    }
}

/// A ramfb device configured through fw_cfg.
#[derive(Clone, Copy, Debug)]
pub struct Ramfb<F: ConfigFiles = FwCfg> {
    fw_cfg: F,
}

impl<F: ConfigFiles> Ramfb<F> {
    /// Creates a driver that talks to fw_cfg at `fw_cfg`.
    pub const fn new(fw_cfg: F) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::vec::Vec;

    /// fw_cfg with one item, which logs what's written to it.
    struct FakeFwCfg {
        item: Option<(&'static str, FileEntry)>,
        accept: bool,
        writes: RefCell<Vec<(u16, Vec<u8>)>>,
    }

    impl FakeFwCfg {
        fn with(name: &'static str, size: u32, selector: u16) -> Self {
            Self {
                item: Some((name, FileEntry { size, selector })),
                accept: true,
                writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl ConfigFiles for &FakeFwCfg {
        fn find_file(&self, name: &str) -> Option<FileEntry> {
            self.item.filter(|(item, _)| *item == name).map(|(_, entry)| entry)
        }

        fn dma_write(&self, selector: u16, data: &[u8]) -> bool {
            self.writes.borrow_mut().push((selector, data.to_vec()));
            self.accept
        }
    }

    #[test]
    fn the_config_block_is_big_endian_in_qemus_order() {
        let fw_cfg = FakeFwCfg::with("etc/ramfb", 28, 0x0025);
        let ramfb = Ramfb::new(&fw_cfg);
        assert!(ramfb.present());
        assert!(ramfb.configure(0x1234_5678_9abc_d000, 1024, 768, 1280));
        let writes = fw_cfg.writes.borrow();
        assert_eq!(writes.len(), 1);
        let (selector, block) = &writes[0];
        assert_eq!(*selector, 0x0025);
        let mut expected = Vec::new();
        expected.extend_from_slice(&0x1234_5678_9abc_d000u64.to_be_bytes());
        // DRM's `XR24` code is a little-endian fourcc; QEMU reads it as a big-endian number.
        expected.extend_from_slice(&[0x34, 0x32, 0x52, 0x58]);
        expected.extend_from_slice(&0u32.to_be_bytes());
        expected.extend_from_slice(&1024u32.to_be_bytes());
        expected.extend_from_slice(&768u32.to_be_bytes());
        // The stride is in bytes: 1280 pixels of four.
        expected.extend_from_slice(&5120u32.to_be_bytes());
        assert_eq!(block, &expected);
    }

    #[test]
    fn the_fourcc_is_xrgb8888() {
        assert_eq!(FOURCC_XRGB8888.to_le_bytes(), *b"XR24");
    }

    #[test]
    fn no_ramfb_item_is_no_display() {
        let fw_cfg = FakeFwCfg::with("etc/other", 28, 0x0025);
        let ramfb = Ramfb::new(&fw_cfg);
        assert!(!ramfb.present());
        assert!(!ramfb.configure(0x4000_0000, 640, 480, 640));
        let none = FakeFwCfg { item: None, ..FakeFwCfg::with("", 0, 0) };
        assert!(!Ramfb::new(&none).configure(0x4000_0000, 640, 480, 640));
        assert!(fw_cfg.writes.borrow().is_empty());
        assert!(none.writes.borrow().is_empty());
    }

    #[test]
    fn an_item_of_the_wrong_size_is_left_alone() {
        for size in [0, 27, 29, 4096] {
            let fw_cfg = FakeFwCfg::with("etc/ramfb", size, 0x0025);
            assert!(!Ramfb::new(&fw_cfg).configure(0x4000_0000, 640, 480, 640));
            assert!(fw_cfg.writes.borrow().is_empty(), "size {size}");
        }
    }

    #[test]
    fn a_refused_write_is_a_failure() {
        let mut fw_cfg = FakeFwCfg::with("etc/ramfb", 28, 0x0030);
        fw_cfg.accept = false;
        assert!(!Ramfb::new(&fw_cfg).configure(0x4000_0000, 640, 480, 640));
        assert_eq!(fw_cfg.writes.borrow().len(), 1);
    }
}
