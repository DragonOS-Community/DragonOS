use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy)]
pub struct PageCacheStatsSnapshot {
    pub file_pages: u64,
    pub file_mapped: u64,
    pub file_dirty: u64,
    pub file_writeback: u64,
    pub shmem_pages: u64,
    pub unevictable: u64,
    pub drop_pagecache: u64,
}

static FILE_PAGES: AtomicU64 = AtomicU64::new(0);
static FILE_MAPPED: AtomicU64 = AtomicU64::new(0);
static FILE_DIRTY: AtomicU64 = AtomicU64::new(0);
static FILE_WRITEBACK: AtomicU64 = AtomicU64::new(0);
static SHMEM_PAGES: AtomicU64 = AtomicU64::new(0);
static UNEVICTABLE: AtomicU64 = AtomicU64::new(0);
static DROP_PAGECACHE: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn inc_file_pages() {
    FILE_PAGES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_file_pages() {
    FILE_PAGES.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_file_mapped() {
    FILE_MAPPED.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_file_mapped() {
    FILE_MAPPED.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_file_dirty() {
    FILE_DIRTY.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_file_dirty() {
    FILE_DIRTY.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_file_writeback() {
    FILE_WRITEBACK.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_file_writeback() {
    FILE_WRITEBACK.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_shmem_pages() {
    SHMEM_PAGES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_shmem_pages() {
    SHMEM_PAGES.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_unevictable() {
    UNEVICTABLE.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn dec_unevictable() {
    UNEVICTABLE.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn inc_drop_pagecache() {
    DROP_PAGECACHE.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn snapshot() -> PageCacheStatsSnapshot {
    PageCacheStatsSnapshot {
        file_pages: FILE_PAGES.load(Ordering::Relaxed),
        file_mapped: FILE_MAPPED.load(Ordering::Relaxed),
        file_dirty: FILE_DIRTY.load(Ordering::Relaxed),
        file_writeback: FILE_WRITEBACK.load(Ordering::Relaxed),
        shmem_pages: SHMEM_PAGES.load(Ordering::Relaxed),
        unevictable: UNEVICTABLE.load(Ordering::Relaxed),
        drop_pagecache: DROP_PAGECACHE.load(Ordering::Relaxed),
    }
}
