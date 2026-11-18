//! Address space IDs: which tag each space's TLB entries carry, where the hardware has
//! them (AArch64), so a CPU can change tables without forgetting everything it cached.
//!
//! A space has one ASID for as long as it lives, shared by every thread using it, and
//! gives it back when it goes, once the caller has flushed it everywhere. When the
//! ASIDs run out, which with 8-bit ASIDs is 255 live spaces, they're all taken back at
//! once: a new generation starts, every CPU's TLB is flushed of every user entry, and
//! each space gets a new ASID the next time it's loaded. The spaces loaded on a CPU at
//! that moment keep theirs, reserved in the new generation, since that CPU goes on
//! filling its TLB with them until it changes tables: no other space may be given one.
//! This is the scheme Linux uses on arm64, without its lock-free fast path: the caller
//! holds one lock around all of it.
//!
//! A space's *context* is its generation and ASID, `generation << 16 | asid`, zero
//! before it's first loaded. ASID 0 is the kernel's own tables'.

/// Every CPU's state and every ASID's, for up to `CPUS` CPUs.
pub struct Asids<const CPUS: usize> {
    bits: u32,
    generation: u64,
    /// One bit per ASID, taken in this generation.
    used: [u64; 1024],
    /// Where the search for a free ASID starts.
    next: usize,
    /// The context each CPU has loaded, or zero for the kernel's tables.
    active: [u64; CPUS],
    /// The context each CPU had loaded at the last rollover, kept for its space.
    reserved: [u64; CPUS],
}

const fn generation_of(context: u64) -> u64 {
    context >> 16
}

const fn asid_of(context: u64) -> u16 {
    context as u16
}

impl<const CPUS: usize> Asids<CPUS> {
    /// With `bits`-bit ASIDs: 8 or 16, or 0 for a port that has none, where every
    /// space is ASID 0 and nothing ever rolls over.
    pub const fn new(bits: u32) -> Self {
        Self { bits, generation: 1, used: [0; 1024], next: 1, active: [0; CPUS], reserved: [0; CPUS] }
    }

    /// How wide the ASIDs given out are.
    pub const fn bits(&self) -> u32 {
        self.bits
    }

    /// Gives out ASIDs of `bits` from now on, starting a generation, so the caller must
    /// flush every user entry from every CPU, as after an [`activate`](Self::activate)
    /// that says so. For a self-test to exercise the 8-bit path on a CPU with 16-bit
    /// ASIDs, which takes the narrower ones just as well. Not for a port without ASIDs.
    pub fn set_bits(&mut self, bits: u32) {
        self.bits = bits;
        self.rollover();
    }

    /// Rollovers so far, plus one.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// One past the largest ASID given out.
    const fn limit(&self) -> usize {
        if self.bits == 0 {
            0
        } else {
            1 << self.bits
        }
    }

    fn is_used(&self, asid: usize) -> bool {
        self.used[asid / 64] & 1 << (asid % 64) != 0
    }

    fn set_used(&mut self, asid: usize, used: bool) {
        if used {
            self.used[asid / 64] |= 1 << (asid % 64);
        } else {
            self.used[asid / 64] &= !(1 << (asid % 64));
        }
    }

    fn allocate(&mut self) -> Option<u16> {
        let limit = self.limit();
        let start = self.next.clamp(1, limit.max(1));
        let found = (start..limit).chain(1..start.min(limit)).find(|&asid| !self.is_used(asid))?;
        self.set_used(found, true);
        self.next = found + 1;
        Some(found as u16)
    }

    /// Starts a generation: every ASID is free but the ones loaded on a CPU now.
    fn rollover(&mut self) {
        self.generation += 1;
        self.used = [0; 1024];
        self.next = 1;
        for cpu in 0..CPUS {
            let context = self.active[cpu];
            self.reserved[cpu] = context;
            if context != 0 {
                self.set_used(usize::from(asid_of(context)), true);
            }
        }
    }

