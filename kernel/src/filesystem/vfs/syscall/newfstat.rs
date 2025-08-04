use system_error::SystemError;

use crate::filesystem::vfs::stat;

#[inline(never)]
pub fn do_newfstat(fd: i32, user_stat_buf_ptr: usize) -> Result<usize, SystemError> {
    if user_stat_buf_ptr == 0 {
        return Err(SystemError::EFAULT);
    }
    let stat = stat::vfs_fstat(fd)?;
    // log::debug!("newfstat fd: {}, stat.size: {:?}",fd,stat.size);
    stat::cp_new_stat(stat, user_stat_buf_ptr).map(|_| 0)
}
