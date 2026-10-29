//! Generic block queries belong to the published disk/partition node. Driver
//! ioctls remain a separate fallback and never define the generic ABI.
use super::GenDisk;
use crate::{
    driver::base::block::block_device::{BlockDevice, LBA_SIZE},
    syscall::user_access::UserBufferWriter,
};
use system_error::SystemError;

pub(super) enum BlockIoctl {
    SizeSectors,
    SizeBytes,
    LogicalBlockSize,
    CacheBlockSize,
    PhysicalBlockSize,
    MinimumIo,
    OptimalIo,
    AlignmentOffset,
}

impl BlockIoctl {
    pub(super) fn from_cmd(cmd: u32) -> Option<Self> {
        Some(match cmd {
            0x1260 => Self::SizeSectors,         // BLKGETSIZE: unsigned long
            0x8008_1272 => Self::SizeBytes,      // BLKGETSIZE64: u64
            0x8008_1270 => Self::CacheBlockSize, // BLKBSZGET: int (opcode encodes size_t)
            0x1268 => Self::LogicalBlockSize,    // BLKSSZGET: int
            0x127b => Self::PhysicalBlockSize,   // BLKPBSZGET: unsigned int
            0x1278 => Self::MinimumIo,           // BLKIOMIN: unsigned int
            0x1279 => Self::OptimalIo,           // BLKIOOPT: unsigned int
            0x127a => Self::AlignmentOffset,     // BLKALIGNOFF: int
            _ => return None,
        })
    }

    pub(super) fn execute(
        self,
        disk: &GenDisk,
        device: &dyn BlockDevice,
        output: usize,
    ) -> Result<usize, SystemError> {
        match self {
            Self::SizeSectors | Self::SizeBytes => {
                let range = if disk.idx.is_none() {
                    device.disk_range()
                } else {
                    disk.range
                };
                let sectors = range
                    .lba_end
                    .checked_sub(range.lba_start)
                    .ok_or(SystemError::EOVERFLOW)?;
                match self {
                    Self::SizeSectors => put(output, sectors),
                    Self::SizeBytes => {
                        let bytes = u64::try_from(sectors)
                            .ok()
                            .and_then(|sectors| sectors.checked_mul(LBA_SIZE as u64))
                            .ok_or(SystemError::EOVERFLOW)?;
                        put(output, bytes)
                    }
                    _ => unreachable!(),
                }
            }
            Self::CacheBlockSize => put(output, disk.soft_block_size() as i32),
            _ => {
                let limits = device.io_limits();
                if !limits.is_valid() {
                    return Err(SystemError::EINVAL);
                }
                match self {
                    Self::LogicalBlockSize => put(output, limits.logical_block_size as i32),
                    Self::PhysicalBlockSize => put(output, limits.physical_block_size),
                    Self::MinimumIo => put(output, limits.io_min),
                    Self::OptimalIo => put(output, limits.io_opt),
                    Self::AlignmentOffset => {
                        let start = u64::try_from(disk.range.lba_start)
                            .map_err(|_| SystemError::EOVERFLOW)?;
                        let alignment = limits
                            .node_alignment(start, disk.idx.is_some())
                            .ok_or(SystemError::EOVERFLOW)?;
                        put(output, alignment)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn put<T: Copy>(output: usize, value: T) -> Result<usize, SystemError> {
    let mut writer = UserBufferWriter::new(output as *mut u8, core::mem::size_of::<T>(), true)?;
    writer.buffer_protected(0)?.write_one(0, &value)?;
    Ok(0)
}
