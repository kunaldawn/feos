//! Deterministic fuzzing shared by the parser crates' `fuzz` tests.
//!
//! Each test binary installs [`LimitAllocator`], then mutates valid seeds with a
//! fixed-seed generator and runs the parser on [`run_constrained`]'s thread. The
//! runs are repeatable and bounded, and need nothing beyond `cargo test`. The
//! constraints match the kernel, so these count as failures:
//!
//! - a panic, including an arithmetic overflow under a dev profile,
//! - an infallible allocation past the heap limit, which aborts the binary,
//! - recursion past the stack limit, which faults the binary.
//!
//! [`with_request_limit`] also fails every single request above a size, on the
//! calling thread only, so that code which asks with `try_reserve` sees its
//! allocation fail without the binary aborting: the shell's byte budgets.
//!
//! Other crates include this file with `#[path]`, so it needs no shared crate.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The kernel heap arena (`kernel/src/allocator.rs`).
pub const HEAP_LIMIT: usize = 64 * 1024 * 1024;

/// A kernel process stack (`STACK_SIZE` in `kernel/src/sched.rs`).
pub const KERNEL_STACK: usize = 128 * 1024;

/// The stack a run gets.
///
/// The kernel is built optimised. Unoptimised frames are several times larger,
/// so a dev-profile run gets more room to leave the same margin. The `fuzz`
/// profile is optimised but keeps debug assertions, so `make fuzz` sets
/// `FEOS_FUZZ_STACK` to [`KERNEL_STACK`] instead. `--release` uses it too.
pub fn stack_limit() -> usize {
    let configured = std::env::var("FEOS_FUZZ_STACK").ok().and_then(|value| value.parse().ok());
    configured.unwrap_or(if cfg!(debug_assertions) { 512 * 1024 } else { KERNEL_STACK })
}

static USED: AtomicUsize = AtomicUsize::new(0);

std::thread_local! {
    /// The largest single request this thread may make. A `const` initialiser
    /// and a `Copy` value: reading it from the allocator allocates nothing.
    static REQUEST_LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn request_allowed(size: usize) -> bool {
    REQUEST_LIMIT.try_with(|limit| size <= limit.get()).unwrap_or(true)
}

/// Runs `body` with every request on this thread above `bytes` failing.
///
/// An infallible allocation that fails still aborts the binary, so what this
/// finds is a large buffer taken without `try_reserve`: an allocation the kernel
/// can't refuse either.
pub fn with_request_limit<R>(bytes: usize, body: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST_LIMIT.with(|limit| limit.set(self.0));
        }
    }
    let _restore = Restore(REQUEST_LIMIT.with(|limit| limit.replace(bytes)));
    body()
}

/// How many requests [`with_request_limit`] has refused, in all threads.
pub fn refused() -> usize {
    REFUSED.load(Ordering::Relaxed)
}

static REFUSED: AtomicUsize = AtomicUsize::new(0);

/// Fails allocations once [`HEAP_LIMIT`] bytes are live, and those above a
/// thread's [`with_request_limit`].
pub struct LimitAllocator;

// SAFETY: every call forwards to `System` with the caller's layout. The counter
// only decides whether to forward.
unsafe impl GlobalAlloc for LimitAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !request_allowed(layout.size()) {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        if USED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size() > HEAP_LIMIT {
            USED.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        USED.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() && !request_allowed(new_size) {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        let grown = new_size.saturating_sub(layout.size());
        if USED.fetch_add(grown, Ordering::Relaxed) + grown > HEAP_LIMIT {
            USED.fetch_sub(grown, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        let moved = System.realloc(ptr, layout, new_size);
        if moved.is_null() {
            USED.fetch_sub(grown, Ordering::Relaxed);
        } else {
            USED.fetch_sub(layout.size().saturating_sub(new_size), Ordering::Relaxed);
        }
        moved
    }
}

/// Runs `body` on a thread with the kernel's stack size, one at a time, so that
/// each run has the whole heap budget to itself.
pub fn run_constrained(body: impl FnOnce() + Send + 'static) {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let thread = std::thread::Builder::new()
        .stack_size(stack_limit())
        .spawn(body)
        .expect("spawn fuzz thread");
    if let Err(panic) = thread.join() {
        std::panic::resume_unwind(panic);
    }
}

/// xorshift64*: small, fixed and good enough to pick mutations.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A value below `limit`, which must not be zero.
    pub fn below(&mut self, limit: usize) -> usize {
        (self.next_u64() % limit as u64) as usize
    }
}

const INTERESTING_BYTES: [u8; 8] = [0x00, 0x01, 0x20, 0x7f, 0x80, 0xfe, 0xff, b'\n'];
const INTERESTING_WORDS: [u32; 8] = [
    0,
    1,
    2,
    0x7fff_ffff,
    0x8000_0000,
    0x0fff_fff7,
    0xffff_fff7,
    0xffff_ffff,
];

/// Applies one to eight random edits to `data`, keeping it under `max_len`.
pub fn mutate(rng: &mut Rng, data: &mut Vec<u8>, max_len: usize) {
    for _ in 0..1 + rng.below(8) {
        if data.is_empty() {
            data.push(rng.next_u64() as u8);
            continue;
        }
        let at = rng.below(data.len());
        match rng.below(8) {
            0 => data[at] ^= 1 << rng.below(8),
            1 => data[at] = INTERESTING_BYTES[rng.below(INTERESTING_BYTES.len())],
            2 => data[at] = rng.next_u64() as u8,
            3 => {
                let word = INTERESTING_WORDS[rng.below(INTERESTING_WORDS.len())].to_le_bytes();
                let end = (at + 4).min(data.len());
                data[at..end].copy_from_slice(&word[..end - at]);
            }
            4 => data.truncate(at),
            5 => {
                let end = (at + 1 + rng.below(32)).min(data.len());
                data.drain(at..end);
            }
            6 => {
                // Repeat a run, which is what grows nesting and counts.
                let end = (at + 1 + rng.below(64)).min(data.len());
                let run = data[at..end].to_vec();
                for _ in 0..1 + rng.below(16) {
                    if data.len() + run.len() > max_len {
                        break;
                    }
                    data.splice(at..at, run.iter().copied());
                }
            }
            _ => {
                // Copy a run elsewhere, which splices valid structure.
                let end = (at + 1 + rng.below(64)).min(data.len());
                let run = data[at..end].to_vec();
                let to = rng.below(data.len());
                let stop = (to + run.len()).min(data.len());
                data[to..stop].copy_from_slice(&run[..stop - to]);
            }
        }
    }
}

/// Multiplies an iteration count by `FEOS_FUZZ_SCALE`, for longer manual runs.
pub fn scaled(iterations: usize) -> usize {
    let scale = std::env::var("FEOS_FUZZ_SCALE").ok();
    iterations * scale.and_then(|value| value.parse().ok()).unwrap_or(1)
}

/// Feeds `iterations` mutations of each seed to `check`.
pub fn fuzz_bytes(
    seed: u64,
    seeds: &[Vec<u8>],
    iterations: usize,
    max_len: usize,
    mut check: impl FnMut(&[u8]),
) {
    let mut rng = Rng::new(seed);
    let mut input = Vec::with_capacity(max_len);
    for base in seeds {
        check(base);
        for _ in 0..scaled(iterations) {
            input.clear();
            input.extend_from_slice(base);
            mutate(&mut rng, &mut input, max_len);
            check(&input);
        }
    }
}
