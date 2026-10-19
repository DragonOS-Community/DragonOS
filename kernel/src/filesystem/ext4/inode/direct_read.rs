//! Native direct-read admission retains the original user iterator. Mapping
//! lifetime protects only planning and source I/O; user faults run afterwards.
use super::LockedExt4Inode;
use crate::{
    filesystem::vfs::{self, file::File, IndexNode},
    libs::mutex::Mutex,
    mm::allocation::MemoryAllocationError,
    syscall::user_buffer::UserBuffer,
};
use kdepends::another_ext4::{self, ReadSegment};
use system_error::SystemError;

struct ReadBatch {
    count: usize,
    checked_mapping: bool,
}

/// Alignment of the original destination iterator, independent of the batch
/// scratch. Kernel callers supply CPU memory, with no user/DMA address mask.
enum ReadDestination<'a, 'b> {
    User(&'a UserBuffer<'b>),
    Kernel,
}

impl ReadDestination<'_, '_> {
    fn is_aligned_at(&self, offset: usize, len: usize, mask: usize) -> bool {
        match self {
            Self::User(writer) => writer.is_aligned_at(offset, len, mask, mask),
            Self::Kernel => len & mask == 0,
        }
    }
}

impl LockedExt4Inode {
    pub(super) fn read_direct_user(
        &self,
        offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        let len = len.min(writer.len());
        if len == 0 {
            return Ok(0);
        }
        let initial_size = {
            let _operation = self.begin_operation()?;
            let _size = self.size_lock.read();
            self.inner.lock().cached_file_size.ok_or(SystemError::EIO)? as usize
        };
        if offset >= initial_size {
            return Ok(0);
        }
        let mut total = 0;
        let mut checked_mapping = false;
        let mut scratch = vfs::read_buffer::ReadScratch::default();
        while total < len {
            let position = offset.checked_add(total).ok_or(SystemError::EOVERFLOW)?;
            // The planner requires whole filesystem blocks, independently of
            // the device's (possibly smaller) direct-I/O alignment.
            let block_offset = position % another_ext4::BLOCK_SIZE;
            let want = (vfs::read_buffer::MAX_CHUNK - block_offset).min(len - total);
            let buffer = scratch.prepare(want)?;
            let batch = File::recover_io(|| {
                self.prepare_direct_read_batch(
                    position,
                    len - total,
                    total,
                    ReadDestination::User(writer),
                    buffer,
                    initial_size,
                    checked_mapping,
                )
            })?;
            checked_mapping |= batch.checked_mapping;
            if batch.count == 0 {
                return Err(SystemError::EIO);
            }
            // Every operation/admission/size/io/mapping guard has returned.
            // A destination mapping may fault back into this same inode.
            let copied = writer.write_prefix_to_user(total, &buffer[..batch.count])?;
            total += copied;
            if copied != batch.count {
                // ext4 does not request IOMAP_DIO_PARTIAL: a later fault is
                // an operation error even when earlier bytes were modified.
                return Err(SystemError::EFAULT);
            }
        }
        // iomap executes the requested transfer before clipping its reported
        // result. A later destination fault must not disappear behind EOF.
        Ok(total.min(initial_size - offset))
    }

