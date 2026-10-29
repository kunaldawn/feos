//! Intel High Definition Audio (HDA) driver.
//!
//! QEMU exposes an ICH6-compatible HDA controller as `-device intel-hda` with
//! an `hda-output` codec (playback only). The controller is a PCI device with a
//! memory BAR; this driver programs the controller's corb/rirb command rings,
//! discovers the codec's DAC and output pin, and plays interleaved 16-bit PCM
//! through one output stream descriptor.
//!
//! The driver is polled and interrupt-free: it waits for the buffer-completion
//! status bit (with a deadline from the HAL clock) instead of taking an IRQ.
//!
//! # Memory
//!
//! The command rings, buffer descriptor list and PCM buffer live inside the
//! driver's static state, so [`IntelHda::new`] can be used in a `static` with
//! no allocator. [`IntelHda`] owns its buffers and is meant to be a singleton;
//! the spin lock only protects against re-entrant calls.
//!
//! # Addresses
//!
//! DMA addresses are the buffers' virtual addresses. `feos` runs with the MMU
//! off, so virtual and physical addresses are identical. A future port with
//! paging enabled must translate these before handing them to the device.

#![no_std]

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};

use feos_hal::{Audio, AudioError, Clock};
use feos_sync::SpinLock;

/// Playback rate. The codec is programmed for 48 kHz, 16-bit stereo.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channel count.
pub const CHANNELS: u16 = 2;
/// Maximum block length: three seconds of stereo samples.
pub const MAX_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize * 3;

const FMT_48K_16_STEREO: u16 = 0x0011; // 48 kHz, 16-bit, 2 channels, PCM
const STREAM_TAG: u32 = 1;
const STREAM_CHANNEL: u16 = 0;

const CORB_ENTRIES: usize = 256;
const RIRB_ENTRIES: usize = 256;

// Controller registers (ICH6 layout).
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0e;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4a;
const CORBCTL: usize = 0x4c;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5a;
const RIRBCTL: usize = 0x5c;

// Per-stream registers, relative to the stream descriptor base.
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0c;
const SD_FMT: usize = 0x12;
const SD_BDLPL: usize = 0x18;
const SD_BDLPU: usize = 0x1c;

/// Output stream descriptors start at 0x80; 0..3 are inputs, 4..7 outputs.
const SD_INDEX: usize = 4;

const GCTL_RESET: u32 = 1 << 0;
const GCTL_FLUSH: u32 = 1 << 1;
const CORB_RUN: u8 = 1 << 1;
const RIRB_RUN: u8 = 1 << 1;
const RIRBWP_RESET: u16 = 1 << 15;
const SD_RUN: u32 = 1 << 1;
const SD_RESET: u32 = 1 << 0;
const SD_COMPLETE: u8 = 1 << 2;
const SD_INT_MASK: u8 = 0x1c;

const AMP_UNMUTE_0DB: u16 = 0xb07f; // output amp + left/right + 0 dB, not muted
const PIN_OUTPUT_ENABLE: u16 = 1 << 6;

// Codec verbs (12-bit IDs).
const VERB_GET_PARAMETER: u16 = 0xf00;
const VERB_SET_STREAM_FORMAT: u16 = 0x200;
const VERB_SET_AMP_GAIN_MUTE: u16 = 0x300;
const VERB_SET_POWER_STATE: u16 = 0x705;
const VERB_SET_CONVERTER_STREAM: u16 = 0x706;
const VERB_SET_PIN_CONTROL: u16 = 0x707;

// Codec parameters.
const PARAM_NODE_COUNT: u16 = 0x04;
const PARAM_AUDIO_WIDGET_CAP: u16 = 0x09;
const PARAM_PIN_CAP: u16 = 0x0c;

const WIDGET_AUDIO_OUTPUT: u32 = 0;
const WIDGET_PIN_COMPLEX: u32 = 4;
const PIN_CAP_OUTPUT: u32 = 1 << 4;

const COMMAND_TIMEOUT: usize = 1_000_000;

#[repr(align(128))]
struct Corb([u32; CORB_ENTRIES]);

#[repr(align(128))]
struct Rirb([[u32; 2]; RIRB_ENTRIES]);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BdlEntry {
    address: u64,
    length: u32,
    flags: u32,
}

#[repr(align(128))]
struct Bdl([BdlEntry; 1]);

#[repr(align(128))]
struct Pcm([i16; MAX_SAMPLES]);

