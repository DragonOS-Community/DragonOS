use alloc::vec::Vec;
use system_error::SystemError;

/// 去除Vec中所有的\0,并在结尾添加\0
#[inline]
pub(super) fn trim_string(data: &mut Vec<u8>) {
    data.retain(|x| *x != 0);
    data.push(0);
}

/// proc文件系统读取函数
pub(super) fn proc_read(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: &[u8],
) -> Result<usize, SystemError> {
    let start = data.len().min(offset);
    let end = data.len().min(offset + len);

    // buffer空间不足
    if buf.len() < (end - start) {
        return Err(SystemError::ENOBUFS);
    }

    // 拷贝数据
    let src = &data[start..end];
    buf[0..src.len()].copy_from_slice(src);
    return Ok(src.len());
}