    /// Native kernel direct I/O uses the same mapped/hole admission as user
    /// reads, filling the caller's CPU slice without another scratch buffer.
    pub(super) fn read_direct_kernel(
        &self,
        offset: usize,
        len: usize,
        buffer: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let len = len.min(buffer.len());
        if len == 0 {
            return Ok(0);
        }
        let initial_size = {
            let _operation = self.begin_operation()?;
            let _size = self.size_lock.read();
            self.inner.lock().cached_file_size.ok_or(SystemError::EIO)? as usize
        };
        if offset >= initial_size {
            return Ok(0);
        }
        let mut total = 0;
        let mut checked_mapping = false;
        while total < len {
            let position = offset.checked_add(total).ok_or(SystemError::EOVERFLOW)?;
            let block_offset = position % another_ext4::BLOCK_SIZE;
            let want = (vfs::read_buffer::MAX_CHUNK - block_offset).min(len - total);
            let batch = self.prepare_direct_read_batch(
                position,
                len - total,
                total,
                ReadDestination::Kernel,
                &mut buffer[total..total + want],
                initial_size,
                checked_mapping,
            )?;
            if batch.count == 0 {
                return Err(SystemError::EIO.into());
            }
            checked_mapping |= batch.checked_mapping;
            total += batch.count;
        }
        Ok(total.min(initial_size - offset))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_direct_read_batch(
        &self,
        position: usize,
        remaining: usize,
        output_offset: usize,
        destination: ReadDestination<'_, '_>,
        buffer: &mut [u8],
        initial_size: usize,
        checked_mapping: bool,
    ) -> Result<ReadBatch, MemoryAllocationError> {
        let _operation = self.begin_operation()?;
        let _admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_range_before_eager(position, buffer.len())?;
        let _size = self.size_lock.read();
        let _io = self.io_lock.lock();
        let _mapping = self.mapping_io.begin_read()?;
        let (fs, inode_num, current_size) = {
            let inner = self.inner.lock();
            (
                inner.concret_fs(),
                inner.inner_inode_num,
                inner.cached_file_size.ok_or(SystemError::EIO)? as usize,
            )
        };
        let start = position - position % another_ext4::BLOCK_SIZE;
        let window_end = position
            .checked_add(buffer.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let window_len = window_end
            .checked_sub(start)
            .ok_or(SystemError::EOVERFLOW)?
            .checked_add(another_ext4::BLOCK_SIZE - 1)
            .ok_or(SystemError::EOVERFLOW)?
            / another_ext4::BLOCK_SIZE
            * another_ext4::BLOCK_SIZE;
        let plan = fs.retry_metadata_read_contention(|| {
            fs.fs.plan_direct_read(inode_num, start, window_len, 16)
        })?;
        let Some(segment) = plan.segments.into_iter().find(|segment| {
            let start = segment.logical_start() as usize * another_ext4::BLOCK_SIZE;
            let end = start + segment.block_count() as usize * another_ext4::BLOCK_SIZE;
            start <= position && position < end
        }) else {
            return Err(SystemError::EIO.into());
        };
        let logical_start = (segment.logical_start() as usize)
            .checked_mul(another_ext4::BLOCK_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let segment_len = (segment.block_count() as usize)
            .checked_mul(another_ext4::BLOCK_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let within = position - logical_start;
        // This range is clipped by mapping/request boundaries, but not i_size:
        // an aligned request can legitimately return a non-sector EOF tail.
        let mapped_len = (segment_len - within).min(remaining);
        let count = mapped_len.min(buffer.len());
        match segment {
            ReadSegment::Zero { .. } => buffer[..count].fill(0),
            ReadSegment::Mapped { physical_start, .. } => {
                let mask = (1usize << fs.gendisk.block_size_log2()) - 1;
                // Current devices expose one logical-sector alignment, with
                // no separate DMA mask. Keep the two iterator masks explicit.
                if (position | mapped_len) & mask != 0
                    || (!checked_mapping
                        && !destination.is_aligned_at(output_offset, remaining, mask))
                {
                    return Err(SystemError::EINVAL.into());
                }
                let physical = usize::try_from(physical_start)
                    .map_err(|_| SystemError::EOVERFLOW)?
                    .checked_mul(another_ext4::BLOCK_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?
                    .checked_add(within)
                    .ok_or(SystemError::EOVERFLOW)?;
                let capacity = fs
                    .gendisk
                    .range()
                    .len()
                    .checked_mul(crate::driver::base::block::block_device::LBA_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?;
                if physical.checked_add(count).ok_or(SystemError::EOVERFLOW)? > capacity {
                    return Err(SystemError::EFBIG.into());
                }
                // Only the current inode's validated mapped extent supplies
                // this data, including its final block's EOF tail. Unwritten
                // or unallocated blocks take the Zero branch instead.
                let copied = IndexNode::read_at(
                    &*fs.gendisk,
                    physical,
                    count,
                    &mut buffer[..count],
                    Mutex::new(vfs::FilePrivateData::Unused).lock(),
                )?;
                if copied != count {
                    return Err(SystemError::EIO.into());
                }
                // The full mapped transfer still admits/copies destination
                // pages beyond reported EOF. Never expose residual storage
                // bytes there: the page-cache contract already treats that
                // tail as zero, independent of lower truncate persistence.
                let valid = current_size
                    .min(initial_size)
                    .saturating_sub(position)
                    .min(count);
                buffer[valid..count].fill(0);
                return Ok(ReadBatch {
                    count,
                    checked_mapping: true,
                });
            }
        }
        Ok(ReadBatch {
            count,
            checked_mapping: false,
        })
    }
}
