use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, object_for_task, owned::OwnedSlice,
    },
    filesystem::vfs::{file::File, MAX_RW_COUNT},
    mm::{access_ok, allocation::MemoryAllocationError, VirtAddr},
    process::ProcessManager,
    syscall::user_access::{
        copy_from_user_protected, user_accessible_len, UserBufferReader, UserBufferWriter,
    },
};

/// Linux UIO_MAXIOV: maximum number of iovec structures per syscall
const IOV_MAX: usize = 1024;
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    /// 缓冲区的起始地址
    pub iov_base: *mut u8,
    /// 缓冲区的长度
    pub iov_len: usize,
}

// A CPU metadata allocation budget, independent of page-table geometry.
// Each chunk is one order-0 page on the supported x86_64/RISC-V MM backends.
const CHUNK_BYTES: usize = 4096;
const ITEMS_PER_CHUNK: usize = CHUNK_BYTES / core::mem::size_of::<IoVec>();
const MAX_CHUNKS: usize = IOV_MAX.div_ceil(ITEMS_PER_CHUNK);

/// One immutable userspace iovec snapshot. Bounded typed allocations avoid
/// high-order physical requirements; UserBuffer borrows these same elements.
#[derive(Debug)]
pub struct IoVecs {
    chunks: [Option<OwnedSlice<IoVec>>; MAX_CHUNKS],
    total_len: usize,
}

impl IoVecs {
    fn empty() -> Self {
        Self {
            chunks: core::array::from_fn(|_| None),
            total_len: 0,
        }
    }

    fn try_allocate_in(
        count: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        debug_assert!(count <= IOV_MAX);
        let mut result = Self::empty();
        for (index, chunk) in result.chunks.iter_mut().enumerate() {
            let remaining = count.saturating_sub(index * ITEMS_PER_CHUNK);
            if remaining == 0 {
                break;
            }
            *chunk = Some(OwnedSlice::try_filled(
                creator,
                KernelByteKind::SlabUnreclaimable,
                remaining.min(ITEMS_PER_CHUNK),
                IoVec {
                    iov_base: core::ptr::null_mut(),
                    iov_len: 0,
                },
            )?);
        }
        Ok(result)
    }

    pub fn total_len(&self) -> usize {
        self.total_len
    }

    pub fn iter(&self) -> impl Iterator<Item = &IoVec> {
        self.chunks
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|chunk| chunk.iter())
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut IoVec> {
        self.chunks
            .iter_mut()
            .filter_map(Option::as_mut)
            .flat_map(|chunk| chunk.iter_mut())
    }

    fn get_mut(&mut self, index: usize) -> &mut IoVec {
        &mut self.chunks[index / ITEMS_PER_CHUNK]
            .as_mut()
            .expect("allocated iovec chunk")[index % ITEMS_PER_CHUNK]
    }

