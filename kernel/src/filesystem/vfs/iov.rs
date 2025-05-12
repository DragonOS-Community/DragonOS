use alloc::vec::Vec;
use system_error::SystemError;

use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
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
        self.0.iter().map(|x| x.iov_len).sum()
    }

    /// Constructs `IoVecs` from an array of `IoVec` in userspace.
    ///
    /// # Arguments
    ///
    /// * `iov` - Pointer to the array of `IoVec` in userspace
    /// * `iovcnt` - Number of `IoVec` elements in the array
    /// * `readv` - Whether this is for the `readv` syscall (currently unused)
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
        let iovs_reader = UserBufferReader::new(iov, iovcnt * core::mem::size_of::<IoVec>(), true)?;

        // 将用户空间的IoVec转换为引用（注意：这里的引用是静态的，因为用户空间的IoVec不会被释放）
        let iovs = iovs_reader.buffer::<IoVec>(0)?;

        let mut slices: Vec<IoVec> = Vec::with_capacity(iovs.len());

        for iov in iovs.iter() {
            if iov.iov_len == 0 {
                continue;
            }

            let _ = UserBufferWriter::new(iov.iov_base, iov.iov_len, true)?;
            slices.push(*iov);
        }

        return Ok(Self(slices));
    }

    /// Aggregates data from all IoVecs into a single buffer.
    ///
    /// This function reads data from each IoVec in sequence and combines them into
    /// a single contiguous buffer.
    ///
    /// # Returns
    ///
    /// Returns a [`Vec<u8>`] containing all the data from the IoVecs.
    ///
    /// # Examples
    ///
    /// ```rust
    /// let iovecs = IoVecs::from_user(/* ... */)?;
    /// let buffer = iovecs.gather();
    /// ```
    pub fn gather(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for slice in self.0.iter() {
            let buf_reader = UserBufferReader::new(slice.iov_base, slice.iov_len, true).unwrap();
            let slice = buf_reader.buffer::<u8>(0).unwrap();
            buf.extend_from_slice(slice);
        }
        return buf;
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
    pub fn scatter(&self, data: &[u8]) {
        let mut data: &[u8] = data;
        for slice in self.0.iter() {
            let len = core::cmp::min(slice.iov_len, data.len());
            if len == 0 {
                continue;
            }

            let mut buf_writer =
                UserBufferWriter::new(slice.iov_base, slice.iov_len, true).unwrap();
            let slice = buf_writer.buffer::<u8>(0).unwrap();

            slice[..len].copy_from_slice(&data[..len]);
            data = &data[len..];
        }
    }

    /// Creates a buffer with capacity equal to the total length of all IoVecs.
    ///
    /// # Arguments
    ///
    /// * `set_len` - If true, sets the length of the returned Vec to the total length of all IoVecs.
    ///               If false, the returned Vec will have length 0 but capacity equal to the total length.
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