    /// The ASID the space whose context is `context` has on `cpu`, which is about to load
    /// its tables, and whether every user entry must be flushed from every CPU before
    /// it does: a new generation has begun. Updates the context if it changed.
    pub fn activate(&mut self, context: &mut u64, cpu: usize) -> (u16, bool) {
        if self.bits == 0 {
            return (0, false);
        }
        let old = *context;
        if old != 0 && generation_of(old) == self.generation {
            self.active[cpu] = old;
            return (asid_of(old), false);
        }
        let mut flush = false;
        let asid = if old != 0 && self.reserved.contains(&old) {
            // Loaded somewhere at the rollover: it keeps its ASID, which nobody else
            // could be given.
            let asid = asid_of(old);
            let new = self.generation << 16 | u64::from(asid);
            for reserved in &mut self.reserved {
                if *reserved == old {
                    *reserved = new;
                }
            }
            asid
        } else if old != 0 && usize::from(asid_of(old)) < self.limit() && !self.is_used(usize::from(asid_of(old))) {
            // Its old one, if nobody has it yet: flushed at the rollover, so clean.
            let asid = asid_of(old);
            self.set_used(usize::from(asid), true);
            asid
        } else {
            match self.allocate() {
                Some(asid) => asid,
                None => {
                    self.rollover();
                    flush = true;
                    // The loaded spaces hold at most `CPUS` of the new generation's.
                    self.allocate().unwrap_or(0)
                }
            }
        };
        let new = self.generation << 16 | u64::from(asid);
        *context = new;
        self.active[cpu] = new;
        (asid, flush)
    }

    /// `cpu` has loaded the kernel's tables.
    pub fn deactivate(&mut self, cpu: usize) {
        self.active[cpu] = 0;
    }

    /// The ASID whose entries are the space's on some CPU, to flush a page of: none if
    /// it hasn't been loaded since a rollover flushed everything it had.
    pub fn current(&self, context: u64) -> Option<u16> {
        let live = context != 0 && (generation_of(context) == self.generation || self.reserved.contains(&context));
        (live && self.bits != 0).then_some(asid_of(context))
    }

