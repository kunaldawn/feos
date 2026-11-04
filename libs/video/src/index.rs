//! The sample table behind the stateful decoder: where each coded frame lives,
//! when it's displayed, and which frames decoding can start from.
//!
//! Tables hold byte offsets rather than slices, so a decoder can own its input.
//! They're built once when a file is opened, with fallible allocation under a
//! limit, because the kernel heap is small and exhaustion is fatal there.

use alloc::vec::Vec;

use crate::error::{Result, VideoError};

/// Resource limits for [`VideoDecoder`](crate::VideoDecoder).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecoderLimits {
    /// Most coded frames an index may hold.
    pub max_frames: usize,
    /// Largest accepted decoded frame width.
    pub max_width: u32,
    /// Largest accepted decoded frame height.
    pub max_height: u32,
    /// Largest accepted estimate of the codec's working memory.
    pub max_memory: usize,
}

impl Default for DecoderLimits {
    fn default() -> Self {
        Self {
            max_frames: 1 << 18,
            max_width: 16384,
            max_height: 16384,
            max_memory: 48 * 1024 * 1024,
        }
    }
}

/// The frame may be a place to start decoding, according to the container.
pub(crate) const FLAG_KEY: u8 = 1;
/// The sample carries H.264 parameter sets that later frames may rely on.
pub(crate) const FLAG_PARAMS: u8 = 2;

/// One coded frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub offset: u32,
    pub size: u32,
    /// Presentation time in milliseconds, or a raw value before
    /// [`Index::finish_timing`].
    pub pts_ms: i32,
    pub flags: u8,
}

impl Sample {
    pub(crate) fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }
}

/// A sample table, in decode order, and display times, in display order.
#[derive(Default)]
pub(crate) struct Index {
    pub samples: Vec<Sample>,
    /// Display time of each frame in display order.
    pub times_ms: Vec<u32>,
    /// Time from the last frame's start to the end of the video.
    pub last_delay_ms: u32,
}

/// Appends to a vector, reporting exhaustion instead of aborting.
pub(crate) fn try_push<T>(items: &mut Vec<T>, item: T) -> Result<()> {
    items.try_reserve(1).map_err(|_| VideoError::OutOfMemory)?;
    items.push(item);
    Ok(())
}

impl Index {
    /// Appends a sample that must lie inside an input of `input_len` bytes.
    pub(crate) fn push(
        &mut self,
        limits: &DecoderLimits,
        input_len: usize,
        offset: usize,
        size: usize,
        pts_ms: i64,
        flags: u8,
    ) -> Result<()> {
        if self.samples.len() >= limits.max_frames {
            return Err(VideoError::LimitExceeded("too many frames"));
        }
        let end = offset.checked_add(size).ok_or(VideoError::Overflow)?;
        if end > input_len {
            return Err(VideoError::UnexpectedEnd);
        }
        let (Ok(offset), Ok(size)) = (u32::try_from(offset), u32::try_from(size)) else {
            return Err(VideoError::LimitExceeded("file too large"));
        };
        let pts_ms = pts_ms.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        try_push(
            &mut self.samples,
            Sample {
                offset,
                size,
                pts_ms,
                flags,
            },
        )
    }

    /// Normalises sample times to start at zero and builds the display-order
    /// time list. Display order is the sorted presentation times: `decode` and
    /// the demuxers number frames in display order.
    pub(crate) fn finish_timing(&mut self, fallback_delay_ms: u32) -> Result<()> {
        let min = self
            .samples
            .iter()
            .map(|sample| sample.pts_ms)
            .min()
            .unwrap_or(0);
        self.times_ms
            .try_reserve_exact(self.samples.len())
            .map_err(|_| VideoError::OutOfMemory)?;
        for sample in &mut self.samples {
            sample.pts_ms = sample.pts_ms.saturating_sub(min);
            self.times_ms.push(sample.pts_ms.max(0) as u32);
        }
        self.times_ms.sort_unstable();
        self.last_delay_ms = match self.times_ms.as_slice() {
            [.., before, last] if last > before => last - before,
            _ => fallback_delay_ms,
        };
        Ok(())
    }

    /// Fills in uniform timing from a frame rate.
    pub(crate) fn set_uniform_timing(&mut self, frame_rate: Option<(u32, u32)>) {
        let (num, den) = frame_rate.unwrap_or((0, 0));
        for (i, sample) in self.samples.iter_mut().enumerate() {
            sample.pts_ms = if num == 0 || den == 0 {
                0
            } else {
                ((i as u64)
                    .saturating_mul(1000)
                    .saturating_mul(den as u64)
                    / num as u64)
                    .min(i32::MAX as u64) as i32
            };
        }
    }

    /// Time of the frame at `index` in display order.
    pub(crate) fn time_ms(&self, index: u32) -> Option<u32> {
        self.times_ms.get(index as usize).copied()
    }

    /// Delay from the frame at `index` to the next one.
    pub(crate) fn delay_ms(&self, index: u32) -> u32 {
        let index = index as usize;
        match (self.times_ms.get(index), self.times_ms.get(index + 1)) {
            (Some(now), Some(next)) if next > now => next - now,
            (Some(_), None) => self.last_delay_ms,
            _ => 0,
        }
    }

    /// Total running time.
    pub(crate) fn duration_ms(&self) -> u64 {
        self.times_ms
            .last()
            .map_or(0, |&last| last as u64 + self.last_delay_ms as u64)
    }

    /// The frame on screen at `ms`.
    pub(crate) fn frame_at_time(&self, ms: u64) -> u32 {
        let ms = ms.min(u32::MAX as u64) as u32;
        let after = self.times_ms.partition_point(|&time| time <= ms);
        after.saturating_sub(1) as u32
    }

    /// Bytes of a sample in `input`.
    pub(crate) fn bytes<'i>(&self, input: &'i [u8], index: usize) -> &'i [u8] {
        let Some(sample) = self.samples.get(index) else {
            return &[];
        };
        let start = sample.offset as usize;
        input
            .get(start..start.saturating_add(sample.size as usize))
            .unwrap_or(&[])
    }
}