struct Controller {
    mmio: usize,
    clock: Option<&'static dyn Clock>,
    ready: bool,
    codec: u8,
    dac: u8,
    pin: u8,
    corb_wp: u8,
    rirb_rp: u8,
    corb: Corb,
    rirb: Rirb,
    bdl: Bdl,
    pcm: Pcm,
}

impl Controller {
    const fn new() -> Self {
        Self {
            mmio: 0,
            clock: None,
            ready: false,
            codec: 0,
            dac: 0,
            pin: 0,
            corb_wp: 0,
            rirb_rp: 0,
            corb: Corb([0; CORB_ENTRIES]),
            rirb: Rirb([[0; 2]; RIRB_ENTRIES]),
            bdl: Bdl([BdlEntry {
                address: 0,
                length: 0,
                flags: 0,
            }]),
            pcm: Pcm([0; MAX_SAMPLES]),
        }
    }

    fn write8(&self, offset: usize, value: u8) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.mmio + offset) as *mut u8, value) }
    }

    fn read16(&self, offset: usize) -> u16 {
        // SAFETY: see `read8`.
        unsafe { read_volatile((self.mmio + offset) as *const u16) }
    }

    fn write16(&self, offset: usize, value: u16) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.mmio + offset) as *mut u16, value) }
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: see `read8`.
        unsafe { read_volatile((self.mmio + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.mmio + offset) as *mut u32, value) }
    }

    /// Puts the controller into reset and back out again.
    fn reset_controller(&self) {
        let gctl = self.read32(GCTL);
        self.write32(GCTL, gctl | GCTL_RESET);
        for _ in 0..100_000 {
            if self.read32(GCTL) & GCTL_RESET != 0 {
                break;
            }
        }

        // Leaving reset performs the actual controller reset.
        self.write32(GCTL, gctl & !GCTL_RESET);
        for _ in 0..100_000 {
            if self.read32(GCTL) & GCTL_RESET == 0 {
                break;
            }
        }

        self.write32(GCTL, self.read32(GCTL) | GCTL_FLUSH);
    }

    /// Points the controller's command/response rings at the static buffers.
    fn setup_rings(&mut self) {
        self.corb_wp = 0;
        self.rirb_rp = 0;

        let corb = addr_of!(self.corb.0) as u64;
        let rirb = addr_of!(self.rirb.0) as u64;

        self.write32(CORBLBASE, corb as u32);
        self.write32(CORBUBASE, (corb >> 32) as u32);
        self.write16(CORBRP, 0);
        self.write16(CORBWP, 0);

        self.write32(RIRBLBASE, rirb as u32);
        self.write32(RIRBUBASE, (rirb >> 32) as u32);
        self.write16(RIRBWP, RIRBWP_RESET);
        // Keep the response ring armed across many commands; responses are
        // polled, so interrupts are never enabled.
        self.write16(RINTCNT, 0xff);

        self.write8(CORBCTL, CORB_RUN);
        self.write8(RIRBCTL, RIRB_RUN);
    }

    /// Sends one 4-bit codec / 8- or 16-bit payload verb and returns its
    /// response.
    fn command(&mut self, node: u8, verb: u16, payload: u16) -> Option<u32> {
        let command = ((self.codec as u32) << 28)
            | ((node as u32) << 20)
            | ((verb as u32) << 8)
            | payload as u32;

        let next = self.corb_wp.wrapping_add(1);
        // SAFETY: `next` is a valid index into the ring and the entry is only
        // mutated here while the controller is under the driver's lock.
        unsafe {
            write_volatile(addr_of_mut!(self.corb.0[next as usize]), command);
        }
        compiler_fence(Ordering::SeqCst);
        self.corb_wp = next;
        self.write16(CORBWP, next as u16);

        for _ in 0..COMMAND_TIMEOUT {
            let wp = self.read16(RIRBWP) as u8;
            if wp != self.rirb_rp {
                // SAFETY: `wp` is a valid index; the controller wrote the
                // response through DMA.
                let response = unsafe { read_volatile(addr_of!(self.rirb.0[wp as usize][0])) };
                self.rirb_rp = wp;
                return Some(response);
            }
            core::hint::spin_loop();
        }

        None
    }

    fn parameter(&mut self, node: u8, parameter: u16) -> Option<u32> {
        self.command(node, VERB_GET_PARAMETER, parameter)
    }

    /// Finds the audio function group, its output DAC and its output pin.
    fn discover(&mut self) -> Option<()> {
        let root = self.parameter(0, PARAM_NODE_COUNT)?;
        let function_count = root & 0xffff;
        let function_start = (root >> 16) & 0xffff;
        if function_count == 0 {
            return None;
        }

        let afg = function_start as u8;
        let nodes = self.parameter(afg, PARAM_NODE_COUNT)?;
        let count = (nodes & 0xffff) as usize;
        let start = ((nodes >> 16) & 0xffff) as usize;

        let mut dac = None;
        let mut pin = None;
        for node in start..start + count {
            let node = node as u8;
            let capabilities = self.parameter(node, PARAM_AUDIO_WIDGET_CAP)?;
            match (capabilities >> 20) & 0xf {
                WIDGET_AUDIO_OUTPUT if dac.is_none() => dac = Some(node),
                WIDGET_PIN_COMPLEX if pin.is_none() => {
                    let pin_capabilities = self.parameter(node, PARAM_PIN_CAP)?;
                    if pin_capabilities & PIN_CAP_OUTPUT != 0 {
                        pin = Some(node);
                    }
                }
                _ => {}
            }
        }

        self.dac = dac?;
        self.pin = pin?;
        Some(())
    }

    fn configure_codec(&mut self) -> Option<()> {
        // Wake the function group and the widgets along the output path.
        self.command(self.dac, VERB_SET_POWER_STATE, 0)?;
        self.command(self.pin, VERB_SET_POWER_STATE, 0)?;

        self.command(self.dac, VERB_SET_STREAM_FORMAT, FMT_48K_16_STEREO)?;
        self.command(
            self.dac,
            VERB_SET_CONVERTER_STREAM,
            ((STREAM_TAG as u16) << 4) | STREAM_CHANNEL,
        )?;
        self.command(self.dac, VERB_SET_AMP_GAIN_MUTE, AMP_UNMUTE_0DB)?;
        self.command(self.pin, VERB_SET_PIN_CONTROL, PIN_OUTPUT_ENABLE)?;
        Some(())
    }

    fn stream_base(&self) -> usize {
        self.mmio + 0x80 + SD_INDEX * 0x20
    }

    fn reset_stream(&mut self) {
        let base = self.stream_base();
        // SAFETY: the stream descriptor belongs to the controller's MMIO BAR.
        unsafe {
            write_volatile((base + SD_CTL) as *mut u32, SD_RESET);
        }
        for _ in 0..100_000 {
            // SAFETY: see above.
            if unsafe { read_volatile((base + SD_CTL) as *const u32) } & SD_RESET != 0 {
                break;
            }
        }
        // SAFETY: see above.
        unsafe {
            write_volatile((base + SD_CTL) as *mut u32, 0);
        }
        for _ in 0..100_000 {
            // SAFETY: see above.
            if unsafe { read_volatile((base + SD_CTL) as *const u32) } & SD_RESET == 0 {
                break;
            }
        }
        // SAFETY: see above.
        unsafe {
            write_volatile((base + SD_STS) as *mut u8, SD_INT_MASK);
        }
    }

    fn stop_stream(&mut self) {
        let base = self.stream_base();
        // SAFETY: the stream descriptor belongs to the controller's MMIO BAR.
        unsafe {
            write_volatile((base + SD_CTL) as *mut u32, 0);
            write_volatile((base + SD_STS) as *mut u8, SD_INT_MASK);
        }
    }

    fn init(&mut self, mmio: usize, clock: &'static dyn Clock) -> Option<CodecInfo> {
        self.mmio = mmio;
        self.clock = Some(clock);
        self.ready = false;

        self.reset_controller();

        let state = self.read16(STATESTS);
        if state == 0 {
            return None;
        }
        self.codec = state.trailing_zeros() as u8;
        self.write16(STATESTS, state);

        self.setup_rings();
        self.discover()?;
        self.configure_codec()?;
        self.reset_stream();

        let vendor = self.parameter(0, 0x00).unwrap_or(0);
        self.ready = true;
        Some(CodecInfo {
            vendor_id: vendor,
            codec: self.codec,
            dac: self.dac,
            pin: self.pin,
        })
    }

    fn play(&mut self, samples: &[i16]) -> Result<(), AudioError> {
        if !self.ready {
            return Err(AudioError::Unavailable);
        }
        if samples.is_empty() {
            return Err(AudioError::Empty);
        }
        if samples.len() > MAX_SAMPLES {
            return Err(AudioError::TooLong);
        }

        // Drop a trailing half-frame; the codec consumes whole stereo frames.
        let count = samples.len() & !(CHANNELS as usize - 1);
        if count == 0 {
            return Err(AudioError::Empty);
        }

        self.pcm.0[..count].copy_from_slice(&samples[..count]);
        compiler_fence(Ordering::SeqCst);

        let bytes = (count * 2) as u32;
        let address = addr_of!(self.pcm.0) as u64;
        let bdl_address = addr_of!(self.bdl.0) as u64;

        self.stop_stream();

        // SAFETY: the BDL is a static buffer owned by this driver; the
        // controller only reads it through DMA while the stream is running.
        unsafe {
            write_volatile(
                addr_of_mut!(self.bdl.0[0]),
                BdlEntry {
                    address,
                    length: bytes,
                    flags: 1, // interrupt on completion
                },
            );
        }
        compiler_fence(Ordering::SeqCst);

        let base = self.stream_base();
        // SAFETY: stream descriptor registers inside the controller BAR.
        unsafe {
            write_volatile((base + SD_BDLPL) as *mut u32, bdl_address as u32);
            write_volatile((base + SD_BDLPU) as *mut u32, (bdl_address >> 32) as u32);
            write_volatile((base + SD_CBL) as *mut u32, bytes);
            write_volatile((base + SD_LVI) as *mut u16, 0);
            write_volatile((base + SD_FMT) as *mut u16, FMT_48K_16_STEREO);
            write_volatile((base + SD_STS) as *mut u8, SD_INT_MASK);
            compiler_fence(Ordering::SeqCst);
            write_volatile((base + SD_CTL) as *mut u32, (STREAM_TAG << 20) | SD_RUN);
        }

        let Some(clock) = self.clock else {
            self.stop_stream();
            return Err(AudioError::Unavailable);
        };

        let duration_ms = (count as u64 * 1000) / (SAMPLE_RATE as u64 * CHANNELS as u64);
        let deadline = clock.uptime_ms().saturating_add(duration_ms + 2_000);

        while clock.uptime_ms() < deadline {
            // SAFETY: stream status register inside the controller BAR.
            if unsafe { read_volatile((base + SD_STS) as *const u8) } & SD_COMPLETE != 0 {
                // The codec buffers part of the stream internally; give it a
                // moment to drain the tail before stopping the DMA engine.
                clock.spin_ms(100);
                self.stop_stream();
                return Ok(());
            }
            clock.spin_ms(2);
        }

        self.stop_stream();
        Err(AudioError::Timeout)
    }
}

