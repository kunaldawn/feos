//! Graphics support: the ramfb framebuffer and a small demo renderer.

pub mod demo;
pub mod ramfb;

use feos_gfx::Canvas;

/// Initializes the framebuffer and returns a canvas for it.
///
/// Returns `None` when QEMU was started without `-device ramfb`. The canvas
/// borrows the static scanout buffer for the rest of the kernel's lifetime.
pub fn init() -> Option<Canvas<'static>> {
    if !ramfb::setup() {
        return None;
    }

    let ptr = ramfb::buffer_ptr();
    let len = ramfb::buffer_len();
    // SAFETY: `ptr` points at the page-aligned static FRAMEBUFFER, which is
    // only handed out here. The kernel is single-core and the returned canvas
    // owns the only reference for its lifetime.
    let pixels: &'static mut [u32] = unsafe { core::slice::from_raw_parts_mut(ptr, len) };

    Canvas::new(pixels, ramfb::WIDTH, ramfb::HEIGHT, ramfb::STRIDE)
}

/// Physical address of the scanout buffer.
pub fn framebuffer_addr() -> usize {
    ramfb::buffer_ptr() as usize
}
