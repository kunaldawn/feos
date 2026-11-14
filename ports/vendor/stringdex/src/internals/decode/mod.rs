use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::iter;

use super::encode::{BitmapContainer, bitmap_container};

pub fn unescape_ascii(mut string: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let mut i = 0;
    loop {
        let rem = match string {
            [] => return Some(i),
            [b'\\', b't', rem @ ..] => {
                out.push(b'\t');
                rem
            }
            [b'\\', b'r', rem @ ..] => {
                out.push(b'\r');
                rem
            }
            [b'\\', b'n', rem @ ..] => {
                out.push(b'\n');
                rem
            }
            [b'\\', b'\'', rem @ ..] => {
                out.push(b'\'');
                rem
            }
            [b'\\', b'"', rem @ ..] => {
                out.push(b'\"');
                rem
            }
            [b'\\', b'\\', rem @ ..] => {
                out.push(b'\\');
                rem
            }
            [b'\\', b'x', a, b, rem @ ..] => {
                let tbl = [
                    b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'a', b'b', b'c',
                    b'd', b'e', b'f',
                ];
                out.push(
                    u8::try_from(tbl.iter().position(|x| x == a)? << 4).ok()?
                        | u8::try_from(tbl.iter().position(|x| x == b)?).ok()?,
                );
                rem
            }
            [c, rem @ ..] => {
                out.push(*c);
                rem
            }
        };
        i += string.len() - rem.len();
        string = rem;
    }
}

pub fn read_vlqhex_from_bytes(string: &[u8]) -> Option<(u32, usize)> {
    let mut n = 0u32;
    for (i, &c) in string.iter().enumerate() {
        n = (n << 4) | u32::from(c & 0xF);
        if c >= 96 {
            return Some((n, i + 1));
        }
    }
    None
}

pub fn u40_from_be_bytes(bytes: [u8; 5]) -> u64 {
    (u64::from(bytes[0]) << 32)
        | (u64::from(bytes[1]) << 24)
        | (u64::from(bytes[2]) << 16)
        | (u64::from(bytes[3]) << 8)
        | u64::from(bytes[4])
}

/// Read two characters hexadecimal from `string`.
pub fn read_hex_from_string(string: &str) -> Result<u8, HexError> {
    let tbl = [
        b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'a', b'b', b'c', b'd', b'e',
        b'f',
    ];
    match string.as_bytes() {
        &[a, b, ..] => {
            let ax = tbl
                .iter()
                .position(|x| *x == a)
                .and_then(|pos| u8::try_from(pos).ok())
                .ok_or(HexError)?;
            let bx = tbl
                .iter()
                .position(|x| *x == b)
                .and_then(|pos| u8::try_from(pos).ok())
                .ok_or(HexError)?;
            Ok((ax << 4) | bx)
        }
        _ => Err(HexError),
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct HexError;

impl Display for HexError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("invalid hexadecimal")
    }
}

impl Debug for HexError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("HexError")
    }
}

impl Error for HexError {}

