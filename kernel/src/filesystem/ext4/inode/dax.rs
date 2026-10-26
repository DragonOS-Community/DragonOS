//! Immutable ext4 mappings over an owned device direct-access capability.
//! File layout stays in ext4; device addresses and lifetime stay in the block layer.
use super::{Ext4FileSystem, LockedExt4Inode};
use crate::{
    arch::MMArch,
    driver::base::block::direct_access::DirectAccessRange,
    filesystem::{ext4::filesystem::Ext4DaxMode, vfs},
    mm::{
        allocation::MemoryAllocationError,
        fault::{FaultFlags, FaultRetryWait, PageFaultMessage},
        MemoryManagementArch, PhysAddr, VmFaultReason, VmFlags,
    },
    syscall::user_buffer::UserBuffer,
};
use alloc::{sync::Arc, vec::Vec};
use kdepends::another_ext4::{self, FileType, ReadPlan, ReadSegment};
use system_error::SystemError;

// Linux fs/ext4/ext4.h: extents are the only layout planned by another_ext4.
const EXTENTS: u32 = 0x0008_0000;
const DAX: u32 = 0x0200_0000;
const DAX_INCOMPATIBLE: u32 = 0x0000_0800 | 0x0000_4000 | 0x0010_0000 | 0x1000_0000;
const BLOCK: usize = another_ext4::BLOCK_SIZE;

#[derive(Debug)]
struct MetadataReadRetry {
    fs: Arc<Ext4FileSystem>,
    observed: u64,
}

impl FaultRetryWait for MetadataReadRetry {
    fn wait(&self) -> Result<(), MemoryAllocationError> {
        self.fs
            .wait_metadata_mutation_progress(self.observed)
            .map_err(Into::into)
    }
}

impl LockedExt4Inode {
    pub(super) fn initial_dax_policy(
        fs: &Arc<Ext4FileSystem>,
        inode_num: u32,
        file_type: FileType,
    ) -> Result<bool, SystemError> {
        if MMArch::PAGE_SIZE != BLOCK
            || file_type != FileType::RegularFile
            || !fs.is_read_only()
            || fs.dax_access.is_none()
        {
            return Ok(false);
        }
        let mode = fs._mount_options.dax.unwrap_or(Ext4DaxMode::Inode);
        if mode == Ext4DaxMode::Never {
            return Ok(false);
        }
        let flags = fs.retry_metadata_read_contention(|| fs.fs.inode_flags(inode_num))?;
        Ok(flags & EXTENTS != 0
            && flags & DAX_INCOMPATIBLE == 0
            && (mode == Ext4DaxMode::Always || flags & DAX != 0))
    }

    fn dax_source(&self) -> Result<(Arc<Ext4FileSystem>, usize, u32), SystemError> {
        let inner = self.inner.lock();
        let size = usize::try_from(inner.cached_file_size.ok_or(SystemError::EIO)?)
            .map_err(|_| SystemError::EOVERFLOW)?;
        Ok((inner.concret_fs(), size, inner.inner_inode_num))
    }

