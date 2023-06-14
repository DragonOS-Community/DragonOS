//! 这个文件用于放置一些内核态访问用户态的函数

use crate::{
    mm::{verify_area, VirtAddr},
    syscall::SystemError,
};

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


pub unsafe fn copy_to_user(dest: VirtAddr, src:&[u8]) -> Result<usize, SystemError> {
    verify_area(dest, src.len()).map_err(|_| SystemError::EFAULT)?;

    let p = dest.data() as *mut u8;
    // 拷贝数据
    p.copy_from_nonoverlapping(src.as_ptr(), src.len());
    return Ok(src.len());
}