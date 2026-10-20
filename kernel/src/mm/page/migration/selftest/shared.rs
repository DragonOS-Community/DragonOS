//! Cache migration boundaries use production allocation, pin, read-DMA and
//! writeback claim/completion APIs. Actual disk durability is a userspace test.
use super::*;
use crate::filesystem::{
    page_cache::PageCache,
    vfs::{FileSystem, FileType, IndexNode, InodeMode},
};
use alloc::{vec, vec::Vec};

struct CacheFixture {
    cache: Arc<PageCache>,
    account: Arc<MemoryAccount>,
    _inode: Option<Arc<dyn IndexNode>>,
    _fs: Option<Arc<dyn FileSystem>>,
    dirty: bool,
}

impl CacheFixture {
    fn new(shmem: bool, dirty: bool) -> Result<Self, SystemError> {
        let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
        account.set_policy(MemoryPolicy {
            max: Some(1),
            ..MemoryPolicy::default()
        });
        // A real inode supplies ordinary File dirty-retention ownership. This
        // separate cache deliberately has no backend: no fake I/O is submitted.
        let (inode, fs) = if shmem {
            (None, None)
        } else {
            let fs = crate::filesystem::ramfs::RamFS::new();
            let fixture_error = |error| match error {
                crate::mm::allocation::MemoryAllocationError::System(error) => error,
                crate::mm::allocation::MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
            };
            let inode = fs
                .root_inode()
                .create(
                    "migration-cache",
                    FileType::File,
                    InodeMode::from_bits_truncate(0o600),
                )
                .map_err(fixture_error)?;
            inode.resize(MMArch::PAGE_SIZE).map_err(fixture_error)?;
            (Some(inode), Some(fs as Arc<dyn FileSystem>))
        };
        let weak = inode.as_ref().map(Arc::downgrade);
        let cache = if shmem {
            PageCache::new_shmem(weak, None)
        } else {
            PageCache::new_unowned(weak, None)
        };
        let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
        let pin = cache
            .manager()
            .try_commit_overwrite_pinned(0, &account, &creator)
            .map_err(|_| SystemError::ENOMEM)?;
        let page = pin.page();
        {
            let mut data = page.write();
            for (index, byte) in unsafe { data.as_slice_mut() }.iter_mut().enumerate() {
                *byte = pattern(index, false);
            }
        }
        drop(page);
        drop(pin);
        if dirty {
            cache
                .try_write(17, &[0xa7], &account, &creator)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        Ok(Self {
            cache,
            account,
            _inode: inode,
            _fs: fs,
            dirty,
        })
    }

    fn page(&self) -> Option<Arc<Page>> {
        self.cache.manager().peek_page(0)
    }

    fn valid(&self) -> bool {
        let Some(page) = self.page() else {
            return false;
        };
        let data = page.read();
        data.map_count() == 0
            && data.flags().contains(PageFlags::PG_LRU)
            && data.flags().contains(PageFlags::PG_DIRTY) == self.dirty
            && unsafe { data.as_slice() }
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == pattern(index, self.dirty))
            && page
                .memory_account()
                .is_some_and(|account| Arc::ptr_eq(account, &self.account))
            && self.account.current_pages() == 1
    }
}

fn pattern(index: usize, dirty: bool) -> u8 {
    if dirty && index == 17 {
        0xa7
    } else {
        (index.wrapping_mul(37) ^ (index >> 5)) as u8
    }
}

fn migrate_cached(cache: &Arc<PageCache>) -> Result<MigrationResult, MigrationError> {
    for _ in 0..128 {
        match super::super::migrate_cached_page(cache, 0) {
            Err(MigrationError::Busy) => crate::sched::sched_yield(),
            result => return result,
        }
    }
    Err(MigrationError::Busy)
}

