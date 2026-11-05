//! The Intel HDA controller.

use feos_driver_intel_hda::{CodecInfo, IntelHda};
use feos_driver_pci::{ConfigSpace, PciDevice};
use feos_hal::{Audio, Clock};

use crate::{wire_audio, IrqRouter};

/// The machine's Intel HDA controller.
///
/// Its command rings and PCM buffer live in this static. The controller is
/// initialised by [`bring_up`] and then shared with apps through `feos-hal`'s
/// [`Audio`] trait.
pub static AUDIO: IntelHda = IntelHda::new();

/// A discovered audio device and its codec details.
pub struct AudioDevice {
    /// Playback interface handed to apps.
    pub device: &'static dyn Audio,
    /// Codec nodes found during initialisation.
    pub codec: CodecInfo,
}

/// Initialises the controller at `function`, whose BAR is mapped at `bar`.
pub fn bring_up<C: ConfigSpace>(
    function: &PciDevice<C>,
    bar: usize,
    clock: &'static dyn Clock,
    router: &impl IrqRouter,
) -> Option<AudioDevice> {
    let codec = AUDIO.init(bar, clock)?;
    wire_audio(&AUDIO, function, router);
    Some(AudioDevice {
        device: &AUDIO,
        codec,
    })
}
