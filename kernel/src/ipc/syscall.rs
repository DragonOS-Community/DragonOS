use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::file::{File, FileMode},
    include::bindings::bindings::pt_regs,
    syscall::SystemError,
};

use super::pipe::LockedPipeInode;

#[no_mangle]
/// @brief 调用匿名管道
pub extern "C" fn sys_pipe(regs: &pt_regs) -> u64 {
    let fd: *mut i32 = regs.r8 as *mut i32;
    return do_pipe(fd)
        .map(|x| x as u64)
        .unwrap_or_else(|e| e.to_posix_errno() as u64);
}

pub fn do_pipe(fd: *mut i32) -> Result<i64, SystemError> {
    let pipe_ptr = LockedPipeInode::new();
    let read_file = File::new(pipe_ptr.clone(), FileMode::O_RDONLY);
    let write_file = File::new(pipe_ptr.clone(), FileMode::O_WRONLY);

    let read_fd = current_pcb().alloc_fd(read_file.unwrap(), None);
    if !read_fd.is_ok() {
        return Err(read_fd.unwrap_err());
    }

    let write_fd = current_pcb().alloc_fd(write_file.unwrap(), None);
    if !write_fd.is_ok() {
        return Err(write_fd.unwrap_err());
    }
    unsafe {
        *fd.offset(0) = read_fd.unwrap();
        *fd.offset(1) = write_fd.unwrap();
    }
    return Ok(0);
}
