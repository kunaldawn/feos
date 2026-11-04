//! Intel High Definition Audio (HDA) driver.
//!
//! QEMU exposes an ICH6-compatible HDA controller as `-device intel-hda` with
//! an `hda-output` codec (playback only). The controller is a PCI device with a
//! memory BAR; this driver programs the controller's corb/rirb command rings,
//! discovers the codec's DAC and output pin, and streams interleaved 16-bit PCM
//! through one output stream descriptor.
//!
//! # Streaming
//!
//! DMA runs cyclically over a ring described by a multi-entry buffer descriptor
//! list. [`Audio::write`] copies samples in behind what is queued and never
//! blocks; [`Audio::play`] is built on it. The driver is polled and
//! interrupt-free, so the ring has to look after itself between calls:
//!
//! - Every poll reads the link position and zeroes what the hardware has just
//!   played. If the writer falls behind or stops, DMA runs on into silence
//!   rather than replaying old samples.
//! - Once the queue has played out, [`Audio::service`] stops the stream. The
//!   kernel calls it from the timer interrupt, so this happens even if the app
//!   has gone.
//!
//! The lock is only held to copy samples or touch registers, never across a
//! wait, so [`Audio::stop`] and [`Audio::is_ready`] answer at once and a killed
//! process can't leave the device wedged.
//!
//! # Memory
//!
//! The command rings, buffer descriptor list and PCM ring live inside the
//! driver's static state, so [`IntelHda::new`] can be used in a `static` with
//! no allocator. [`IntelHda`] owns its buffers and is meant to be a singleton.
//!
//! # Addresses
//!
//! DMA addresses are the buffers' virtual addresses. Those are identical to
//! physical addresses here, because the kernel identity-maps memory. A future
//! port that doesn't must translate these before handing them to the device.

#![cfg_attr(not(test), no_std)]

use core::mem::MaybeUninit;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use feos_hal::{dma, Audio, AudioError, Clock};
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
/// CORBSIZE and RIRBSIZE encoding for 256 entries.
const RING_SIZE_256: u8 = 0x02;

/// Entries in the buffer descriptor list. The specification requires at least
/// two; these are all in one ring, so the count only sets the fetch granularity.
const BDL_ENTRIES: usize = 32;
/// Bytes per entry: whole stereo frames.
const BDL_ENTRY_BYTES: usize = 1536 * FRAME_BYTES;
/// Bytes per stereo frame.
const FRAME_BYTES: usize = CHANNELS as usize * 2;
/// Size of the cyclic PCM ring, about one second.
const RING_BYTES: usize = BDL_ENTRIES * BDL_ENTRY_BYTES;
const RING_SAMPLES: usize = RING_BYTES / 2;
/// Silence to let through after the queue empties before stopping the stream.
/// The codec buffers part of the stream, so stopping sooner would cut the tail.
const DRAIN_BYTES: u64 = (SAMPLE_RATE as u64 / 10) * FRAME_BYTES as u64;
/// How far ahead of the hardware to resume after an underrun.
const RESUME_LEAD_BYTES: u64 = 1024 * FRAME_BYTES as u64;
/// Longest wait between two checks while [`Audio::play`] blocks.
const PLAY_POLL_MS: u64 = 20;

// Controller registers (ICH6 layout).
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0e;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4a;
const CORBCTL: usize = 0x4c;
const CORBSIZE: usize = 0x4e;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5a;
const RIRBCTL: usize = 0x5c;
const RIRBSTS: usize = 0x5d;
const RIRBSIZE: usize = 0x5e;

// Per-stream registers, relative to the stream descriptor base.
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0c;
const SD_FMT: usize = 0x12;
const SD_BDLPL: usize = 0x18;
const SD_BDLPU: usize = 0x1c;

/// Output stream descriptors start at 0x80; 0..3 are inputs, 4..7 outputs.
const SD_INDEX: usize = 4;

