//! The display: a virtio-gpu when the machine has one, and ramfb otherwise.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use feos_bootinfo::Framebuffer;
use feos_driver_fw_cfg::FwCfg;
use feos_driver_ramfb::Ramfb;
use feos_driver_virtio_gpu::VirtioGpu;
use feos_gfx::Canvas;
use feos_sync::SpinLock;
use feos_virtio_core::AnyTransport;

/// Width of the ramfb scanout buffer.
pub const DISPLAY_WIDTH: usize = 1024;

/// Height of the ramfb scanout buffer.
pub const DISPLAY_HEIGHT: usize = 768;

/// The ramfb scanout buffer. Page aligned as required by the device.
#[repr(align(4096))]
struct ScanoutBuffer([u32; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

static mut SCANOUT: ScanoutBuffer = ScanoutBuffer([0; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

/// Off-screen buffer used by the desktop to compose a frame before presenting.
#[repr(align(4096))]
struct BackBuffer([u32; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

static mut BACK: BackBuffer = BackBuffer([0; DISPLAY_WIDTH * DISPLAY_HEIGHT]);

/// The virtio-gpu in use, instead of ramfb, when the machine has one.
static GPU: SpinLock<Option<VirtioGpu<AnyTransport>>> = SpinLock::new(None);
/// Display size, which a GPU can make anything: `(width << 32) | height`.
static DISPLAY_SIZE: AtomicU64 =
    AtomicU64::new(((DISPLAY_WIDTH as u64) << 32) | DISPLAY_HEIGHT as u64);
/// Address of the scanout, when it isn't the static one.
static GPU_SCANOUT: AtomicUsize = AtomicUsize::new(0);
/// The largest mode kept to. Two buffers of this come out of a 64 MiB heap.
const MAX_GPU_WIDTH: u32 = 1920;
const MAX_GPU_HEIGHT: u32 = 1200;
/// The smallest the desktop lays out sensibly.
const MIN_GPU_WIDTH: u32 = 640;
const MIN_GPU_HEIGHT: u32 = 480;

/// What firmware left us, if it did.
static FIRMWARE: SpinLock<Option<Framebuffer>> = SpinLock::new(None);
/// Whether the firmware's framebuffer is what's on the screen.
static FIRMWARE_IN_USE: AtomicBool = AtomicBool::new(false);

/// Says what firmware has set up, for [`init`] to use.
pub fn set_firmware_framebuffer(framebuffer: Framebuffer) {
    if framebuffer.base != 0 {
        *FIRMWARE.lock() = Some(framebuffer);
    }
}

/// Physical address of the scanout buffer.
pub fn framebuffer_addr() -> usize {
    let scanout = GPU_SCANOUT.load(Ordering::Acquire);
    if scanout != 0 {
        return scanout;
    }
    // SAFETY: taking the address of a `static mut` does not create a
    // reference; the buffer is only exposed once through `init`.
    unsafe { core::ptr::addr_of_mut!(SCANOUT.0) as *mut u32 as usize }
}

/// Returns a canvas over the off-screen composition buffer.
///
/// The desktop renders into this buffer and then copies it to the scanout
/// buffer in one pass, which avoids showing a half-drawn frame.
pub fn back_buffer() -> Option<Canvas<'static>> {
    let (width, height) = display_size();
    if (width, height) != (DISPLAY_WIDTH, DISPLAY_HEIGHT) {
        return Canvas::new(leak_pixels(width, height)?, width, height, width);
    }
    // SAFETY: `BACK` is a static, page-aligned buffer that is only handed out
    // here. The returned canvas owns the only reference for the rest of the
    // program; the desktop is the sole foreground consumer.
    let ptr = unsafe { core::ptr::addr_of_mut!(BACK.0).cast::<u32>() };
    let pixels: &'static mut [u32] =
        unsafe { core::slice::from_raw_parts_mut(ptr, DISPLAY_WIDTH * DISPLAY_HEIGHT) };
    Canvas::new(pixels, DISPLAY_WIDTH, DISPLAY_HEIGHT, DISPLAY_WIDTH)
}

/// The size of the display.
pub fn display_size() -> (usize, usize) {
    let packed = DISPLAY_SIZE.load(Ordering::Acquire);
    ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize)
}

/// A zeroed pixel buffer that lasts for good. A static can't be sized at run time.
fn leak_pixels(width: usize, height: usize) -> Option<&'static mut [u32]> {
    let mut pixels = alloc::vec::Vec::new();
    pixels.try_reserve_exact(width * height).ok()?;
    pixels.resize(width * height, 0u32);
    Some(alloc::boxed::Box::leak(pixels.into_boxed_slice()))
}

fn gpu_flush(x: u32, y: u32, width: u32, height: u32) {
    let rect = feos_driver_virtio_gpu::Rect { x, y, width, height };
    if let Some(gpu) = GPU.lock().as_mut() {
        // A lost flush is a stale screen until the next one, not worth stopping for.
        let _ = gpu.flush(rect);
    }
}

/// Brings up a virtio-gpu, at the mode it asks for.
fn init_gpu(transport: AnyTransport) -> Option<Canvas<'static>> {
    let mut gpu = VirtioGpu::new(transport).ok()?;
    let (width, height) = gpu
        .preferred_mode()
        .ok()
        .flatten()
        .unwrap_or((DISPLAY_WIDTH as u32, DISPLAY_HEIGHT as u32));
    let width = width.clamp(MIN_GPU_WIDTH, MAX_GPU_WIDTH);
    let height = height.clamp(MIN_GPU_HEIGHT, MAX_GPU_HEIGHT);
    let pixels = leak_pixels(width as usize, height as usize)?;
    let address = pixels.as_ptr() as usize;
    gpu.attach(address as u64, width, height).ok()?;
    *GPU.lock() = Some(gpu);
    GPU_SCANOUT.store(address, Ordering::Release);
    DISPLAY_SIZE.store(((width as u64) << 32) | height as u64, Ordering::Release);
    feos_hal::display::set_flush(gpu_flush, width, height);
    Canvas::new(pixels, width as usize, height as usize, width as usize)
}

/// Which driver is showing the display.
pub fn display_name() -> &'static str {
    if FIRMWARE_IN_USE.load(Ordering::Acquire) {
        "firmware framebuffer"
    } else if GPU_SCANOUT.load(Ordering::Acquire) != 0 {
        "virtio-gpu"
    } else {
        "ramfb"
    }
}

/// Uses the framebuffer firmware has set up. It needs no telling about changes, so
/// this is the same to everything above as ramfb.
fn init_firmware() -> Option<Canvas<'static>> {
    let framebuffer = (*FIRMWARE.lock())?;
    let (width, height, stride) = (
        framebuffer.width as usize,
        framebuffer.height as usize,
        framebuffer.stride as usize,
    );
    let pixels = stride.checked_mul(height)?;
    if width == 0 || height == 0 || stride < width || (pixels * 4) as u64 > framebuffer.size {
        return None;
    }
    // SAFETY: firmware says this is a framebuffer of this size, and the boards map all
    // of the address space it can be in. Only the foreground session draws to it.
    let memory = unsafe { core::slice::from_raw_parts_mut(framebuffer.base as usize as *mut u32, pixels) };
    GPU_SCANOUT.store(framebuffer.base as usize, Ordering::Release);
    FIRMWARE_IN_USE.store(true, Ordering::Release);
    DISPLAY_SIZE.store(((width as u64) << 32) | height as u64, Ordering::Release);
    Canvas::new(memory, width, height, stride)
}

