//! Node buffer geometry follows effective open/mount references, including
//! the whole-disk reference held by every partition user.
use super::*;

impl GenDisk {
    pub fn soft_block_size(&self) -> usize {
        self.soft_block_size.load(Ordering::Acquire)
    }

    fn whole_node(&self, device: &dyn BlockDevice) -> Option<Arc<GenDisk>> {
        self.idx?;
        device
            .blkdev_meta()
            .inner()
            .gendisks
            .get(&Self::ENTIRE_DISK_IDX)
            .cloned()
    }

    fn holders(&self) -> usize {
        self.open_holders.load(Ordering::Acquire) + self.mount_holders.load(Ordering::Acquire)
    }

    fn init_block_size_locked(&self, device: &dyn BlockDevice) -> Result<(), SystemError> {
        if self.holders() != 0 {
            return Ok(());
        }
        let size = self.byte_capacity()?;
        let mut block = device.io_limits().logical_block_size as usize;
        if !(LBA_SIZE..=crate::arch::MMArch::PAGE_SIZE).contains(&block) || !block.is_power_of_two()
        {
            return Err(SystemError::EINVAL);
        }
        while block < crate::arch::MMArch::PAGE_SIZE && size & block == 0 {
            block <<= 1;
        }
        self.soft_block_size.store(block, Ordering::Release);
        Ok(())
    }

    pub(super) fn prepare_holder_locked(
        &self,
        device: &dyn BlockDevice,
    ) -> Result<(), SystemError> {
        if let Some(whole) = self.whole_node(device) {
            whole.init_block_size_locked(device)?;
        }
        self.init_block_size_locked(device)
    }

    pub(super) fn add_holder_locked(&self, device: &dyn BlockDevice, mount: bool) {
        let count = if mount {
            &self.mount_holders
        } else {
            &self.open_holders
        };
        count.fetch_add(1, Ordering::Release);
        if let Some(whole) = self.whole_node(device) {
            let count = if mount {
                &whole.mount_holders
            } else {
                &whole.open_holders
            };
            count.fetch_add(1, Ordering::Release);
        }
    }

    fn release_one_holder_locked(&self, mount: bool) {
        let count = if mount {
            &self.mount_holders
        } else {
            &self.open_holders
        };
        let old = count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(old != 0);
        if mount && old == 1 {
            self.mount_claimant.store(0, Ordering::Release);
        }
        if self.holders() == 0 {
            // A close cannot leave a phantom opener after a writeback failure.
            let _ = self.flush_and_retire_raw_cache_locked();
        }
    }

    pub(super) fn release_holder_locked(&self, device: &dyn BlockDevice, mount: bool) {
        self.release_one_holder_locked(mount);
        if let Some(whole) = self.whole_node(device) {
            whole.release_one_holder_locked(mount);
        }
    }

    pub(super) fn check_mount_topology_locked(
        &self,
        device: &dyn BlockDevice,
    ) -> Result<(), SystemError> {
        let partition_mounts: usize = device
            .blkdev_meta()
            .inner()
            .gendisks
            .values()
            .filter(|node| node.idx.is_some())
            .map(|node| node.mount_holders.load(Ordering::Acquire))
            .sum();
        let conflict = if self.idx.is_none() {
            partition_mounts != 0
        } else {
            self.whole_node(device)
                .is_some_and(|whole| whole.mount_holders.load(Ordering::Acquire) > partition_mounts)
        };
        if conflict {
            Err(SystemError::EBUSY)
        } else {
            Ok(())
        }
    }

    pub(crate) fn has_mount_conflict_locked(&self, device: &dyn BlockDevice) -> bool {
        self.mount_holders.load(Ordering::Acquire) != 0
            || self.check_mount_topology_locked(device).is_err()
    }

    fn set_soft_block_size_locked(
        &self,
        device: &dyn BlockDevice,
        size: usize,
    ) -> Result<(), SystemError> {
        if !(LBA_SIZE..=crate::arch::MMArch::PAGE_SIZE).contains(&size)
            || !size.is_power_of_two()
            || size < device.io_limits().logical_block_size as usize
        {
            return Err(SystemError::EINVAL);
        }
        if self.soft_block_size() == size {
            return Ok(());
        }
        self.flush_and_retire_raw_cache_locked()?;
        self.soft_block_size.store(size, Ordering::Release);
        Ok(())
    }

    pub(super) fn set_soft_block_size(&self, size: usize) -> Result<(), SystemError> {
        let device = self.block_device()?;
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        if self.has_mount_conflict_locked(&*device) {
            return Err(SystemError::EBUSY);
        }
        self.set_soft_block_size_locked(&*device, size)
    }
}

impl GenDiskMountGuard {
    /// A mount already owns its exclusive device claim. Use the same buffer
    /// transition without re-entering the userspace claim admission.
    pub fn set_block_size(&self, size: usize) -> Result<(), SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _gate = self.bdev.blkdev_meta().raw_cache_gate.write();
        self.disk.check_online()?;
        if self.disk.mount_claimant.load(Ordering::Acquire) != self.claimant.bits() {
            return Err(SystemError::ESTALE);
        }
        self.disk.set_soft_block_size_locked(&*self.bdev, size)
    }

    pub fn set_min_block_size(&self, size: usize) -> Result<(), SystemError> {
        self.set_block_size(size.max(self.bdev.io_limits().logical_block_size as usize))
    }
}
