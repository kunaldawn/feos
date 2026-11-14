use std::{
    io::{self, Write},
    iter,
};

pub use self::bitmap_container::BitmapContainer;

pub mod bitmap_container;

/// Convert byte `n` into two characters hexadecimal.
pub(crate) fn write_hex_to_string(n: u8, string: &mut String) {
    let tbl = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    string.extend([tbl[n as usize >> 4], tbl[n as usize & 0xf]]);
}

pub fn write_base64_to_bytes(parts: &[u8], mut out: impl io::Write) -> io::Result<()> {
    let tbl = [
        b'A', b'B', b'C', b'D', b'E', b'F', b'G', b'H', b'I', b'J', b'K', b'L', b'M', b'N', b'O',
        b'P', b'Q', b'R', b'S', b'T', b'U', b'V', b'W', b'X', b'Y', b'Z', b'a', b'b', b'c', b'd',
        b'e', b'f', b'g', b'h', b'i', b'j', b'k', b'l', b'm', b'n', b'o', b'p', b'q', b'r', b's',
        b't', b'u', b'v', b'w', b'x', b'y', b'z', b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
        b'8', b'9', b'+', b'/', b'=',
    ];
    for chunk in parts.chunks(3) {
        let sextet1 = chunk[0] >> 2;
        let sextet2 = ((chunk[0] & 0x3) << 4) | (*chunk.get(1).unwrap_or(&0) >> 4);
        let sextet3 = chunk.get(1).map_or(64, |&x| {
            (*chunk.get(2).unwrap_or(&0) >> 6) | ((x & 0xf) << 2)
        });
        let sextet4 = chunk.get(2).map_or(64, |&x| x & 0x3f);
        out.write_all(&[
            tbl[sextet1 as usize],
            tbl[sextet2 as usize],
            tbl[sextet3 as usize],
            tbl[sextet4 as usize],
        ])?;
    }
    Ok(())
}

pub(crate) fn u40_to_be_bytes(number: u64) -> [u8; 5] {
    assert!(number == (number & 0xFF_FFFF_FFFF));
    [
        (number >> 32) as u8,
        (number >> 24) as u8,
        (number >> 16) as u8,
        (number >> 8) as u8,
        number as u8,
    ]
}

pub(crate) fn lower_u16_to_le_bytes(n: u32) -> [u8; size_of::<u16>()] {
    ((n & u32::from(u16::MAX)) as u16).to_le_bytes()
}

pub(crate) fn write_vlqhex_to_bytes(value: u32, string: &mut Vec<u8>) {
    // https://rust-lang.github.io/rustc-dev-guide/rustdoc-internals/search.html
    // describes the encoding in more detail.
    //
    // This version doesn't use zig-zag encoding, unlike the older version that was used for
    // type signatures (it's not needed).
    let mut shift: u32 = 28;
    // first skip leading zeroes
    while shift < 32 {
        let hexit = (value >> shift) & 0xF;
        if hexit != 0 || shift == 0 {
            break;
        }
        shift = shift.wrapping_sub(4);
    }
    // now write the rest
    let mut buf = [0; 8];
    let mut cursor = 0;
    while shift < 32 {
        let hexit = u8::try_from((value >> shift) & 0xF).unwrap();
        let hex = if shift == 0 { b'`' } else { b'@' } + hexit;
        buf[cursor] = hex;
        cursor += 1;
        if shift == 0 {
            break;
        }
        shift -= 4;
    }
    string.extend_from_slice(&buf[..cursor]);
}

