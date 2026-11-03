//! FEOS-DOS: an x86 virtual machine and DOS prompt for feos.
//!
//! FEOS-DOS is an original, dependency-free `no_std` implementation of the
//! parts of a PC that a DOS program expects:
//!
//! * an 8086/80186 interpreter ([`cpu`]) with the full integer instruction
//!   set, flags, segmentation, interrupts and `REP` string operations;
//! * 1 MiB of conventional memory, the interrupt vector table and the BIOS
//!   data area ([`memory`]);
//! * an 80x25 VGA text adapter with the CP437 character set ([`vga`],
//!   [`font`]);
//! * the BIOS services a DOS program uses: `INT 10h` video, `INT 16h`
//!   keyboard, `INT 1Ah` time and the usual equipment/memory calls ([`bios`]);
//! * a DOS kernel with a writable RAM drive, handles, a DTA, file search and
//!   the `INT 21h` calls a text-mode program needs ([`dos`]);
//! * a `COMMAND.COM`-style prompt ([`shell`]) with the familiar built-ins.
//!
//! The machine is driven by [`DosBox::frame`], which executes a cycle budget
//! and then draws the text screen into a [`feos_gfx::Canvas`]. Keyboard input
//! is queued with [`keyboard::Keyboard`], so the same core runs under the
//! desktop, the serial shell and host tests.
//!
//! # Scope
//!
//! This is a DOS *text-mode* machine. Graphics adapters (CGA/EGA/VGA modes),
//! the x87 FPU, protected mode, DOS extenders, sound cards and networking are
//! deliberately outside this crate's first version; [`cpu`] reports the
//! machine as an 8086 so programs can choose a compatible code path.
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
pub mod keyboard;
pub mod machine;
pub mod memory;
pub mod mouse;
pub mod opl;
pub mod opl3;
pub mod ramdisk;
pub mod shell;
pub mod vga;

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
