//! A split virtqueue.
//!
//! The memory layout suits every transport: a legacy device only learns where
//! the descriptors are, and expects the available ring straight after them and
//! the used ring at the next page. Two page-aligned areas give exactly that, and
//! a modern device is simply told the same addresses.

use feos_hal::dma;

/// The next descriptor in the chain is in `next`.
pub const DESC_F_NEXT: u16 = 1;
/// The device writes this buffer.
pub const DESC_F_WRITE: u16 = 2;

/// `VIRTQ_AVAIL_F_NO_INTERRUPT`: the queue is polled.
const AVAIL_F_NO_INTERRUPT: u16 = 1;
/// End of the free list.
const NONE: u16 = u16::MAX;

/// One virtqueue descriptor.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Descriptor {
    /// Physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// [`DESC_F_NEXT`] and [`DESC_F_WRITE`].
    pub flags: u16,
    /// Next descriptor, with [`DESC_F_NEXT`].
    pub next: u16,
}

impl Descriptor {
    const EMPTY: Self = Self {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    };
}

/// One buffer of a request.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    /// Physical address.
    pub addr: u64,
    /// Length in bytes.
    pub len: u32,
    /// True when the device writes it.
    pub device_writes: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UsedElement {
    id: u32,
    len: u32,
}

#[repr(C, align(4096))]
struct DriverArea<const N: usize> {
    desc: [Descriptor; N],
    avail_flags: u16,
    avail_idx: u16,
    avail_ring: [u16; N],
}

#[repr(C, align(4096))]
struct DeviceArea<const N: usize> {
    used_flags: u16,
    used_idx: u16,
    used_ring: [UsedElement; N],
}

/// A split virtqueue of `N` descriptors, where `N` is a power of two.
///
/// The device holds its address once the queue is activated, so a `Queue` must
/// not move after [`addresses`](Self::addresses) was handed to a transport: put
/// it in a static or a box.
#[repr(C)]
pub struct Queue<const N: usize> {
    driver: DriverArea<N>,
    device: DeviceArea<N>,
    /// Head of the free descriptor list.
    free_head: u16,
    free_count: u16,
    /// Next value for the available index.
    avail_next: u16,
    /// What the device has been told about.
    avail_published: u16,
    /// How far the used ring has been consumed.
    used_seen: u16,
}

impl<const N: usize> Queue<N> {
    /// An empty queue. Call [`reset`](Self::reset) before use.
    pub const fn new() -> Self {
        Self {
            driver: DriverArea {
                desc: [Descriptor::EMPTY; N],
                avail_flags: 0,
                avail_idx: 0,
                avail_ring: [0; N],
            },
            device: DeviceArea {
                used_flags: 0,
                used_idx: 0,
                used_ring: [UsedElement { id: 0, len: 0 }; N],
            },
            free_head: NONE,
            free_count: 0,
            avail_next: 0,
            avail_published: 0,
            used_seen: 0,
        }
    }

    /// Clears the rings and threads every descriptor on the free list.
    pub fn reset(&mut self) {
        for index in 0..N {
            self.driver.desc[index] = Descriptor {
                next: if index + 1 < N { index as u16 + 1 } else { NONE },
                ..Descriptor::EMPTY
            };
            self.driver.avail_ring[index] = 0;
            self.device.used_ring[index] = UsedElement { id: 0, len: 0 };
        }
        // Polled: the device should not interrupt for this queue.
        self.driver.avail_flags = AVAIL_F_NO_INTERRUPT;
        self.driver.avail_idx = 0;
        self.device.used_flags = 0;
        self.device.used_idx = 0;
        self.free_head = if N == 0 { NONE } else { 0 };
        self.free_count = N as u16;
        self.avail_next = 0;
        self.avail_published = 0;
        self.used_seen = 0;
    }