// checked against roaring-rs in
// https://gitlab.com/notriddle/roaring-test
pub fn write_bitmap_to_bytes(domain: &[u32], out: &mut Vec<u8>) -> std::io::Result<()> {
    #[cold]
    fn write_short_domain(domain: &[u32], out: &mut Vec<u8>) -> io::Result<()> {
        debug_assert!(domain.len() <= 2);

        let mut start_offset = size_of::<u32>() as u32 * 2;
        if !domain.is_empty() {
            start_offset += size_of::<u32>() as u32 * 2;
        }

        out.reserve(start_offset as usize + size_of::<u16>() * domain.len());

        let size = if domain.is_empty() { 0 } else { 1 };
        let count_minus_one = u32::try_from(domain.len()).unwrap().checked_sub(1);
        out.write_all(&u32::to_le_bytes(SERIAL_COOKIE_NO_RUNCONTAINER))?;
        out.write_all(&u32::to_le_bytes(size))?;
        if let Some(count_minus_one) = count_minus_one {
            out.write_all(&u32::to_le_bytes(
                (count_minus_one << 16) | (domain[0] & 0xffff_0000) >> 16,
            ))?;
            out.write_all(&u32::to_le_bytes(start_offset))?;

            out.extend(domain.iter().copied().flat_map(|value| {
                let value = u16::try_from(value & 0x0000_ffff).unwrap();
                u16::to_le_bytes(value)
            }));
        }
        Ok(())
    }

    let short_domain = match domain {
        [] | [_] => true,
        &[a, b] if a & 0xffff_0000 == b & 0xffff_0000 => true,
        _ => false,
    };

    // This shows up in benchmarks.
    if short_domain {
        return write_short_domain(domain, out);
    }

    // https://arxiv.org/pdf/1603.06549.pdf
    let mut keys = Vec::<u16>::new();
    let mut containers = Vec::<BitmapContainer>::new();
    let mut key: u16;
    let mut domain_iter = domain.iter().copied().peekable();
    let mut has_run = false;
    let mut prev = None;
    while let Some(entry) = domain_iter.next() {
        assert!(prev <= Some(entry));
        prev = Some(entry);
        key = (entry >> 16)
            .try_into()
            .expect("shifted off the top 16 bits, so it should fit");
        let value: u16 = (entry & 0x00_00_FF_FF)
            .try_into()
            .expect("AND 16 bits, so it should fit");
        let mut container = BitmapContainer::Array(bitmap_container::Array(vec![value]));
        while let Some(entry) = domain_iter.peek().copied() {
            let entry_key: u16 = (entry >> 16)
                .try_into()
                .expect("shifted off the top 16 bits, so it should fit");
            if entry_key != key {
                break;
            }
            domain_iter.next().expect("peeking just succeeded");
            container.push(
                (entry & 0x00_00_FF_FF)
                    .try_into()
                    .expect("AND 16 bits, so it should fit"),
            );
        }
        keys.push(key);
        has_run = container.try_make_run() || has_run;
        containers.push(container);
    }
    // https://github.com/RoaringBitmap/RoaringFormatSpec
    const SERIAL_COOKIE_NO_RUNCONTAINER: u32 = 12346;
    const SERIAL_COOKIE: u32 = 12347;
    const NO_OFFSET_THRESHOLD: u32 = 4;
    let size: u32 = containers.len().try_into().unwrap();

    let mut additional = if has_run {
        size_of::<u32>() + containers.len().div_ceil(8)
    } else {
        size_of::<u32>() * 2
    };
    debug_assert_eq!(containers.len(), keys.len());
    additional += containers.len()
        * size_of::<u32>()
        * if !has_run || size >= NO_OFFSET_THRESHOLD {
            2
        } else {
            1
        };
    additional += containers
        .iter()
        .map(|container| match container {
            BitmapContainer::Bits(bitmap_container::Bits(bits)) => size_of_val(bits.as_slice()),
            BitmapContainer::Array(bitmap_container::Array(array)) => size_of_val(array.as_slice()),
            BitmapContainer::Run(bitmap_container::Run(run)) => {
                size_of::<u16>() + size_of_val(run.as_slice())
            }
        })
        .sum::<usize>();
    out.reserve(additional);

    #[cfg(test)]
    let original_len = out.len();

    let start_offset = if has_run {
        out.write_all(&u32::to_le_bytes(SERIAL_COOKIE | ((size - 1) << 16)))?;
        out.extend(containers.chunks(8).map(|set| {
            let mut b = 0;
            for (i, container) in set.iter().enumerate() {
                if matches!(container, &BitmapContainer::Run(..)) {
                    b |= 1 << i;
                }
            }
            b
        }));
        if size < NO_OFFSET_THRESHOLD {
            4 + 4 * size + size.div_ceil(8)
        } else {
            4 + 8 * size + size.div_ceil(8)
        }
    } else {
        out.write_all(&u32::to_le_bytes(SERIAL_COOKIE_NO_RUNCONTAINER))?;
        out.write_all(&u32::to_le_bytes(size))?;
        4 + 4 + 4 * size + 4 * size
    };
    out.extend(
        iter::zip(&keys, &containers)
            .map(|(&key, container)| {
                let key: u32 = key.into();
                let count: u32 = container.popcount() - 1;
                (count << 16) | key
            })
            .flat_map(u32::to_le_bytes),
    );
    if !has_run || size >= NO_OFFSET_THRESHOLD {
        // offset header
        let mut starting_offset = start_offset;
        for container in &containers {
            out.write_all(&u32::to_le_bytes(starting_offset))?;
            starting_offset += match container {
                BitmapContainer::Bits(_) => 8192u32,
                BitmapContainer::Array(bitmap_container::Array(array)) => {
                    u32::try_from(array.len()).unwrap() * 2
                }
                BitmapContainer::Run(bitmap_container::Run(run)) => {
                    2 + u32::try_from(run.len()).unwrap() * 4
                }
            };
        }
    }
    for container in &containers {
        match container {
            BitmapContainer::Bits(bitmap_container::Bits(bits)) => {
                out.extend(bits.iter().copied().flat_map(u64::to_le_bytes));
            }
            BitmapContainer::Array(bitmap_container::Array(array)) => {
                out.extend(array.iter().copied().flat_map(u16::to_le_bytes));
            }
            BitmapContainer::Run(bitmap_container::Run(run)) => {
                out.write_all(&u16::to_le_bytes(run.len().try_into().unwrap()))?;
                out.extend(
                    run.iter()
                        .copied()
                        .flat_map(|(start, lenm1)| [start, lenm1])
                        .flat_map(u16::to_le_bytes),
                );
            }
        }
    }

    #[cfg(test)]
    assert_eq!(original_len + additional, out.len());

    Ok(())
}

#[cfg(test)]
mod tests;
