//! USB descriptors: what a device says about itself. All of it is untrusted, so a
//! length that lies ends the walk.

use alloc::vec::Vec;

/// Standard descriptor types.
pub mod kind {
    /// A device descriptor.
    pub const DEVICE: u8 = 1;
    /// A configuration descriptor, and everything under it.
    pub const CONFIGURATION: u8 = 2;
    /// An interface descriptor.
    pub const INTERFACE: u8 = 4;
    /// An endpoint descriptor.
    pub const ENDPOINT: u8 = 5;
    /// A HID class descriptor.
    pub const HID: u8 = 0x21;
    /// A HID report descriptor.
    pub const REPORT: u8 = 0x22;
    /// A SuperSpeed endpoint companion.
    pub const SS_COMPANION: u8 = 0x30;
}

/// The fields of a device descriptor this driver reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeviceDescriptor {
    /// Largest packet on the default endpoint, as a byte count.
    pub max_packet0: u16,
    /// Vendor ID.
    pub vendor: u16,
    /// Product ID.
    pub product: u16,
    /// USB version, in BCD.
    pub usb_version: u16,
    /// Number of configurations.
    pub configurations: u8,
}

/// Reads a device descriptor. A SuperSpeed device gives its packet size as a power.
pub fn parse_device(data: &[u8]) -> Option<DeviceDescriptor> {
    if data.len() < 18 || data[0] < 18 || data[1] != kind::DEVICE {
        return None;
    }
    let usb_version = u16::from_le_bytes([data[2], data[3]]);
    let packet = data[7];
    let max_packet0 = if usb_version >= 0x0300 {
        if packet > 12 {
            return None;
        }
        1u16 << packet
    } else {
        u16::from(packet)
    };
    if max_packet0 == 0 {
        return None;
    }
    Some(DeviceDescriptor {
        max_packet0,
        vendor: u16::from_le_bytes([data[8], data[9]]),
        product: u16::from_le_bytes([data[10], data[11]]),
        usb_version,
        configurations: data[17],
    })
}

/// An endpoint of an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Endpoint {
    /// Address: the number, with the top bit set for IN.
    pub address: u8,
    /// 2 for bulk and 3 for interrupt.
    pub transfer: u8,
    /// Largest packet.
    pub max_packet: u16,
    /// The device's polling interval, in its own units.
    pub interval: u8,
    /// Extra packets per burst on SuperSpeed.
    pub max_burst: u8,
}

impl Endpoint {
    /// The transfer type for bulk.
    pub const BULK: u8 = 2;
    /// The transfer type for interrupt.
    pub const INTERRUPT: u8 = 3;

    /// True for an endpoint that sends to the host.
    pub const fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// The endpoint's number.
    pub const fn number(&self) -> u8 {
        self.address & 0x0f
    }
}

/// An interface and its endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Interface {
    /// Interface number.
    pub number: u8,
    /// Alternate setting.
    pub alternate: u8,
    /// Class code.
    pub class: u8,
    /// Subclass code.
    pub subclass: u8,
    /// Protocol code.
    pub protocol: u8,
    /// Length of its HID report descriptor.
    pub hid_report_length: u16,
    /// Its endpoints.
    pub endpoints: Vec<Endpoint>,
}

/// What a configuration descriptor says.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Configuration {
    /// The value that selects this configuration.
    pub value: u8,
    /// Its interfaces, alternate settings included.
    pub interfaces: Vec<Interface>,
}

/// The total length a configuration descriptor's head says is coming.
pub fn configuration_length(head: &[u8]) -> Option<usize> {
    if head.len() < 9 || head[1] != kind::CONFIGURATION {
        return None;
    }
    Some(usize::from(u16::from_le_bytes([head[2], head[3]])))
}

