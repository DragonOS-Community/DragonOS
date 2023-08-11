use crate::{include::bindings::bindings::verify_area, syscall::SystemError};

#[derive(Debug)]
pub struct UserBufferWriter<T> {
    addr: *mut T,
    len: usize,
}

#[derive(Debug)]
pub struct UserBufferReader<T> {
    addr: *const T,
    len: usize,
}

impl<T: core::marker::Copy> UserBufferReader<T> {
    /// 构造一个指向用户空间位置的BufferReader
    ///
    /// @param addr 用户空间指针
    /// @param len 是元素数量，不是byte长度
    /// @return 构造成功返回UserbufferReader实例，否则返回错误码
    ///
    pub fn new(addr: *const T, len: usize) -> Result<Self, SystemError> {
        if unsafe { !verify_area(addr as u64, (len * core::mem::size_of::<T>()) as u64) } {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self { addr, len });
    }

    /// 从用户空间读取数据(到变量中)
    ///
    /// @return 返回用户空间数据的切片(对单个结构体就返回长度为一的切片)
    ///
    pub fn read_from_user(&self) -> Result<&[T], SystemError> {
        let items: &[T] = unsafe { core::slice::from_raw_parts(self.addr, self.len) };
        return Ok(items);
    }

    /// 从用户空间拷贝数据(到指定地址中)
    ///
    /// @param dst 目标地址指针
    /// @return 拷贝成功的话返回拷贝的元素数量
    ///
    pub fn copy_from_user(&self, dst: &mut [T]) -> Result<usize, SystemError> {
        let src: &[T] = unsafe { core::slice::from_raw_parts(self.addr, self.len) };
        dst.copy_from_slice(&src);
        return Ok(src.len());
    }
}

impl<T: core::marker::Copy> UserBufferWriter<T> {
    /// 构造一个指向用户空间位置的BufferWriter
    ///
    /// @param addr 用户空间指针
    /// @param len 是元素数量，不是byte长度
    /// @return 构造成功返回UserbufferWriter实例，否则返回错误码
    ///
    pub fn new(addr: *mut T, len: usize) -> Result<Self, SystemError> {
        if unsafe { !verify_area(addr as u64, (len * core::mem::size_of::<T>()) as u64) } {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self { addr, len });
    }

    /// 从结构体写入数据到用户空间
    ///
    /// @param data 要写入的数据(如果是单个对象，也封装成只有一个元素的切片)
    /// @return Result<(), SystemError>
    ///
    pub fn write_to_user(&self, data: &[T]) -> Result<(), SystemError> {
        let buf = unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) };
        buf.copy_from_slice(data);
        return Ok(());
    }

    /// 从指定地址写入数据到用户空间
    ///
    /// @param data 要写入的数据地址
    /// @return 返回写入元素的数量
    ///
    pub fn copy_to_user(&self, src: &[T]) -> Result<usize, SystemError> {
        let dst: &mut [T] = unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) };
        dst.copy_from_slice(&src);
        return Ok(src.len());
    }

    pub fn get_buffer(&self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) }
    }
}
