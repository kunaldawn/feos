//! A small TCP/IP stack, written for feos: Ethernet, ARP, IPv4, ICMP echo, UDP, a
//! DHCP client, a DNS client and TCP.
//!
//! Nothing here touches hardware. [`stack::Stack`] is driven by calls and a clock
//! value, over a [`feos_hal::net::NetDevice`], so all of it runs on the host in
//! tests. [`service::Service`] puts a lock around one, and provides the blocking
//! socket calls of [`feos_hal::net::Network`].
//!
//! Every parser takes bytes off the wire, which are untrusted: they use checked
//! access, and answer `None` rather than panic. See `docs/networking.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod dhcp;
pub mod dns;
pub mod service;
pub mod stack;
pub mod tcp;
pub mod wire;

pub use feos_hal::net::{Endpoint, Ipv4Addr, NetConfig, NetDevice, NetError, NetStats, Network, SocketId, SocketInfo};
