//! Exercise real PageCache publication/claim/completion, without a raw-driver
//! test switch or allocator injection in production.
use super::*;

#[derive(Debug)]
struct BlockBackend {
    fail_reads: AtomicBool,
    fail_third_once: AtomicBool,
    terminal: AtomicBool,
    reads: AtomicUsize,
    first_writes: AtomicUsize,
    third_writes: AtomicUsize,
}

impl PageCacheBackend for BlockBackend {
    fn cache_block_size(&self) -> usize {
        512
    }
    fn npages(&self) -> usize {
        1
    }
    fn read_page(&self, _: usize, _: &mut [u8]) -> Result<usize, SystemError> {
        Err(SystemError::EIO)
    }
    fn write_page(&self, _: usize, _: &[u8]) -> Result<usize, SystemError> {
        Err(SystemError::EIO)
    }
    fn read_page_range(&self, _: usize, _: usize, bytes: &mut [u8]) -> Result<usize, SystemError> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        if self.fail_reads.load(Ordering::Acquire) {
            return Err(SystemError::EIO);
        }
        bytes.fill(b'a');
        Ok(bytes.len())
    }
    fn write_page_blocks(&self, _: usize, _: &[u8], remaining: &mut u8) -> Result<(), SystemError> {
        if self.terminal.load(Ordering::Acquire) {
            return Err(SystemError::EIO);
        }
        if *remaining & 1 != 0 {
            self.first_writes.fetch_add(1, Ordering::AcqRel);
            *remaining &= !1;
        }
        if *remaining & 4 != 0 {
            if self.fail_third_once.swap(false, Ordering::AcqRel) {
                return Err(SystemError::ENOMEM);
            }
            self.third_writes.fetch_add(1, Ordering::AcqRel);
            *remaining &= !4;
        }
        if *remaining != 0 {
            return Err(SystemError::EIO);
        }
        Ok(())
    }
    fn retry_writeback_error(&self, error: SystemError) -> bool {
        error == SystemError::ENOMEM
    }
}

pub(super) fn run() -> Result<bool, SystemError> {
    #[cfg(feature = "mm_selftest")]
    if !crate::driver::base::block::gendisk::run_raw_block_prefix_selftest()? {
        return Ok(false);
    }

    let fs = crate::filesystem::ramfs::RamFS::new();
    let inode = fs
        .root_inode()
        .create(
            "subpage-cache",
            crate::filesystem::vfs::FileType::File,
            crate::filesystem::vfs::InodeMode::from_bits_truncate(0o600),
        )
        .map_err(PageCache::legacy_allocation_error)?;
    inode
        .resize(MMArch::PAGE_SIZE)
        .map_err(PageCache::legacy_allocation_error)?;
    let backend = Arc::new(BlockBackend {
        fail_reads: AtomicBool::new(true),
        fail_third_once: AtomicBool::new(true),
        terminal: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
        first_writes: AtomicUsize::new(0),
        third_writes: AtomicUsize::new(0),
    });
    let cache = PageCache::new_unowned(Some(Arc::downgrade(&inode)), Some(backend.clone()));
    let root = MemoryAccount::new(0, None)?;
    let creator = ObjectAccount::new(&root)?;
    let write = |offset, bytes: &[u8]| {
        cache
            .try_write(offset, bytes, &root, &creator)
            .map_err(PageCache::legacy_allocation_error)
    };
    if write(0, &[b'c'; 512])? != 512
        || backend.reads.load(Ordering::Acquire) != 0
        || cache.get_ready_page(0).is_some()
    {
        return Ok(false);
    }
    if write(513, b"x") != Err(SystemError::EIO) {
        return Ok(false);
    }
    let entry = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    if entry.valid_blocks.load(Ordering::Acquire) != 1
        || entry.dirty_blocks.load(Ordering::Acquire) != 1
    {
        return Ok(false);
    }
    let mut read = [0; 512];
    if cache
        .try_read(0, &mut read, &root, &creator)
        .map_err(PageCache::legacy_allocation_error)
        != Err(SystemError::EIO)
        || entry.dirty_blocks.load(Ordering::Acquire) != 1
    {
        return Ok(false);
    }
    backend.fail_reads.store(false, Ordering::Release);
    if cache
        .try_read(0, &mut read, &root, &creator)
        .map_err(PageCache::legacy_allocation_error)?
        != 512
        || read != [b'c'; 512]
        || !entry.is_read_ready()
    {
        return Ok(false);
    }
    write(1024, &[b'd'; 512])?;
    if cache
        .manager()
        .launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)
        != Err(SystemError::ENOMEM)
        || entry.dirty_blocks.load(Ordering::Acquire) != 4
        || backend.first_writes.load(Ordering::Acquire) != 1
    {
        return Ok(false);
    }
    cache
        .manager()
        .launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
    if backend.first_writes.load(Ordering::Acquire) != 1
        || backend.third_writes.load(Ordering::Acquire) != 1
        || cache.has_dirty_or_writeback_pages()
    {
        return Ok(false);
    }

    // An old terminal completion cannot erase a newer front dirty block.
    write(0, &[b'e'; 512])?;
    let mut batch = match PageCacheManager::claim_next_writeback_batch(
        &cache,
        0,
        0,
        MMArch::PAGE_SIZE,
        None,
        false,
        None,
    )? {
        WritebackClaimOutcome::Claimed(batch) => batch,
        _ => return Ok(false),
    };
    PageCacheManager::snapshot_writeback_batch(&mut batch)?;
    write(1024, &[b'f'; 512])?;
    backend.terminal.store(true, Ordering::Release);
    let _ = PageCacheManager::submit_writeback_batch(batch)?;
    if entry.dirty_blocks.load(Ordering::Acquire) != 4 || !cache.has_dirty_pages() {
        return Ok(false);
    }
    backend.terminal.store(false, Ordering::Release);
    cache
        .manager()
        .launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
    // Cancellation is not a terminal I/O failure: restore only the guard's
    // outstanding range and preserve a front successor without new errseq.
    write(0, &[b'g'; 512])?;
    let mut before = 0;
    let _ = cache.check_and_advance_writeback_error(&mut before);
    let batch = match PageCacheManager::claim_next_writeback_batch(
        &cache,
        0,
        0,
        MMArch::PAGE_SIZE,
        None,
        false,
        None,
    )? {
        WritebackClaimOutcome::Claimed(batch) => batch,
        _ => return Ok(false),
    };
    write(1024, &[b'h'; 512])?;
    drop(batch);
    if entry.dirty_blocks.load(Ordering::Acquire) != 5
        || cache.has_writeback_pages()
        || cache.check_writeback_error_since(before).is_some()
    {
        return Ok(false);
    }
    cache
        .manager()
        .launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
    cache.manager().resize(0)?;
    drop(entry);
    drop(cache);
    Ok(root.current_pages() == 0)
}
