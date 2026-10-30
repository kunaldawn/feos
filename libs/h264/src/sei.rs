use alloc::vec::Vec;

use crate::bitstream::BitstreamReader;

/// SEI message (H.264 spec section 7.3.2.3).
#[derive(Debug)]
pub struct SeiMessage {
    pub payload_type: u32,
    pub payload: SeiPayload,
}

#[derive(Debug)]
pub enum SeiPayload {
    UserDataUnregistered { uuid: [u8; 16], data: Vec<u8> },
    Unknown { data: Vec<u8> },
}

/// Parse all SEI messages from RBSP data (NAL header byte already stripped).
pub fn parse_sei(rbsp: &[u8]) -> Result<Vec<SeiMessage>, &'static str> {
    let mut messages = Vec::new();
    let mut reader = BitstreamReader::new(rbsp);

    while reader.more_rbsp_data() {
        reader.align_to_byte();

        // Read payloadType: sum of 0xFF bytes, plus final byte < 0xFF
        let mut payload_type: u32 = 0;
        loop {
            let byte = reader.read_bits(8)? as u8;
            payload_type = payload_type.saturating_add(byte as u32);
            if byte != 0xFF {
                break;
            }
        }

        // Read payloadSize: same encoding
        let mut payload_size: u32 = 0;
        loop {
            let byte = reader.read_bits(8)? as u8;
            payload_size = payload_size.saturating_add(byte as u32);
            if byte != 0xFF {
                break;
            }
        }

        let size = payload_size as usize;
        if size > reader.bits_remaining() / 8 {
            return Err("SEI payload size exceeds RBSP length");
        }
        let mut payload_data = Vec::with_capacity(size);
        for _ in 0..size {
            payload_data.push(reader.read_bits(8)? as u8);
        }

        let payload = match payload_type {
            5 => parse_user_data_unregistered(&payload_data)?,
            _ => SeiPayload::Unknown { data: payload_data },
        };

        messages.push(SeiMessage {
            payload_type,
            payload,
        });
    }

    Ok(messages)
}

fn parse_user_data_unregistered(data: &[u8]) -> Result<SeiPayload, &'static str> {
    if data.len() < 16 {
        return Err("user_data_unregistered payload too short for UUID");
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&data[..16]);
    Ok(SeiPayload::UserDataUnregistered {
        uuid,
        data: data[16..].to_vec(),
    })
}

#[cfg(all(test, feature = "upstream-tests"))]
mod tests {
    use super::*;
    use crate::nal::{NalUnitType, parse_annex_b};
    use alloc::string::String;

    #[test]
    fn test_parse_sei_single_frame() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/single_frame.h264"
        ))
        .unwrap();
        let nals = parse_annex_b(&data);
        let sei_nal = nals
            .iter()
            .find(|n| n.nal_unit_type == NalUnitType::Sei)
            .unwrap();
        let messages = parse_sei(&sei_nal.rbsp).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload_type, 5); // user_data_unregistered

        match &messages[0].payload {
            SeiPayload::UserDataUnregistered { uuid, data } => {
                // UUID should be 16 bytes (x264's UUID)
                assert_eq!(uuid.len(), 16);
                // Data should contain x264 encoder string
                let text = String::from_utf8_lossy(data);
                assert!(text.contains("x264"), "expected x264 string, got: {text}");
            }
            _ => panic!("expected UserDataUnregistered"),
        }
    }
}
