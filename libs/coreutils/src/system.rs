//! Platform facts the utilities need without depending on `feos-app`.

/// Memory accounting reported by the kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemoryInfo {
    /// Total bytes managed by the kernel heap.
    pub total: usize,
    /// Bytes currently in use.
    pub used: usize,
    /// Bytes still available.
    pub free: usize,
}

/// System information exposed by the shell host.
///
/// The board and kernel implement this; unit tests use [`NoSystem`].
pub trait System: Sync {
    /// Operating system name (`uname -s`).
    fn sysname(&self) -> &'static str {
        "FEOS"
    }

    /// Network node name (`uname -n`, `hostname`).
    fn nodename(&self) -> &'static str {
        "feos"
    }

    /// Kernel release (`uname -r`).
    fn release(&self) -> &'static str {
        "0.1.0"
    }

    /// Kernel version (`uname -v`).
    fn version(&self) -> &'static str {
        "feos"
    }

    /// Machine hardware name (`uname -m`).
    fn machine(&self) -> &'static str {
        "aarch64"
    }

    /// Effective user name (`whoami`).
    fn username(&self) -> &'static str {
        "feos"
    }

    /// Numeric user id.
    fn uid(&self) -> u32 {
        1000
    }

    /// Numeric group id.
    fn gid(&self) -> u32 {
        1000
    }

    /// Milliseconds since boot.
    fn uptime_ms(&self) -> u64 {
        0
    }

    /// Timer frequency in Hz.
    fn clock_hz(&self) -> u64 {
        0
    }

    /// Number of online CPUs.
    fn cpu_count(&self) -> usize {
        1
    }

    /// Calling process id.
    fn pid(&self) -> u32 {
        1
    }

    /// Memory accounting, when the kernel provides it.
    fn memory(&self) -> Option<MemoryInfo> {
        None
    }
}

/// A system implementation that reports only static defaults.
pub struct NoSystem;

impl System for NoSystem {}
