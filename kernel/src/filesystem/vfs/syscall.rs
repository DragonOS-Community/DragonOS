use core::ffi::{c_char, CStr};

use crate::{
    arch::asm::current::current_pcb,
    include::bindings::bindings::{pt_regs, EINVAL},
};

use super::{core::do_open, file::FileMode};

/// @brief 打开文件
///
/// @param regs->r8 path 文件路径
/// @param regs->r9 o_flags 打开文件的标志位
///
/// @return u64 文件描述符编号，或者是错误码
#[no_mangle]
pub extern "C" fn sys_open(regs: &pt_regs) -> u64 {
    let path: &CStr = unsafe { CStr::from_ptr(regs.r8 as usize as *const c_char) };
    let path: Result<&str, core::str::Utf8Error> = path.to_str();
    if path.is_err() {
        return (-(EINVAL as i32)) as u64;
    }
    let path: &str = path.unwrap();
    let flags = regs.r9;

    let open_flags: FileMode = FileMode::from_bits_truncate(flags as u32);
    let r: Result<i32, i32> = do_open(path, open_flags);

    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err() as u64;
    }
}

/// @brief 关闭文件
///
/// @param regs->r8 fd：文件描述符编号
#[no_mangle]
pub extern "C" fn sys_close(regs: &pt_regs) -> u64 {
    let fd = regs.r8 as i32;
    let r: Result<(), i32> = current_pcb().drop_fd(fd);

    if r.is_ok() {
        return 0;
    } else {
        return r.unwrap_err() as u64;
    }
}
