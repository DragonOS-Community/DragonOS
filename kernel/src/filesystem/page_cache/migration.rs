//! Cache membership admission for fixed-identity physical page migration.
//!
//! The entry itself never changes identity: dirty certificates, range tags,
//! subpage state and accounting all survive the replacement of its data Page.

use super::{Arc, InnerPageCache, MutexGuard, Page, PageCache, PageState};

/// Excludes both cache membership changes and every external entry owner.
///
/// The migration transaction owns the source Page isolation and all its MM
/// locks. It must restore this slot before releasing the guard on rollback;
/// taking this guard alone does not authorize moving or freeing either frame.
pub(crate) struct PageCacheMigrationGuard<'a> {
    inner: MutexGuard<'a, InnerPageCache>,
    index: usize,
}

impl PageCacheMigrationGuard<'_> {
    /// The cache mutex prevents a new entry owner after the admission check.
    /// Moving the unique target Arc here lets the transaction consume the old
    /// cache owner without changing any entry metadata or emitting callbacks.
    pub(crate) fn page_mut(&mut self) -> &mut Arc<Page> {
        let entry = self
            .inner
            .pages
            .get_mut(&self.index)
            .expect("migration entry disappeared under the cache mutex");
        &mut Arc::get_mut(entry)
            .expect("migration entry gained an owner under the cache mutex")
            .storage
            .page
    }
}

impl PageCache {
    /// Capture the fixed-PFN owner, including Loading pages. The transaction's
    /// later entry admission distinguishes an I/O conflict from a missing key.
    pub(crate) fn try_migration_source(
        &self,
        index: usize,
    ) -> Result<Arc<Page>, super::SystemError> {
        let inner = self
            .inner
            .try_lock()
            .map_err(|_| super::SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        inner
            .pages
            .get(&index)
            .map(|entry| entry.page.clone())
            .ok_or(super::SystemError::ENOENT)
    }

    /// Try the cache part of a migration commit without waiting for I/O,
    /// readers or filesystem callbacks. The caller retains source isolation.
    ///
    /// A raw Page Arc is deliberately not counted here: the transaction must
    /// still consume the source with Arc::try_unwrap after detaching all known
    /// owners. Entry owners, including read/DMA/writeback/wait tokens, prevent
    /// this admission even when active_users is zero.
    pub(crate) fn try_migration_page(
        &self,
        index: usize,
        expected: &Arc<Page>,
    ) -> Option<PageCacheMigrationGuard<'_>> {
        let mut inner = self.inner.try_lock().ok()?;
        if self.mapping_unevictable() || inner.writeback_pages.contains(&index) {
            return None;
        }
        let dirty = inner.dirty_pages.contains(&index);
        let entry = inner.pages.get_mut(&index)?;
        if !Arc::ptr_eq(&entry.page, expected)
            || entry.active_users() != 0
            || !matches!(entry.state(), PageState::UpToDate | PageState::Dirty)
            || (entry.state() == PageState::Dirty) != dirty
        {
            return None;
        }
        // Do not clone this entry before checking uniqueness. In particular,
        // get_entry() would itself create an owner and reject every attempt.
        Arc::get_mut(entry)?;
        Some(PageCacheMigrationGuard { inner, index })
    }
}

/// Copy-only observation: retaining this value cannot block entry uniqueness.
#[cfg(feature = "mm_selftest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MigrationCacheSnapshot {
    entry_address: usize,
    cache_instance: u64,
    entry_instance: u64,
    state: u8,
    dirty_incarnation: u64,
    dirty_transition_kind: u8,
    valid_blocks: u8,
    dirty_blocks: u8,
    writeback_tag: u64,
    writeback_incarnation: u64,
    accounting: u8,
    dirty_indexed: bool,
    writeback_indexed: bool,
}

#[cfg(feature = "mm_selftest")]
pub(crate) struct MigrationWritebackClaim {
    batch: Option<super::ClaimedWritebackBatch>,
}

#[cfg(feature = "mm_selftest")]
impl MigrationWritebackClaim {
    pub(crate) fn complete(mut self) -> Result<(), super::SystemError> {
        super::PageCacheManager::complete_writeback_batch(self.batch.take().unwrap(), Ok(()))
    }
}

#[cfg(feature = "mm_selftest")]
impl PageCache {
    pub(crate) fn migration_test_snapshot(&self, index: usize) -> Option<MigrationCacheSnapshot> {
        use super::Ordering;
        let inner = self.inner.lock();
        let entry = inner.pages.get(&index)?;
        Some(MigrationCacheSnapshot {
            entry_address: Arc::as_ptr(entry) as usize,
            cache_instance: self.instance_id,
            entry_instance: entry.instance_id,
            state: entry.state.load(Ordering::Acquire),
            dirty_incarnation: entry.dirty_incarnation.load(Ordering::Acquire),
            dirty_transition_kind: entry.dirty_transition_kind.load(Ordering::Acquire),
            valid_blocks: entry.valid_blocks.load(Ordering::Acquire),
            dirty_blocks: entry.dirty_blocks.load(Ordering::Acquire),
            writeback_tag: entry.writeback_tag.load(Ordering::Acquire),
            writeback_incarnation: entry.writeback_incarnation.load(Ordering::Acquire),
            accounting: entry.accounting.load(Ordering::Acquire),
            dirty_indexed: inner.dirty_pages.contains(&index),
            writeback_indexed: inner.writeback_pages.contains(&index),
        })
    }

    /// Exercise the real claim/snapshot/completion protocol without pretending
    /// to submit device I/O. Disk durability is checked by the userspace test.
    pub(crate) fn migration_test_claim_writeback(
        self: &Arc<Self>,
        index: usize,
    ) -> Result<MigrationWritebackClaim, super::SystemError> {
        use super::{MMArch, PageCacheManager, SystemError, WritebackClaimOutcome};
        use crate::mm::MemoryManagementArch;
        let size = index
            .checked_add(1)
            .and_then(|count| count.checked_mul(MMArch::PAGE_SIZE))
            .ok_or(SystemError::EOVERFLOW)?;
        let WritebackClaimOutcome::Claimed(mut batch) =
            PageCacheManager::claim_next_writeback_batch(
                self, index, index, size, None, false, None,
            )?
        else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        };
        PageCacheManager::snapshot_writeback_batch(&mut batch)?;
        Ok(MigrationWritebackClaim { batch: Some(batch) })
    }
}
