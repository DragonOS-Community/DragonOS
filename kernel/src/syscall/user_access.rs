//! 这个文件用于放置一些内核态访问用户态数据的函数

use core::mem::size_of;

use alloc::{string::String, vec::Vec};

use crate::mm::{verify_area, VirtAddr};

use super::SystemError;

/// 清空用户空间指定范围内的数据
///
/// ## 参数
///
/// - `dest`：用户空间的目标地址
/// - `len`：要清空的数据长度
///
/// ## 返回值
///
/// 返回清空的数据长度
///
/// ## 错误
///
/// - `EFAULT`：目标地址不合法
pub unsafe fn clear_user(dest: VirtAddr, len: usize) -> Result<usize, SystemError> {
    verify_area(dest, len).map_err(|_| SystemError::EFAULT)?;

    let p = dest.data() as *mut u8;
    // 清空用户空间的数据
    p.write_bytes(0, len);
    return Ok(len);
}

pub unsafe fn copy_to_user(dest: VirtAddr, src: &[u8]) -> Result<usize, SystemError> {
    verify_area(dest, src.len()).map_err(|_| SystemError::EFAULT)?;

    let p = dest.data() as *mut u8;
    // 拷贝数据
    p.copy_from_nonoverlapping(src.as_ptr(), src.len());
    return Ok(src.len());
}

/// 从用户空间拷贝数据到内核空间
pub unsafe fn copy_from_user(dst: &mut [u8], src: VirtAddr) -> Result<usize, SystemError> {
    verify_area(src, dst.len()).map_err(|_| SystemError::EFAULT)?;

    let src: &[u8] = core::slice::from_raw_parts(src.data() as *const u8, dst.len());
    // 拷贝数据
    dst.copy_from_slice(&src);

    return Ok(dst.len());
}

/// 检查并从用户态拷贝一个 C 字符串。
///
/// 一旦遇到非法地址，就会返回错误
///
/// ## 参数
///
/// - `user`：用户态的 C 字符串指针
/// - `max_length`：最大拷贝长度
///
/// ## 返回值
///
/// 返回拷贝的 C 字符串
///
/// ## 错误
///
/// - `EFAULT`：用户态地址不合法
pub fn check_and_clone_cstr(
    user: *const u8,
    max_length: Option<usize>,
) -> Result<String, SystemError> {
    if user.is_null() {
        return Ok(String::new());
    }

    // 从用户态读取，直到遇到空字符 '\0' 或者达到最大长度
    let mut buffer = Vec::new();
    for i in 0.. {
        if max_length.is_some() && max_length.as_ref().unwrap() <= &i {
            break;
        }

        let addr = unsafe { user.add(i) };
        let mut c = [0u8; 1];
        unsafe {
            copy_from_user(&mut c, VirtAddr::new(addr as usize))?;
        }
        if c[0] == 0 {
            break;
        }
        buffer.push(c[0]);
    }
    return Ok(String::from_utf8(buffer).map_err(|_| SystemError::EFAULT)?);
}

/// 检查并从用户态拷贝一个 C 字符串数组
///
/// 一旦遇到空指针，就会停止拷贝. 一旦遇到非法地址，就会返回错误
/// ## 参数
///
/// - `user`：用户态的 C 字符串指针数组
///
/// ## 返回值
///
/// 返回拷贝的 C 字符串数组
///
/// ## 错误
///
/// - `EFAULT`：用户态地址不合法
pub fn check_and_clone_cstr_array(user: *const *const u8) -> Result<Vec<String>, SystemError> {
    if user.is_null() {
        Ok(Vec::new())
    } else {
        // kdebug!("check_and_clone_cstr_array: {:p}\n", user);
        let mut buffer = Vec::new();
        for i in 0.. {
            let addr = unsafe { user.add(i) };
            let str_ptr: *const u8;
            // 读取这个地址的值（这个值也是一个指针）
            unsafe {
                let dst = [0usize; 1];
                let mut dst = core::mem::transmute::<[usize; 1], [u8; size_of::<usize>()]>(dst);
                copy_from_user(&mut dst, VirtAddr::new(addr as usize))?;
                let dst = core::mem::transmute::<[u8; size_of::<usize>()], [usize; 1]>(dst);
                str_ptr = dst[0] as *const u8;

                // kdebug!("str_ptr: {:p}, addr:{addr:?}\n", str_ptr);
            }

            if str_ptr.is_null() {
                break;
            }
            // 读取这个指针指向的字符串
            let string = check_and_clone_cstr(str_ptr, None)?;
            // 将字符串放入 buffer 中
            buffer.push(string);
        }
        return Ok(buffer);
    }
}

#[derive(Debug)]
pub struct UserBufferWriter<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

#[derive(Debug)]
pub struct UserBufferReader<'a> {
    buffer: &'a [u8],
    len: usize,
}

