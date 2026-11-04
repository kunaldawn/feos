//! Early boot: 32-bit multiboot entry and the jump into long mode.

use core::arch::global_asm;

global_asm!(include_str!("boot.S"), options(att_syntax));
