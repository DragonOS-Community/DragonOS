use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    mm::verify_area,
    mm::VirtAddr,
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

/// 用于存储多个来自用户空间的IoVec
///
/// 由于目前内核中的文件系统还不支持分散读写，所以暂时只支持将用户空间的IoVec聚合成一个缓冲区，然后进行操作。
/// TODO：支持分散读写
#[derive(Debug)]
pub struct IoVecs(Vec<IoVec>);

impl IoVecs {
    /// 获取IoVecs中所有缓冲区的总长度
    #[inline(never)]
    pub fn total_len(&self) -> usize {
        self.0
            .iter()
            .try_fold(0usize, |acc, x| acc.checked_add(x.iov_len))
            .unwrap_or(usize::MAX)
    }

    /// Borrow the validated iovec list.
    pub fn iovs(&self) -> &[IoVec] {
        &self.0
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
    /// - The userspace memory is not modified during this operation
    #[inline(never)]
    pub unsafe fn from_user(
        iov: *const IoVec,
        iovcnt: usize,
        _readv: bool,
    ) -> Result<Self, SystemError> {
        // Linux: iovcnt must be > 0 and not unreasonably large.
        if iovcnt == 0 || iovcnt > IOV_MAX {
            return Err(SystemError::EINVAL);
        }

        let elem_size = core::mem::size_of::<IoVec>();
        let total_bytes = iovcnt.checked_mul(elem_size).ok_or(SystemError::EINVAL)?;

        // Only does range check (user range) here.
        let iovs_reader = UserBufferReader::new(iov, total_bytes, true)?;

        // Use exception-table protected copy to avoid kernel faults when userspace pointer is bad.
        let iovs_buf = iovs_reader.buffer_protected(0)?;

        let mut slices: Vec<IoVec> = Vec::with_capacity(iovcnt);
        for idx in 0..iovcnt {
            let offset = idx * elem_size;
            let one: IoVec = iovs_buf.read_one(offset)?;

            // Linux behavior: always validate iov_base is a user pointer, even when iov_len==0.
            // This matches Linux access_ok(addr, 0) behavior and is required by gVisor tests.
            let base = VirtAddr::new(one.iov_base as usize);

            // Only do lightweight address range check (like Linux's access_ok).
            // This checks that the address range is within user space limits,
            // but does NOT traverse page tables or check actual mappings.
            // Actual page mapping/permission checks happen during copy operations.
            verify_area(base, one.iov_len)?;

            // Skip zero-length iovecs after validation
            if one.iov_len == 0 {
                continue;
            }

            // If the first byte isn't writable/readable at all, fail early with EFAULT.
            // Partial accessibility is handled by the syscall implementation.
            // Note: user_accessible_len returns 0 for null pointers (addr.is_null() check),
            // so null pointer detection is covered here.
            let accessible = user_accessible_len(base, one.iov_len, _readv /* check_write */);
            if accessible == 0 {
                return Err(SystemError::EFAULT);
            }

            slices.push(one);
        }

        Ok(Self(slices))
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
        let mut buf = Vec::with_capacity(self.total_len());
        let mut read_any = false;

        for iov in self.0.iter() {
            let base = VirtAddr::new(iov.iov_base as usize);
            // 检查从 iov_base 开始有多少 bytes 在 vma 内部且实际可以访问
            let accessible = user_accessible_len(base, iov.iov_len, false /* read */);

            // 如果一个字节都不能访问
            if accessible == 0 {
                if !read_any {
                    return Err(SystemError::EFAULT);
                }
                return Ok(buf);
            }

            // 使用异常保护的拷贝，与 scatter 保持一致
            let mut chunk = alloc::vec![0u8; accessible];
            match unsafe { copy_from_user_protected(&mut chunk, base) } {
                Ok(_) => {
                    buf.extend_from_slice(&chunk);
                    read_any = true;
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

            // 如果没有读取完整个 iov，说明遇到了不可访问的区域
            if accessible < iov.iov_len {
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
    pub fn scatter(&self, data: &[u8]) -> Result<(), SystemError> {
        let mut remaining = data;
        let mut written_any = false;

        for iov in self.0.iter() {
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
            remaining = &remaining[accessible..];

            if accessible < want {
                // Hit an unmapped/forbidden region; stop as Linux does.
                break;
            }
        }

        Ok(())
    }

    /// Creates a buffer with capacity equal to the total length of all IoVecs.
    ///
    /// # Arguments
    ///
    /// * `set_len` - If true, sets the length of the returned Vec to the total length of all IoVecs.
    ///   If false, the returned Vec will have length 0 but capacity equal to the total length.
    ///
    /// # Returns
    ///
    /// A new [`Vec<u8>`] with capacity (and potentially length) equal to the total length of all IoVecs.
    pub fn new_buf(&self, set_len: bool) -> Vec<u8> {
        let total_len = self.total_len();
        let mut buf: Vec<u8> = Vec::with_capacity(total_len);

        if set_len {
            buf.resize(total_len, 0);
        }
        return buf;
    }
}
