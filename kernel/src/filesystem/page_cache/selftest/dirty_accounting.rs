//! Dirty accounting follows fixed backing capability; actual writeback does not.
use super::*;

#[derive(Debug)]
struct Backend {
    writeback: bool,
}

impl PageCacheBackend for Backend {
    fn supports_writeback(&self) -> bool {
        self.writeback
    }

    fn npages(&self) -> usize {
        PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES
    }

    fn read_page(&self, _: usize, _: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _: usize, bytes: &[u8]) -> Result<usize, SystemError> {
        Ok(bytes.len())
    }
}

fn state_counts(before: pc_stats::PageCacheStatsSnapshot, dirty: usize, writeback: usize) -> bool {
    let now = pc_stats::snapshot();
    (now.file_dirty as i128 - before.file_dirty as i128 - dirty as i128).abs()
        <= PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        && (now.file_writeback as i128 - before.file_writeback as i128 - writeback as i128).abs()
            <= PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
}

fn transition(cache: &PageCache, state: PageState) {
    let inner = cache.inner.lock();
    for entry in inner.pages.values() {
        entry.account_state_transition(entry.state(), state);
        entry.set_state(state);
    }
}

fn lifecycle(writeback: bool) -> Result<bool, SystemError> {
    let backend = Arc::new(Backend { writeback });
    let cache = if writeback {
        PageCache::new_unowned(None, Some(backend))
    } else {
        PageCache::new_shmem(None, Some(backend))
    };
    // Keep the counter fixture outside clean reclaim, as in global_wiring.
    cache.set_unevictable(true);
    for index in 0..PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES {
        cache.get_or_create_page_zero(index)?;
    }
    let first = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    let before = pc_stats::snapshot();
    let dirty = if writeback {
        PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES
    } else {
        0
    };
    let qualification =
        PageEntryAccounting::accounts_dirty(first.accounting.load(Ordering::Acquire)) == writeback;
    transition(&cache, PageState::Dirty);
    let dirtied = state_counts(before, dirty, 0);
    transition(&cache, PageState::Dirty);
    let repeated = state_counts(before, dirty, 0);
    transition(&cache, PageState::Writeback);
    let writing = state_counts(before, 0, PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES);
    transition(&cache, PageState::Dirty);
    let redirtied = state_counts(before, dirty, 0);
    transition(&cache, PageState::UpToDate);
    let clean = state_counts(before, 0, 0);
    transition(&cache, PageState::Dirty);
    drop(cache);
    let removed =
        state_counts(before, 0, 0) && first.accounting() == PageEntryAccounting::Unaccounted;
    // A detached completion must not resurrect counters or membership.
    first.account_state_transition(PageState::Dirty, PageState::Writeback);
    first.set_state(PageState::Writeback);
    first.account_state_transition(PageState::Writeback, PageState::UpToDate);
    first.set_state(PageState::UpToDate);
    let late = state_counts(before, 0, 0);
    let ok =
        qualification && dirtied && repeated && writing && redirtied && clean && removed && late;
    if !ok {
        log::error!(
            "dirty accounting writeback={writeback} qualification={qualification} dirtied={dirtied} repeated={repeated} writing={writing} redirtied={redirtied} clean={clean} removed={removed} late={late}"
        );
    }
    Ok(ok)
}

fn late_backend() -> Result<bool, SystemError> {
    let backend = Arc::new(Backend { writeback: false });
    let empty = PageCache::new_shmem(None, None);
    empty.set_backend(backend.clone())?;
    let initialized = !empty.supports_writeback()
        && matches!(empty.set_backend(backend.clone()), Err(SystemError::EINVAL));
    let page = empty.get_or_create_page_zero(0)?;
    let entry = empty.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    let born_resident =
        !PageEntryAccounting::accounts_dirty(entry.accounting.load(Ordering::Acquire));
    drop(page);
    drop(empty);

    let live = PageCache::new_shmem(None, None);
    let page = live.get_or_create_page_zero(0)?;
    let entry = live.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    let rejected = matches!(live.set_backend(backend), Err(SystemError::EINVAL))
        && live.backend().is_none()
        && live.supports_writeback()
        && PageEntryAccounting::accounts_dirty(entry.accounting.load(Ordering::Acquire));
    drop(page);
    drop(live);
    Ok(initialized && born_resident && rejected)
}

pub(super) fn run() -> Result<bool, SystemError> {
    Ok(lifecycle(false)? && lifecycle(true)? && late_backend()?)
}
