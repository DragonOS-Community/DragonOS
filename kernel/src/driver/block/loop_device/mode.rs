//! Loop queue geometry and effective direct-I/O mode publication.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::memory::io::MemoryIoScope,
    driver::base::{
        block::{
            block_device::{BlockDevice, LBA_SIZE},
            gendisk::GenDisk,
            manager::block_dev_manager,
        },
        device::device_number::DeviceNumber,
    },
    filesystem::vfs::file::{File, FileFlags},
    mm::MemoryManagementArch,
};

use super::{
    constants::{LoopFlags, LoopState},
    loop_device::{LoopDevice, LOOP_BACKING_TOPOLOGY_LOCK},
};

/// The ioctl argument is unsigned long. Validate before narrowing to avoid
/// accepting a large value whose low 32 bits happen to be a supported size.
pub(super) fn validate_logical_block_size(size: usize) -> Result<u32, SystemError> {
    if !(LBA_SIZE..=MMArch::PAGE_SIZE).contains(&size) || !size.is_power_of_two() {
        return Err(SystemError::EINVAL);
    }
    Ok(size as u32)
}

fn filesystem_logical_block_size(file: &Arc<File>) -> Result<Option<u32>, SystemError> {
    let number = file.inode().fs().info().blk_dev_id;
    if number == 0 {
        return Ok(None);
    }
    let number = u32::try_from(number).map_err(|_| SystemError::ENODEV)?;
    let disk = block_dev_manager()
        .lookup_gendisk_by_device_number(DeviceNumber::from(number))
        .ok_or(SystemError::ENODEV)?;
    let logical = disk.block_device()?.io_limits().logical_block_size;
    Ok(Some(logical))
}

/// CONFIGURE's zero means the queue default, not SET_BLOCK_SIZE's zero.
/// Linux inherits an O_DIRECT backing file's superblock device geometry;
/// the raw block inode's device alone does not imply that default.
pub(super) fn configured_logical_block_size(
    file: &Arc<File>,
    size: u32,
) -> Result<u32, SystemError> {
    if size != 0 {
        return validate_logical_block_size(size as usize);
    }
    if file.flags().contains(FileFlags::O_DIRECT) {
        if let Some(logical) = filesystem_logical_block_size(file)? {
            return validate_logical_block_size(logical as usize);
        }
    }
    Ok(LBA_SIZE as u32)
}

/// Resolve backing geometry without retaining the registry or inner lock
/// across backend callbacks. Tmpfs deliberately accepts direct intent while
/// implementing it as buffered I/O, as Linux does.
pub(super) fn effective_dio(
    file: &Arc<File>,
    offset: usize,
    logical_block_size: u32,
    requested: bool,
) -> Result<bool, SystemError> {
    if !requested || !file.supports_direct_io() {
        return Ok(false);
    }
    let inode = file.inode();
    let logical = if let Some(disk) = inode.as_any_ref().downcast_ref::<GenDisk>() {
        Some(disk.block_device()?.io_limits().logical_block_size)
    } else {
        filesystem_logical_block_size(file)?
    };
    let Some(logical) = logical else {
        return Ok(true);
    };
    if logical == 0 {
        return Err(SystemError::EINVAL);
    }
    Ok(logical_block_size >= logical && offset.is_multiple_of(logical as usize))
}

/// Linux mode publication follows backing fsync even if its terminal result
/// failed. NOIO permits typed admission recovery without recursive block I/O.
pub(super) fn sync_before_mode_change(file: &Arc<File>) {
    let _noio = MemoryIoScope::enter_noio();
    let _ = File::recover_io(|| file.try_sync_range_and_check_wb_error(0, usize::MAX, false));
}

/// Both operations publish the same effective queue/mode generation. The
/// variants retain their distinct admission and no-change semantics.
#[derive(Clone, Copy)]
enum QueueUpdate {
    DirectIo(bool),
    LogicalBlockSize(usize),
}

impl LoopDevice {
    pub(super) fn set_direct_io(&self, requested: bool) -> Result<(), SystemError> {
        self.update_queue(QueueUpdate::DirectIo(requested))
    }

    pub(super) fn set_logical_block_size(&self, size: usize) -> Result<(), SystemError> {
        self.update_queue(QueueUpdate::LogicalBlockSize(size))
    }

    fn update_queue(&self, update: QueueUpdate) -> Result<(), SystemError> {
        let _noio = MemoryIoScope::enter_noio();
        // Raw-cache writeback may itself enter the loop I/O gate. Complete it
        // before taking that gate exclusive, in the shared owner lock order.
        let _raw = self.blkdev_meta().raw_cache_gate.write();
        if matches!(update, QueueUpdate::LogicalBlockSize(_)) && self.has_mount_holders() {
            // Claim precedes even a no-op size change, as in loop_set_block_size.
            return Err(SystemError::EBUSY);
        }
        for _ in 0..16 {
            let (backing, offset, generation, current, old_logical) = {
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
                    inner.logical_block_size,
                )
            };
            let (logical, requested) = match update {
                QueueUpdate::DirectIo(requested) => (old_logical, requested),
                QueueUpdate::LogicalBlockSize(size) => {
                    let logical = validate_logical_block_size(size)?;
                    if logical == old_logical {
                        return Ok(());
                    }
                    (
                        logical,
                        current || backing.flags().contains(FileFlags::O_DIRECT),
                    )
                }
            };
            let enabled = effective_dio(&backing, offset, logical, requested)?;
            if matches!(update, QueueUpdate::DirectIo(true)) && !enabled {
                return Err(SystemError::EINVAL);
            }
            let changed = enabled != current || logical != old_logical;
            if changed {
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
                if changed {
                    inner.logical_block_size = logical;
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