    /// Constructs `IoVecs` from an array of `IoVec` in userspace.
    ///
    /// # Arguments
    ///
    /// * `iov` - Pointer to the array of `IoVec` in userspace
    /// * `iovcnt` - Number of `IoVec` elements in the array
    /// * `readv` - Whether this is for the `readv` syscall (true = check write permission)
    ///
    /// # Returns
    ///
    /// Returns `Ok(IoVecs)` on success, or `Err(SystemError)` if any error occurs.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it operates on raw pointers from userspace.
    /// The caller must ensure:
    /// - The pointer `iov` is valid and points to at least `iovcnt` valid `IoVec` structures
    ///
    /// The copied metadata is retained once imported; userspace may race with
    /// the copy, as with Linux import_iovec(), but cannot modify the snapshot.
    #[inline(never)]
    pub unsafe fn from_user(
        iov: *const IoVec,
        iovcnt: usize,
        _readv: bool,
    ) -> Result<Self, SystemError> {
        if iovcnt > IOV_MAX {
            return Err(SystemError::EINVAL);
        }
        // Linux accepts an empty vector without inspecting the pointer. The
        // descriptor/mode checks still happen in the syscall's I/O path.
        if iovcnt == 0 {
            return Ok(Self::empty());
        }

        let elem_size = core::mem::size_of::<IoVec>();
        let total_bytes = iovcnt.checked_mul(elem_size).ok_or(SystemError::EINVAL)?;

        // Only does range check (user range) here.
        let iovs_reader =
            UserBufferReader::new(iov, total_bytes, true).map_err(|_| SystemError::EFAULT)?;

        // Use exception-table protected copy to avoid kernel faults when userspace pointer is bad.
        let iovs_buf = iovs_reader.buffer_protected(0)?;

        // Prepare all owned storage before copying any userspace metadata.
        // Failed partial allocations drop before recovery; completed snapshots
        // are never discarded and reimported during allocation recovery.
        let creator = object_for_task(&ProcessManager::current_pcb());
        let mut slices = File::recover_io(|| Self::try_allocate_in(iovcnt, &creator))?;
        for idx in 0..iovcnt {
            let offset = idx * elem_size;
            let one: IoVec = iovs_buf.read_one(offset)?;

            if one.iov_len > isize::MAX as usize {
                return Err(SystemError::EINVAL);
            }
            *slices.get_mut(idx) = one;
        }

        // Linux's single-segment ITER_UBUF fast path clips before access_ok;
        // multi-segment import validates each original range before clipping.
        if iovcnt == 1 {
            let one = slices.get_mut(0);
            one.iov_len = one.iov_len.min(MAX_RW_COUNT);
        }
        let mut remaining = MAX_RW_COUNT;
        for one in slices.iter_mut() {
            access_ok(VirtAddr::new(one.iov_base as usize), one.iov_len)
                .map_err(|_| SystemError::EFAULT)?;
            one.iov_len = one.iov_len.min(remaining);
            remaining -= one.iov_len;
        }

        slices.total_len = MAX_RW_COUNT - remaining;
        Ok(slices)
    }

    /// Aggregates data from all IoVecs into a single buffer.
    ///
    /// This function reads data from each IoVec in sequence and combines them into
    /// a single contiguous buffer.
    ///
    /// **Returns:**
    ///
    /// Returns a [`Vec<u8>`] containing the data copied from the IoVecs.
    ///
    /// **To Be patient:**
    ///
    /// If a buffer is only partially accessible, data is copied up to **the first
    /// inaccessible byte** and the remaining iovecs are ignored. If no data can be
    /// read at all, `Err(SystemError::EFAULT)` is returned.
    pub fn gather(&self) -> Result<Vec<u8>, SystemError> {
        self.gather_prefix(self.total_len())
    }

    /// Materialize only the request admitted by the File write policy.
    pub fn gather_prefix(&self, limit: usize) -> Result<Vec<u8>, SystemError> {
        const GATHER_SCRATCH_LEN: usize = 64 * 1024;

        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        let mut read_any = false;

        for iov in self.iter() {
            if buf.len() == limit {
                break;
            }
            let len = iov.iov_len.min(limit - buf.len());
            if len == 0 {
                continue;
            }
            let base = VirtAddr::new(iov.iov_base as usize);
            // 检查从 iov_base 开始有多少 bytes 在 vma 内部且实际可以访问
            let accessible = user_accessible_len(base, len, false /* read */);

            // 如果一个字节都不能访问
            if accessible == 0 {
                if !read_any {
                    return Err(SystemError::EFAULT);
                }
                return Ok(buf);
            }

            let mut copied = 0usize;
            while copied < accessible {
                let want = core::cmp::min(GATHER_SCRATCH_LEN, accessible - copied);
                if scratch.len() < want {
                    scratch
                        .try_reserve(want - scratch.len())
                        .map_err(|_| SystemError::ENOMEM)?;
                    scratch.resize(want, 0);
                }

                // 使用异常保护的拷贝，与 scatter 保持一致
                match unsafe { copy_from_user_protected(&mut scratch[..want], base + copied) } {
                    Ok(_) => {
                        buf.try_reserve(want).map_err(|_| SystemError::ENOMEM)?;
                        buf.extend_from_slice(&scratch[..want]);
                    }
                    Err(SystemError::EFAULT) => {
                        // Linux: return partial data if any bytes were copied.
                        if !read_any {
                            return Err(SystemError::EFAULT);
                        }
                        return Ok(buf);
                    }
                    Err(e) => return Err(e),
                }

                copied += want;
                if want > 0 {
                    read_any = true;
                }
            }

            // 如果没有读取完整个 iov，说明遇到了不可访问的区域
            if accessible < len {
                return Ok(buf);
            }
        }

        Ok(buf)
    }

