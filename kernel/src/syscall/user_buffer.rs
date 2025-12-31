//! 用户缓冲区包装类型
//!
//! 提供对用户空间缓冲区的安全访问接口，所有操作都通过异常表保护

use crate::mm::VirtAddr;
use num::traits::FromBytes;
use system_error::SystemError;

/// 用户空间缓冲区的安全包装
///
/// 这个类型封装了对用户空间内存的访问，确保所有读写操作
/// 都通过异常表机制处理可能的页错误
pub struct UserBuffer<'a> {
    /// 用户空间地址
    user_addr: VirtAddr,
    /// 缓冲区长度
    len: usize,
    /// 生命周期标记
    _phantom: core::marker::PhantomData<&'a ()>,
}

impl<'a> UserBuffer<'a> {
    /// 创建一个新的用户缓冲区包装
    ///
    /// # 参数
    /// - `addr`: 用户空间地址
    /// - `len`: 缓冲区长度
    ///
    /// # 安全性
    /// 调用者必须确保地址和长度是有效的用户空间范围
    pub(crate) unsafe fn new(addr: VirtAddr, len: usize) -> Self {
        Self {
            user_addr: addr,
            len,
            _phantom: core::marker::PhantomData,
        }
    }

    /// 获取缓冲区长度
    pub fn len(&self) -> usize {
        self.len
    }

    /// 检查缓冲区是否为空
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn user_addr(&self) -> VirtAddr {
        self.user_addr
    }

    /// 从用户缓冲区读取数据到内核缓冲区
    ///
    /// # 参数
    /// - `offset`: 用户缓冲区内的偏移量
    /// - `dst`: 目标内核缓冲区
    ///
    /// # 返回值
    /// - `Ok(len)`: 成功读取的字节数
    /// - `Err(SystemError)`: 读取失败（如地址不在VMA中）
    pub fn read_from_user(&self, offset: usize, dst: &mut [u8]) -> Result<usize, SystemError> {
        // offset == src.len is valid, as long as don't try to dereference it in &src[offset..]
        if offset > self.len {
            return Err(SystemError::EINVAL);
        }

        let available = self.len - offset;
        let copy_len = core::cmp::min(dst.len(), available);

        if copy_len == 0 {
            return Ok(0);
        }

        let src_addr = VirtAddr::new(self.user_addr.data() + offset);

        unsafe {
            crate::syscall::user_access::copy_from_user_protected(&mut dst[..copy_len], src_addr)
        }
    }

    /// 将内核缓冲区数据写入用户缓冲区
    ///
    /// # 参数
    /// - `offset`: 用户缓冲区内的偏移量
    /// - `src`: 源内核缓冲区
    ///
    /// # 返回值
    /// - `Ok(len)`: 成功写入的字节数
    /// - `Err(SystemError)`: 写入失败（如地址不在VMA中）
    pub fn write_to_user(&mut self, offset: usize, src: &[u8]) -> Result<usize, SystemError> {
        // offset == src.len is valid, as long as don't try to dereference it in &src[offset..]
        if offset > self.len {
            return Err(SystemError::EINVAL);
        }

        let available = self.len - offset;
        let copy_len = core::cmp::min(src.len(), available);

        if copy_len == 0 {
            return Ok(0);
        }

        let dst_addr = VirtAddr::new(self.user_addr.data() + offset);

        unsafe { crate::syscall::user_access::copy_to_user_protected(dst_addr, &src[..copy_len]) }
    }

