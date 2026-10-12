//! Page-level VMA source identities. Splits replace an existing logical
//! mapping's payload, while ordinary mapping births add a distinct lineage.
//! Page state/statistics and all accounting remain in `mm::page`.
use super::ucontext::LockedVMA;
use alloc::sync::Arc;
use core::mem;
use hashbrown::HashMap;

#[derive(Debug)]
pub(super) enum PageVmaSet {
    Empty,
    One(Arc<LockedVMA>),
    Two(Arc<LockedVMA>, Arc<LockedVMA>),
    Many(HashMap<u64, Arc<LockedVMA>>),
}

impl PageVmaSet {
    pub(super) fn new() -> Self {
        Self::Empty
    }

    pub(super) fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::One(_) => 1,
            Self::Two(_, _) => 2,
            Self::Many(vmas) => vmas.len(),
        }
    }

    pub(super) fn contains(&self, vma: &LockedVMA) -> bool {
        match self {
            Self::Empty => false,
            Self::One(existing) => existing.as_ref() == vma,
            Self::Two(first, second) => first.as_ref() == vma || second.as_ref() == vma,
            Self::Many(vmas) => vmas
                .get(&vma.lineage_id())
                .is_some_and(|existing| existing.id() == vma.id()),
        }
    }

    pub(super) fn insert(&mut self, vma: Arc<LockedVMA>) -> bool {
        match self {
            Self::Empty => {
                *self = Self::One(vma);
                true
            }
            Self::One(existing) => {
                if existing.as_ref() == vma.as_ref() {
                    false
                } else {
                    assert_ne!(existing.lineage_id(), vma.lineage_id());
                    let Self::One(existing) = mem::replace(self, Self::Empty) else {
                        unreachable!();
                    };
                    *self = Self::Two(existing, vma);
                    true
                }
            }
            Self::Two(first, second) => {
                if first.as_ref() == vma.as_ref() || second.as_ref() == vma.as_ref() {
                    return false;
                }
                assert_ne!(first.lineage_id(), vma.lineage_id());
                assert_ne!(second.lineage_id(), vma.lineage_id());

                // Allocate before taking the inline state so allocation failure cannot discard
                // either existing reverse mapping.
                let mut vmas = HashMap::with_capacity(3);
                let Self::Two(first, second) = mem::replace(self, Self::Empty) else {
                    unreachable!();
                };
                vmas.insert(first.lineage_id(), first);
                vmas.insert(second.lineage_id(), second);
                vmas.insert(vma.lineage_id(), vma);
                *self = Self::Many(vmas);
                true
            }
            Self::Many(vmas) => {
                if let Some(existing) = vmas.get(&vma.lineage_id()) {
                    assert_eq!(
                        existing.id(),
                        vma.id(),
                        "one PFN cannot belong to two fragments of the same VMA lineage"
                    );
                    false
                } else {
                    vmas.insert(vma.lineage_id(), vma);
                    true
                }
            }
        }
    }

    /// Splitting preserves the physical mapping: replace its identity in the
    /// existing slot, without growing a hash table or transient map count.
    pub(super) fn replace_for_split(&mut self, old: &LockedVMA, new: Arc<LockedVMA>) -> bool {
        assert_eq!(old.lineage_id(), new.lineage_id());
        let slot = match self {
            Self::Empty => None,
            Self::One(existing) if existing.id() == old.id() => Some(existing),
            Self::Two(first, _) if first.id() == old.id() => Some(first),
            Self::Two(_, second) if second.id() == old.id() => Some(second),
            Self::Many(vmas) => vmas
                .get_mut(&old.lineage_id())
                .filter(|existing| existing.id() == old.id()),
            _ => None,
        };
        if let Some(slot) = slot {
            // The split caller pins old for the entire transaction, so this
            // cannot destroy a VMA body beneath the page lock.
            *slot = new;
            true
        } else {
            false
        }
    }

    /// The mlock subset may keep its empty capacity until its containing page
    /// retires; removing a source never allocates or frees backing here.
    pub(super) fn remove_keep_backing(&mut self, vma: &LockedVMA) {
        if let Self::Many(vmas) = self {
            if vmas
                .get(&vma.lineage_id())
                .is_some_and(|existing| existing.id() == vma.id())
            {
                vmas.remove(&vma.lineage_id());
            }
        } else {
            let (_, retired) = self.remove_deferred(vma);
            debug_assert!(retired.is_none());
        }
    }

    /// The caller pins this VMA and retires an empty heap backing outside
    /// resource locks. No other live source may be taken out of this set.
    pub(super) fn remove_deferred(
        &mut self,
        vma: &LockedVMA,
    ) -> (bool, Option<HashMap<u64, Arc<LockedVMA>>>) {
        match self {
            Self::Empty => (false, None),
            Self::One(existing) => {
                if existing.as_ref() != vma {
                    return (false, None);
                }
                *self = Self::Empty;
                (true, None)
            }
            Self::Two(first, second) => {
                let remove_first = first.as_ref() == vma;
                let remove_second = second.as_ref() == vma;
                if !remove_first && !remove_second {
                    return (false, None);
                }

                let Self::Two(first, second) = mem::replace(self, Self::Empty) else {
                    unreachable!();
                };
                *self = Self::One(if remove_first { second } else { first });
                (true, None)
            }
            Self::Many(vmas) => {
                let removed = vmas
                    .get(&vma.lineage_id())
                    .is_some_and(|existing| existing.id() == vma.id());
                if removed {
                    vmas.remove(&vma.lineage_id());
                }
                if removed && vmas.is_empty() {
                    let Self::Many(retired) = mem::replace(self, Self::Empty) else {
                        unreachable!();
                    };
                    return (true, Some(retired));
                }
                (removed, None)
            }
        }
    }
}
