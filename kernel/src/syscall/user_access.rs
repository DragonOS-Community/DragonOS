//! 这个文件用于放置一些内核态访问用户态数据的函数
use core::mem::size_of;

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    mm::{verify_area, VirtAddr},
};

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
    // 如果指定了最大长度，则限定最大长度，然后从用户态读取
    if max_length.is_some() {
        let max_length = max_length.unwrap();
        verify_area(VirtAddr::new(user as usize), max_length)?;
        let user_buf = unsafe { core::slice::from_raw_parts(user, max_length) };
        let s = core::str::from_utf8(user_buf)
            .map_err(|_| SystemError::EFAULT)?
            .to_string();

        return Ok(s);
    } else {
        // 如果没有指定最大长度，就从用户态读取，直到遇到空字符
        let mut buffer = Vec::new();
        for i in 0.. {
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
