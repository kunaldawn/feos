//! FEOS-DOS: an x86 virtual machine and DOS for feos.
//!
//! The crate implements the parts of a PC that DOS programs expect:
//!
//! * an 80386 interpreter ([`cpu`]) with real and protected mode, paging, task
//!   switches and an x87 ([`fpu`]);
//! * RAM with a real A20 gate ([`memory`]), 8259 PICs, an 8254 PIT, the keyboard
//!   controller and 8237 DMA ([`hw`], [`keyboard`]);
//! * a VGA adapter with its register core, and text, CGA, EGA and VGA modes
//!   ([`vga`], [`int10`], [`font`]);
//! * a ROM whose interrupt vectors enter the Rust BIOS through callbacks, so
//!   programs can hook and chain them ([`bios`]);
//! * a DOS kernel with an MCB arena, PSPs, handles, FCBs, `EXEC` and a writable
//!   RAM drive ([`dos`], [`ramdisk`]), plus XMS and EMS ([`xms`]);
//! * a `COMMAND.COM`-style prompt with batch files ([`shell`]);
//! * OPL3 FM, a PC speaker and a Sound Blaster 2.0 DSP ([`opl`], [`sb`]), and a
//!   mouse driver ([`mouse`]).
//!
//! The machine is driven by [`DosBox::frame_paced`] or [`DosBox::frame`], which
//! execute a budget of guest work and return, so the same core runs under the
//! desktop, the serial shell and host tests. See `docs/dosbox.md` for what works
//! and how far each game in the library gets.
//!
//! ```no_run
//! use feos_dosbox::DosBox;
//!
//! let mut dos = DosBox::new(None);
//! dos.keyboard().push_ascii(b'd', 0x20);
//! dos.keyboard().push_ascii(b'i', 0x17);
//! dos.keyboard().push_ascii(b'r', 0x13);
//! dos.keyboard().push_ascii(b'\r', 0x1C);
//! dos.frame(0, 100_000);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bios;
pub mod cpu;
pub mod demo;
pub mod dos;
pub mod f80;
pub mod font;
pub mod fpu;
pub mod hw;
pub mod int10;
pub mod keyboard;
pub mod machine;
pub mod memory;
pub mod mouse;
pub mod opl;
pub mod opl3;
pub mod ramdisk;
pub mod sb;
pub mod shell;
pub mod vga;
pub mod xms;

pub use cpu::Cpu;
pub use dos::Dos;
pub use keyboard::Keyboard;
pub use machine::{DosBox, SystemBus};
pub use memory::Memory;
pub use ramdisk::RamDisk;
pub use shell::Shell;
pub use vga::Vga;

/// Product name shown by the prompt, `VER` and the desktop entry.
pub const PRODUCT: &str = "FEOS-DOS";
/// Version shown by `VER`.
pub const VERSION: &str = "1.0";
/// The DOS version reported by `INT 21h` function 30h.
pub const DOS_VERSION: u8 = 5;