/// Information about the codec discovered at initialisation.
#[derive(Clone, Copy, Debug)]
pub struct CodecInfo {
    /// Codec vendor ID (QEMU reports `0x1af4xxxx`).
    pub vendor_id: u32,
    /// Codec address on the HDA link.
    pub codec: u8,
    /// Node ID of the output converter (DAC).
    pub dac: u8,
    /// Node ID of the output pin.
    pub pin: u8,
}

/// A polled Intel HDA controller.
///
/// `IntelHda` can be created in a `static` and shared through
/// `&'static dyn feos_hal::Audio`.
pub struct IntelHda {
    state: SpinLock<Controller>,
}

impl IntelHda {
    /// Creates an uninitialised controller with its DMA buffers in `.bss`.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(Controller::new()),
        }
    }

    /// Resets the controller and discovers the codec behind `mmio`.
    ///
    /// `mmio` is the memory BAR assigned to the PCI device. Returns `None`
    /// when no codec responds or the expected DAC/pin widgets are missing.
    pub fn init(&self, mmio: usize, clock: &'static dyn Clock) -> Option<CodecInfo> {
        self.state.lock().init(mmio, clock)
    }

    /// Stops playback and forgets the current stream.
    pub fn reset(&self) {
        let mut controller = self.state.lock();
        controller.stop_stream();
        controller.ready = false;
    }
}

impl Default for IntelHda {
    fn default() -> Self {
        Self::new()
    }
}

impl Audio for IntelHda {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn channels(&self) -> u16 {
        CHANNELS
    }

    fn is_ready(&self) -> bool {
        self.state.lock().ready
    }

    fn play(&self, samples: &[i16]) -> Result<(), AudioError> {
        self.state.lock().play(samples)
    }

    fn stop(&self) {
        self.state.lock().stop_stream();
    }

    fn max_samples(&self) -> usize {
        MAX_SAMPLES
    }
}
