use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use path_base::{Path, PathBuf};
use system_error::SystemError;

use crate::process::ProcessControlBlock;

use super::{fcntl::AtFlags, FileType, IndexNode, ROOT_INODE};

/// @brief 切分路径字符串，返回最左侧那一级的目录名和剩余的部分。
///
/// 举例：对于 /123/456/789/   本函数返回的第一个值为123, 第二个值为456/789
#[allow(dead_code)]
pub fn split_path(path: &str) -> (&str, Option<&str>) {
    let mut path_split: core::str::SplitN<&str> = path.trim_matches('/').splitn(2, "/");
    let comp = path_split.next().unwrap_or("");
    let rest_opt = path_split.next();

    return (comp, rest_opt);
}

/// @brief 切分路径字符串，返回最右侧那一级的目录名和剩余的部分。
///
/// 举例：对于 /123/456/789/   本函数返回的第一个值为789, 第二个值为123/456
#[allow(dead_code)]
pub fn rsplit_path(path: &str) -> (&str, Option<&str>) {
    let mut path_split: core::str::RSplitN<&str> = path.trim_matches('/').rsplitn(2, "/");
    let comp = path_split.next().unwrap_or("");
    let rest_opt = path_split.next();

    return (comp, rest_opt);
}

/// 根据dirfd和path，计算接下来开始lookup的inode和剩余的path
///
/// ## 返回值
///
/// 返回值为(需要执行lookup的inode, 剩余的path)
pub fn user_path_at(
    pcb: &Arc<ProcessControlBlock>,
    dirfd: i32,
    path: &Path,
) -> Result<(Arc<dyn IndexNode>, PathBuf), SystemError> {
    if path.is_absolute() {
        return Ok((ROOT_INODE(), PathBuf::from(path)));
    }

    // 如果path不是绝对路径，则需要拼接
    // 如果dirfd不是AT_FDCWD，则需要检查dirfd是否是目录
    return if dirfd != AtFlags::AT_FDCWD.bits() {
        let binding = pcb.fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(dirfd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        let file_guard = file.lock();
        // 如果dirfd不是目录，则返回错误码ENOTDIR
        if file_guard.file_type() != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        Ok((file_guard.inode(), PathBuf::from(path)))
    } else {
        Ok((ROOT_INODE(), PathBuf::from(pcb.basic().cwd()).join(path)))
    };
}

#[allow(dead_code)]
pub fn clean_path(path: &str) -> String {
    let mut tmp = path;
    let mut clean = String::new();
    loop {
        match split_path(tmp) {
            (key, Some(rest)) => {
                match key {
                    "." => {}
                    ".." => {
                        clean = rsplit_path(&clean).1.unwrap_or("").to_string();
                    }
                    others => {
                        clean = clean + "/" + others;
                    }
                };
                tmp = rest;
            }
            (key, None) => {
                match key {
                    "." => {}
                    ".." => {
                        clean = rsplit_path(&clean).1.unwrap_or("").to_string();
                    }
                    others => {
                        clean = clean + "/" + others;
                    }
                };
                break;
            }
        }
    }
    clean
}
