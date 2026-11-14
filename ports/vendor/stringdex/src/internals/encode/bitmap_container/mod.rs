use std::{cmp, mem};

#[derive(Debug)]
pub struct Bits(pub Box<[u64; 1024]>);

impl Bits {
    fn contains(&self, value: u16) -> bool {
        let hunk = self.0[value as usize >> 6];
        hunk & (1 << (value & 0x3F)) != 0
    }

    fn popcount(&self) -> u32 {
        self.0.iter().copied().map(|x| x.count_ones()).sum()
    }

    fn push(&mut self, value: u16) {
        self.0[value as usize >> 6] |= 1 << (value & 0x3F);
    }

    fn push_to_vec(&self, key: u32, result: &mut Vec<u32>) {
        let mut value = 0u32;
        while value < 65536 {
            if self.contains(u16::try_from(value).unwrap()) {
                result.push((key << 16) | value);
            }
            value += 1;
        }
    }
}

#[derive(Debug)]
pub struct Array(pub Vec<u16>);

impl Array {
    fn contains(&self, value: u16) -> bool {
        self.0.binary_search(&value).is_ok()
    }

    fn popcount(&self) -> u32 {
        self.0
            .len()
            .try_into()
            .expect("array can't be bigger than 2**32")
    }

    pub(crate) fn push_to_vec(&self, key: u32, result: &mut Vec<u32>) {
        result.extend(
            self.0
                .iter()
                .copied()
                .map(u32::from)
                .map(|value| (key << 16) | value),
        )
    }
}

#[derive(Debug)]
pub struct Run(pub Vec<(u16, u16)>);

impl Run {
    fn contains(&self, value: u16) -> bool {
        self.0
            .binary_search_by(|&(start, lenm1)| {
                if start + lenm1 < value {
                    cmp::Ordering::Less
                } else if start > value {
                    cmp::Ordering::Greater
                } else {
                    cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    fn popcount(&self) -> u32 {
        self.0
            .iter()
            .copied()
            .map(|(_, lenm1)| u32::from(lenm1) + 1)
            .sum()
    }

    fn push(&mut self, value: u16) {
        if let Some((start, lenm1)) = self.0.last_mut()
            && *start + *lenm1 + 1 == value
        {
            *lenm1 += 1;
        } else {
            self.0.push((value, 0));
        }
    }

    fn push_to_vec(&self, key: u32, result: &mut Vec<u32>) {
        for (start, lenm1) in self.0.iter().copied() {
            let start = u32::from(start);
            result.extend((start..=start + u32::from(lenm1)).map(|value| (key << 16) | value));
        }
    }
}

/// Used during bitmap encoding
#[derive(Debug)]
pub enum BitmapContainer {
    /// number of ones, bits
    Bits(Bits),
    /// list of entries
    Array(Array),
    /// list of (start, len-1)
    Run(Run),
}

impl BitmapContainer {
    pub fn contains(&self, value: u16) -> bool {
        match self {
            BitmapContainer::Bits(bits) => bits.contains(value),
            BitmapContainer::Array(array) => array.contains(value),
            BitmapContainer::Run(run) => run.contains(value),
        }
    }

    pub(super) fn popcount(&self) -> u32 {
        match self {
            BitmapContainer::Bits(bits) => bits.popcount(),
            BitmapContainer::Array(array) => array.popcount(),
            BitmapContainer::Run(run) => run.popcount(),
        }
    }

    pub(super) fn push(&mut self, value: u16) {
        match self {
            BitmapContainer::Bits(bits) => {
                bits.push(value);
            }
            BitmapContainer::Array(Array(array)) => {
                if array.len() >= 4096 - 1 {
                    let array = mem::take(array);
                    let mut bits = Bits(Box::new([0; 1024]));
                    for value in array {
                        bits.push(value);
                    }
                    bits.push(value);
                    *self = BitmapContainer::Bits(bits);
                } else {
                    array.push(value);
                }
            }
            BitmapContainer::Run(runs) => {
                runs.push(value);
            }
        }
    }

    pub(super) fn try_make_run(&mut self) -> bool {
        match self {
            BitmapContainer::Bits(Bits(bits)) => {
                let mut r: u64 = 0;
                let mut chunks = bits.iter().copied().peekable();
                while let Some(chunk) = chunks.next() {
                    let next_chunk = chunks.peek().copied().unwrap_or_default();
                    r += !chunk & u64::from((chunk << 1).count_ones());
                    r += !next_chunk & u64::from((chunk >> 63).count_ones());
                }
                if (2 + 4 * r) >= 8192 {
                    return false;
                }
                let bits = mem::replace(bits, Box::new([0; 1024]));
                let mut run = Run(Vec::new());
                for (i, bits) in bits.iter().copied().enumerate() {
                    if bits == 0 {
                        continue;
                    }
                    for j in 0..64 {
                        let value = (u16::try_from(i).unwrap() << 6) | j;
                        if bits & (1 << j) != 0 {
                            run.push(value);
                        }
                    }
                }
                *self = BitmapContainer::Run(run);
                true
            }
            BitmapContainer::Array(Array(array)) if array.len() <= 5 => false,
            BitmapContainer::Array(Array(array)) => {
                let mut r = 0;
                let mut prev = None;
                for value in array.iter().copied() {
                    if value.checked_sub(1) != prev {
                        r += 1;
                    }
                    prev = Some(value);
                }
                if 2 + 4 * r >= 2 * array.len() + 2 {
                    return false;
                }
                let array = mem::take(array);
                let mut run = Run(Vec::new());
                for value in array {
                    run.push(value);
                }
                *self = BitmapContainer::Run(run);
                true
            }
            BitmapContainer::Run(_) => true,
        }
    }

    pub(crate) fn push_to_vec(&self, key: u16, result: &mut Vec<u32>) {
        let key: u32 = key.into();
        match self {
            BitmapContainer::Run(run) => {
                run.push_to_vec(key, result);
            }
            BitmapContainer::Array(array) => {
                array.push_to_vec(key, result);
            }
            BitmapContainer::Bits(bits) => {
                bits.push_to_vec(key, result);
            }
        }
    }
}
