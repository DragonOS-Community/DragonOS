//! Node-local raw block mapping. Admission is shared by every node of a disk;
//! payload callbacks never re-enter that gate after publishing Writeback.
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use super::{BlockDevice, GenDisk, LoopDevice};
use crate::{
    arch::MMArch,
    cgroup::memory::io::{MemoryIoScope, MemoryIoSource},
    filesystem::{
        page_cache::{PageCache, PageCacheBackend},
        vfs::{file::File, DelegatedWriteResult, IndexNode, WriteSyncIntent},
    },
    mm::{allocation::MemoryAllocationError, MemoryManagementArch},
};

#[derive(Debug)]
struct RawBlockBackend {
    disk: Weak<GenDisk>,
}

impl PageCacheBackend for RawBlockBackend {
    fn read_page(&self, index: usize, bytes: &mut [u8]) -> Result<usize, SystemError> {
        let disk = self.disk.upgrade().ok_or(SystemError::ENODEV)?;
        let offset = index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let _noio = MemoryIoScope::enter_noio();
        File::recover_io(|| disk.read_payload(offset, bytes))
    }

    fn write_page(&self, index: usize, bytes: &[u8]) -> Result<usize, SystemError> {
        let disk = self.disk.upgrade().ok_or(SystemError::ENODEV)?;
        let offset = index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let _noio = MemoryIoScope::enter_noio();
        // Submission errors have not committed payload and can be retried.
        // A completed result belongs to the lower owner's exact-sync protocol
        // (loop's write_piece already performs that recovery); never replay it.
        let result = File::recover_io(|| disk.write_payload(offset, bytes, WriteSyncIntent::None))?;
        result.sync_result.map_err(|error| match error {
            MemoryAllocationError::System(error) => error,
            MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
        })?;
        Ok(result.written_len)
    }

    fn npages(&self) -> usize {
        self.disk
            .upgrade()
            .and_then(|disk| disk.byte_capacity().ok())
            .unwrap_or(0)
            .div_ceil(MMArch::PAGE_SIZE)
    }

    fn read_window_limit(&self, start: usize, pages: usize) -> Result<usize, SystemError> {
        let disk = self.disk.upgrade().ok_or(SystemError::ENODEV)?;
        let offset = start
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let bytes = pages
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok(disk.byte_capacity()?.saturating_sub(offset).min(bytes))
    }

    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        let disk = self.disk.upgrade().ok_or(SystemError::ENODEV)?;
        let device = disk.block_device()?;
        let _gate = device.blkdev_meta().raw_cache_gate.read();
        disk.check_online()?;
        claim()
    }

    fn try_with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        let disk = self.disk.upgrade().ok_or(SystemError::ENODEV)?;
        let device = disk.block_device()?;
        let Some(_gate) = device.blkdev_meta().raw_cache_gate.try_read() else {
            return Ok(false);
        };
        disk.check_online()?;
        claim()?;
        Ok(true)
    }

    fn writeback_claim_in_worker(&self) -> bool {
        true
    }

    fn retry_writeback_error(&self, error: SystemError) -> bool {
        matches!(
            error,
            SystemError::ENOMEM | SystemError::EAGAIN_OR_EWOULDBLOCK | SystemError::EBUSY
        )
    }
}

impl GenDisk {
    pub(super) fn existing_raw_cache(&self) -> Option<Arc<PageCache>> {
        self.raw_cache.lock().clone()
    }

    pub(super) fn raw_cache(&self) -> Arc<PageCache> {
        if let Some(cache) = self.existing_raw_cache() {
            return cache;
        }
        // Creation may allocate and register the mapping; never do it with the
        // cache slot, block metadata, or loop configuration spin locks held.
        let inode: Weak<dyn IndexNode> = self.self_ref.clone();
        let cache = PageCache::new_unowned(
            Some(inode),
            Some(Arc::new(RawBlockBackend {
                disk: self.self_ref.clone(),
            })),
        );
        let mut slot = self.raw_cache.lock();
        slot.get_or_insert(cache).clone()
    }

    pub(crate) fn has_dirty_raw_cache_locked(&self) -> bool {
        self.existing_raw_cache()
            .is_some_and(|cache| cache.has_dirty_pages())
    }

    pub(crate) fn has_raw_writeback_locked(&self) -> bool {
        self.existing_raw_cache()
            .is_some_and(|cache| cache.has_writeback_pages())
    }

    pub(crate) fn sync_raw_cache_locked(&self) -> Result<(), SystemError> {
        self.sync_raw_cache_range_locked(0, usize::MAX)
    }

    pub(super) fn sync_raw_cache_range_locked(
        &self,
        first: usize,
        last: usize,
    ) -> Result<(), SystemError> {
        let Some(cache) = self.existing_raw_cache() else {
            return Ok(());
        };
        let _noio = MemoryIoScope::enter_noio();
        let result = cache
            .manager()
            .launder_range_for_invalidate_with_stable_size(first, last, self.byte_capacity()?);
        // An allocation/start error is not evidence that older submitted
        // payloads ended. Always drain those before the caller may change media.
        let wait = cache.manager().wait_writeback_range(first, last);
        result.and(wait)
    }

