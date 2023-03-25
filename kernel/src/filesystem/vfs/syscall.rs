use core::ffi::{c_char, CStr};

use alloc::{boxed::Box, string::ToString};

use crate::{
    arch::asm::{current::current_pcb, ptrace::user_mode},
    include::bindings::bindings::{
        pt_regs, verify_area, AT_REMOVEDIR, EBADF, EFAULT, EINVAL, ENAMETOOLONG, ENOENT, ENOTDIR,
        EPERM, PAGE_2M_SIZE, PAGE_4K_SIZE, PROC_MAX_FD_NUM, SEEK_CUR, SEEK_END, SEEK_MAX, SEEK_SET,
    },
    io::SeekFrom,
    kerror,
};

use super::{
    core::{do_lseek, do_mkdir, do_open, do_read, do_remove_dir, do_unlink_at, do_write},
    file::{File, FileMode},
    Dirent, FileType, ROOT_INODE,
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

/// @brief 切换工作目录
///
/// @param dest_path 目标路径
///
/// @return   返回码  描述  
///      0       |          成功  
///         
///   EACCESS    |        权限不足        
///
///    ELOOP     | 解析path时遇到路径循环
///
/// ENAMETOOLONG |       路径名过长       
///
///    ENOENT    |  目标文件或目录不存在  
///
///    ENODIR    |  检索期间发现非目录项  
///
///    ENOMEM    |      系统内存不足      
///
///    EFAULT    |       错误的地址      
///  
/// ENAMETOOLONG |        路径过长        
#[no_mangle]
pub extern "C" fn sys_chdir(regs: &pt_regs) -> u64 {
    if regs.r8 == 0 {
        return -(EFAULT as i32) as u64;
    }
    let ptr = regs.r8 as usize as *const c_char;
    // 权限校验
    if ptr.is_null()
        || (user_mode(regs) && unsafe { !verify_area(ptr as u64, PAGE_2M_SIZE as u64) })
    {
        return -(EINVAL as i32) as u64;
    }

    let dest_path: &CStr = unsafe { CStr::from_ptr(ptr) };
    let dest_path: Result<&str, core::str::Utf8Error> = dest_path.to_str();

    if dest_path.is_err() {
        return (-(EINVAL as i32)) as u64;
    }

    let dest_path: &str = dest_path.unwrap();

    if dest_path.len() == 0 {
        return (-(EINVAL as i32)) as u64;
    } else if dest_path.len() >= PAGE_4K_SIZE as usize {
        return (-(ENAMETOOLONG as i32)) as u64;
    }

    let path = Box::new(dest_path.clone());
    let inode = match ROOT_INODE().lookup(&path) {
        Err(e) => {
            kerror!("Change Directory Failed, Error = {}", e);
            return (-(ENOENT as i32)) as u64;
        }
        Ok(i) => i,
    };

    match inode.metadata() {
        Err(e) => {
            kerror!("INode Get MetaData Failed, Error = {}", e);
            return (-(ENOENT as i32)) as u64;
        }
        Ok(i) => {
            if let FileType::Dir = i.file_type {
                return 0;
            } else {
                return (-(ENOTDIR as i32)) as u64;
            }
        }
    }
}

/// @brief 获取目录中的数据
///
/// @param fd 文件描述符号
/// @return uint64_t dirent的总大小
#[no_mangle]
pub extern "C" fn sys_getdents(regs: &pt_regs) -> u64 {
    let fd = regs.r8 as i32;
    let count = regs.r10 as i64;
    let dirent = match unsafe { (regs.r9 as usize as *mut Dirent).as_mut() } {
        None => {
            return 0;
        }
        Some(dirent) => dirent,
    };

    if fd < 0 || fd as u32 > PROC_MAX_FD_NUM {
        return (-(EBADF as i32)) as u64;
    }

    if count < 0 {
        return (-(EINVAL as i32)) as u64;
    }

    // 获取fd
    let file: &mut File = match current_pcb().get_file_mut_by_fd(fd) {
        None => {
            return (-(EBADF as i32)) as u64;
        }
        Some(file) => file,
    };
    // kdebug!("file={file:?}");

    return match file.readdir(dirent) {
        Err(_) => 0,
        Ok(len) => len,
    };
}

/// @brief 创建文件夹
///
/// @param path(r8) 路径 / mode(r9) 模式
///
/// @return uint64_t 负数错误码 / 0表示成功
#[no_mangle]
pub extern "C" fn sys_mkdir(regs: &pt_regs) -> u64 {
    let ptr = regs.r8 as usize as *const c_char;
    if ptr.is_null()
        || (user_mode(regs) && unsafe { !verify_area(ptr as u64, PAGE_2M_SIZE as u64) })
    {
        return -(EINVAL as i32) as u64;
    }
    let path: &CStr = unsafe { CStr::from_ptr(ptr) };
    let path: Result<&str, core::str::Utf8Error> = path.to_str();
    let mode = regs.r9;

    if path.is_err() {
        return (-(EINVAL as i32)) as u64;
    }

    let path = &path.unwrap().to_string();
    if path.trim() == "" {
        return (-(EINVAL as i32)) as u64;
    }

    return match do_mkdir(&path.trim(), FileMode::from_bits_truncate(mode as u32)) {
        Err(err) => {
            kerror!("Failed in do_mkdir, Error Code = {}", err);
            err as u64
        }
        Ok(_) => 0,
    };
}

///@brief 删除文件夹、取消文件的链接、删除文件的系统调用
///
///@param regs->r8 dfd 进程相对路径基准目录的文件描述符(见fcntl.h)
///
///@param regs->r9 路径名称字符串
///
///@param regs->r10 flag 预留的标志位，暂时未使用，请置为0。
///
///@return uint64_t 错误码
#[no_mangle]
pub extern "C" fn sys_unlink_at(regs: &pt_regs) -> u64 {
    let _dfd = regs.r8;
    let ptr = regs.r9 as usize as *const c_char;
    if ptr.is_null()
        || (user_mode(regs) && unsafe { !verify_area(ptr as u64, PAGE_2M_SIZE as u64) })
    {
        return -(EINVAL as i32) as u64;
    }
    let path: &CStr = unsafe { CStr::from_ptr(ptr) };
    let path: Result<&str, core::str::Utf8Error> = path.to_str();
    let flag = regs.r10;
    if path.is_err() {
        return (-(EINVAL as i32)) as u64;
    }

    let path = &path.unwrap().to_string();
    // kdebug!("sys_unlink_at={path:?}");
    if (flag & (!(AT_REMOVEDIR as u64))) != 0_u64 {
        return (-(EINVAL as i32)) as u64;
    }

    if (flag & (AT_REMOVEDIR as u64)) > 0 {
        // kdebug!("rmdir");
        match do_remove_dir(&path) {
            Err(err) => {
                kerror!("Failed to Remove Directory, Error Code = {}", err);
                return err as u64;
            }
            Ok(_) => {
                return 0;
            }
        }
    }

    // kdebug!("rm");
    match do_unlink_at(&path, FileMode::from_bits_truncate(flag as u32)) {
        Err(err) => {
            kerror!("Failed to Remove Directory, Error Code = {}", err);
            return err as u64;
        }
        Ok(_) => {
            return 0;
        }
    }
}
