use alloc::sync::Arc;
use log::warn;
use system_error::SystemError;

use super::{
    fcntl::AtFlags,
    file::{File, FileMode},
    syscall::{ModeType, OpenHow, OpenHowResolve},
    utils::{rsplit_path, user_path_at},
    FileType, IndexNode, MAX_PATHLEN, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::{
    driver::base::block::SeekFrom, process::ProcessManager,
    syscall::user_access::check_and_clone_cstr,
};
use crate::{filesystem::vfs::syscall::UtimensFlags, process::cred::Kgid};
use crate::{
    process::cred::GroupInfo,
    time::{syscall::PosixTimeval, PosixTimeSpec},
};
use alloc::string::String;

pub(super) fn do_faccessat(
    dirfd: i32,
    path: *const u8,
    mode: ModeType,
    flags: u32,
) -> Result<usize, SystemError> {
    if (mode.bits() & (!ModeType::S_IRWXO.bits())) != 0 {
        return Err(SystemError::EINVAL);
    }

    if (flags
        & (!((AtFlags::AT_EACCESS | AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH).bits()
            as u32)))
        != 0
    {
        return Err(SystemError::EINVAL);
    }

    // let follow_symlink = flags & AtFlags::AT_SYMLINK_NOFOLLOW.bits() as u32 == 0;

    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
    let path = path.to_str().map_err(|_| SystemError::EINVAL)?;

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // todo: 接着完善（可以借鉴linux 6.1.9的do_faccessat）
    return Ok(0);
}

pub fn do_fchmodat(dirfd: i32, path: *const u8, _mode: ModeType) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
    let path = path.to_str().map_err(|_| SystemError::EINVAL)?;

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    warn!("do_fchmodat: not implemented yet\n");
    // todo: 真正去改变文件的权限

    return Ok(0);
}

pub fn do_fchownat(
    dirfd: i32,
    path: &str,
    uid: usize,
    gid: usize,
    flag: AtFlags,
) -> Result<usize, SystemError> {
    // 检查flag是否合法
    if flag.contains(!(AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH)) {
        return Err(SystemError::EINVAL);
    }

    let follow_symlink = flag.contains(!AtFlags::AT_SYMLINK_NOFOLLOW);
    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let inode = if follow_symlink {
        inode.lookup_follow_symlink2(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES, false)
    } else {
        inode.lookup(path.as_str())
    };

    if inode.is_err() {
        let errno = inode.clone().unwrap_err();
        // 文件不存在
        if errno == SystemError::ENOENT {
            return Err(SystemError::ENOENT);
        }
    }

    let inode = inode.unwrap();

    return chown_common(inode, uid, gid);
}

fn chown_common(inode: Arc<dyn IndexNode>, uid: usize, gid: usize) -> Result<usize, SystemError> {
    let mut meta = inode.metadata()?;
    let cred = ProcessManager::current_pcb().cred();
    let current_uid = cred.uid.data();
    let current_gid = cred.gid.data();
    let mut group_info = GroupInfo::default();
    if let Some(info) = cred.group_info.as_ref() {
        group_info = info.clone();
    }

    // 检查权限
    match current_uid {
        0 => {
            meta.uid = uid;
            meta.gid = gid;
        }
        _ => {
            // 非文件所有者不能更改信息，且不能更改uid
            if current_uid != meta.uid || uid != meta.uid {
                return Err(SystemError::EPERM);
            }
            if gid != current_gid && !group_info.gids.contains(&Kgid::from(gid)) {
                return Err(SystemError::EPERM);
            }
            meta.gid = gid;
        }
    }

    meta.mode.remove(ModeType::S_ISUID | ModeType::S_ISGID);
    inode.set_metadata(&meta)?;

    return Ok(0);
}

pub fn ksys_fchown(fd: i32, uid: usize, gid: usize) -> Result<usize, SystemError> {
    let fd_table = &ProcessManager::current_pcb().fd_table();
    let fd_table = fd_table.read();

    let inode = fd_table.get_file_by_fd(fd).unwrap().inode();

    let result = chown_common(inode, uid, gid);

    drop(fd_table);

    return result;
}

pub(super) fn do_sys_open(
    dfd: i32,
    path: &str,
    o_flags: FileMode,
    mode: ModeType,
    follow_symlink: bool,
) -> Result<usize, SystemError> {
    let how = OpenHow::new(o_flags, mode, OpenHowResolve::empty());
    return do_sys_openat2(dfd, path, how, follow_symlink);
}

