use core::slice::from_raw_parts;
use system_error::SystemError;

pub mod addr;
pub mod link;
pub mod route;

/// 网卡名字长度
const IFNAME_SIZE: usize = 16;

pub(super) fn convert_one_from_raw_buf<T>(src: &[u8]) -> Result<&T, SystemError> {
    log::info!("convert_one_from_raw_buf: src.len() = {}", src.len());
    if core::mem::size_of::<T>() > src.len() {
        return Err(SystemError::EINVAL);
    }
    let byte_buffer: &[u8] = &src[..core::mem::size_of::<T>()];

    let chunks = unsafe { from_raw_parts(byte_buffer.as_ptr() as *const T, 1) };
    let data = &chunks[0];
    return Ok(data);
}