    /// Asks the device to interrupt when it finishes a chain, or not to.
    pub fn set_interrupts(&mut self, enabled: bool) {
        let flags = if enabled { 0 } else { AVAIL_F_NO_INTERRUPT };
        // SAFETY: the flags are part of the queue this driver owns, and the device
        // reads them concurrently.
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(self.driver.avail_flags), flags) };
        dma::wmb();
    }

    /// Queue size.
    pub const fn size(&self) -> u16 {
        N as u16
    }

    /// Physical `(descriptors, available ring, used ring)` addresses.
    pub fn addresses(&self) -> (u64, u64, u64) {
        (
            core::ptr::addr_of!(self.driver.desc) as u64,
            core::ptr::addr_of!(self.driver.avail_flags) as u64,
            core::ptr::addr_of!(self.device.used_flags) as u64,
        )
    }

    /// Queues `segments` as one chain, without telling the device yet. Returns
    /// the chain's head, or `None` when not enough descriptors are free.
    pub fn add(&mut self, segments: &[Segment]) -> Option<u16> {
        if segments.is_empty() || segments.len() > self.free_count as usize {
            return None;
        }
        let head = self.free_head;
        let mut current = head;
        for (index, segment) in segments.iter().enumerate() {
            let slot = current as usize;
            let next = self.driver.desc[slot].next;
            let mut flags = 0;
            if segment.device_writes {
                flags |= DESC_F_WRITE;
            }
            if index + 1 < segments.len() {
                flags |= DESC_F_NEXT;
            }
            self.driver.desc[slot] = Descriptor {
                addr: segment.addr,
                len: segment.len,
                flags,
                next,
            };
            if index + 1 == segments.len() {
                self.free_head = next;
            } else {
                current = next;
            }
        }
        self.free_count -= segments.len() as u16;
        self.push_avail(head);
        Some(head)
    }

    /// Offers an already written chain to the device again, as a receive queue
    /// does with each buffer it consumes.
    pub fn resubmit(&mut self, head: u16) {
        self.push_avail(head);
    }

    fn push_avail(&mut self, head: u16) {
        let slot = self.avail_next as usize & (N - 1);
        self.driver.avail_ring[slot] = head;
        self.avail_next = self.avail_next.wrapping_add(1);
    }

    /// Publishes queued chains. Returns `true` when the device has to be
    /// notified; the caller then calls [`Transport::queue_notify`].
    ///
    /// [`Transport::queue_notify`]: crate::Transport::queue_notify
    pub fn publish(&mut self) -> bool {
        if self.avail_published == self.avail_next {
            return false;
        }
        self.avail_published = self.avail_next;
        dma::wmb();
        // SAFETY: the index is part of the queue this driver owns. It's volatile
        // because the device reads it concurrently.
        unsafe {
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!(self.driver.avail_idx),
                self.avail_next,
            );
        }
        // The notification must not pass the index on its way to the device.
        dma::wmb();
        true
    }

    /// Takes the next finished chain as `(head, bytes written by the device)`.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        // SAFETY: the device writes this index concurrently, so it's volatile.
        let used =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!(self.device.used_idx)) };
        if used == self.used_seen {
            return None;
        }
        // The ring entry and the buffers were written before the index.
        dma::rmb();
        let slot = self.used_seen as usize & (N - 1);
        // SAFETY: `slot` is inside the ring. The device owns these bytes, so a
        // plain read could be cached across the loop that polls them.
        let (id, len) = unsafe {
            let element = core::ptr::addr_of!(self.device.used_ring[slot]);
            (
                core::ptr::read_volatile(core::ptr::addr_of!((*element).id)),
                core::ptr::read_volatile(core::ptr::addr_of!((*element).len)),
            )
        };
        self.used_seen = self.used_seen.wrapping_add(1);
        Some((id.min(u16::MAX as u32) as u16, len))
    }

    /// Returns a chain's descriptors to the free list. A head from a confused
    /// device is ignored rather than trusted.
    pub fn free_chain(&mut self, head: u16) {
        let mut current = head as usize;
        let mut walked = 0usize;
        while current < N && walked < N {
            let flags = self.driver.desc[current].flags;
            let next = self.driver.desc[current].next;
            walked += 1;
            if flags & DESC_F_NEXT == 0 {
                self.driver.desc[current].next = self.free_head;
                self.free_head = head;
                self.free_count = (self.free_count as usize + walked).min(N) as u16;
                return;
            }
            current = next as usize;
        }
    }

    /// Descriptors available for [`add`](Self::add).
    pub const fn free_count(&self) -> u16 {
        self.free_count
    }
}

