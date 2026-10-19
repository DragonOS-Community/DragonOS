//! Effective direct-I/O mode and its backing-device geometry.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    cgroup::memory::io::MemoryIoScope,
    driver::base::{
        block::{
            block_device::{BlockDevice, LBA_SIZE},
            gendisk::GenDisk,
            manager::block_dev_manager,
        },
        device::device_number::DeviceNumber,
    },
    filesystem::vfs::file::File,
};

use super::{
    constants::{LoopFlags, LoopState},
    loop_device::{LoopDevice, LOOP_BACKING_TOPOLOGY_LOCK},
};

/// Resolve backing geometry without retaining the block registry lock across
/// backend callbacks. Filesystems without a backing bdev (e.g. tmpfs) accept
/// direct intent even when they deliberately implement it as buffered I/O.
pub(super) fn effective_dio(
    file: &Arc<File>,
    offset: usize,
    requested: bool,
) -> Result<bool, SystemError> {
    if !requested || !file.supports_direct_io() {
        return Ok(false);
    }
    let inode = file.inode();
    let logical = if let Some(disk) = inode.as_any_ref().downcast_ref::<GenDisk>() {
        disk.block_device()?.io_limits().logical_block_size
    } else {
        let number = inode.fs().info().blk_dev_id;
        if number == 0 {
            return Ok(true);
        }
        let number = u32::try_from(number).map_err(|_| SystemError::ENODEV)?;
        let disk = block_dev_manager()
            .lookup_gendisk_by_device_number(DeviceNumber::from(number))
            .ok_or(SystemError::ENODEV)?;
        disk.block_device()?.io_limits().logical_block_size
    };
    if logical == 0 {
        return Err(SystemError::EINVAL);
    }
    Ok(LBA_SIZE >= logical as usize && offset.is_multiple_of(logical as usize))
}

/// Linux mode publication follows backing fsync even if its terminal result
/// failed. NOIO permits typed admission recovery without recursive block I/O.
pub(super) fn sync_before_mode_change(file: &Arc<File>) {
    let _noio = MemoryIoScope::enter_noio();
    let _ = File::recover_io(|| file.try_sync_range_and_check_wb_error(0, usize::MAX, false));
}

impl LoopDevice {
    pub(super) fn set_direct_io(&self, requested: bool) -> Result<(), SystemError> {
        let _noio = MemoryIoScope::enter_noio();
        // Raw-cache writeback may itself enter the loop I/O gate. Complete it
        // before taking that gate exclusive, in the shared owner lock order.
        let _raw = self.blkdev_meta().raw_cache_gate.write();
        for _ in 0..16 {
            let (backing, offset, generation, current) = {
                let inner = self.inner();
                match inner.state() {
                    LoopState::Bound => {}
                    LoopState::Draining => continue,
                    _ => return Err(SystemError::ENXIO),
                }
                (
                    inner.backing_file.clone().ok_or(SystemError::ENODEV)?,
                    inner.offset,
                    inner.generation,
                    inner.flags.contains(LoopFlags::DIRECT_IO),
                )
            };
            let enabled = effective_dio(&backing, offset, requested)?;
            if requested && !enabled {
                return Err(SystemError::EINVAL);
            }
            if enabled != current {
                self.prepare_raw_mapping_change_locked()?;
            }
            // Excludes payload, not requests preparing scratch. Never drain
            // active pins under this gate: those pins may be waiting for it.
            let gate = self.io_gate.write();
            if enabled != current {
                // Linux synchronizes the retained backing OFD but deliberately
                // ignores terminal fsync errors when changing effective mode.
                // NOIO prevents recovery from recursing through our held gate.
                sync_before_mode_change(&backing);
            }
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            if inner.state() == LoopState::Bound
                && inner.generation == generation
                && inner
                    .backing_file
                    .as_ref()
                    .is_some_and(|file| Arc::ptr_eq(file, &backing))
            {
                if enabled != current {
                    inner.flags.set(LoopFlags::DIRECT_IO, enabled);
                    inner.generation = inner.generation.wrapping_add(1);
                }
                return Ok(());
            }
            // Release all locks, especially the I/O gate, before retrying a
            // competing drainer which may be waiting for an admitted request.
            drop(inner);
            drop(_config);
            drop(_topology);
            drop(gate);
        }
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }
}