    /// Scatters the given data into the IoVecs.
    ///
    /// This function writes data sequentially to each IoVec, splitting the input data
    /// across multiple buffers as needed. If the input data is smaller than the total
    /// capacity of the IoVecs, only the required amount of data will be written.
    /// If the input data is larger than the total capacity, the excess data will be ignored.
    ///
    /// # Arguments
    ///
    /// * `data` - The data to be scattered across the IoVecs
    ///
    /// # Examples
    ///
    /// ```rust
    /// let iovecs = IoVecs::from_user(/* ... */)?;
    /// iovecs.scatter(&[1, 2, 3, 4, 5]);
    /// ```
    pub fn scatter(&self, data: &[u8]) -> Result<usize, SystemError> {
        let mut remaining = data;
        let mut written_any = false;
        let mut total_written = 0usize;

        for iov in self.iter() {
            if remaining.is_empty() {
                break;
            }

            let want = core::cmp::min(iov.iov_len, remaining.len());
            if want == 0 {
                continue;
            }

            let base = VirtAddr::new(iov.iov_base as usize);
            let accessible = user_accessible_len(base, want, true /*write*/);
            if accessible == 0 {
                if !written_any {
                    return Err(SystemError::EFAULT);
                }
                break;
            }

            let mut writer = UserBufferWriter::new(iov.iov_base, accessible, true)?;
            let mut user_buf = writer.buffer_protected(0)?;
            user_buf.write_to_user(0, &remaining[..accessible])?;

            written_any = true;
            total_written = total_written.saturating_add(accessible);
            remaining = &remaining[accessible..];

            if accessible < want {
                // Hit an unmapped/forbidden region; stop as Linux does.
                break;
            }
        }

        Ok(total_written)
    }

    /// Scatters the complete input or reports a user-memory fault.
    ///
    /// This is intended for record-oriented readers, where a short copy is an
    /// error even though the record may already have been consumed. Bytes
    /// copied before the fault are not rolled back.
    pub fn scatter_exact(&self, data: &[u8]) -> Result<(), SystemError> {
        if self.scatter(data)? != data.len() {
            return Err(SystemError::EFAULT);
        }
        Ok(())
    }
}

/// Exercise actual metadata allocations and their charge rollback without
/// importing userspace or exposing a new debug interface.
#[cfg(feature = "mm_selftest")]
pub(crate) fn run_accounting_selftest() -> Result<bool, MemoryAllocationError> {
    use crate::cgroup::memory::{MemoryAccount, MemoryPolicy};
    use crate::mm::allocator::kernel_allocator::allocation_footprint;
    use core::alloc::Layout;

    let root = MemoryAccount::new(0x696f7600, None)?;
    let account = MemoryAccount::new(0x696f7601, Some(root.clone()))?;
    let creator = ObjectAccount::new(&account)?;
    let mut valid = true;
    for count in [0, 1, ITEMS_PER_CHUNK, ITEMS_PER_CHUNK + 1, IOV_MAX] {
        let storage = IoVecs::try_allocate_in(count, &creator)?;
        let mut expected_bytes = 0;
        for chunk in storage.chunks.iter().filter_map(Option::as_ref) {
            let layout = Layout::array::<IoVec>(chunk.len()).map_err(|_| SystemError::ENOMEM)?;
            valid &= layout.size() <= CHUNK_BYTES;
            expected_bytes += allocation_footprint(layout);
        }
        valid &= storage.iter().count() == count
            && storage.iter().all(|iov| iov.iov_len == 0)
            && account.snapshot().kernel_bytes.iter().sum::<u64>() == expected_bytes;
        drop(storage);
        valid &= account.current_pages() == 0
            && root.current_pages() == 0
            && account
                .snapshot()
                .kernel_bytes
                .iter()
                .all(|bytes| *bytes == 0);
    }
    // The first full page is admitted, while the following chunk fails.
    // Cancellation must release that first page before the caller recovers.
    account.set_policy(MemoryPolicy {
        max: Some(1),
        ..Default::default()
    });
    let failed = IoVecs::try_allocate_in(ITEMS_PER_CHUNK + 1, &creator);
    account.set_policy(MemoryPolicy::default());
    valid &= matches!(failed, Err(MemoryAllocationError::Charge(_)))
        && account.current_pages() == 0
        && root.current_pages() == 0
        && account
            .snapshot()
            .kernel_bytes
            .iter()
            .all(|bytes| *bytes == 0);
    Ok(valid)
}