fn cache_identity(shmem: bool, dirty: bool, offline: bool) -> Result<bool, SystemError> {
    let f = CacheFixture::new(shmem, dirty)?;
    let before_entry = f.cache.migration_test_snapshot(0);
    let before_charge = f.account.snapshot();
    let source = f.page().ok_or(SystemError::EFAULT)?;
    let old_phys = source.phys_address();
    let weak = Arc::downgrade(&source);
    drop(source);
    if offline {
        f.account.offline();
    }
    let migrated = migrate_cached(&f.cache).is_ok();
    let after_charge = f.account.snapshot();
    let new_phys = f.page().map(|page| page.phys_address());
    Ok(migrated
        && new_phys.is_some_and(|physical| physical != old_phys)
        && weak.upgrade().is_none()
        && f.valid()
        && before_entry.is_some()
        && before_entry == f.cache.migration_test_snapshot(0)
        && before_charge.current == after_charge.current
        && before_charge.peak == after_charge.peak
        && before_charge.file == after_charge.file
        && before_charge.shmem == after_charge.shmem
        && before_charge.stats == after_charge.stats
        && before_charge.events == after_charge.events)
}

fn held_owner(kind: u8) -> Result<bool, SystemError> {
    let f = CacheFixture::new(true, true)?;
    let before = f.cache.migration_test_snapshot(0);
    let old_phys = f.page().ok_or(SystemError::EFAULT)?.phys_address();
    let cache_pin = (kind == 0)
        .then(|| f.cache.manager().peek_page_pinned(0))
        .flatten();
    let raw = (kind == 1).then(|| f.page()).flatten();
    let access = if kind == 2 {
        Some(
            f.page()
                .ok_or(SystemError::EFAULT)?
                .try_pin_access()
                .map_err(|_| SystemError::EBUSY)?,
        )
    } else {
        None
    };
    if (kind == 0 && cache_pin.is_none()) || (kind == 1 && raw.is_none()) {
        return Ok(false);
    }
    let busy = matches!(
        super::super::migrate_cached_page(&f.cache, 0),
        Err(MigrationError::Busy)
    );
    let unchanged = f.valid()
        && f.page().is_some_and(|page| page.phys_address() == old_phys)
        && before == f.cache.migration_test_snapshot(0);
    drop(access);
    drop(raw);
    drop(cache_pin);
    Ok(busy && unchanged && migrate_cached(&f.cache).is_ok() && f.valid())
}

fn dma_reservation() -> Result<bool, SystemError> {
    let cache = PageCache::new_shmem(None, None);
    let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
    account.set_policy(MemoryPolicy {
        max: Some(1),
        ..MemoryPolicy::default()
    });
    let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
    let reservation = {
        let _invalidate = cache.invalidate_read();
        cache
            .manager()
            .try_reserve_read_dma(0, 1, false, &account, &creator)
            .map_err(|_| SystemError::ENOMEM)?
    };
    let descriptor = reservation
        .descriptors()
        .next()
        .ok_or(SystemError::EFAULT)?;
    let before = cache.migration_test_snapshot(0);
    let prepared_busy = matches!(
        super::super::migrate_cached_page(&cache, 0),
        Err(MigrationError::Busy)
    );
    reservation.mark_submitted()?;
    let submitted_busy = matches!(
        super::super::migrate_cached_page(&cache, 0),
        Err(MigrationError::Busy)
    );
    let same_descriptor = reservation.descriptors().next().is_some_and(|current| {
        current.paddr() == descriptor.paddr() && current.vaddr() == descriptor.vaddr()
    });
    reservation.mark_completed()?;
    let completed_busy = matches!(
        super::super::migrate_cached_page(&cache, 0),
        Err(MigrationError::Busy)
    );
    let unchanged = before == cache.migration_test_snapshot(0);
    // No device was submitted by this selftest. Publishing zero initialized
    // payload exercises the real unread-tail initialization and Loading exit.
    let published = reservation.publish_completed(0)?;
    drop(published);
    drop(reservation);
    let migrated = migrate_cached(&cache).is_ok();
    let page = cache.manager().peek_page(0).ok_or(SystemError::EFAULT)?;
    let data = page.read();
    Ok(prepared_busy
        && submitted_busy
        && completed_busy
        && same_descriptor
        && unchanged
        && migrated
        && page.phys_address() != descriptor.paddr()
        && unsafe { data.as_slice() }.iter().all(|byte| *byte == 0)
        && account.current_pages() == 1
        && account.snapshot().peak == 1)
}

