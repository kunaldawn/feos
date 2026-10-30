//! Sample conversion helpers shared by the container decoders.

pub(crate) fn u8_to_i16(value: u8) -> i16 {
    ((value as i16) - 128) << 8
}

pub(crate) fn i24_to_i16(bytes: [u8; 3], big_endian: bool) -> i16 {
    let value = if big_endian {
        ((bytes[0] as i32) << 16) | ((bytes[1] as i32) << 8) | bytes[2] as i32
    } else {
        ((bytes[2] as i32) << 16) | ((bytes[1] as i32) << 8) | bytes[0] as i32
    };
    // Sign-extend from 24 bits and keep the top 16 bits.
    (value << 8 >> 16) as i16
}

pub(crate) fn i32_to_i16(value: i32) -> i16 {
    (value >> 16) as i16
}

pub(crate) fn f32_to_i16(value: f32) -> i16 {
    let scaled = value * 32767.0;
    if scaled.is_nan() {
        0
    } else {
        scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }
}

pub(crate) fn f64_to_i16(value: f64) -> i16 {
    let scaled = value * 32767.0;
    if scaled.is_nan() {
        0
    } else {
        scaled.clamp(i16::MIN as f64, i16::MAX as f64) as i16
    }
}

pub(crate) fn f32_from_be(bytes: [u8; 4]) -> f32 {
    f32::from_bits(u32::from_be_bytes(bytes))
}

pub(crate) fn mulaw_to_i16(value: u8) -> i16 {
    let value = !value;
    let sign = value & 0x80;
    let exponent = (value >> 4) & 0x07;
    let mantissa = value & 0x0f;
    let sample = (((mantissa as i32) << 3) + 0x84) << exponent;
    let sample = sample - 0x84;
    if sign != 0 {
        sample as i16
    } else {
        (-sample) as i16
    }
}

pub(crate) fn alaw_to_i16(value: u8) -> i16 {
    let value = value ^ 0x55;
    let sign = value & 0x80;
    let exponent = (value >> 4) & 0x07;
    let mantissa = value & 0x0f;
    let mut sample = (mantissa as i32) << 4;
    if exponent == 0 {
        sample += 8;
    } else {
        sample += 0x108;
        sample <<= exponent - 1;
    }
    if sign != 0 {
        sample as i16
    } else {
        (-sample) as i16
    }
}
