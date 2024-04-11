use core::{any::Any};
use core::fmt::Debug;
use core::cmp::Ordering;
use core::hash::Hash;

use alloc::string::ToString;
use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, process::ProcessControlBlock};

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

            // 如果dirfd不是目录，则返回错误码ENOTDIR
            if file.file_type() != FileType::Dir {
                return Err(SystemError::ENOTDIR);
            }

            inode = file.inode();
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


// pub trait Keyable: Any + Sync + Send + Debug {
//     fn key(&self) -> Arc<String>;
// }

// #[derive(Debug)]
// pub enum Key<T: Keyable> {
//     Inner(T),
//     Cmp(Arc<String>),
// }

// impl<T: Keyable> Key<T> {
//     pub fn unwrap(&self) -> Arc<String> {
//         match self {
//             Key::Inner(k) => k.key(),
//             Key::Cmp(k) => k.clone(),
//         }
//     }
// }

// impl<T: Keyable> Hash for Key<T> {
//     fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
//         self.unwrap().hash(state)
//     }
// }

// impl<T: Keyable> PartialEq for Key<T> {
//     fn eq(&self, other: &Self) -> bool {
//         self.unwrap() == other.unwrap()
//     }
// }

// impl<T: Keyable> Eq for Key<T> {}

// impl<T: Keyable> PartialOrd for Key<T> {
//     fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
//         Some(self.cmp(other))
//     }
// }

// impl<T: Keyable> Ord for Key<T> {
//     fn cmp(&self, other: &Self) -> Ordering {
//         self.unwrap().cmp(&other.unwrap())
//     }
// }

#[derive(Debug)]
pub struct DName(pub Arc<SpinLock<String>>);

impl PartialEq for DName {
    fn eq(&self, other: &Self) -> bool {
        return *self.0.lock() == *other.0.lock()
    }
}
impl Eq for DName {}

impl Hash for DName {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.lock().hash(state)
    }
}

impl PartialOrd for DName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

impl Ord for DName {
    fn cmp(&self, other: &Self) -> Ordering {
        return self.0.lock().cmp(&(*other.0.lock()))
    }
}

impl Default for DName {
    fn default() -> Self {
        Self(Arc::new(SpinLock::new(String::new())))
    }
}

impl From<String> for DName {
    fn from(value: String) -> Self {
        Self(Arc::from(SpinLock::new(value)))
    }
}

impl From<&str> for DName {
    fn from(value: &str) -> Self {
        Self(Arc::from(SpinLock::new(String::from(value))))
    }
}

impl Clone for DName {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

// impl AsRef<String> for DName {
//     fn as_ref(&self) -> &String {
//         &self.0.lock()
//     }
// }

// impl AsMut<String> for DName {
//     fn as_mut(&mut self) -> &mut String {
//         &mut self.0.lock()
//     }
// }

// impl AsRef<DName> for str {
//     fn as_ref(&self) -> &DName {
//         &DName::from(self)
//     }
// }

impl ToString for DName {
    fn to_string(&self) -> String {
        self.0.lock().clone()
    }
}