    /// 从用户缓冲区读取单个值
    ///
    /// # 类型参数
    /// - `T`: 要读取的类型
    ///
    /// # 参数
    /// - `offset`: 用户缓冲区内的偏移量
    ///
    /// # 返回值
    /// - `Ok(value)`: 成功读取的值
    /// - `Err(SystemError)`: 读取失败
    pub fn read_one<T>(&self, offset: usize) -> Result<T, SystemError> {
        let size = core::mem::size_of::<T>();
        if offset + size > self.len {
            return Err(SystemError::EINVAL);
        }

        let mut value = core::mem::MaybeUninit::<T>::uninit();
        let dst_slice =
            unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size) };

        self.read_from_user(offset, dst_slice)?;

        Ok(unsafe { value.assume_init() })
    }

    /// 向用户缓冲区写入单个值
    ///
    /// # 类型参数
    /// - `T`: 要写入的类型
    ///
    /// # 参数
    /// - `offset`: 用户缓冲区内的偏移量
    /// - `value`: 要写入的值
    ///
    /// # 返回值
    /// - `Ok(())`: 成功
    /// - `Err(SystemError)`: 写入失败
    pub fn write_one<T>(&mut self, offset: usize, value: &T) -> Result<(), SystemError> {
        let size = core::mem::size_of::<T>();
        if offset + size > self.len {
            return Err(SystemError::EINVAL);
        }

        let src_slice =
            unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size) };

        self.write_to_user(offset, src_slice)?;
        Ok(())
    }

    /// 读取整个用户缓冲区的内容到一个新的Vec
    ///
    /// # 返回值
    /// - `Ok(Vec<u8>)`: 包含所有数据的向量
    /// - `Err(SystemError)`: 读取失败
    pub fn read_all(&self) -> Result<alloc::vec::Vec<u8>, SystemError> {
        let mut buffer = vec![0; self.len];

        let read_len = self.read_from_user(0, &mut buffer)?;

        // 如果读取的长度小于分配的长度，调整
        if read_len < self.len {
            buffer.truncate(read_len);
        }

        Ok(buffer)
    }

    /// 将数据写入整个用户缓冲区
    ///
    /// # 参数
    /// - `data`: 要写入的数据
    ///
    /// # 返回值
    /// - `Ok(len)`: 成功写入的字节数
    /// - `Err(SystemError)`: 写入失败
    pub fn write_all(&mut self, data: &[u8]) -> Result<usize, SystemError> {
        self.write_to_user(0, data)
    }

    /// 从用户缓冲区读取一个 Number 类型, Native Endian
    /// ## Example
    /// ```
    /// let buffer = UserBuffer::new(vec![0u8; 4]);
    /// let value: u32 = buffer.read_ne_byte(0).unwrap();
    /// assert_eq!(value, 0);
    /// ```
    #[inline(always)]
    pub fn read_ne_byte<T: FromBytes>(&self, offset: usize) -> Result<T, SystemError> {
        self.read_one(offset)
    }

    /// 从用户缓冲区写入一个 Number 类型, Native Endian
    /// ## Example
    /// ```
    /// let buffer = UserBuffer::new(vec![0u8; 4]);
    /// buffer.write_ne_byte(0, 0x12345678).unwrap();
    /// let value: u32 = buffer.read_ne_byte(0).unwrap();
    /// assert_eq!(value, 0x12345678);
    /// ```
    #[inline(always)]
    pub fn write_ne_byte<T: FromBytes>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<(), SystemError> {
        self.write_one(offset, &value)
    }

    /// 将用户缓冲区的指定范围清零
    ///
    /// 这个方法使用带异常表保护的 memset 实现，直接将用户空间内存设置为零。
    /// 如果访问的地址不在有效的 VMA 中，会安全地返回错误而不会导致内核崩溃。
    ///
    /// # 参数
    /// - `offset`: 要清零的起始偏移量
    /// - `len`: 要清零的字节数
    ///
    /// # 返回值
    /// - `Ok(())`: 成功清零
    /// - `Err(SystemError)`: 清零失败（如地址不在VMA中）
    ///
    /// # 示例
    /// ```rust
    /// let mut buffer = old_act_writer.buffer_protected(0)?;
    /// buffer.clear_range(0, 64)?; // 清零前64字节
    /// ```
    pub fn clear_range(&mut self, offset: usize, len: usize) -> Result<(), SystemError> {
        use crate::arch::MMArch;
        use crate::mm::MemoryManagementArch;

        if len == 0 {
            return Ok(());
        }

        if offset >= self.len {
            return Err(SystemError::EINVAL);
        }

        let available = self.len - offset;
        let clear_len = core::cmp::min(len, available);

        if clear_len == 0 {
            return Ok(());
        }

        let dst_addr = (self.user_addr.data() + offset) as *mut u8;

        // 使用架构相关的带异常表保护的 memset。
        // 注意：用户页可能以 PAGE_COPY（COW）形式映射为只读 PTE，
        // 此时内核直接写会被 CR0.WP 拦截。
        // 与 copy_to_user_protected 保持一致，清零时也临时关闭内核写保护。
        MMArch::disable_kernel_wp();
        let result = unsafe { MMArch::memset_with_exception_table(dst_addr, 0, clear_len) };
        MMArch::enable_kernel_wp();

        if result == 0 {
            Ok(())
        } else {
            Err(SystemError::EFAULT)
        }
    }

    /// 将整个用户缓冲区清零
    ///
    /// 这个方法是 `clear_range(0, self.len())` 的便捷封装。
    /// 使用带异常表保护的 memset 实现，高效且安全。
    ///
    /// # 返回值
    /// - `Ok(())`: 成功清零
    /// - `Err(SystemError)`: 清零失败
    ///
    /// # 示例
    /// ```rust
    /// let mut buffer = old_act_writer.buffer_protected(0)?;
    /// buffer.clear()?; // 清零整个缓冲区
    /// ```
    pub fn clear(&mut self) -> Result<(), SystemError> {
        self.clear_range(0, self.len)
    }
}