impl<'a> UserBufferReader<'a> {
    /// 构造一个指向用户空间位置的BufferReader，为了兼容类似传入 *const u8 的情况，使用单独的泛型来进行初始化
    ///
    /// @param addr 用户空间指针
    /// @param len 缓冲区的字节长度
    /// @param frm_user 代表是否要检验地址来自用户空间
    /// @return 构造成功返回UserbufferReader实例，否则返回错误码
    ///
    pub fn new<U>(addr: *const U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        if from_user && verify_area(VirtAddr::new(addr as usize), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: unsafe { core::slice::from_raw_parts(addr as *const u8, len) },
            len,
        });
    }

    /// 从用户空间读取数据(到变量中)
    ///
    /// @return 返回用户空间数据的切片(对单个结构体就返回长度为一的切片)
    ///
    pub fn read_from_user<T>(&self, offset: usize) -> Result<&[T], SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        match self.convert_with_offset(&self.buffer, offset) {
            Err(e) => return Err(e),
            Ok(data) => return Ok(data),
        }
    }

    /// 从用户空间拷贝数据(到指定地址中)
    ///
    /// @param dst 目标地址指针
    /// @return 拷贝成功的话返回拷贝的元素数量
    ///
    pub fn copy_from_user<T: core::marker::Copy>(
        &self,
        dst: &mut [T],
        offset: usize,
    ) -> Result<usize, SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        match self.convert_with_offset(&self.buffer, offset) {
            Err(e) => return Err(e),
            Ok(data) => {
                dst.copy_from_slice(data);
            }
        }
        return Ok(dst.len());
    }

    fn convert_with_offset<T>(&self, src: &'a [u8], offset: usize) -> Result<&'a [T], SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        if offset >= src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &[u8] = &src[offset..];
        if byte_buffer.len() % core::mem::size_of::<T>() != 0 {
            return Err(SystemError::EINVAL);
        }
        // let size :usize= core::mem::size_of::<T>();
        //     let chunks = src.chunks_exact(size);
        //     self.data = chunks.map(|chunk| {
        //     let array: [u8;core::mem::size_of::<T>()] = chunk.try_into().unwrap();
        //     unsafe{core::mem::transmute_copy::<[u8;core::mem::size_of::<T>()],T>(&array)}
        // }).collect();
        let (prefix, chunks, suffix) = unsafe { byte_buffer.align_to::<T>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(SystemError::EINVAL);
        }
        return Ok(chunks);
    }
}

impl<'a> UserBufferWriter<'a> {
    /// 构造一个指向用户空间位置的BufferWriter
    ///
    /// @param addr 用户空间指针
    /// @param len 缓冲区的字节长度
    /// @return 构造成功返回UserbufferWriter实例，否则返回错误码
    ///
    pub fn new<U>(addr: *mut U, len: usize, from_user: bool) -> Result<Self, SystemError> {
        if from_user
            && verify_area(
                VirtAddr::new(addr as usize),
                (len * core::mem::size_of::<U>()) as usize,
            )
            .is_err()
        {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self {
            buffer: unsafe {
                core::slice::from_raw_parts_mut(addr as *mut u8, len * core::mem::size_of::<U>())
            },
            len,
        });
    }

    /// 从结构体写入数据到用户空间
    ///
    /// @param data 要写入的数据(如果是单个对象，也封装成只有一个元素的切片)
    /// @return Result<(), SystemError>
    ///
    pub fn write_to_user<T: core::marker::Copy>(
        &'a mut self,
        data: &'a [T],
        offset: usize,
    ) -> Result<(), SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        match Self::convert_with_offset(self.buffer, offset) {
            Err(e) => Err(e),
            Ok(dst) => {
                dst.copy_from_slice(data);
                return Ok(());
            }
        }
    }

    /// 从指定地址写入数据到用户空间
    ///
    /// @param data 要写入的数据地址
    /// @return 返回写入元素的数量
    ///
    pub fn copy_to_user<T: core::marker::Copy>(
        &'a mut self,
        src: &'a [T],
        offset: usize,
    ) -> Result<usize, SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        match Self::convert_with_offset(self.buffer, offset) {
            Err(_) => return Err(SystemError::EINVAL),
            Ok(dst) => {
                dst.copy_from_slice(&src);
                return Ok(src.len());
            }
        }
    }

    pub fn get_buffer<T>(&'a mut self, offset: usize) -> Result<&mut [T], SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        match Self::convert_with_offset(self.buffer, offset) {
            Err(_) => return Err(SystemError::EINVAL),
            Ok(buffer) => return Ok(buffer),
        }
    }

    fn convert_with_offset<T>(src: &'a mut [u8], offset: usize) -> Result<&'a mut [T], SystemError>
    where
        [u8; core::mem::size_of::<T>()]:,
    {
        if offset >= src.len() {
            return Err(SystemError::EINVAL);
        }
        let byte_buffer: &mut [u8] = &mut src[offset..];
        if byte_buffer.len() % core::mem::size_of::<T>() != 0 {
            return Err(SystemError::EINVAL);
        }
        // let size :usize= core::mem::size_of::<T>();
        //     let chunks = src.chunks_exact(size);
        //     self.data = chunks.map(|chunk| {
        //     let array: [u8;core::mem::size_of::<T>()] = chunk.try_into().unwrap();
        //     unsafe{core::mem::transmute_copy::<[u8;core::mem::size_of::<T>()],T>(&array)}
        // }).collect();
        let (prefix, chunks, suffix) = unsafe { byte_buffer.align_to_mut::<T>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(SystemError::EINVAL);
        }
        return Ok(chunks);
    }
}