impl<const N: usize> Default for Queue<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the device's part: completes the oldest published chain.
    fn complete<const N: usize>(queue: &mut Queue<N>, seen: &mut u16, len: u32) {
        let head = queue.driver.avail_ring[*seen as usize & (N - 1)];
        *seen = seen.wrapping_add(1);
        let slot = queue.device.used_idx as usize & (N - 1);
        queue.device.used_ring[slot] = UsedElement {
            id: head as u32,
            len,
        };
        queue.device.used_idx = queue.device.used_idx.wrapping_add(1);
    }

    fn segment(addr: u64, device_writes: bool) -> Segment {
        Segment {
            addr,
            len: 16,
            device_writes,
        }
    }

    fn boxed<const N: usize>() -> Box<Queue<N>> {
        let mut queue = Box::new(Queue::<N>::new());
        queue.reset();
        queue
    }

    #[test]
    fn the_layout_is_what_a_legacy_device_computes() {
        let queue = boxed::<64>();
        let (desc, avail, used) = queue.addresses();
        assert_eq!(desc % 4096, 0);
        assert_eq!(avail, desc + 16 * 64);
        // Legacy: the used ring is at the next page boundary after the
        // available ring's last field.
        let avail_end = avail + 6 + 2 * 64;
        assert_eq!(used, avail_end.next_multiple_of(4096));

        let large = boxed::<256>();
        let (desc, avail, used) = large.addresses();
        assert_eq!(used, (avail + 6 + 2 * 256).next_multiple_of(4096));
        assert_eq!(desc % 4096, 0);
    }

    #[test]
    fn chains_link_flag_and_return_to_the_free_list() {
        let mut queue = boxed::<8>();
        let head = queue
            .add(&[segment(1, false), segment(2, true), segment(3, true)])
            .unwrap();
        assert_eq!(queue.free_count(), 5);
        let first = queue.driver.desc[head as usize];
        assert_eq!((first.addr, first.flags), (1, DESC_F_NEXT));
        let second = queue.driver.desc[first.next as usize];
        assert_eq!((second.addr, second.flags), (2, DESC_F_NEXT | DESC_F_WRITE));
        let third = queue.driver.desc[second.next as usize];
        assert_eq!((third.addr, third.flags), (3, DESC_F_WRITE));

        assert!(queue.publish());
        assert!(!queue.publish(), "nothing new to announce");
        assert_eq!(queue.driver.avail_idx, 1);

        assert_eq!(queue.pop_used(), None);
        let mut seen = 0;
        complete(&mut queue, &mut seen, 17);
        assert_eq!(queue.pop_used(), Some((head, 17)));
        queue.free_chain(head);
        assert_eq!(queue.free_count(), 8);
    }

    #[test]
    fn a_chain_too_long_is_refused_and_changes_nothing() {
        let mut queue = boxed::<2>();
        let segments = [segment(1, false), segment(2, false), segment(3, false)];
        assert_eq!(queue.add(&segments), None);
        assert_eq!(queue.free_count(), 2);
        assert!(queue.add(&segments[..2]).is_some());
        assert_eq!(queue.add(&segments[..1]), None);
    }

    #[test]
    fn the_rings_wrap() {
        let mut queue = boxed::<4>();
        let mut seen = 0;
        for round in 0..1000u64 {
            let head = queue.add(&[segment(round, false)]).unwrap();
            assert!(queue.publish());
            complete(&mut queue, &mut seen, 0);
            let (done, _) = queue.pop_used().unwrap();
            assert_eq!(done, head);
            assert_eq!(queue.driver.desc[done as usize].addr, round);
            queue.free_chain(done);
        }
        assert_eq!(queue.free_count(), 4);
    }

    #[test]
    fn a_bad_head_from_the_device_is_ignored() {
        let mut queue = boxed::<4>();
        queue.free_chain(99);
        assert_eq!(queue.free_count(), 4);
    }
}