fn writeback_claim() -> Result<bool, SystemError> {
    let f = CacheFixture::new(false, true)?;
    let old_phys = f.page().ok_or(SystemError::EFAULT)?.phys_address();
    let claim = f.cache.migration_test_claim_writeback(0)?;
    let claimed = f.cache.migration_test_snapshot(0);
    let busy = matches!(
        super::super::migrate_cached_page(&f.cache, 0),
        Err(MigrationError::Busy)
    );
    let unchanged = claimed == f.cache.migration_test_snapshot(0)
        && f.page().is_some_and(|page| page.phys_address() == old_phys);
    claim.complete()?;
    let completed = f.cache.migration_test_snapshot(0);
    let migrated = migrate_cached(&f.cache).is_ok();
    let page = f.page().ok_or(SystemError::EFAULT)?;
    let data = page.read();
    Ok(busy
        && unchanged
        && migrated
        && completed == f.cache.migration_test_snapshot(0)
        && page.phys_address() != old_phys
        && !data
            .flags()
            .intersects(PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK)
        && unsafe { data.as_slice() }
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == pattern(index, true))
        && f.account.current_pages() == 1
        && f.account.snapshot().peak == 1)
}

fn snapshot_and_recency() -> Result<bool, SystemError> {
    let first = CacheFixture::new(true, false)?;
    let f = CacheFixture::new(true, true)?;
    let last = CacheFixture::new(true, false)?;
    let old_phys = f.page().ok_or(SystemError::EFAULT)?.phys_address();
    let first_phys = first.page().ok_or(SystemError::EFAULT)?.phys_address();
    let last_phys = last.page().ok_or(SystemError::EFAULT)?.phys_address();
    let mut held = None;
    let count = crate::mm::page::page_reclaimer_lock().lru.len();
    for _ in 0..count.div_ceil(64) {
        let mut snapshot = crate::mm::page::page_reclaimer_lock().reclaim_snapshot(64);
        for slot in &mut snapshot {
            if slot
                .as_ref()
                .is_some_and(|page| page.phys_address() == old_phys)
            {
                held = slot.take();
                break;
            }
        }
        if held.is_some() {
            break;
        }
    }
    let order = |target: PhysAddr| -> Vec<u8> {
        crate::mm::page::page_reclaimer_lock()
            .lru
            .iter()
            .filter_map(|(physical, _)| {
                if *physical == first_phys {
                    Some(0)
                } else if *physical == target {
                    Some(1)
                } else if *physical == last_phys {
                    Some(2)
                } else {
                    None
                }
            })
            .collect()
    };
    let before = order(old_phys);
    let busy = held.is_some()
        && matches!(
            super::super::migrate_cached_page(&f.cache, 0),
            Err(MigrationError::Busy)
        );
    let rollback_order = before == order(old_phys);
    drop(held);
    let migrated = migrate_cached(&f.cache).is_ok();
    let new_phys = f.page().ok_or(SystemError::EFAULT)?.phys_address();
    Ok(busy
        && rollback_order
        && migrated
        && old_phys != new_phys
        && before.len() == 3
        && before == order(new_phys)
        && f.valid())
}

pub(super) fn checks() -> Vec<(&'static str, bool)> {
    vec![
        (
            "cached_file_clean",
            cache_identity(false, false, false).unwrap_or(false),
        ),
        (
            "cached_file_dirty",
            cache_identity(false, true, false).unwrap_or(false),
        ),
        (
            "cached_shmem_clean",
            cache_identity(true, false, false).unwrap_or(false),
        ),
        (
            "cached_shmem_dirty",
            cache_identity(true, true, false).unwrap_or(false),
        ),
        (
            "cached_offline_charge",
            cache_identity(true, true, true).unwrap_or(false),
        ),
        ("cached_entry_pin_busy", held_owner(0).unwrap_or(false)),
        ("cached_extra_arc_busy", held_owner(1).unwrap_or(false)),
        ("cached_access_pin_busy", held_owner(2).unwrap_or(false)),
        (
            "cached_read_dma_reservation_busy",
            dma_reservation().unwrap_or(false),
        ),
        (
            "cached_writeback_claim_busy",
            writeback_claim().unwrap_or(false),
        ),
        (
            "cached_reclaimer_snapshot_recency",
            snapshot_and_recency().unwrap_or(false),
        ),
    ]
}
