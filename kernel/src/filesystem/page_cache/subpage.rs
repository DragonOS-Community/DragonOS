//! Sub-page validity and dirty ranges share the existing page/entry lifetime.
//! The page lock protects bytes and validity; `inner` publishes dirty masks.
use super::*;

pub(super) const CACHE_SECTOR_SIZE: usize = 512;
// Even a page smaller than a sector needs one whole-page validity/dirty bit.
// This also preserves the nonempty mask for ports with placeholder geometry.
pub(super) const CACHE_SECTORS: usize = MMArch::PAGE_SIZE.div_ceil(CACHE_SECTOR_SIZE);
const _: () = assert!(CACHE_SECTORS > 0 && CACHE_SECTORS <= u8::BITS as usize);
pub(super) const ALL_BLOCKS: u8 = ((1u16 << CACHE_SECTORS) - 1) as u8;

pub(super) fn blocks_mask(start: usize, end: usize) -> u8 {
    let first = start / CACHE_SECTOR_SIZE;
    let last = end.div_ceil(CACHE_SECTOR_SIZE).min(CACHE_SECTORS);
    if first >= last {
        return 0;
    }
    (((1u16 << last) - 1) ^ ((1u16 << first) - 1)) as u8
}

impl PageEntry {
    pub(super) fn is_read_ready(&self) -> bool {
        self.state().is_ready() && self.valid_blocks.load(Ordering::Acquire) == ALL_BLOCKS
    }
}

impl PageCache {
    /// All successful front copies publish validity and the exact dirty
    /// incarnation together. Metadata callbacks and token writers use the
    /// same operation as ordinary eager writes.
    pub(super) fn publish_written_range_locked(
        &self,
        inner: &mut InnerPageCache,
        item: &CopyItem,
        page: &mut InnerPage,
        reservation: Option<&mut PageDirtyReservation>,
    ) -> PageCacheDirtyPublication {
        let entry = &item.entry;
        let start = item.page_offset;
        let len = item.sub_len;
        let mask = if let Some(block) = self.subpage_block_size() {
            blocks_mask(start / block * block, (start + len).div_ceil(block) * block)
        } else {
            ALL_BLOCKS
        };
        entry.valid_blocks.fetch_or(mask, Ordering::Release);
        if entry.valid_blocks.load(Ordering::Acquire) == ALL_BLOCKS {
            page.add_flags(PageFlags::PG_UPTODATE);
        }
        if let Some(reservation) = reservation {
            assert!(reservation.active && inner.dirty_preparations != 0);
            inner.dirty_preparations -= 1;
            reservation.active = false;
        }
        Self::publish_front_dirty_blocks_locked(
            &mut inner.dirty_pages,
            item.page_index,
            entry,
            mask,
        )
    }

    pub(super) fn subpage_block_size(&self) -> Option<usize> {
        self.backend()
            .map(|backend| backend.cache_block_size())
            .filter(|size| *size < MMArch::PAGE_SIZE)
    }

    /// Read a whole folio, or only the incomplete boundary blocks of a write.
    /// Callers pin the entry and exclude invalidation. Existing dirty bytes
    /// are never overwritten, even when a later missing-block read fails.
    pub(super) fn prepare_subpage_blocks(
        &self,
        index: usize,
        entry: &Arc<PageEntry>,
        overwrite: Option<(usize, usize)>,
    ) -> Result<(), SystemError> {
        let Some(block_size) = self.subpage_block_size() else {
            return Ok(());
        };
        if block_size < CACHE_SECTOR_SIZE || !block_size.is_power_of_two() {
            return Err(SystemError::EINVAL);
        }
        let required = match overwrite {
            None => ALL_BLOCKS,
            Some((start, end)) => {
                let touched = blocks_mask(
                    start / block_size * block_size,
                    end.div_ceil(block_size) * block_size,
                );
                let overwritten = blocks_mask(
                    start.div_ceil(block_size) * block_size,
                    end / block_size * block_size,
                );
                touched & !overwritten
            }
        };
        if entry.valid_blocks.load(Ordering::Acquire) & required == required {
            return Ok(());
        }
        // Same domain -> page order as full-page backend population. A full
        // overwrite or cache hit requires no read admission/extra lifetime.
        let _domain_io = self.try_acquire_domain_io()?;
        let backend = self.backend().ok_or(SystemError::EIO)?;
        let mut page = entry.page.write();
        for start in (0..MMArch::PAGE_SIZE).step_by(block_size) {
            let end = start + block_size;
            let mask = blocks_mask(start, end);
            if required & mask == 0 {
                continue;
            }
            if entry.valid_blocks.load(Ordering::Relaxed) & mask == mask {
                continue;
            }
            let bytes = unsafe { page.as_slice_mut() };
            let read = backend.read_page_range(index, start, &mut bytes[start..end])?;
            if read > block_size {
                return Err(SystemError::EIO);
            }
            bytes[start + read..end].fill(0);
            entry.valid_blocks.fetch_or(mask, Ordering::Release);
        }
        if entry.valid_blocks.load(Ordering::Acquire) == ALL_BLOCKS {
            page.add_flags(PageFlags::PG_UPTODATE);
        }
        Ok(())
    }
}