/// `GCTL.CRST`: 0 holds the controller in reset, 1 runs it.
const GCTL_RUN: u32 = 1 << 0;
const CORB_RUN: u8 = 1 << 1;
const CORBRP_RESET: u16 = 1 << 15;
const RIRB_RUN: u8 = 1 << 1;
const RIRBWP_RESET: u16 = 1 << 15;
const RIRBSTS_CLEAR: u8 = 0x05;
const SD_RUN: u32 = 1 << 1;
const SD_RESET: u32 = 1 << 0;
const SD_INT_MASK: u8 = 0x1c;

/// Output amp, left and right, not muted. The low bits are the gain.
const AMP_SET_OUTPUT: u16 = 0xb000;
/// Gain to fall back on when the codec reports no amp capabilities.
const AMP_GAIN_MAX: u16 = 0x7f;
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
const PARAM_OUT_AMP_CAP: u16 = 0x12;

const WIDGET_AUDIO_OUTPUT: u32 = 0;
const WIDGET_PIN_COMPLEX: u32 = 4;
const PIN_CAP_OUTPUT: u32 = 1 << 4;

const COMMAND_TIMEOUT: usize = 1_000_000;
/// Polls of a register that the controller should change promptly.
const REGISTER_POLLS: usize = 100_000;

/// The output amp setting for 0 dB, from an amp capability value.
///
/// Bits 6:0 of the capability are the gain step that is 0 dB. Unity gain matters
/// because any higher step distorts, and any lower step reduces the level.
fn amp_payload(caps: u32) -> u16 {
    if caps == 0 {
        AMP_SET_OUTPUT | AMP_GAIN_MAX
    } else {
        AMP_SET_OUTPUT | (caps & 0x7f) as u16
    }
}

/// Where the stream is, in bytes since it started.
///
/// This is the host-testable core of the driver. All three counters only grow;
/// `offset` maps one to its place in the ring.
#[derive(Clone, Copy)]
struct Ring {
    /// Bytes the hardware has consumed.
    read: u64,
    /// Bytes queued so far. Below `read` after an underrun.
    write: u64,
    /// Everything below this has been zeroed in the ring.
    cleared: u64,
    /// Position register value when the stream started. See [`Ring::observe`].
    start_position: u32,
    /// Latest position register value.
    last_position: u32,
    /// Latest position in the ring.
    last_offset: u32,
}

impl Ring {
    const fn new(start_position: u32) -> Self {
        Self {
            read: 0,
            write: 0,
            cleared: 0,
            start_position,
            last_position: start_position,
            last_offset: 0,
        }
    }

    /// Takes in a reading of the link position register.
    ///
    /// A controller is meant to clear the register on stream reset, but QEMU
    /// doesn't. It then counts on from the old value until the ring wraps, after
    /// which it counts from zero. `start_position` removes that offset for the
    /// first lap. Wrap detection needs a reading at least once per ring, which
    /// the kernel's timer tick provides.
    fn observe(&mut self, position: u32) {
        if self.start_position != 0 && position < self.last_position {
            self.start_position = 0;
        }
        let offset = position.saturating_sub(self.start_position) % RING_BYTES as u32;
        let delta = (offset + RING_BYTES as u32 - self.last_offset) % RING_BYTES as u32;
        self.read += delta as u64;
        self.last_position = position;
        self.last_offset = offset;
    }

    /// Bytes queued and not yet consumed.
    const fn queued(&self) -> u64 {
        self.write.saturating_sub(self.read)
    }

    /// Bytes that can be queued now.
    const fn free(&self) -> u64 {
        RING_BYTES as u64 - self.queued()
    }

    /// Has the hardware played through the queue and the drain margin?
    const fn drained(&self) -> bool {
        self.read >= self.write + DRAIN_BYTES
    }

    /// Moves the write position ahead of the hardware after an underrun.
    ///
    /// Returns `true` if it moved. Samples written at the hardware's position
    /// would be played partly or not at all.
    fn resume_after_underrun(&mut self) -> bool {
        if self.write >= self.read {
            return false;
        }
        self.write = self.read + RESUME_LEAD_BYTES;
        true
    }

