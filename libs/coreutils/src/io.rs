//! Byte-oriented input/output used by the core utilities.
//!
//! Commands never print directly. They write to a [`Output`] and read from an
//! [`Input`], which lets the shell give them a pipe, a file or the console
//! without the command knowing the difference.

use alloc::vec::Vec;

/// A byte source.
pub trait Input {
    /// Reads up to `out.len()` bytes. Returns zero at end of input.
    fn read(&mut self, out: &mut [u8]) -> usize;

    /// Whether this is an interactive terminal.
    fn is_terminal(&self) -> bool {
        false
    }

    /// Reads until end of input.
    fn read_to_end(&mut self) -> Vec<u8> {
        let mut data = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = self.read(&mut chunk);
            if read == 0 {
                return data;
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
}

/// An in-memory output, used for pipelines and redirection.
pub struct BytesOutput {
    data: Vec<u8>,
}

impl BytesOutput {
    /// Creates an empty output.
    pub fn new() -> Self {
        Self { data: Vec::new() }
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
        self.data.extend_from_slice(bytes);
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
