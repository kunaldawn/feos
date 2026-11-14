use std::{mem, num::NonZeroUsize};

#[derive(Clone, Debug)]
pub enum TinySet<T = u32> {
    Singular(T),
    Plural(Vec<T>),
}

const _: [u8; mem::size_of::<Vec<u32>>()] = [0; mem::size_of::<TinySet>()];

impl<T: Clone + Copy + Default + Eq + Ord + PartialEq + PartialOrd> Default for TinySet<T> {
    fn default() -> TinySet<T> {
        TinySet::<T>::empty()
    }
}

impl<T: Clone + Copy + Default + Eq + Ord + PartialEq + PartialOrd> TinySet<T> {
    pub fn empty() -> TinySet<T> {
        TinySet::Plural(Vec::new())
    }
    pub fn push(&mut self, item: T) {
        let mut v = match self {
            TinySet::Plural(v) if v.is_empty() => {
                *self = TinySet::Singular(item);
                return;
            }
            TinySet::Singular(t) => vec![std::mem::take(t)],
            TinySet::Plural(v) => mem::take(v),
        };
        match v.binary_search(&item) {
            Ok(_index) => {}
            Err(index) => v.insert(index, item),
        }
        *self = TinySet::Plural(v);
    }
    pub fn as_slice(&self) -> &[T] {
        match self {
            TinySet::Singular(t) => std::slice::from_ref(t),
            TinySet::Plural(v) => &v[..],
        }
    }
    pub fn len(&self) -> usize {
        match self {
            TinySet::Singular(_) => 1,
            TinySet::Plural(v) => v.len(),
        }
    }
    pub fn extend_from_slice(&mut self, slice: &[T]) {
        if slice.is_empty() {
            // yes, this happens pretty often
        } else if slice.len() == 1 && self.is_empty() {
            *self = TinySet::Singular(slice[0]);
        } else if self.is_empty() {
            *self = TinySet::Plural(slice.to_vec());
        } else {
            let old_self = mem::take(self);
            let mut new_self = Vec::with_capacity(slice.len() + old_self.len());
            let mut slice_iter = slice.iter().copied().peekable();
            let mut old_self_iter = old_self.iter().copied().peekable();
            loop {
                let item = match (slice_iter.peek().copied(), old_self_iter.peek().copied()) {
                    (None, None) => break,
                    (Some(s), None) => {
                        slice_iter.next();
                        s
                    }
                    (None, Some(t)) => {
                        old_self_iter.next();
                        t
                    }
                    (Some(s), Some(t)) if s < t => {
                        slice_iter.next();
                        s
                    }
                    (Some(s), Some(t)) if t < s => {
                        old_self_iter.next();
                        t
                    }
                    (Some(_s), Some(t)) => {
                        // must be equal
                        slice_iter.next();
                        old_self_iter.next();
                        t
                    }
                };
                new_self.push(item);
            }
            *self = if new_self.len() == 1 {
                TinySet::Singular(new_self[0])
            } else {
                TinySet::Plural(new_self)
            };
        }
    }
}

impl<T: Clone + Copy + Default + Eq + Ord + PartialEq + PartialOrd> std::ops::Deref for TinySet<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

#[derive(Clone, Copy)]
pub struct OptUsize(NonZeroUsize);

impl std::fmt::Debug for OptUsize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let u: usize = self.into();
        std::fmt::Debug::fmt(&u, f)
    }
}

impl OptUsize {
    pub fn new(u: usize) -> OptUsize {
        let n = !u;
        OptUsize(NonZeroUsize::new(n).unwrap())
    }
}

impl From<OptUsize> for usize {
    fn from(this: OptUsize) -> usize {
        let n: usize = this.0.into();
        !n
    }
}
impl From<&OptUsize> for usize {
    fn from(this: &OptUsize) -> usize {
        let n: usize = this.0.into();
        !n
    }
}
impl From<usize> for OptUsize {
    fn from(u: usize) -> OptUsize {
        let n = !u;
        OptUsize(NonZeroUsize::new(n).unwrap())
    }
}