/// Read base64 from `string`.
pub fn read_base64_from_bytes(string: &[u8], out: &mut Vec<u8>) -> Result<(), Base64Error> {
    let base64_val = |c| {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let is_base64_char = |c| base64_val(c).is_some();

    let (chunks, []) = string.as_chunks::<4>() else {
        return Err(Base64Error);
    };
    let Some((last, chunks)) = chunks.split_last() else {
        return Ok(());
    };

    let is_chunk_valid = |chunk: &[u8]| chunk.iter().all(|&c| is_base64_char(c));
    let is_last_chunk_valid = |last: [u8; 4]| match last {
        [chunk @ .., b'=', b'='] => is_chunk_valid(&chunk),
        [chunk @ .., b'='] => is_chunk_valid(&chunk),
        _ => is_chunk_valid(&last),
    };

    if chunks.iter().any(|chunk| !is_chunk_valid(chunk)) || !is_last_chunk_valid(*last) {
        return Err(Base64Error);
    }

    out.reserve(chunks.len() * 3 + /* TODO: maybe make this exact? */ 3);

    let decode_chunk = |chunk: [u8; 4]| {
        let [sextet1, sextet2, sextet3, sextet4] = chunk.map(|c| base64_val(c).unwrap());
        let byte1 = (sextet1 << 2) | (sextet2 >> 4);
        let byte2 = ((sextet2 & 0xf) << 4) | (sextet3 >> 2);
        let byte3 = ((sextet3 & 0x3) << 6) | sextet4;
        [byte1, byte2, byte3]
    };

    out.extend(chunks.iter().copied().flat_map(decode_chunk));

    match *last {
        // 8 bits
        [chunk @ .., b'=', b'='] => {
            let [sextet1, sextet2] = chunk.map(|c| base64_val(c).unwrap());
            let byte1 = (sextet1 << 2) | (sextet2 >> 4);
            out.push(byte1);
        }
        // 16 bits
        [chunk @ .., b'='] => {
            let [sextet1, sextet2, sextet3] = chunk.map(|c| base64_val(c).unwrap());
            let byte1 = (sextet1 << 2) | (sextet2 >> 4);
            let byte2 = ((sextet2 & 0xf) << 4) | (sextet3 >> 2);
            out.extend_from_slice(&[byte1, byte2]);
        }
        // 24 bits
        _ => {
            out.extend_from_slice(&decode_chunk(*last));
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Base64Error;

impl Display for Base64Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("invalid base64")
    }
}

impl Debug for Base64Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("Base64Error")
    }
}

impl Error for Base64Error {}

#[derive(Default)]
pub struct RoaringBitmap {
    keys: Vec<u16>,
    containers: Vec<BitmapContainer>,
}

trait SliceExt<T> {
    fn split_off_chunk<'a, const N: usize>(self: &mut &'a Self) -> &'a [T; N];
}

impl<T> SliceExt<T> for [T] {
    fn split_off_chunk<'a, const N: usize>(self: &mut &'a Self) -> &'a [T; N] {
        let (chunk, rest) = self.split_first_chunk().unwrap();
        *self = rest;
        chunk
    }
}