    pub(crate) fn invalidate_raw_cache_locked(&self) -> Result<(), SystemError> {
        if let Some(cache) = self.existing_raw_cache() {
            let _invalidate = cache.invalidate_write();
            cache.manager().invalidate_all_clean()?;
        }
        Ok(())
    }

    pub(crate) fn retire_raw_cache_locked(&self) -> Result<(), SystemError> {
        if let Some(cache) = self.existing_raw_cache() {
            cache.manager().resize(0)?;
        }
        Ok(())
    }

    pub(crate) fn reset_raw_cache_writeback_error_locked(&self) {
        if let Some(cache) = self.existing_raw_cache() {
            cache.reset_writeback_error();
        }
    }

    pub(super) fn buffered_read(
        &self,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let (_gate, cache, len) = loop {
            let gate = device.blkdev_meta().raw_cache_gate.read();
            self.check_online()?;
            let len = bytes
                .len()
                .min(self.byte_capacity()?.saturating_sub(offset));
            if len == 0 {
                return Ok(0);
            }
            if let Some(cache) = self.existing_raw_cache() {
                break (gate, cache, len);
            }
            drop(gate);
            // None->Some must exclude a filesystem payload which observed
            // None under a read guard. Re-enter read admission afterwards:
            // a competing mapping change can run between these two guards.
            let _install = device.blkdev_meta().raw_cache_gate.write();
            self.check_online()?;
            if offset < self.byte_capacity()? {
                self.raw_cache();
            }
        };
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        let source = MemoryIoSource::from_owners(&owner, &creator);
        let _source = source.enter();
        let mut done = 0;
        while done < len {
            let count =
                (MMArch::PAGE_SIZE - ((offset + done) & MMArch::PAGE_OFFSET_MASK)).min(len - done);
            match cache.try_read(
                offset + done,
                &mut bytes[done..done + count],
                &owner,
                &creator,
            ) {
                Ok(0) => break,
                Ok(count) => done += count,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    pub(super) fn buffered_write(
        &self,
        offset: usize,
        bytes: &[u8],
    ) -> Result<usize, MemoryAllocationError> {
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        loop {
            let gate = device.blkdev_meta().raw_cache_gate.read();
            self.check_online()?;
            self.check_writable(&*device)?;
            if bytes.is_empty() {
                return Ok(0);
            }
            if offset >= self.byte_capacity()? {
                return Err(SystemError::ENOSPC.into());
            }
            if self.existing_raw_cache().is_some() {
                return self.buffered_write_locked(offset, bytes);
            }
            drop(gate);
            let _install = device.blkdev_meta().raw_cache_gate.write();
            self.check_online()?;
            self.check_writable(&*device)?;
            if offset < self.byte_capacity()? {
                self.raw_cache();
            }
        }
    }

    fn buffered_write_locked(
        &self,
        offset: usize,
        bytes: &[u8],
    ) -> Result<usize, MemoryAllocationError> {
        let capacity = self.byte_capacity()?;
        if offset >= capacity {
            return Err(SystemError::ENOSPC.into());
        }
        let len = bytes.len().min(capacity - offset);
        let cache = self.raw_cache();
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        let source = MemoryIoSource::from_owners(&owner, &creator);
        let _source = source.enter();
        let mut done = 0;
        while done < len {
            let count =
                (MMArch::PAGE_SIZE - ((offset + done) & MMArch::PAGE_OFFSET_MASK)).min(len - done);
            match cache.try_write(offset + done, &bytes[done..done + count], &owner, &creator) {
                Ok(0) => break,
                Ok(count) => done += count,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(done)
    }

    pub(super) fn buffered_write_operation(
        &self,
        offset: usize,
        bytes: &[u8],
        sync: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        if sync == WriteSyncIntent::None {
            return self
                .buffered_write(offset, bytes)
                .map(|written| DelegatedWriteResult::new(written, Ok(())));
        }
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        self.check_writable(&*device)?;
        if bytes.is_empty() {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        let written = self.buffered_write_locked(offset, bytes)?;
        let mut result = DelegatedWriteResult::new(written, Ok(()));
        if written != 0 {
            let end = offset + written - 1;
            result.sync_result = self
                .sync_raw_cache_range_locked(
                    offset >> MMArch::PAGE_SHIFT,
                    end >> MMArch::PAGE_SHIFT,
                )
                .map_err(Into::into)
                .and_then(|()| self.sync_payload());
            result.sync_range = Some((offset, end, sync != WriteSyncIntent::Full));
        }
        Ok(result)
    }

    pub(super) fn read_payload(
        &self,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let len = bytes
            .len()
            .min(self.byte_capacity()?.saturating_sub(offset));
        if len == 0 {
            return Ok(0);
        }
        let physical = self
            .range()
            .lba_start
            .checked_mul(super::LBA_SIZE)
            .and_then(|start| start.checked_add(offset))
            .ok_or(SystemError::EOVERFLOW)?;
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_read_bytes_operation(physical, len, &mut bytes[..len]);
        }
        device
            .read_at_bytes(physical, len, &mut bytes[..len])
            .map_err(Into::into)
    }

    pub(super) fn write_payload(
        &self,
        offset: usize,
        bytes: &[u8],
        sync: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        let device = self.block_device()?;
        self.check_writable(&*device)?;
        if bytes.is_empty() {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        let capacity = self.byte_capacity()?;
        if offset >= capacity {
            return Err(SystemError::ENOSPC.into());
        }
        let len = bytes.len().min(capacity - offset);
        let physical = self
            .range()
            .lba_start
            .checked_mul(super::LBA_SIZE)
            .and_then(|start| start.checked_add(offset))
            .ok_or(SystemError::EOVERFLOW)?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_write_bytes_operation(physical, len, &bytes[..len], sync);
        }
        if device.uses_delegated_write_sync() {
            return device
                .write_at_bytes_with_sync(physical, len, &bytes[..len], sync)
                .map_err(Into::into);
        }
        let written = device.write_at_bytes(physical, len, &bytes[..len])?;
        // The raw-cache admission keeps this mapping stable across both calls.
        // Flush failure belongs to the completed write: returning it as a
        // submission error would allow recovery to replay committed payload.
        let sync_result = if written != 0 && sync != WriteSyncIntent::None {
            device.sync().map_err(Into::into)
        } else {
            Ok(())
        };
        Ok(DelegatedWriteResult::new(written, sync_result))
    }

    /// Submit a filesystem read without populating the raw-device cache.
    /// Admission covers submission only: BIO owns its payload until completion.
    pub(crate) fn submit_read(
        &self,
        relative_lba: usize,
        count: usize,
    ) -> Result<Arc<crate::driver::base::block::bio::BioRequest>, SystemError> {
        if count == 0 {
            return Err(SystemError::EINVAL);
        }
        let offset = relative_lba
            .checked_mul(super::LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let len = count
            .checked_mul(super::LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let checked_start = || {
            self.check_online()?;
            if end > self.byte_capacity()? {
                return Err(SystemError::EFBIG);
            }
            self.range()
                .lba_start
                .checked_add(relative_lba)
                .ok_or(SystemError::EOVERFLOW)
        };
        let gate = device.blkdev_meta().raw_cache_gate.read();
        let start = checked_start()?;
        if self.existing_raw_cache().is_none() {
            // The first raw cache is installed under the exclusive gate, so a
            // buffered write cannot publish dirty bytes before this submission.
            return device.submit_bio_read(start, count);
        }
        drop(gate);
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        let start = checked_start()?;
        self.sync_raw_cache_range_locked(
            offset >> MMArch::PAGE_SHIFT,
            (end - 1) >> MMArch::PAGE_SHIFT,
        )?;
        device.submit_bio_read(start, count)
    }

    pub(super) fn coherent_read(
        &self,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let gate = device.blkdev_meta().raw_cache_gate.read();
        self.check_online()?;
        if self.existing_raw_cache().is_none() {
            return self.read_payload(offset, bytes);
        }
        drop(gate);
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        let len = bytes
            .len()
            .min(self.byte_capacity()?.saturating_sub(offset));
        if len == 0 {
            return Ok(0);
        }
        let first = offset >> MMArch::PAGE_SHIFT;
        let last = (offset + len - 1) >> MMArch::PAGE_SHIFT;
        self.sync_raw_cache_range_locked(first, last)?;
        self.read_payload(offset, &mut bytes[..len])
    }

    pub(super) fn coherent_write(
        &self,
        offset: usize,
        bytes: &[u8],
        sync: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, MemoryAllocationError> {
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let gate = device.blkdev_meta().raw_cache_gate.read();
        self.check_online()?;
        if self.existing_raw_cache().is_none() {
            return self.write_payload(offset, bytes, sync);
        }
        drop(gate);
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        self.check_writable(&*device)?;
        if bytes.is_empty() {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        let capacity = self.byte_capacity()?;
        if offset >= capacity {
            return Err(SystemError::ENOSPC.into());
        }
        let len = bytes.len().min(capacity - offset);
        let first = offset >> MMArch::PAGE_SHIFT;
        let last = (offset + len - 1) >> MMArch::PAGE_SHIFT;
        self.sync_raw_cache_range_locked(first, last)?;
        if let Some(cache) = self.existing_raw_cache() {
            let _invalidate = cache.invalidate_write();
            cache.manager().invalidate_range(first, last)?;
        }
        self.write_payload(offset, &bytes[..len], sync)
    }

    pub(super) fn sync_payload(&self) -> Result<(), MemoryAllocationError> {
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_sync();
        }
        device.sync().map_err(Into::into)
    }

    pub(super) fn sync_owner_range(
        &self,
        first: usize,
        last: usize,
    ) -> Result<(), MemoryAllocationError> {
        let device = self.block_device()?;
        let _noio = MemoryIoScope::enter_noio();
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        self.sync_raw_cache_range_locked(first, last)?;
        self.sync_payload()
    }
}
