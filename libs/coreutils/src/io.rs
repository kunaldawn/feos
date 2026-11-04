//! Byte-oriented input/output used by the core utilities.
//!
//! Commands never print directly. They write to a [`Output`] and read from an
//! [`Input`], which lets the shell give them a pipe, a file or the console
//! without the command knowing the difference.

use alloc::vec::Vec;

use crate::fs::FsError;

/// Largest amount of data one stream buffer may hold.
///
/// Pipelines are buffered in memory and the kernel heap is a fixed arena, so a
/// runaway producer has to fail with an error before it exhausts the heap.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// A byte source.
pub trait Input {
    /// Reads up to `out.len()` bytes. Returns zero at end of input.
    fn read(&mut self, out: &mut [u8]) -> usize;

    /// Whether this is an interactive terminal.
    fn is_terminal(&self) -> bool {
        false
    }

    /// Reads bytes with no line discipline, for the interactive line editor.
    fn read_raw(&mut self, out: &mut [u8]) -> usize {
        self.read(out)
    }

    /// Reads raw bytes only when they arrive almost immediately, returning
    /// zero otherwise. This is how a lone Escape is told from a sequence.
    fn read_pending(&mut self, out: &mut [u8]) -> usize {
        self.read_raw(out)
    }

    /// Reads until end of input, keeping whatever fits the stream budget.
    fn read_to_end(&mut self) -> Vec<u8> {
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = self.read(&mut chunk);
            if read == 0
                || data.len() + read > MAX_STREAM_BYTES
                || data.try_reserve(read).is_err()
            {
                return data;
            }
            data.extend_from_slice(&chunk[..read]);
        }
    }

    /// Reads until end of input, or fails with [`FsError::NoSpace`] when the
    /// data would exceed `limit` bytes or the heap cannot hold it.
    fn try_read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, FsError> {
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = self.read(&mut chunk);
            if read == 0 {
                return Ok(data);
            }
            if data.len() + read > limit || data.try_reserve(read).is_err() {
                return Err(FsError::NoSpace);
            }
            data.extend_from_slice(&chunk[..read]);
        }
    }

    /// Reads one line, including the trailing newline when present.
    fn read_line(&mut self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if self.read(&mut byte) == 0 {
                return if line.is_empty() { None } else { Some(line) };
            }
            if line.len() >= MAX_STREAM_BYTES || line.try_reserve(1).is_err() {
                return Some(line);
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                return Some(line);
            }
        }
    }
}

/// A byte sink.
pub trait Output {
    /// Writes all bytes.
    fn write(&mut self, bytes: &[u8]);

    /// Flushes buffered output, when the sink buffers.
    fn flush(&mut self) {}

    /// Whether this is an interactive terminal.
    fn is_terminal(&self) -> bool {
        false
    }

    /// Whether writes are being discarded because the sink overflowed or the
    /// command was cancelled. Long-running commands poll this and stop.
    fn is_broken(&self) -> bool {
        false
    }

    /// Writes a string.
    fn write_str(&mut self, text: &str) {
        self.write(text.as_bytes());
    }
}

/// An in-memory input, used for pipes and file redirection.
pub struct BytesInput {
    data: Vec<u8>,
    position: usize,
}

impl BytesInput {
    /// Creates an input over `data`.
    pub fn new(data: Vec<u8>) -> Self {
        Self { data, position: 0 }
    }

    /// Creates an empty input.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// The number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }
}

impl Input for BytesInput {
    fn read(&mut self, out: &mut [u8]) -> usize {
        let available = self.data.len().saturating_sub(self.position);
        let count = available.min(out.len());
        if count == 0 {
            return 0;
        }
        out[..count].copy_from_slice(&self.data[self.position..self.position + count]);
        self.position += count;
        count
    }

    fn try_read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, FsError> {
        if self.remaining() > limit {
            return Err(FsError::NoSpace);
        }
        // The buffer moves out instead of being copied: a pipeline stage never
        // needs two copies of its input.
        let mut data = core::mem::take(&mut self.data);
        if self.position > 0 {
            data.drain(..self.position.min(data.len()));
            self.position = 0;
        }
        Ok(data)
    }
}

/// An in-memory output, used for pipelines and redirection.
pub struct BytesOutput {
    data: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl BytesOutput {
    /// Creates an empty output.
    pub fn new() -> Self {
        Self::with_limit(MAX_STREAM_BYTES)
    }

    /// Creates an empty output that keeps at most `limit` bytes.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            data: Vec::new(),
            limit,
            overflowed: false,
        }
    }

    /// True when bytes were dropped because of the limit or a full heap.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns the accumulated bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Borrows the accumulated bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
}

impl Default for BytesOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl Output for BytesOutput {
    fn write(&mut self, bytes: &[u8]) {
        if self.overflowed
            || self.data.len().saturating_add(bytes.len()) > self.limit
            || self.data.try_reserve(bytes.len()).is_err()
        {
            self.overflowed = true;
            return;
        }
        self.data.extend_from_slice(bytes);
    }

    fn is_broken(&self) -> bool {
        self.overflowed
    }
}

/// An output that discards everything.
pub struct NullOutput;

impl Output for NullOutput {
    fn write(&mut self, _bytes: &[u8]) {}
}

/// An input that is always at end of file.
pub struct NullInput;

impl Input for NullInput {
    fn read(&mut self, _out: &mut [u8]) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_output_stops_at_its_limit() {
        let mut out = BytesOutput::with_limit(4);
        out.write(b"abc");
        assert!(!out.is_broken());
        out.write(b"de");
        assert!(out.is_broken());
        assert_eq!(out.bytes(), b"abc");
    }

    #[test]
    fn bytes_input_moves_its_remainder_out() {
        let mut input = BytesInput::new(b"abcd".to_vec());
        let mut first = [0u8; 1];
        assert_eq!(input.read(&mut first), 1);
        assert_eq!(input.try_read_to_end(10).unwrap(), b"bcd");
        assert_eq!(input.remaining(), 0);
    }

    #[test]
    fn stream_reads_respect_the_limit() {
        let mut input = BytesInput::new(alloc::vec![0; 32]);
        assert_eq!(input.try_read_to_end(16), Err(FsError::NoSpace));
    }
}