impl RoaringBitmap {
    pub fn from_bytes(u8array: &[u8]) -> Option<(RoaringBitmap, usize)> {
        // https://github.com/RoaringBitmap/RoaringFormatSpec
        //
        // Roaring bitmaps are used for flags that can be kept in their
        // compressed form, even when loaded into memory. This decoder
        // turns the containers into objects, but uses byte array
        // slices of the original format for the data payload.
        let mut cur = u8array;
        let has_runs = *cur.split_off_first()? == 0x3b;
        cur = &cur[1..];
        let size = if has_runs {
            usize::from(u16::from_le_bytes(*cur.split_off_chunk())) + 1
        } else {
            cur = &cur[2..];
            usize::try_from(u32::from_le_bytes(*cur.split_off_chunk())).unwrap()
        };
        let is_run = if has_runs {
            let is_run_len = size.div_ceil(8);
            cur.split_off(..is_run_len).unwrap()
        } else {
            &[]
        };

        fn read_keys_and_cardinalities<'a>(
            u8array: &mut &'a [u8],
            size: usize,
        ) -> impl Iterator<Item = (u16, usize)> + 'a {
            let len = size * 2 * size_of::<u16>();
            u8array
                .split_off(..len)
                .unwrap()
                .as_chunks::<{ size_of::<u16>() }>()
                .0
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(|[key, cardinality]| {
                    let key = u16::from_le_bytes(key);
                    let cardinality = usize::from(u16::from_le_bytes(cardinality)) + 1;
                    (key, cardinality)
                })
        }

        let keys_and_cardinalities = read_keys_and_cardinalities(&mut cur, size);

        fn read_offsets<'a>(
            u8array: &mut &'a [u8],
            size: usize,
            has_runs: bool,
        ) -> Option<impl Iterator<Item = usize> + 'a> {
            (!has_runs || size >= 4).then(|| {
                let len = size_of::<u32>() * size;

                u8array
                    .split_off(..len)
                    .unwrap()
                    .as_chunks()
                    .0
                    .iter()
                    .copied()
                    .map(|offset| usize::try_from(u32::from_le_bytes(offset)).unwrap())
            })
        }

        let mut offsets = read_offsets(&mut cur, size, has_runs).into_iter().flatten();

        trait ReadContainer: Sized {
            fn read(u8array: &mut &[u8], cardinality: usize) -> Self;
        }

        impl ReadContainer for bitmap_container::Run {
            fn read(u8array: &mut &[u8], _cardinality: usize) -> Self {
                let runcount = usize::from(u16::from_le_bytes(*u8array.split_off_chunk()));
                let items = u8array
                    .split_off(..(runcount * 2) * size_of::<u16>())
                    .unwrap();
                let runparts = items
                    .as_chunks::<{ size_of::<u16>() }>()
                    .0
                    .as_chunks()
                    .0
                    .iter()
                    .map(|&[a, b]| (u16::from_le_bytes(a), u16::from_le_bytes(b)))
                    .collect();

                Self(runparts)
            }
        }

        impl ReadContainer for bitmap_container::Bits {
            fn read(u8array: &mut &[u8], _: usize) -> Self {
                let len = size_of::<u64>() * 1024;
                let bits = u8array
                    .split_off(..len)
                    .unwrap()
                    .as_chunks()
                    .0
                    .iter()
                    .copied()
                    .map(u64::from_le_bytes)
                    .collect::<Box<[_]>>()
                    .try_into()
                    .unwrap();

                Self(bits)
            }
        }

        impl ReadContainer for bitmap_container::Array {
            fn read(u8array: &mut &[u8], cardinality: usize) -> Self {
                let len = size_of::<u16>() * cardinality;
                let array = u8array
                    .split_off(..len)
                    .unwrap()
                    .as_chunks()
                    .0
                    .iter()
                    .copied()
                    .map(u16::from_le_bytes)
                    .collect();

                Self(array)
            }
        }

        let (keys, containers) = keys_and_cardinalities
            .zip(iter::repeat_with(|| offsets.next()))
            .enumerate()
            .map(|(j, ((key, cardinality), offset))| {
                if let Some(offset) = offset {
                    let i = u8array.len() - cur.len();
                    assert_eq!(offset, i, "corrupt bitmap {j}: {i} / {}", offset);
                }
                let container = if !is_run.is_empty() && is_run[j >> 3] & (1 << (j & 0x7)) != 0 {
                    BitmapContainer::Run(ReadContainer::read(&mut cur, cardinality))
                } else if cardinality >= 4096 {
                    BitmapContainer::Bits(ReadContainer::read(&mut cur, cardinality))
                } else {
                    BitmapContainer::Array(ReadContainer::read(&mut cur, cardinality))
                };
                (key, container)
            })
            .unzip();

        Some((
            RoaringBitmap { keys, containers },
            u8array.len() - cur.len(),
        ))
    }

    pub fn contains(&self, keyvalue: u32) -> bool {
        let key = u16::try_from(keyvalue >> 16).unwrap();
        let value = u16::try_from(keyvalue & 0xFFFF).unwrap();
        if let Ok(idx) = self.keys.binary_search(&key) {
            self.containers[idx].contains(value)
        } else {
            false
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        let mut result = Vec::new();
        assert_eq!(self.containers.len(), self.keys.len());
        for (container, &key) in iter::zip(&self.containers, &self.keys) {
            container.push_to_vec(key, &mut result);
        }
        result
    }
}

#[cfg(test)]
mod tests;
