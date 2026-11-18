//! The clocks: monotonic time in nanoseconds from the port's counter, and the wall clock
//! as an offset from it, from the board's RTC.
//!
//! The RTC counts whole seconds, so one reading puts the wall clock up to a second
//! behind. At boot the kernel takes that coarse reading at once, so the clock is never
//! the epoch on a machine that has a date, and then a task watches for the RTC's second
//! to change and takes the offset again at that edge, which is exact to the polling
//! interval. It's kept only if it puts the clock ahead (both readings are behind the true
//! time, so the larger is the better), which means the wall clock never steps back. Boot
//! doesn't wait for it.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use feos_hal::time::{ns_to_ticks, period_ns, ticks_to_ns, wall_offset};
use feos_hal::{Arch, Board, Clock};

use crate::{Machine, Port};

/// The latest monotonic time any CPU has returned. Counters on different CPUs may
/// disagree by a little (TSCs that firmware didn't line up), and a thread that moves
/// between them must still never see time go back.
static LATEST_NS: AtomicU64 = AtomicU64::new(0);

/// `wall = monotonic + WALL_OFFSET`, in nanoseconds. Zero, the epoch at boot, with no RTC.
static WALL_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Whether the board has an RTC that answered.
static HAVE_RTC: AtomicBool = AtomicBool::new(false);

/// Most polls of the RTC for its second to change, a millisecond apart: a little over a
/// second's worth, so a clock that has stopped can't keep the task forever.
const EDGE_POLLS: u32 = 1_500;

fn frequency() -> u64 {
    Port::services().frequency()
}

/// Nanoseconds since boot, from the counter: never less than any earlier reading on any
/// CPU.
pub fn monotonic_ns() -> u64 {
    let now = ticks_to_ns(Port::services().ticks(), frequency());
    LATEST_NS.fetch_max(now, Ordering::AcqRel).max(now)
}

/// Nanoseconds since the Unix epoch.
pub fn realtime_ns() -> u64 {
    monotonic_ns().saturating_add(WALL_OFFSET.load(Ordering::Acquire))
}

/// Wall-clock seconds since the epoch, if the machine has a clock that knows the date.
pub fn unix_time() -> Option<u64> {
    HAVE_RTC.load(Ordering::Acquire).then(|| realtime_ns() / feos_abi::NANOS_PER_SECOND)
}

/// The wall clock for files' timestamps, to the nanosecond, once the machine knows the
/// date: what the tree stamps writes, renames and the rest with (section 3.3 of the ABI
/// design). Without an RTC, nothing is stamped, as before.
pub fn file_time() -> Option<feos_fs::Timestamp> {
    HAVE_RTC.load(Ordering::Acquire).then(|| feos_fs::Timestamp(realtime_ns()))
}

/// The monotonic time a wall-clock time is, now: what a `CLOCK_REALTIME` deadline is
/// turned into when a call starts. One before boot is zero, which has passed.
pub fn realtime_to_monotonic(ns: u64) -> u64 {
    ns.saturating_sub(WALL_OFFSET.load(Ordering::Acquire))
}

/// The counter value at which the monotonic clock reads `ns`, for the scheduler to wait
/// for.
pub fn deadline_ticks(ns: u64) -> u64 {
    ns_to_ticks(ns, frequency())
}

/// Nanoseconds of one period of the counter, rounded up.
pub fn resolution_ns() -> u64 {
    period_ns(frequency())
}

/// Takes the RTC's reading, coarsely. Before the scheduler starts.
pub fn init() {
    if let Some(seconds) = Machine::unix_time() {
        WALL_OFFSET.store(wall_offset(seconds, monotonic_ns()), Ordering::Release);
        HAVE_RTC.store(true, Ordering::Release);
    }
}

/// Starts the task that takes the RTC's reading again as its second changes. Once the
/// scheduler runs.
pub fn start() {
    use feos_app::{Processes, SpawnIo};
    if !HAVE_RTC.load(Ordering::Acquire) {
        return;
    }
    let task = alloc::boxed::Box::new(|| {
        sample_edge();
        0
    });
    if let Err(error) = crate::sched::PROCESSES.spawn_task("rtc", SpawnIo::default(), task) {
        feos_console::println!("[time] no RTC task, the wall clock stays coarse: {error:?}");
    }
}

/// Waits for the RTC's second to change, and sets the wall clock from that moment.
fn sample_edge() {
    let Some(first) = Machine::unix_time() else {
        return;
    };
    for _ in 0..EDGE_POLLS {
        crate::sched::sleep_ms(1);
        match Machine::unix_time() {
            Some(seconds) if seconds != first => {
                // Both offsets are low, never high: each takes the counter after the RTC
                // has been read, so the second it reads began before that moment. This
                // one is low by the time since the edge (the poll interval, and any
                // preemption since the read), the coarse one by where in its second boot
                // read it. The larger is the closer, and keeping it means the wall clock
                // never steps back.
                WALL_OFFSET.fetch_max(wall_offset(seconds, monotonic_ns()), Ordering::AcqRel);
                return;
            }
            Some(_) => {}
            None => return,
        }
    }
}