    /// Copies a bounded source window. No guard escapes to a user copy.
    fn dax_read_chunk(
        &self,
        offset: usize,
        buffer: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let _operation = self.begin_operation()?;
        let (fs, size, inode_num) = self.dax_source()?;
        let count = buffer.len().min(size.saturating_sub(offset));
        if count == 0 {
            return Ok(0);
        }
        let start = offset / BLOCK * BLOCK;
        let end = offset.checked_add(count).ok_or(SystemError::EOVERFLOW)?;
        let length =
            end.checked_add(BLOCK - 1).ok_or(SystemError::EOVERFLOW)? / BLOCK * BLOCK - start;
        let plan =
            fs.retry_metadata_read_contention(|| fs.fs.plan_read(inode_num, start, length, 16))?;
        let access = fs.dax_access.as_ref().ok_or(SystemError::EIO)?;
        let mut copied = 0;
        for segment in plan.segments {
            let logical = (segment.logical_start() as usize)
                .checked_mul(BLOCK)
                .ok_or(SystemError::EOVERFLOW)?;
            let segment_len = (segment.block_count() as usize)
                .checked_mul(BLOCK)
                .ok_or(SystemError::EOVERFLOW)?;
            let segment_end = logical
                .checked_add(segment_len)
                .ok_or(SystemError::EOVERFLOW)?;
            let position = offset.checked_add(copied).ok_or(SystemError::EOVERFLOW)?;
            if position >= segment_end {
                continue;
            }
            if position < logical {
                return Err(SystemError::EIO.into());
            }
            let take = (segment_end - position).min(count - copied);
            match segment {
                ReadSegment::Zero { .. } => buffer[copied..copied + take].fill(0),
                ReadSegment::Mapped { physical_start, .. } => {
                    let physical = usize::try_from(physical_start)
                        .map_err(|_| SystemError::EOVERFLOW)?
                        .checked_mul(BLOCK)
                        .and_then(|base| base.checked_add(position - logical))
                        .ok_or(SystemError::EOVERFLOW)?;
                    access.read_at(physical, &mut buffer[copied..copied + take])?;
                }
            }
            copied += take;
            if copied == count {
                return Ok(copied);
            }
        }
        Err(SystemError::EIO.into())
    }

