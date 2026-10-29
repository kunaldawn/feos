//! Early boot: the first instructions executed by the CPU.

use core::arch::global_asm;

global_asm!(include_str!("boot.S"));
