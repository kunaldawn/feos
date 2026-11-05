//! Mass storage: Bulk-Only Transport and the SCSI commands a disk needs.

/// A command block wrapper.
pub const CBW_LEN: usize = 31;
/// A command status wrapper.
pub const CSW_LEN: usize = 13;
const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;

/// USB's class code for mass storage.
pub const CLASS: u8 = 8;
/// The subclass for SCSI commands.
pub const SUBCLASS_SCSI: u8 = 6;
/// The protocol code for Bulk-Only Transport.
pub const PROTOCOL_BOT: u8 = 0x50;

/// Which way a command's data goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// To the host.
    In,
    /// To the device.
    Out,
}

/// Writes a command block wrapper.
pub fn build_cbw(tag: u32, length: u32, direction: Direction, lun: u8, command: &[u8]) -> [u8; CBW_LEN] {
    let mut out = [0u8; CBW_LEN];
    out[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    out[4..8].copy_from_slice(&tag.to_le_bytes());
    out[8..12].copy_from_slice(&length.to_le_bytes());
    out[12] = if direction == Direction::In { 0x80 } else { 0 };
    out[13] = lun;
    let count = command.len().min(16);
    out[14] = count as u8;
    out[15..15 + count].copy_from_slice(&command[..count]);
    out
}

/// What a command status wrapper says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    /// Bytes of the expected data that didn't move.
    pub residue: u32,
    /// Zero for success, one for a failed command, two for a phase error.
    pub code: u8,
}

/// Reads a command status wrapper for the command with this tag.
pub fn parse_csw(data: &[u8], tag: u32) -> Option<Status> {
    let bytes = data.get(..CSW_LEN)?;
    if u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != CSW_SIGNATURE {
        return None;
    }
    if u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) != tag {
        return None;
    }
    Some(Status {
        residue: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        code: bytes[12],
    })
}

/// SCSI's TEST UNIT READY.
pub const fn test_unit_ready() -> [u8; 6] {
    [0; 6]
}

/// SCSI's REQUEST SENSE, for this many bytes.
pub const fn request_sense(length: u8) -> [u8; 6] {
    [0x03, 0, 0, 0, length, 0]
}

/// SCSI's INQUIRY, for this many bytes.
pub const fn inquiry(length: u8) -> [u8; 6] {
    [0x12, 0, 0, 0, length, 0]
}

/// SCSI's READ CAPACITY (10).
pub const fn read_capacity() -> [u8; 10] {
    [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

fn read_write(opcode: u8, lba: u32, blocks: u16) -> [u8; 10] {
    let lba = lba.to_be_bytes();
    let blocks = blocks.to_be_bytes();
    [opcode, 0, lba[0], lba[1], lba[2], lba[3], 0, blocks[0], blocks[1], 0]
}

/// SCSI's READ (10).
pub fn read10(lba: u32, blocks: u16) -> [u8; 10] {
    read_write(0x28, lba, blocks)
}

/// SCSI's WRITE (10).
pub fn write10(lba: u32, blocks: u16) -> [u8; 10] {
    read_write(0x2a, lba, blocks)
}

/// SCSI's SYNCHRONIZE CACHE (10).
pub const fn synchronize_cache() -> [u8; 10] {
    [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// What READ CAPACITY says: the last block's address and the block size.
pub fn parse_capacity(data: &[u8]) -> Option<(u32, u32)> {
    let bytes = data.get(..8)?;
    let last = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let size = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if size < 512 || !size.is_power_of_two() || size > 1 << 16 {
        return None;
    }
    Some((last, size))
}

/// The sense key in a REQUEST SENSE answer.
pub fn sense_key(data: &[u8]) -> Option<u8> {
    let code = *data.first()? & 0x7f;
    if code != 0x70 && code != 0x71 {
        return None;
    }
    Some(*data.get(2)? & 0xf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cbw_is_laid_out_as_specified() {
        let cbw = build_cbw(0x11223344, 512, Direction::In, 1, &read10(0x01020304, 2));
        assert_eq!(&cbw[..4], b"USBC");
        assert_eq!(cbw[4..8], 0x11223344u32.to_le_bytes());
        assert_eq!(cbw[8..12], 512u32.to_le_bytes());
        assert_eq!((cbw[12], cbw[13], cbw[14]), (0x80, 1, 10));
        assert_eq!(&cbw[15..25], &[0x28, 0, 1, 2, 3, 4, 0, 0, 2, 0]);
        assert_eq!(build_cbw(1, 0, Direction::Out, 0, &[])[12], 0);
    }

    #[test]
    fn a_csw_must_match_its_command() {
        let mut csw = [0u8; CSW_LEN];
        csw[..4].copy_from_slice(b"USBS");
        csw[4..8].copy_from_slice(&7u32.to_le_bytes());
        csw[8..12].copy_from_slice(&3u32.to_le_bytes());
        csw[12] = 1;
        assert_eq!(parse_csw(&csw, 7), Some(Status { residue: 3, code: 1 }));
        assert_eq!(parse_csw(&csw, 8), None);
        assert_eq!(parse_csw(&csw[..12], 7), None);
        csw[0] = 0;
        assert_eq!(parse_csw(&csw, 7), None);
    }

    #[test]
    fn capacity_rejects_nonsense() {
        assert_eq!(parse_capacity(&[0, 0, 0xff, 0xff, 0, 0, 2, 0]), Some((0xffff, 512)));
        assert_eq!(parse_capacity(&[0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse_capacity(&[0, 0, 0, 0, 0, 0, 2, 1]), None);
        assert_eq!(parse_capacity(&[0, 0, 0]), None);
    }

    #[test]
    fn the_sense_key_is_found() {
        assert_eq!(sense_key(&[0x70, 0, 6, 0]), Some(6));
        assert_eq!(sense_key(&[0xf0, 0, 0x26, 0]), Some(6));
        assert_eq!(sense_key(&[0x00, 0, 6, 0]), None);
        assert_eq!(sense_key(&[]), None);
    }
}
