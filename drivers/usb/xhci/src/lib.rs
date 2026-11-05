//! An xHCI USB host controller, and what's plugged straight into its root ports:
//! HID keyboards and pointers, and Bulk-Only mass storage.
//!
//! [`Xhci::init`] resets the controller, enumerates every port that has something
//! connected, and leaves each device it understands configured. After that:
//!
//! - HID interrupt endpoints each have one transfer outstanding. Reports are decoded
//!   when someone polls [`Xhci::poll_key`] or [`Xhci::poll_pointer`], because that's
//!   how every input device on this system works.
//! - A [`UsbDisk`] is an ordinary [`feos_hal::BlockIo`], and waits for its transfers on
//!   the controller's interrupt, or polls when interrupts are masked early in boot.
//!
//! Limits: no hubs, and no hot plug: what's there at boot is what there is. One
//! interrupter, one event ring segment, one LUN, and disks up to 2 TiB, which is all
//! READ (10) can reach. Transfers are to buffers the caller owns: every board
//! identity-maps RAM, and a 64-bit bus is assumed, so an address is a bus address.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod controller;
pub mod descriptors;
pub mod hid;
pub mod msc;

pub use controller::{UsbDisk, Xhci};
