use core::ffi::{c_char, CStr};

use crate::{
    arch::asm::{current::current_pcb, ptrace::user_mode},
    include::bindings::bindings::{
        pt_regs, verify_area, EINVAL, EPERM, SEEK_CUR, SEEK_END, SEEK_MAX, SEEK_SET,
    },
    io::SeekFrom,
};

use super::{
    core::{do_lseek, do_open, do_read, do_write},
    file::FileMode,
};

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
        return r.unwrap() as u64;
    } else {
        return r.unwrap_err() as u64;
    }
}

/// @brief 关闭文件的系统调用函数
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

/// @brief 读取文件的系统调用函数
///
/// @param regs->r8 文件描述符编号
/// @param regs->r9 输出缓冲区
/// @param regs->r10 要读取的长度
#[no_mangle]
pub extern "C" fn sys_read(regs: &pt_regs) -> u64 {
    let fd = regs.r8 as i32;
    let buf_vaddr = regs.r9 as usize;
    let len = regs.r10 as usize;

    // 判断缓冲区是否来自用户态，进行权限校验
    if user_mode(regs) && unsafe { !verify_area(buf_vaddr as u64, len as u64) } {
        // 来自用户态，而buffer在内核态，这样的操作不被允许
        return (-(EPERM as i32)) as u64;
    }

    let buf: &mut [u8] =
        unsafe { core::slice::from_raw_parts_mut::<'static, u8>(buf_vaddr as *mut u8, len) };

    let r: Result<usize, i32> = do_read(fd, buf);

    if r.is_ok() {
        return r.unwrap() as u64;
    } else {
        return r.unwrap_err() as u64;
    }
}

/// @brief 向文件写入数据的系统调用函数
///
/// @param regs->r8 文件描述符编号
/// @param regs->r9 输入缓冲区
/// @param regs->r10 要写入的长度
#[no_mangle]
pub extern "C" fn sys_write(regs: &pt_regs) -> u64 {
    let fd = regs.r8 as i32;
    let buf_vaddr = regs.r9 as usize;
    let len = regs.r10 as usize;

    // 判断缓冲区是否来自用户态，进行权限校验
    if user_mode(regs) && unsafe { !verify_area(buf_vaddr as u64, len as u64) } {
        // 来自用户态，而buffer在内核态，这样的操作不被允许
        return (-(EPERM as i32)) as u64;
    }

    let buf: &[u8] =
        unsafe { core::slice::from_raw_parts::<'static, u8>(buf_vaddr as *mut u8, len) };

    let r: Result<usize, i32> = do_write(fd, buf);

    if r.is_ok() {
        return r.unwrap() as u64;
    } else {
        return r.unwrap_err() as u64;
    }
}

/// @brief 调整文件访问指针位置的系统调用函数
///
/// @param regs->r8 文件描述符编号
/// @param regs->r9 调整偏移量
/// @param regs->r10 调整的模式
#[no_mangle]
pub extern "C" fn sys_lseek(regs: &pt_regs) -> u64 {
    let fd = regs.r8 as i32;
    let offset = regs.r9 as i64;
    let whence = regs.r10 as u32;

    let w: SeekFrom = match whence {
        SEEK_SET => SeekFrom::SeekSet(offset),
        SEEK_CUR => SeekFrom::SeekCurrent(offset),
        SEEK_END => SeekFrom::SeekEnd(offset),
        SEEK_MAX => SeekFrom::SeekEnd(0),
        _ => return (-(EINVAL as i32)) as u64,
    };

    let r: Result<usize, i32> = do_lseek(fd, w);
    if r.is_ok() {
        return r.unwrap() as u64;
    } else {
        return r.unwrap_err() as u64;
    }
}