    /// The space whose context this is has gone, and no CPU has its tables loaded:
    /// gives its ASID back, and says which to flush first, if any CPU could still have
    /// entries tagged with it.
    pub fn release(&mut self, context: u64) -> Option<u16> {
        let asid = self.current(context)?;
        for cpu in 0..CPUS {
            if self.reserved[cpu] == context {
                self.reserved[cpu] = 0;
            }
            if self.active[cpu] == context {
                self.active[cpu] = 0;
            }
        }
        self.set_used(usize::from(asid), false);
        Some(asid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_keeps_its_asid_and_gives_it_back() {
        let mut asids = Asids::<2>::new(8);
        let (mut a, mut b) = (0, 0);
        let (first, flush) = asids.activate(&mut a, 0);
        assert!(!flush && first != 0);
        let (second, _) = asids.activate(&mut b, 1);
        assert_ne!(first, second);
        assert_eq!(asids.activate(&mut a, 1).0, first);
        asids.deactivate(1);
        asids.deactivate(0);
        assert_eq!(asids.release(a), Some(first));
        assert_eq!(asids.current(b), Some(second));
    }

    #[test]
    fn running_out_rolls_over_and_keeps_what_is_loaded() {
        let mut asids = Asids::<2>::new(8);
        let mut loaded = 0;
        let (kept, _) = asids.activate(&mut loaded, 1);
        // CPU 0 runs through more spaces than there are ASIDs, keeping them all alive.
        let mut contexts = vec![0u64; 300];
        let mut flushes = 0;
        for context in &mut contexts {
            let (asid, flush) = asids.activate(context, 0);
            flushes += usize::from(flush);
            assert_ne!(asid, 0);
            // Never the one CPU 1 is still using.
            assert_ne!(asid, kept);
        }
        assert_eq!(flushes, 1);
        assert_eq!(asids.generation(), 2);
        // CPU 1's space comes back with the same ASID.
        assert_eq!(asids.activate(&mut loaded, 1), (kept, false));
        // The spaces of the generation before are no longer anywhere, so get new ones
        // when they're next loaded, distinct from every live one of this generation.
        let mut seen = std::collections::HashSet::new();
        seen.insert(kept);
        for context in contexts.iter_mut().skip(200) {
            let (asid, flush) = asids.activate(context, 0);
            assert!(!flush);
            assert!(seen.insert(asid), "ASID {asid} given twice");
        }
    }

    #[test]
    fn a_stale_context_flushes_nothing_on_release() {
        let mut asids = Asids::<1>::new(8);
        let mut old = 0;
        asids.activate(&mut old, 0);
        asids.deactivate(0);
        // A rollover: `old` wasn't loaded, so its ASID may be someone else's now.
        asids.set_bits(8);
        let mut other = 0;
        let (_, flush) = asids.activate(&mut other, 0);
        assert!(!flush);
        assert_eq!(asids.current(old), None);
        assert_eq!(asids.release(old), None);
        assert!(asids.current(other).is_some());
    }

    /// 1024 live spaces on 8 CPUs, loaded, unloaded and replaced at random, against a
    /// model of every CPU's TLB: entries are tagged with the ASID a CPU loaded a space
    /// under and stay until a flush, a rollover's of everything or a release's of one
    /// ASID. A CPU that loads a space must never find entries of another space's under
    /// that space's ASID. With 8-bit ASIDs that's a rollover every 255 or so new loads,
    /// with up to 8 spaces kept across each; with 16, none.
    fn live_spaces_against_a_tlb_model(bits: u32) -> u64 {
        use std::collections::HashMap;
        const CPUS: usize = 8;
        const SPACES: usize = 1024;
        let mut asids = Asids::<CPUS>::new(bits);
        let mut contexts = vec![0u64; SPACES];
        // A space's identity: replaced spaces are new ones in the same index.
        let mut identity: Vec<usize> = (0..SPACES).collect();
        let mut next_identity = SPACES;
        let mut tlb: Vec<HashMap<u16, usize>> = vec![HashMap::new(); CPUS];
        let mut loaded: [Option<(usize, u16)>; CPUS] = [None; CPUS];
        let mut random = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        for _ in 0..200_000 {
            let cpu = (next() % CPUS as u64) as usize;
            match next() % 16 {
                0 => {
                    // Back to the kernel's tables.
                    asids.deactivate(cpu);
                    loaded[cpu] = None;
                }
                1 => {
                    // A space that isn't loaded anywhere goes, and a new one takes its place.
                    let space = (next() % SPACES as u64) as usize;
                    if loaded.iter().flatten().any(|&(index, _)| index == space) {
                        continue;
                    }
                    if let Some(asid) = asids.release(contexts[space]) {
                        for entries in &mut tlb {
                            entries.remove(&asid);
                        }
                    }
                    contexts[space] = 0;
                    identity[space] = next_identity;
                    next_identity += 1;
                }
                _ => {
                    let space = (next() % SPACES as u64) as usize;
                    let (asid, flush) = asids.activate(&mut contexts[space], cpu);
                    if flush {
                        tlb.iter_mut().for_each(HashMap::clear);
                    }
                    loaded[cpu] = Some((space, asid));
                }
            }
            // Every CPU with tables loaded walks them, and fills its TLB under the ASID
            // it loaded them with: any entry there already must be this space's.
            for (cpu, slot) in loaded.iter().enumerate() {
                let Some((space, asid)) = *slot else {
                    continue;
                };
                let who = identity[space];
                let entry = tlb[cpu].entry(asid).or_insert(who);
                assert_eq!(*entry, who, "CPU {cpu} found another space's entries under ASID {asid}");
            }
            // No two CPUs have different spaces loaded under one ASID.
            let mut seen = HashMap::new();
            for &(space, asid) in loaded.iter().flatten() {
                if bits != 0 {
                    assert_eq!(*seen.entry(asid).or_insert(identity[space]), identity[space]);
                }
            }
        }
        asids.generation()
    }

    #[test]
    fn asids_for_1024_live_spaces() {
        let rolled = live_spaces_against_a_tlb_model(8);
        assert!(rolled > 100, "the 8-bit path rolled over {rolled} times");
        assert_eq!(live_spaces_against_a_tlb_model(16), 1, "16 bits are enough for 1024");
    }

    #[test]
    fn no_asids_means_zero_always() {
        let mut asids = Asids::<1>::new(0);
        let mut context = 0;
        assert_eq!(asids.activate(&mut context, 0), (0, false));
        assert_eq!(asids.current(context), None);
        assert_eq!(asids.release(context), None);
    }
}