    pub(super) fn dax_read_kernel(
        &self,
        offset: usize,
        buffer: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let mut total = 0;
        while total < buffer.len() {
            let position = offset.checked_add(total).ok_or(SystemError::EOVERFLOW)?;
            let take = (buffer.len() - total).min(vfs::read_buffer::MAX_CHUNK);
            let count = match self.dax_read_chunk(position, &mut buffer[total..total + take]) {
                Ok(count) => count,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if count == 0 {
                break;
            }
            total += count;
        }
        Ok(total)
    }

    pub(super) fn dax_read_user(
        &self,
        offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        let len = len.min(writer.len());
        let mut total = 0;
        let mut scratch = vfs::read_buffer::ReadScratch::default();
        while total < len {
            let position = match offset.checked_add(total) {
                Some(position) => position,
                None if total != 0 => break,
                None => return Err(SystemError::EOVERFLOW),
            };
            let take = (len - total).min(vfs::read_buffer::MAX_CHUNK);
            let buffer = match scratch.prepare(take) {
                Ok(buffer) => buffer,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            let count = match vfs::file::File::recover_io(|| self.dax_read_chunk(position, buffer))
            {
                Ok(count) => count,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if count == 0 {
                break;
            }
            // Source planning and its lifecycle owner have returned. The destination
            // may fault recursively into this same inode without any FS/MM guard.
            let copied = match writer.write_prefix_to_user(total, &buffer[..count]) {
                Ok(copied) => copied,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += copied;
            if copied != count {
                break;
            }
        }
        Ok(total)
    }

    pub(in crate::filesystem::ext4) unsafe fn dax_fault(
        &self,
        pfm: &mut PageFaultMessage,
    ) -> VmFaultReason {
        let _operation = match self.begin_operation() {
            Ok(operation) => operation,
            Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let (fs, size, inode_num) = match self.dax_source() {
            Ok(source) => source,
            Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let Some(offset) = pfm
            .backing_pgoff()
            .and_then(|index| index.checked_mul(BLOCK))
        else {
            return VmFaultReason::VM_FAULT_SIGBUS;
        };
        if offset >= size {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        let observed = fs.fs.metadata_mutation_generation();
        let plan = match fs.fs.plan_read(inode_num, offset, BLOCK, 1) {
            Ok(plan) => plan,
            Err(error) if error.code() == another_ext4::ErrCode::EAGAIN => {
                let wait = match Arc::try_new(MetadataReadRetry { fs, observed }) {
                    Ok(wait) => wait,
                    Err(_) => return pfm.allocation_failure(SystemError::ENOMEM.into()),
                };
                pfm.set_retry_wait(wait);
                return VmFaultReason::VM_FAULT_RETRY;
            }
            Err(error) if error.code() == another_ext4::ErrCode::ENOMEM => {
                return pfm.allocation_failure(SystemError::ENOMEM.into());
            }
            Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let mapped = match complete_mapped_page(&plan, offset, size) {
            Ok(mapped) => mapped,
            Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let Some(physical) = mapped else {
            // The demand owner performs cold I/O outside AddressSpace and
            // invalidation guards. Ready managed pages retain ordinary COW.
            let Some(cache) = self.inner.lock().page_cache.clone() else {
                return VmFaultReason::VM_FAULT_SIGBUS;
            };
            let index = offset / BLOCK;
            if let Some(pin) = cache.manager().peek_page_pinned(index) {
                pfm.set_page_pinned(pin);
                return VmFaultReason::empty();
            }
            return match cache.fault_demand_retry_wait(
                index,
                pfm.data_account(),
                pfm.creator_object(),
            ) {
                Ok(wait) => {
                    pfm.set_retry_wait(wait);
                    VmFaultReason::VM_FAULT_RETRY
                }
                Err(error) => pfm.allocation_failure(error),
            };
        };
        let Some(access) = fs.dax_access.as_ref() else {
            return VmFaultReason::VM_FAULT_SIGBUS;
        };
        let paddr = match access.page_paddr(physical) {
            Ok(paddr) => paddr,
            Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let old = pfm.external_pfn();
        if pfm.flags().contains(FaultFlags::FAULT_FLAG_WRITE) {
            if pfm.vm_flags().contains(VmFlags::VM_SHARED) {
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
            let mut source = Vec::new();
            if source.try_reserve_exact(BLOCK).is_err() {
                return pfm.allocation_failure(SystemError::ENOMEM.into());
            }
            source.resize(BLOCK, 0);
            if access.read_at(physical, &mut source).is_err() {
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
            return pfm.cow_external_page(old, &source);
        }
        if let Some(old) = old {
            return if old == paddr {
                VmFaultReason::VM_FAULT_COMPLETED
            } else {
                VmFaultReason::VM_FAULT_SIGBUS
            };
        }
        pfm.map_external_pfn(paddr, false)
    }

    pub(super) fn dax_pin_readonly(
        &self,
        offset: usize,
        expected: PhysAddr,
    ) -> Result<Option<DirectAccessRange>, SystemError> {
        if !self.dax_enabled {
            return Ok(None);
        }
        let (fs, size, _) = self.dax_source()?;
        if offset % BLOCK != 0
            || offset.checked_add(BLOCK).ok_or(SystemError::EOVERFLOW)? > size
            || !expected.check_aligned(BLOCK)
        {
            return Err(SystemError::EFAULT);
        }
        let access = fs.dax_access.as_ref().ok_or(SystemError::EIO)?;
        let base = access.checked_paddr(0, access.len())?;
        let relative = expected
            .data()
            .checked_sub(base.data())
            .ok_or(SystemError::EFAULT)?;
        let range = access.subrange(relative, BLOCK)?;
        if range.page_paddr(0)? != expected {
            return Err(SystemError::EFAULT);
        }
        Ok(Some(range))
    }
}

/// None selects the managed zero/tail path, never an unvalidated raw tail.
fn complete_mapped_page(
    plan: &ReadPlan,
    offset: usize,
    size: usize,
) -> Result<Option<usize>, SystemError> {
    let valid = size.saturating_sub(offset).min(BLOCK);
    if valid == 0 || plan.valid_bytes != valid {
        return Err(SystemError::EIO);
    }
    match plan.segments.as_slice() {
        [ReadSegment::Mapped {
            logical_start,
            physical_start,
            blocks: 1,
        }] if *logical_start as usize == offset / BLOCK => {
            let physical = usize::try_from(*physical_start)
                .map_err(|_| SystemError::EOVERFLOW)?
                .checked_mul(BLOCK)
                .ok_or(SystemError::EOVERFLOW)?;
            Ok((valid == BLOCK).then_some(physical))
        }
        [ReadSegment::Zero {
            logical_start,
            blocks: 1,
        }] if *logical_start as usize == offset / BLOCK => Ok(None),
        _ => Err(SystemError::EIO),
    }
}
