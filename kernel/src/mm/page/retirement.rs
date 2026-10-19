//! Managed-page registry retirement, separate from physical/TLB lifetime.
//!
//! Registry ownership is not a mapping or backing authority. Once the last
//! rmap and every unevictable/backing source are gone, detach both registries
//! under the same exclusive Page guard. Every revoked-PTE transaction must
//! retain its own Page Arc through its TLB barrier, even if another MM removes
//! the last rmap first. The final real owner still frees the frame and charge.
use alloc::sync::Arc;

use super::{
    try_page_manager_lock, try_page_reclaimer_lock, InnerPage, Page, PageFlags, PageManager,
    PageReclaimer,
};

/// Fixed ownership slots; callers release them outside all registry/Page
/// guards. OOM keeps these in its existing batch through the TLB barrier.
#[derive(Default)]
pub(crate) struct RetiredPageOwners {
    _manager: Option<Arc<Page>>,
    _lru: Option<Arc<Page>>,
}

/// Detach eligible unbacked pages, with no lock acquisition or allocation.
///
/// `data` must be the exclusive data guard of `page`. The caller owns both
/// registries already, except non-LRU pages need no reclaimer guard. PG_LRU
/// and membership are published together; terminal retirement unlinks before
/// clearing the flag, so a clear flag cannot hide a live LRU node.
///
/// This is not a physical release: the caller retains a Page Arc until every
/// translation it revoked has crossed its own TLB barrier (or was unpublished).
/// File pages and live backing/unevictable sources keep their authority.
pub(crate) fn detach_unmapped_page_locked(
    page: &Arc<Page>,
    data: &mut InnerPage,
    manager: &mut PageManager,
    reclaimer: Option<&mut PageReclaimer>,
) -> RetiredPageOwners {
    debug_assert_eq!(page.phys_address(), data.phys_address());
    if !data.can_deallocate_after_vma_unmap() {
        return RetiredPageOwners::default();
    }
    assert!(
        !data.flags().contains(PageFlags::PG_LRU) || reclaimer.is_some(),
        "LRU retirement requires reclaimer serialization"
    );
    let manager_owner = manager.remove_page_if_same(page);
    let lru_owner = reclaimer.and_then(|reclaimer| reclaimer.remove_page_if_same(page));
    data.remove_flags(PageFlags::PG_LRU);
    RetiredPageOwners {
        _manager: manager_owner,
        _lru: lru_owner,
    }
}

/// Sleepable counterpart for VMA, COW and unpublished-page cleanup.
///
/// The caller retains `page` through any outstanding TLB barrier. A Page
/// write lock may sleep, but neither registry is acquired by blocking while
/// holding it: existing code also has manager -> Page lock ordering. Retry
/// releases all guards owned here first; caller MM/PT locks remain its own.
pub(crate) fn retire_unmapped_page(page: &Arc<Page>) {
    let owners = loop {
        let mut data = page.write();
        // The common still-mapped or cache/backing-owned case never contends
        // on either global registry. Ordinary non-LRU anon needs only manager.
        if !data.can_deallocate_after_vma_unmap() {
            return;
        }
        let Some(mut manager) = try_page_manager_lock() else {
            drop(data);
            crate::sched::sched_yield();
            continue;
        };
        let mut reclaimer = if data.flags().contains(PageFlags::PG_LRU) {
            match try_page_reclaimer_lock() {
                Some(guard) => Some(guard),
                None => {
                    drop(manager);
                    drop(data);
                    crate::sched::sched_yield();
                    continue;
                }
            }
        } else {
            None
        };
        let owners =
            detach_unmapped_page_locked(page, &mut data, &mut manager, reclaimer.as_deref_mut());
        drop(reclaimer);
        drop(manager);
        drop(data);
        break owners;
    };
    drop(owners);
}

#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;
