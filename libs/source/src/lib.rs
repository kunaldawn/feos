//! Where a media file's bytes come from when it won't fit in memory.
//!
//! Demuxers and decoders mostly work on slices. A [`Source`] is the one thing they
//! need for a file that isn't a slice: bytes at an offset. It's a trait so that this
//! crate, and those above it, stay free of filesystems. [`Window`] keeps the last bytes
//! read, because walking a container means many small reads that sit close together.
//!
//! Reads block for as long as the disk takes, so they belong on a media task, never on
//! the desktop's.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// A file, or anything else with bytes at offsets.
pub trait Source: Send {
    /// How many bytes there are.
    fn len(&self) -> u64;

    /// Whether there are none.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads from `offset`, and says how many bytes it put in `out`. It may be fewer
    /// than asked for. 0 means the end, or that the bytes can't be had.
    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize;

    /// Fills as much of `out` as there are bytes for, and says how much that was.
    fn read_full(&self, offset: u64, out: &mut [u8]) -> usize {
        let mut done = 0;
        while done < out.len() {
            let read = self.read_at(offset + done as u64, &mut out[done..]);
            if read == 0 {
                break;
            }
            done += read;
        }
        done
    }
}

impl Source for &[u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        let Some(rest) = usize::try_from(offset).ok().and_then(|at| self.get(at..)) else {
            return 0;
        };
        let count = rest.len().min(out.len());
        out[..count].copy_from_slice(&rest[..count]);
        count
    }
}

impl<S: Source + Sync + ?Sized> Source for &S {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
        (**self).read_at(offset, out)
    }
}

/// The last bytes read from a source, in a buffer of a fixed size allocated once. For
/// holding alongside the source: [`Window`] is the same with the source in it.
pub struct Cache {
    buffer: Vec<u8>,
    /// File offset of `buffer[0]`.
    base: u64,
    filled: usize,
}

impl Cache {
    /// A cache of `capacity` bytes, or `None` when memory is short.
    pub fn new(capacity: usize) -> Option<Self> {
        Some(Self {
            buffer: zeroed(capacity)?,
            base: 0,
            filled: 0,
        })
    }

    /// The most [`bytes`](Self::bytes) can give at once.
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Up to `want` bytes at `offset`: fewer only at the end of the file or when `want`
    /// is more than the capacity.
    pub fn bytes(&mut self, source: &dyn Source, offset: u64, want: usize) -> &[u8] {
        let want = want.min(self.buffer.len());
        let end = self.base + self.filled as u64;
        if offset < self.base || offset + want as u64 > end {
            // Keep what overlaps, which is what a walk forward wants.
            let keep = if offset >= self.base && offset < end {
                let from = (offset - self.base) as usize;
                self.buffer.copy_within(from..self.filled, 0);
                self.filled - from
            } else {
                0
            };
            self.base = offset;
            self.filled = keep;
            let read = source.read_full(offset + keep as u64, &mut self.buffer[keep..]);
            self.filled += read;
        }
        let from = (offset - self.base) as usize;
        let to = (from + want).min(self.filled);
        &self.buffer[from..to]
    }

    /// Forgets what it has.
    pub fn clear(&mut self) {
        self.filled = 0;
    }
}

/// Reads through a [`Cache`].
pub struct Window<'s> {
    source: &'s dyn Source,
    cache: Cache,
}

impl<'s> Window<'s> {
    /// A window of `capacity` bytes, or `None` when memory is short.
    pub fn new(source: &'s dyn Source, capacity: usize) -> Option<Self> {
        Some(Self {
            source,
            cache: Cache::new(capacity)?,
        })
    }

    /// The file's length.
    pub fn file_len(&self) -> u64 {
        self.source.len()
    }

    /// See [`Cache::bytes`].
    pub fn bytes(&mut self, offset: u64, want: usize) -> &[u8] {
        self.cache.bytes(self.source, offset, want)
    }
}

/// A buffer of `len` zero bytes, or `None` when memory is short.
pub fn zeroed(len: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).ok()?;
    bytes.resize(len, 0);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_gives_what_the_file_has() {
        let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let source = data.as_slice();
        let mut window = Window::new(&source, 256).unwrap();
        assert_eq!(window.bytes(0, 4), &data[0..4]);
        assert_eq!(window.bytes(200, 100), &data[200..300]);
        assert_eq!(window.bytes(4990, 100), &data[4990..]);
        assert!(window.bytes(5000, 10).is_empty());
        assert_eq!(window.bytes(1, 1000).len(), 256);
        assert_eq!(window.bytes(1, 256), &data[1..257]);
    }

    /// A disk that gives a few bytes at a time.
    struct Dribble<'a>(&'a [u8]);

    impl Source for Dribble<'_> {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> usize {
            let at = (offset as usize).min(self.0.len());
            let count = out.len().min(7).min(self.0.len() - at);
            out[..count].copy_from_slice(&self.0[at..at + count]);
            count
        }
    }

    #[test]
    fn short_reads_are_filled_in() {
        let data: Vec<u8> = (0..100).collect();
        let source = Dribble(&data);
        let mut out = [0u8; 50];
        assert_eq!(source.read_full(10, &mut out), 50);
        assert_eq!(&out[..], &data[10..60]);
        let mut window = Window::new(&source, 32).unwrap();
        assert_eq!(window.bytes(90, 32), &data[90..]);
    }
}