    /// Byte position in the ring.
    const fn offset(position: u64) -> usize {
        (position % RING_BYTES as u64) as usize
    }

    /// Splits `[start, start + length)` at the end of the ring.
    fn spans(start: u64, length: usize) -> [(usize, usize); 2] {
        let offset = Self::offset(start);
        let first = length.min(RING_BYTES - offset);
        [(offset, first), (0, length - first)]
    }
}

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
struct Bdl([BdlEntry; BDL_ENTRIES]);

#[repr(align(128))]
struct Pcm([i16; RING_SAMPLES]);

struct Controller {
    mmio: usize,
    clock: Option<&'static dyn Clock>,
    ready: bool,
    codec: u8,
    afg: u8,
    dac: u8,
    pin: u8,
    corb_wp: u8,
    rirb_rp: u8,
    /// The stream is running, and `ring` describes it.
    running: bool,
    /// Counts stream stops, so a blocked `play` can tell its stream has ended.
    generation: u32,
    ring: Ring,
    corb: Corb,
    rirb: Rirb,
    bdl: Bdl,
    pcm: Pcm,
}

impl Controller {
    /// All-zero state, which is valid for every field. Building it this way
    /// keeps the driver's buffers in `.bss` rather than in the image.
    const fn new() -> Self {
        // SAFETY: integers, `false`, arrays of those, and `None` for the
        // reference are all valid as zero bytes.
        unsafe { MaybeUninit::zeroed().assume_init() }
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

    /// Waits until `(register & mask) == expected`, within a bound.
    fn wait32(&self, offset: usize, mask: u32, expected: u32) {
        for _ in 0..REGISTER_POLLS {
            if self.read32(offset) & mask == expected {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Puts the controller into reset and back out again.
    fn reset_controller(&self, clock: &dyn Clock) {
        self.write8(CORBCTL, 0);
        self.write8(RIRBCTL, 0);

        self.write32(GCTL, self.read32(GCTL) & !GCTL_RUN);
        self.wait32(GCTL, GCTL_RUN, 0);

        self.write32(GCTL, self.read32(GCTL) | GCTL_RUN);
        self.wait32(GCTL, GCTL_RUN, GCTL_RUN);

        // Codecs have 521 us after reset to announce themselves in STATESTS.
        clock.spin_ms(1);
    }

    /// Points the controller's command/response rings at the static buffers.
    fn setup_rings(&mut self) {
        self.corb_wp = 0;
        self.rirb_rp = 0;

        let corb = addr_of!(self.corb.0) as u64;
        let rirb = addr_of!(self.rirb.0) as u64;

        self.write8(CORBCTL, 0);
        self.write32(CORBLBASE, corb as u32);
        self.write32(CORBUBASE, (corb >> 32) as u32);
        self.write8(CORBSIZE, RING_SIZE_256);
        self.write16(CORBWP, 0);
        self.write16(CORBRP, CORBRP_RESET);
        for _ in 0..REGISTER_POLLS {
            if self.read16(CORBRP) & CORBRP_RESET != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write16(CORBRP, 0);
        for _ in 0..REGISTER_POLLS {
            if self.read16(CORBRP) & CORBRP_RESET == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        self.write8(RIRBCTL, 0);
        self.write32(RIRBLBASE, rirb as u32);
        self.write32(RIRBUBASE, (rirb >> 32) as u32);
        self.write8(RIRBSIZE, RING_SIZE_256);
        self.write16(RIRBWP, RIRBWP_RESET);
        // Keep the response ring armed across many commands; responses are
        // polled, so interrupts are never enabled.
        self.write16(RINTCNT, 0xff);

        dma::wmb();
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
        dma::wmb();
        self.corb_wp = next;
        self.write16(CORBWP, next as u16);

        for _ in 0..COMMAND_TIMEOUT {
            let wp = self.read16(RIRBWP) as u8;
            if wp != self.rirb_rp {
                dma::rmb();
                // SAFETY: `wp` is a valid index; the controller wrote the
                // response through DMA.
                let response = unsafe { read_volatile(addr_of!(self.rirb.0[wp as usize][0])) };
                self.rirb_rp = wp;
                self.write8(RIRBSTS, RIRBSTS_CLEAR);
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

        self.afg = afg;
        self.dac = dac?;
        self.pin = pin?;
        Some(())
    }

    fn configure_codec(&mut self) -> Option<()> {
        // Wake the function group and the widgets along the output path.
        self.command(self.afg, VERB_SET_POWER_STATE, 0)?;
        self.command(self.dac, VERB_SET_POWER_STATE, 0)?;
        self.command(self.pin, VERB_SET_POWER_STATE, 0)?;

        self.command(self.dac, VERB_SET_STREAM_FORMAT, FMT_48K_16_STEREO)?;
        self.command(
            self.dac,
            VERB_SET_CONVERTER_STREAM,
            ((STREAM_TAG as u16) << 4) | STREAM_CHANNEL,
        )?;
        // A widget without its own amp capabilities inherits the group's.
        let mut amp = self.parameter(self.dac, PARAM_OUT_AMP_CAP)?;
        if amp == 0 {
            amp = self.parameter(self.afg, PARAM_OUT_AMP_CAP)?;
        }
        self.command(self.dac, VERB_SET_AMP_GAIN_MUTE, amp_payload(amp))?;
        self.command(self.pin, VERB_SET_PIN_CONTROL, PIN_OUTPUT_ENABLE)?;
        Some(())
    }

    fn stream_base(&self) -> usize {
        self.mmio + 0x80 + SD_INDEX * 0x20
    }

    fn stream_read8_clear(&self) {
        let base = self.stream_base();
        // SAFETY: stream status register inside the controller BAR.
        unsafe { write_volatile((base + SD_STS) as *mut u8, SD_INT_MASK) };
    }

    fn stream_control(&self) -> u32 {
        // SAFETY: stream control register inside the controller BAR.
        unsafe { read_volatile((self.stream_base() + SD_CTL) as *const u32) }
    }

    fn set_stream_control(&self, value: u32) {
        // SAFETY: stream control register inside the controller BAR.
        unsafe { write_volatile((self.stream_base() + SD_CTL) as *mut u32, value) }
    }

    fn reset_stream(&mut self) {
        self.set_stream_control(0);
        for _ in 0..REGISTER_POLLS {
            if self.stream_control() & SD_RUN == 0 {
                break;
            }
        }
        self.set_stream_control(SD_RESET);
        for _ in 0..REGISTER_POLLS {
            if self.stream_control() & SD_RESET != 0 {
                break;
            }
        }
        self.set_stream_control(0);
        for _ in 0..REGISTER_POLLS {
            if self.stream_control() & SD_RESET == 0 {
                break;
            }
        }
        self.stream_read8_clear();
    }

    /// Reads the position in the ring and zeroes what has been played.
    fn poll(&mut self) {
        if !self.running {
            return;
        }
        // SAFETY: stream position register inside the controller BAR.
        let position = unsafe { read_volatile((self.stream_base() + SD_LPIB) as *const u32) };
        self.ring.observe(position);
        self.clear_up_to(self.ring.read.min(self.ring.write.max(self.ring.cleared)));
    }

    /// Zeroes the ring between `ring.cleared` and `target`.
    fn clear_up_to(&mut self, target: u64) {
        if target <= self.ring.cleared {
            return;
        }
        let length = (target - self.ring.cleared).min(RING_BYTES as u64) as usize;
        for (offset, bytes) in Ring::spans(self.ring.cleared, length) {
            self.pcm.0[offset / 2..(offset + bytes) / 2].fill(0);
        }
        self.ring.cleared = target;
    }

    /// Stops the stream and leaves the ring silent for the next start.
    fn stop_stream(&mut self) {
        if !self.running {
            return;
        }
        self.reset_stream();
        self.running = false;
        self.generation = self.generation.wrapping_add(1);
        self.clear_up_to(self.ring.write);
        self.ring = Ring::new(0);
    }

    /// Starts DMA over the ring, which must already hold the first samples.
    fn start_stream(&mut self) {
        self.reset_stream();

        // SAFETY: stream position register inside the controller BAR.
        let start = unsafe { read_volatile((self.stream_base() + SD_LPIB) as *const u32) };
        let (read, write, cleared) = (self.ring.read, self.ring.write, self.ring.cleared);
        self.ring = Ring::new(start);
        self.ring.read = read;
        self.ring.write = write;
        self.ring.cleared = cleared;

        let bdl_address = addr_of!(self.bdl.0) as u64;
        let base = self.stream_base();
        dma::wmb();
        // SAFETY: stream descriptor registers inside the controller BAR.
        unsafe {
            write_volatile((base + SD_BDLPL) as *mut u32, bdl_address as u32);
            write_volatile((base + SD_BDLPU) as *mut u32, (bdl_address >> 32) as u32);
            // On a conforming controller `start` is zero. See `Ring::observe`:
            // QEMU clamps each transfer to CBL minus the position, so this keeps
            // the first lap from ending early.
            write_volatile((base + SD_CBL) as *mut u32, RING_BYTES as u32 + start);
            write_volatile((base + SD_LVI) as *mut u16, BDL_ENTRIES as u16 - 1);
            write_volatile((base + SD_FMT) as *mut u16, FMT_48K_16_STEREO);
        }
        self.stream_read8_clear();
        dma::wmb();
        self.set_stream_control((STREAM_TAG << 20) | SD_RUN);
        self.running = true;
    }

    /// Describes the whole ring to the controller. Entries raise no interrupt:
    /// QEMU also pauses DMA until such an interrupt is acknowledged.
    fn setup_bdl(&mut self) {
        let start = addr_of!(self.pcm.0) as u64;
        for index in 0..BDL_ENTRIES {
            let entry = BdlEntry {
                address: start + (index * BDL_ENTRY_BYTES) as u64,
                length: BDL_ENTRY_BYTES as u32,
                flags: 0,
            };
            // SAFETY: the BDL is a static buffer owned by this driver; the
            // controller only reads it through DMA while the stream is running.
            unsafe { write_volatile(addr_of_mut!(self.bdl.0[index]), entry) };
        }
    }

    fn init(&mut self, mmio: usize, clock: &'static dyn Clock) -> Option<CodecInfo> {
        self.mmio = mmio;
        self.clock = Some(clock);
        self.ready = false;

        self.reset_controller(clock);

        let state = self.read16(STATESTS);
        if state == 0 {
            return None;
        }
        self.codec = state.trailing_zeros() as u8;
        self.write16(STATESTS, state);

        self.setup_rings();
        self.discover()?;
        self.configure_codec()?;
        self.setup_bdl();
        self.reset_stream();
        self.running = false;
        self.ring = Ring::new(0);

        let vendor = self.parameter(0, 0x00).unwrap_or(0);
        self.ready = true;
        Some(CodecInfo {
            vendor_id: vendor,
            codec: self.codec,
            dac: self.dac,
            pin: self.pin,
        })
    }

    /// Queues as many whole frames as fit and returns how many samples that was.
    fn write(&mut self, samples: &[i16]) -> usize {
        if !self.ready {
            return 0;
        }
        self.poll();

        if self.ring.resume_after_underrun() {
            // The skipped span is below `cleared` or already silent.
            self.clear_up_to(self.ring.read);
        }

        let room = self.ring.free() as usize;
        let bytes = (samples.len() * 2).min(room) / FRAME_BYTES * FRAME_BYTES;
        if bytes == 0 {
            return 0;
        }

        let mut source = samples;
        for (offset, length) in Ring::spans(self.ring.write, bytes) {
            let count = length / 2;
            self.pcm.0[offset / 2..offset / 2 + count].copy_from_slice(&source[..count]);
            source = &source[count..];
        }
        dma::wmb();
        self.ring.write += bytes as u64;

        if !self.running {
            self.start_stream();
        }
        bytes / 2
    }

    /// Periodic housekeeping: tracks the ring, and stops a stream that's done.
    fn service(&mut self) {
        self.poll();
        if self.running && self.ring.drained() {
            self.stop_stream();
        }
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
    /// Mirrors `Controller::ready`, so querying it can't wait.
    ready: AtomicBool,
    /// A stream is running, so the timer tick has something to look after.
    active: AtomicBool,
}

impl IntelHda {
    /// Creates an uninitialised controller with its DMA buffers in `.bss`.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(Controller::new()),
            ready: AtomicBool::new(false),
            active: AtomicBool::new(false),
        }
    }

    /// Resets the controller and discovers the codec behind `mmio`.
    ///
    /// `mmio` is the memory BAR assigned to the PCI device. Returns `None`
    /// when no codec responds or the expected DAC/pin widgets are missing.
    pub fn init(&self, mmio: usize, clock: &'static dyn Clock) -> Option<CodecInfo> {
        let codec = self.state.lock().init(mmio, clock);
        self.ready.store(codec.is_some(), Ordering::Release);
        codec
    }

    /// Stops playback and forgets the current stream.
    pub fn reset(&self) {
        let mut controller = self.state.lock();
        controller.stop_stream();
        controller.ready = false;
        self.ready.store(false, Ordering::Release);
        self.active.store(false, Ordering::Release);
    }

    /// Queues samples and returns the target position for [`play`](Audio::play).
    fn write_tracked(&self, samples: &[i16]) -> (usize, u64, u32) {
        let mut controller = self.state.lock();
        let accepted = controller.write(samples);
        if controller.running {
            self.active.store(true, Ordering::Release);
        }
        (accepted, controller.ring.write, controller.generation)
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
        self.ready.load(Ordering::Acquire)
    }

    fn play(&self, samples: &[i16]) -> Result<(), AudioError> {
        if !self.is_ready() {
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

        let Some(clock) = self.state.lock().clock else {
            return Err(AudioError::Unavailable);
        };
        let duration_ms = (count as u64 * 1000) / (SAMPLE_RATE as u64 * CHANNELS as u64);
        let deadline = clock.uptime_ms().saturating_add(duration_ms + 2_000);

        // No lock is held while waiting, so a process killed here leaves nothing
        // behind: the stream plays out what's queued and `service` stops it.
        let mut sent = 0;
        let (target, generation) = loop {
            let (accepted, target, generation) = self.write_tracked(&samples[sent..count]);
            sent += accepted;
            if sent == count {
                break (target, generation);
            }
            if clock.uptime_ms() >= deadline {
                return Err(AudioError::Timeout);
            }
            clock.idle_ms(PLAY_POLL_MS);
        };

        loop {
            {
                let mut controller = self.state.lock();
                controller.poll();
                // `stop` or the end of the stream both mean nothing is left.
                if controller.generation != generation || controller.ring.read >= target {
                    return Ok(());
                }
            }
            if clock.uptime_ms() >= deadline {
                self.stop();
                return Err(AudioError::Timeout);
            }
            clock.idle_ms(PLAY_POLL_MS);
        }
    }

    fn stop(&self) {
        self.state.lock().stop_stream();
        self.active.store(false, Ordering::Release);
    }

    fn max_samples(&self) -> usize {
        MAX_SAMPLES
    }

    fn write(&self, samples: &[i16]) -> usize {
        self.write_tracked(samples).0
    }

    fn available(&self) -> usize {
        let mut controller = self.state.lock();
        if !controller.ready {
            return 0;
        }
        controller.poll();
        controller.ring.free() as usize / 2
    }

    fn queued(&self) -> usize {
        let mut controller = self.state.lock();
        controller.poll();
        controller.ring.queued() as usize / 2
    }

    fn service(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        // From the timer interrupt: if a task holds the lock, its own call polls
        // the stream anyway, and the next tick can look again.
        if let Some(mut controller) = self.state.try_lock() {
            controller.service();
            if !controller.running {
                self.active.store(false, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RING: u32 = RING_BYTES as u32;

    #[test]
    fn the_bdl_has_at_least_two_entries_and_covers_whole_frames() {
        // DRV-7: the specification requires two or more.
        assert!(BDL_ENTRIES >= 2);
        assert_eq!(BDL_ENTRY_BYTES % FRAME_BYTES, 0);
        assert_eq!(core::mem::size_of::<BdlEntry>(), 16);
        assert_eq!(core::mem::align_of::<Bdl>(), 128);
    }

    #[test]
    fn amp_gain_is_the_codecs_zero_db_step() {
        // DRV-14: 0x7f is well past 0 dB on QEMU, whose caps give 0x4a.
        assert_eq!(amp_payload(0x0005_4a4a), 0xb04a);
        assert_eq!(amp_payload(0), 0xb07f);
    }

    #[test]
    fn position_accumulates_across_wraps() {
        let mut ring = Ring::new(0);
        ring.write = 10 * RING as u64;
        ring.observe(1000);
        assert_eq!(ring.read, 1000);
        ring.observe(RING - 100);
        assert_eq!(ring.read, RING as u64 - 100);
        ring.observe(100);
        assert_eq!(ring.read, RING as u64 + 100);
    }

    #[test]
    fn a_position_register_that_was_not_reset_is_corrected() {
        // QEMU keeps the old value over a stream reset, then wraps to zero.
        let start = 5000;
        let mut ring = Ring::new(start);
        ring.write = 10 * RING as u64;
        ring.observe(start);
        assert_eq!(ring.read, 0);
        ring.observe(start + 1000);
        assert_eq!(ring.read, 1000);
        ring.observe(start + RING - 8);
        assert_eq!(ring.read, RING as u64 - 8);
        // The wrap: the hardware reached the end of the ring.
        ring.observe(8);
        assert_eq!(ring.read, RING as u64 + 8);
        ring.observe(start + 8);
        assert_eq!(ring.read, RING as u64 + start as u64 + 8);
    }

    #[test]
    fn free_space_follows_the_hardware() {
        let mut ring = Ring::new(0);
        assert_eq!(ring.free(), RING as u64);
        ring.write = RING as u64;
        assert_eq!(ring.free(), 0);
        assert_eq!(ring.queued(), RING as u64);
        ring.observe(4096);
        assert_eq!(ring.free(), 4096);
    }

    #[test]
    fn a_stream_counts_as_drained_only_after_the_margin() {
        let mut ring = Ring::new(0);
        ring.write = 4096;
        ring.observe(4096);
        assert_eq!(ring.queued(), 0);
        assert!(!ring.drained());
        ring.observe(4096 + DRAIN_BYTES as u32);
        assert!(ring.drained());
    }

    #[test]
    fn writing_after_an_underrun_lands_ahead_of_the_hardware() {
        let mut ring = Ring::new(0);
        ring.write = 4096;
        assert!(!ring.resume_after_underrun());
        ring.observe(8192);
        assert!(ring.resume_after_underrun());
        assert_eq!(ring.write, 8192 + RESUME_LEAD_BYTES);
        assert!(!ring.resume_after_underrun());
    }

    #[test]
    fn spans_split_at_the_end_of_the_ring() {
        assert_eq!(Ring::spans(0, 100), [(0, 100), (0, 0)]);
        assert_eq!(
            Ring::spans(RING as u64 - 40, 100),
            [(RING_BYTES - 40, 40), (0, 60)]
        );
        assert_eq!(Ring::spans(RING as u64 + 8, 100), [(8, 100), (0, 0)]);
        assert_eq!(Ring::spans(0, RING_BYTES), [(0, RING_BYTES), (0, 0)]);
    }
}
