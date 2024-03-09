use alloc::{string::String, sync::Arc};
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
    path: &str,
) -> Result<(Arc<dyn IndexNode>, String), SystemError> {
    let mut inode = ROOT_INODE();
    let ret_path;
    // 如果path不是绝对路径，则需要拼接
    if path.as_bytes()[0] != b'/' {
        // 如果dirfd不是AT_FDCWD，则需要检查dirfd是否是目录
        if dirfd != AtFlags::AT_FDCWD.bits() {
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

            inode = file_guard.inode();
            ret_path = String::from(path);
        } else {
            let mut cwd = pcb.basic().cwd();
            cwd.push('/');
            cwd.push_str(path);
            ret_path = cwd;
        }
    } else {
        ret_path = String::from(path);
    }

    return Ok((inode, ret_path));
}

pub fn is_absolute(path: &str) -> bool {
    path.strip_prefix('/').is_some()
}

pub fn clean_path(path: &str) -> String {
    let mut tmp = path;
    let mut clean = "";
    loop {
        match split_path(tmp) {
            (key, Some(tmp)) => {
                match key {
                    "." => {},
                    ".." => {clean = rsplit_path(&clean).1.unwrap_or("");},
                    others => { clean = clean + '/' + others; },
                };
            },
            (key, None) => { 
                match key {
                    "." => {},
                    ".." => {clean = rsplit_path(&clean).1.unwrap_or("");},
                    others => { clean = clean + '/' + others; },
                };
                break; 
            }
        }
        // match rest_path.find('/') {
        //     Some(pos) => {
        //         // 找到了，设置下一个要查找的名字
        //         name = String::from(&rest_path[0..pos]);
        //         // 剩余的路径字符串
        //         rest_path = String::from(&rest_path[pos + 1..]);
        //     }
        //     None => {
        //         name = rest_path;
        //         rest_path = String::new();
        //     }
        // }
    }
    String::from(clean)
}

fn path_parse(path: &str) -> Vec<&str> {
    let mut parse = Vec::new();
    let path = Some(path);
    while path.is_some() {
        let (key, path) = rsplit_path(path.unwrap());
        match key {
            "." => {}
            ".." => { parse.pop(); }
            other => { parse.push(other); }
        };
    }
    parse
}