fn do_sys_openat2(
    dirfd: i32,
    path: &str,
    how: OpenHow,
    follow_symlink: bool,
) -> Result<usize, SystemError> {
    // debug!("open path: {}, how: {:?}", path, how);
    let path = path.trim();

    let (inode_begin, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;
    let inode: Result<Arc<dyn IndexNode>, SystemError> = inode_begin.lookup_follow_symlink(
        &path,
        if follow_symlink {
            VFS_MAX_FOLLOW_SYMLINK_TIMES
        } else {
            0
        },
    );

    let inode: Arc<dyn IndexNode> = match inode {
        Ok(inode) => inode,
        Err(errno) => {
            // 文件不存在，且需要创建
            if how.o_flags.contains(FileMode::O_CREAT)
                && !how.o_flags.contains(FileMode::O_DIRECTORY)
                && errno == SystemError::ENOENT
            {
                let (filename, parent_path) = rsplit_path(&path);
                // 查找父目录
                let parent_inode: Arc<dyn IndexNode> =
                    ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;
                // 创建文件
                let inode: Arc<dyn IndexNode> = parent_inode.create(
                    filename,
                    FileType::File,
                    ModeType::from_bits_truncate(0o755),
                )?;
                inode
            } else {
                // 不需要创建文件，因此返回错误码
                return Err(errno);
            }
        }
    };

    let file_type: FileType = inode.metadata()?.file_type;
    // 如果要打开的是文件夹，而目标不是文件夹
    if how.o_flags.contains(FileMode::O_DIRECTORY) && file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 创建文件对象

    let file: File = File::new(inode, how.o_flags)?;

    // 打开模式为“追加”
    if how.o_flags.contains(FileMode::O_APPEND) {
        file.lseek(SeekFrom::SeekEnd(0))?;
    }

    // 如果O_TRUNC，并且，打开模式包含O_RDWR或O_WRONLY，清空文件
    if how.o_flags.contains(FileMode::O_TRUNC)
        && (how.o_flags.contains(FileMode::O_RDWR) || how.o_flags.contains(FileMode::O_WRONLY))
        && file_type == FileType::File
    {
        file.ftruncate(0)?;
    }
    // 把文件对象存入pcb
    let r = ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(file, None)
        .map(|fd| fd as usize);

    return r;
}

/// On Linux, futimens() is a library function implemented on top of
/// the utimensat() system call.  To support this, the Linux
/// utimensat() system call implements a nonstandard feature: if
/// pathname is NULL, then the call modifies the timestamps of the
/// file referred to by the file descriptor dirfd (which may refer to
/// any type of file).
pub fn do_utimensat(
    dirfd: i32,
    pathname: Option<String>,
    times: Option<[PosixTimeSpec; 2]>,
    flags: UtimensFlags,
) -> Result<usize, SystemError> {
    const UTIME_NOW: i64 = (1i64 << 30) - 1i64;
    const UTIME_OMIT: i64 = (1i64 << 30) - 2i64;
    // log::debug!("do_utimensat: dirfd:{}, pathname:{:?}, times:{:?}, flags:{:?}", dirfd, pathname, times, flags);
    let inode = match pathname {
        Some(path) => {
            let (inode_begin, path) =
                user_path_at(&ProcessManager::current_pcb(), dirfd, path.as_str())?;
            let inode = if flags.contains(UtimensFlags::AT_SYMLINK_NOFOLLOW) {
                inode_begin.lookup(path.as_str())?
            } else {
                inode_begin.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?
            };
            inode
        }
        None => {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.write();
            let file = fd_table_guard
                .get_file_by_fd(dirfd)
                .ok_or(SystemError::EBADF)?;
            file.inode()
        }
    };
    let now = PosixTimeSpec::now();
    let mut meta = inode.metadata()?;

    if let Some([atime, mtime]) = times {
        if atime.tv_nsec == UTIME_NOW {
            meta.atime = now;
        } else if atime.tv_nsec != UTIME_OMIT {
            meta.atime = atime;
        }
        if mtime.tv_nsec == UTIME_NOW {
            meta.mtime = now;
        } else if mtime.tv_nsec != UTIME_OMIT {
            meta.mtime = mtime;
        }
        inode.set_metadata(&meta).unwrap();
    } else {
        meta.atime = now;
        meta.mtime = now;
        inode.set_metadata(&meta).unwrap();
    }
    return Ok(0);
}

pub fn do_utimes(path: &str, times: Option<[PosixTimeval; 2]>) -> Result<usize, SystemError> {
    // log::debug!("do_utimes: path:{:?}, times:{:?}", path, times);
    let (inode_begin, path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        path,
    )?;
    let inode = inode_begin.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    let mut meta = inode.metadata()?;

    if let Some([atime, mtime]) = times {
        meta.atime = PosixTimeSpec::from(atime);
        meta.mtime = PosixTimeSpec::from(mtime);
        inode.set_metadata(&meta)?;
    } else {
        let now = PosixTimeSpec::now();
        meta.atime = now;
        meta.mtime = now;
        inode.set_metadata(&meta)?;
    }
    return Ok(0);
}