/// Walks a configuration descriptor and everything under it.
pub fn parse_configuration(data: &[u8]) -> Option<Configuration> {
    if data.len() < 9 || data[1] != kind::CONFIGURATION || usize::from(data[0]) < 9 {
        return None;
    }
    let mut out = Configuration {
        value: data[5],
        interfaces: Vec::new(),
    };
    let mut rest = data.get(usize::from(data[0])..)?;
    // No configuration has more than this, and a device saying so isn't to be trusted.
    let mut budget = 256;
    while rest.len() >= 2 && budget > 0 {
        budget -= 1;
        let length = usize::from(rest[0]);
        if length < 2 || length > rest.len() {
            break;
        }
        let body = &rest[..length];
        match body[1] {
            kind::INTERFACE if length >= 9 => {
                out.interfaces.push(Interface {
                    number: body[2],
                    alternate: body[3],
                    class: body[5],
                    subclass: body[6],
                    protocol: body[7],
                    ..Interface::default()
                });
            }
            kind::ENDPOINT if length >= 7 => {
                if let Some(interface) = out.interfaces.last_mut() {
                    interface.endpoints.push(Endpoint {
                        address: body[2],
                        transfer: body[3] & 3,
                        max_packet: u16::from_le_bytes([body[4], body[5]]) & 0x7ff,
                        interval: body[6],
                        max_burst: 0,
                    });
                }
            }
            kind::HID if length >= 9 => {
                if let Some(interface) = out.interfaces.last_mut() {
                    // The first class descriptor's type is at 6, and its length after.
                    if body[6] == kind::REPORT {
                        interface.hid_report_length = u16::from_le_bytes([body[7], body[8]]);
                    }
                }
            }
            kind::SS_COMPANION if length >= 3 => {
                if let Some(endpoint) = out.interfaces.last_mut().and_then(|interface| interface.endpoints.last_mut()) {
                    endpoint.max_burst = body[2];
                }
            }
            _ => {}
        }
        rest = &rest[length..];
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As QEMU's usb-kbd reports it.
    const KEYBOARD: &[u8] = &[
        9, 2, 34, 0, 1, 1, 0, 0xa0, 50, // configuration
        9, 4, 0, 0, 1, 3, 1, 1, 0, // interface: HID, boot, keyboard
        9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, // HID, report length 63
        7, 5, 0x81, 3, 8, 0, 7, // interrupt IN 1
    ];

    #[test]
    fn reads_a_device_descriptor() {
        let data = [18, 1, 0, 2, 0, 0, 0, 64, 0x27, 0x06, 0x01, 0x00, 0, 0, 1, 2, 3, 1];
        let parsed = parse_device(&data).unwrap();
        assert_eq!((parsed.max_packet0, parsed.vendor, parsed.product), (64, 0x0627, 1));
    }

    #[test]
    fn a_superspeed_packet_size_is_a_power() {
        let data = [18, 1, 0, 3, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 1, 2, 3, 1];
        assert_eq!(parse_device(&data).unwrap().max_packet0, 512);
    }

    #[test]
    fn device_descriptors_that_lie_are_refused() {
        assert!(parse_device(&[18, 1]).is_none());
        let mut data = [18, 1, 0, 3, 0, 0, 0, 200, 0, 0, 0, 0, 0, 0, 1, 2, 3, 1];
        assert!(parse_device(&data).is_none());
        data[1] = 2;
        assert!(parse_device(&data).is_none());
    }

    #[test]
    fn reads_a_keyboard_configuration() {
        assert_eq!(configuration_length(KEYBOARD), Some(34));
        let parsed = parse_configuration(KEYBOARD).unwrap();
        assert_eq!(parsed.value, 1);
        let interface = &parsed.interfaces[0];
        assert_eq!((interface.class, interface.subclass, interface.protocol), (3, 1, 1));
        assert_eq!(interface.hid_report_length, 63);
        assert_eq!(interface.endpoints[0], Endpoint { address: 0x81, transfer: 3, max_packet: 8, interval: 7, max_burst: 0 });
        assert!(interface.endpoints[0].is_in());
        assert_eq!(interface.endpoints[0].number(), 1);
    }

    #[test]
    fn a_companion_descriptor_sets_the_burst() {
        let data = [
            9, 2, 31, 0, 1, 1, 0, 0xa0, 50, 9, 4, 0, 0, 2, 8, 6, 0x50, 0, 7, 5, 0x81, 2, 0, 4, 0, 6, 0x30, 15, 0, 0, 0,
        ];
        let parsed = parse_configuration(&data).unwrap();
        assert_eq!(parsed.interfaces[0].endpoints[0].max_burst, 15);
        assert_eq!(parsed.interfaces[0].endpoints[0].max_packet, 1024);
    }

    #[test]
    fn a_length_that_lies_ends_the_walk() {
        let mut data = KEYBOARD.to_vec();
        data[9] = 200;
        assert!(parse_configuration(&data).unwrap().interfaces.is_empty());
        data[9] = 0;
        assert!(parse_configuration(&data).unwrap().interfaces.is_empty());
        data[9] = 1;
        assert!(parse_configuration(&data).unwrap().interfaces.is_empty());
        assert!(parse_configuration(&KEYBOARD[..3]).is_none());
    }

    #[test]
    fn no_input_panics() {
        let mut state = 0x1357_9bdf_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..100_000usize {
            let mut data = KEYBOARD.to_vec();
            for _ in 0..1 + round % 6 {
                let at = next() as usize % data.len();
                data[at] = next() as u8;
            }
            let length = round % (data.len() + 1);
            let _ = parse_configuration(&data[..length]);
            let _ = configuration_length(&data[..length]);
            let _ = parse_device(&data[..length]);
        }
    }
}
