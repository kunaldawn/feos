//! The USB host controller, as far as a board needs to know: finding it, wiring its
//! interrupt, and what it adds to the machine's input devices and disks.

use alloc::boxed::Box;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use feos_console::println;
use feos_driver_pci::{for_each_function, BarAllocator, ConfigSpace, PciDevice};
use feos_driver_xhci::Xhci;
use feos_hal::{BlockIo, Clock, Keyboard, KeyboardEvent, Pointer, PointerEvent, UsbStats};
use feos_sync::SpinLock;

use crate::IrqRouter;

/// PCI class, subclass and programming interface of an xHCI controller.
const CLASS_XHCI: u32 = 0x0c_03_30;

static CONTROLLER: SpinLock<Option<&'static Xhci>> = SpinLock::new(None);

fn controller() -> Option<&'static Xhci> {
    *CONTROLLER.lock()
}

fn usb_interrupt(data: usize) {
    // SAFETY: the data registered with this handler is the leaked controller.
    let controller = unsafe { &*(data as *const Xhci) };
    controller.handle_interrupt();
}

/// Finds the first xHCI controller on the bus and brings it up. Everything's optional:
/// a machine without one carries on regardless.
pub fn init<C: ConfigSpace + Copy>(
    config: C,
    allocator: &mut BarAllocator,
    router: &impl IrqRouter,
    clock: &'static dyn Clock,
    (width, height): (u16, u16),
) {
    let mut found: Option<PciDevice<C>> = None;
    for_each_function(config, |function| {
        if found.is_none() && function.class_code() == CLASS_XHCI {
            found = Some(function);
        }
    });
    let Some(function) = found else {
        return;
    };
    let Some(base) = function.ensure_memory_bar(0, allocator) else {
        println!("[usb] xHCI: no address for its registers");
        return;
    };
    let msix = function.msix_table(allocator);
    function.enable();
    let mut log = Vec::new();
    let controller = Xhci::init(base as usize, clock, width, height, &mut log);
    for line in log {
        println!("{line}");
    }
    let Some(controller) = controller else {
        return;
    };
    let how = wire(controller, &function, msix, router);
    println!("[usb] xHCI ready: {how}");
    *CONTROLLER.lock() = Some(controller);
}

/// The best interrupt the function and the machine can agree on.
fn wire<C: ConfigSpace>(
    controller: &'static Xhci,
    function: &PciDevice<C>,
    msix: Option<usize>,
    router: &impl IrqRouter,
) -> &'static str {
    let data = controller as *const Xhci as usize;
    if let (Some(table), Some((message, id))) = (msix, router.alloc_msi()) {
        if function.enable_msix(table, 0, message) && feos_irq::register(id, "xhci msi-x", usb_interrupt, data) {
            controller.use_interrupts();
            return "MSI-X";
        }
        function.disable_msix();
    }
    if function.has_msi() {
        if let Some((message, id)) = router.alloc_msi() {
            if feos_irq::register(id, "xhci msi", usb_interrupt, data) && function.enable_msi(message) {
                controller.use_interrupts();
                return "MSI";
            }
        }
    }
    if let Some(pin) = function.interrupt_pin() {
        if let Some(id) = router.pci_intx(function.bus(), function.slot(), pin) {
            function.enable_intx();
            if feos_irq::register(id, "xhci intx", usb_interrupt, data) {
                controller.use_interrupts();
                return "INTx";
            }
        }
    }
    "polled"
}

/// The disks found, once.
pub fn take_disks() -> Vec<Box<dyn BlockIo>> {
    Xhci::take_disks()
        .into_iter()
        .map(|disk| Box::new(disk) as Box<dyn BlockIo>)
        .collect()
}

/// The controller's counters.
pub fn stats() -> Option<UsbStats> {
    controller().map(Xhci::stats)
}

/// The USB keyboard and whatever the machine has of its own.
struct JoinedKeyboard;

static OTHER_KEYBOARD: SpinLock<Option<&'static dyn Keyboard>> = SpinLock::new(None);
static JOINED_KEYBOARD: JoinedKeyboard = JoinedKeyboard;

impl Keyboard for JoinedKeyboard {
    fn poll(&self) -> Option<KeyboardEvent> {
        let other = *OTHER_KEYBOARD.lock();
        other
            .and_then(Keyboard::poll)
            .or_else(|| controller().and_then(Xhci::poll_key))
    }
}

/// The keyboard to give apps: `other`, with a USB keyboard added when there is one.
pub fn keyboard(other: Option<&'static dyn Keyboard>) -> Option<&'static dyn Keyboard> {
    if controller().is_some_and(Xhci::has_keyboard) {
        *OTHER_KEYBOARD.lock() = other;
        Some(&JOINED_KEYBOARD)
    } else {
        other
    }
}

/// The USB pointer and whatever the machine has of its own.
struct JoinedPointer;

static OTHER_POINTER: SpinLock<Option<&'static dyn Pointer>> = SpinLock::new(None);
static JOINED_POINTER: JoinedPointer = JoinedPointer;
/// Whose position is the cursor's: whichever moved last.
static USB_POINTER_LAST: AtomicBool = AtomicBool::new(true);

impl Pointer for JoinedPointer {
    fn poll(&self) -> Option<PointerEvent> {
        if let Some(event) = controller().and_then(Xhci::poll_pointer) {
            USB_POINTER_LAST.store(true, Ordering::Relaxed);
            return Some(event);
        }
        let other = *OTHER_POINTER.lock();
        let event = other.and_then(Pointer::poll)?;
        USB_POINTER_LAST.store(false, Ordering::Relaxed);
        Some(event)
    }

    fn position(&self) -> (u16, u16) {
        let other = *OTHER_POINTER.lock();
        match (other, USB_POINTER_LAST.load(Ordering::Relaxed)) {
            (Some(other), false) => other.position(),
            _ => controller().map_or((0, 0), Xhci::pointer_position),
        }
    }
}

/// The pointer to give apps: `other`, with a USB pointer added when there is one.
pub fn pointer(other: Option<&'static dyn Pointer>) -> Option<&'static dyn Pointer> {
    if controller().is_some_and(Xhci::has_pointer) {
        *OTHER_POINTER.lock() = other;
        Some(&JOINED_POINTER)
    } else {
        other
    }
}