/// Brings up a virtio-gpu if `find_gpu` finds one, then what firmware set up, and ramfb
/// through `fw_cfg` otherwise, and returns a canvas over the scanout buffer.
pub fn init(fw_cfg: FwCfg, find_gpu: fn() -> Option<AnyTransport>) -> Option<Canvas<'static>> {
    if let Some(canvas) = find_gpu().and_then(init_gpu) {
        return Some(canvas);
    }
    if let Some(canvas) = init_firmware() {
        return Some(canvas);
    }
    let ramfb = Ramfb::new(fw_cfg);
    if !ramfb.configure(
        framebuffer_addr() as u64,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
        DISPLAY_WIDTH,
    ) {
        return None;
    }

    // SAFETY: taking the address of a `static mut` does not create a
    // reference; the buffer is only exposed once, here.
    let ptr = unsafe { core::ptr::addr_of_mut!(SCANOUT.0).cast::<u32>() };
    // SAFETY: `SCANOUT` is a static, page-aligned buffer that is only handed
    // out here. The returned canvas owns the only reference for the rest of
    // the program; only the foreground session draws to it.
    let pixels: &'static mut [u32] =
        unsafe { core::slice::from_raw_parts_mut(ptr, DISPLAY_WIDTH * DISPLAY_HEIGHT) };

    Canvas::new(pixels, DISPLAY_WIDTH, DISPLAY_HEIGHT, DISPLAY_WIDTH)
}
