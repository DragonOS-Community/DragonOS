use alloc::sync::Arc;
use system_error::SystemError;

use super::{
    fcntl::AtFlags,
    file::{File, FileFlags},
    permission::PermissionMask,
    syscall::{OpenHow, OpenHowResolve},
    utils::{rsplit_path, should_remove_sgid_on_chown, user_path_at},
    vcore::{check_parent_dir_permission, resolve_parent_inode},
    FileType, IndexNode, InodeMode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::{filesystem::vfs::syscall::UtimensFlags, process::cred::Kgid};
use crate::{
    process::cred::CAPFlags,
    process::cred::GroupInfo,
    time::{syscall::PosixTimeval, PosixTimeSpec},
};
use crate::{process::ProcessManager, syscall::user_access::vfs_check_and_clone_cstr};
use alloc::string::String;

/// 计算创建文件/目录时使用的最终 mode（应用 umask，并丢弃非法位）。
///
/// Linux 语义要点：
/// - umask 仅作用于 0o777（FsStruct 内保证 umask 只包含 S_IRWXUGO）
/// - open/creat 的 `mode` 参数可包含 0o7777（含 suid/sgid/sticky），文件类型位无效
#[inline]
fn apply_umask_for_create(requested: InodeMode, umask: InodeMode) -> InodeMode {
    // 仅保留“权限/特殊位”，不允许用户通过 open() 设定文件类型位。
    let req = InodeMode::from_bits_truncate(requested.bits() & InodeMode::S_IALLUGO.bits());
    // FsStruct::umask() 已保证只含 S_IRWXUGO，因此 !umask 不会影响 suid/sgid/sticky。
    req & !umask
}

/// chmod/fchmod 的核心：保留文件类型位，仅替换权限/特殊位。
#[inline]
fn chmod_preserve_type(old: InodeMode, new_mode: InodeMode) -> InodeMode {
    let old_type = old.bits() & InodeMode::S_IFMT.bits();
    let new_perm = new_mode.bits() & InodeMode::S_IALLUGO.bits();
    InodeMode::from_bits_truncate(old_type | new_perm)
}

pub(super) fn do_faccessat(
    dirfd: i32,
    path: *const u8,
    mode: InodeMode,
    flags: u32,
) -> Result<usize, SystemError> {
    if (mode.bits() & (!InodeMode::S_IRWXO.bits())) != 0 {
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

    let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
    let path = path.to_str().map_err(|_| SystemError::EINVAL)?;
    // log::debug!("do_faccessat path: {:?}", path);

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // todo: 接着完善（可以借鉴linux 6.1.9的do_faccessat）
    return Ok(0);
}

pub fn do_fchmodat(dirfd: i32, path: *const u8, mode: InodeMode) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
    let path = path.to_str().map_err(|_| SystemError::EINVAL)?;

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    let target_inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    do_fchmod(target_inode, mode)
}

/// fchmod：对已解析的 inode 进行 chmod（供 `sys_fchmod` 复用）。
pub fn do_fchmod(inode: Arc<dyn IndexNode>, mode: InodeMode) -> Result<usize, SystemError> {
    let cred = ProcessManager::current_pcb().cred();
    let mut metadata = inode.metadata()?;

    // Linux 语义：chmod/fchmod 需要是 inode 所有者，或具备 CAP_FOWNER
    if cred.fsuid.data() != metadata.uid && !cred.has_capability(CAPFlags::CAP_FOWNER) {
        return Err(SystemError::EPERM);
    }

    metadata.mode = chmod_preserve_type(metadata.mode, mode);
    inode.set_metadata(&metadata)?;
    Ok(0)
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

    // Linux 语义：uid/gid 传入 -1 表示“不更改”。在 syscall 入口处参数被提升为 usize，
    // 因此这里以 usize::MAX 代表 (uid_t/gid_t)-1。
    let change_uid = uid != usize::MAX;
    let change_gid = gid != usize::MAX;
    let old_uid = meta.uid;
    let old_gid = meta.gid;

    // 检查权限
    match current_uid {
        0 => {
            if change_uid {
                meta.uid = uid;
            }
            if change_gid {
                meta.gid = gid;
            }
        }
        _ => {
            // 非文件所有者不能更改信息，且不能更改uid
            if current_uid != meta.uid || (change_uid && uid != meta.uid) {
                return Err(SystemError::EPERM);
            }
            if change_gid {
                if gid != current_gid && !group_info.gids.contains(&Kgid::from(gid)) {
                    return Err(SystemError::EPERM);
                }
                meta.gid = gid;
            }
        }
    }

    // Linux 语义：
    // - chown 目录：不清除 suid/sgid（setgid 目录用于组继承）
    // - chown 普通文件：总是清除 suid；sgid 按 Linux setattr_should_drop_sgid 规则清理
    //   - 若 S_IXGRP 置位：无条件清除 sgid
    //   - 否则（mandatory locking 语义）：仅当调用者不在文件所属组且无 CAP_FSETID 时清除
    let ownership_changed = meta.uid != old_uid || meta.gid != old_gid;
    if ownership_changed && meta.file_type != FileType::Dir {
        // suid always must be killed on chown for non-directories
        meta.mode.remove(InodeMode::S_ISUID);

        if should_remove_sgid_on_chown(meta.mode, old_gid, current_gid, &cred, &group_info) {
            meta.mode.remove(InodeMode::S_ISGID);
        }
    }
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

pub fn do_sys_open(
    dfd: i32,
    path: &str,
    o_flags: FileFlags,
    mode: InodeMode,
) -> Result<usize, SystemError> {
    let how = OpenHow::new(o_flags, mode, OpenHowResolve::empty());

    return do_sys_openat2(dfd, path, how);
}

fn do_sys_openat2(dirfd: i32, path: &str, how: OpenHow) -> Result<usize, SystemError> {
    // log::debug!("openat2: dirfd: {}, path: {}, how: {:?}",dirfd, path, how);
    let path = path.trim();
    let follow_symlink = !how.o_flags.contains(FileFlags::O_NOFOLLOW);
    // 检查空字符串路径
    if path.is_empty() {
        return Err(SystemError::ENOENT);
    }

    // 检查路径末尾斜杠 - 如果以斜杠结尾，目标必须是目录
    let path_ends_with_slash = path.ends_with('/');

    let (inode_begin, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;
    let inode =
        inode_begin.lookup_follow_symlink2(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES, follow_symlink);
    let mut created = false;
    let inode: Arc<dyn IndexNode> = match inode {
        Ok(inode) => inode,
        Err(errno) => {
            // 文件不存在，且需要创建
            if how.o_flags.contains(FileFlags::O_CREAT)
                && !how.o_flags.contains(FileFlags::O_DIRECTORY)
                && errno == SystemError::ENOENT
            {
                // 如果路径以斜杠结尾，不能创建普通文件
                if path_ends_with_slash {
                    return Err(SystemError::EISDIR);
                }

                let (filename, parent_path) = rsplit_path(&path);
                // 检查文件名长度
                if filename.len() > crate::filesystem::vfs::NAME_MAX {
                    return Err(SystemError::ENAMETOOLONG);
                }
                // 查找父目录
                let parent_inode: Arc<dyn IndexNode> =
                    resolve_parent_inode(inode_begin, parent_path)?;
                let parent_md = parent_inode.metadata()?;
                // 父节点必须是目录
                if parent_md.file_type != FileType::Dir {
                    return Err(SystemError::ENOTDIR);
                }
                // Linux 语义：创建文件需要对父目录拥有 W+X（写+搜索）权限
                check_parent_dir_permission(&parent_md)?;

                // 计算创建 mode：应用 umask，遵循 open/creat 语义
                let pcb = ProcessManager::current_pcb();
                let umask = pcb.fs_struct().umask();
                let create_mode = apply_umask_for_create(how.mode, umask);
                // 创建文件
                let inode: Arc<dyn IndexNode> =
                    parent_inode.create(filename, FileType::File, create_mode)?;
                created = true;
                inode
            } else {
                // 不需要创建文件，因此返回错误码
                return Err(errno);
            }
        }
    };
    let metadata = inode.metadata()?;
    let file_type: FileType = metadata.file_type;

    // Linux semantics: socket inodes (S_IFSOCK) cannot be opened via open(2).
    // Users must use socket(2)/connect(2) instead. Linux returns ENXIO.
    if file_type == FileType::Socket {
        return Err(SystemError::ENXIO);
    }
    // 如果路径以斜杠结尾，而目标不是目录，返回 ENOTDIR
    if path_ends_with_slash && file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    // 已存在的文件且指定了 O_CREAT|O_EXCL
    if how.o_flags.contains(FileFlags::O_CREAT)
        && how.o_flags.contains(FileFlags::O_EXCL)
        && !created
    {
        return Err(SystemError::EEXIST);
    }
    // 对已存在的目录使用 O_CREAT 视为错误
    if how.o_flags.contains(FileFlags::O_CREAT) && !created && file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }
    // 目录相关检查
    if file_type == FileType::Dir {
        // 目录上不支持 O_TRUNC
        if how.o_flags.contains(FileFlags::O_TRUNC) {
            return Err(SystemError::EISDIR);
        }
        // 目录上不允许写访问
        let acc_mode = how.o_flags.access_flags();
        if acc_mode == FileFlags::O_WRONLY || acc_mode == FileFlags::O_RDWR {
            return Err(SystemError::EISDIR);
        }
        // 目录不支持 O_DIRECT
        if how.o_flags.contains(FileFlags::O_DIRECT) {
            return Err(SystemError::EINVAL);
        }
    }
    // 非 O_PATH 需要检查访问权限（read/write/truncate）
    // Linux 语义：若本次 open() 触发了创建，则不应因“新 inode 的 mode”而拒绝
    // 当前这次 open() 的访问模式；权限在“后续 reopen()”时生效。
    if !how.o_flags.contains(FileFlags::O_PATH) && !created {
        let acc_mode = how.o_flags.access_flags();
        let mut need = PermissionMask::empty();
        match acc_mode {
            FileFlags::O_RDONLY => need.insert(PermissionMask::MAY_READ),
            FileFlags::O_WRONLY => need.insert(PermissionMask::MAY_WRITE),
            FileFlags::O_RDWR => need.insert(PermissionMask::MAY_READ | PermissionMask::MAY_WRITE),
            _ => {}
        }
        if how.o_flags.contains(FileFlags::O_TRUNC) {
            need.insert(PermissionMask::MAY_WRITE);
        }
        if !need.is_empty() {
            let cred = ProcessManager::current_pcb().cred();
            cred.inode_permission(&metadata, need.bits())?;
        }
    }

    // 如果要打开的是文件夹，而目标不是文件夹
    if how.o_flags.contains(FileFlags::O_DIRECTORY) && file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 如果O_TRUNC，并且是普通文件，清空文件
    // 注意：必须在创建 File 对象之前截断
    // 因为 O_TRUNC 的截断基于文件系统权限，而不是打开模式
    // 例如：open(file, O_RDONLY | O_TRUNC) 是合法的，只要用户对文件有写权限
    if how.o_flags.contains(FileFlags::O_TRUNC) && file_type == FileType::File {
        inode.resize(0)?;
    }
    let file: File = File::new(inode, how.o_flags)?;

    // 把文件对象存入pcb
    let r = ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(file, None)
        .map(|fd| fd as usize);

    return r;
}

/// 为exec打开可执行文件
///
/// 这个函数复用do_sys_openat2的路径解析和权限检查逻辑，
/// 返回Arc<File>和文件描述符，并将文件加入fd table。
///
/// ## 参数
/// - `dirfd`: 目录文件描述符
/// - `path`: 文件路径
///
/// ## 返回值
/// - `Ok((Arc<File>, i32))`: 成功打开的文件和分配的文件描述符
/// - `Err(SystemError)`: 错误
pub fn do_open_execat(dirfd: i32, path: &str) -> Result<Arc<File>, SystemError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let (inode_begin, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;
    let inode = inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    let metadata = inode.metadata()?;
    let file_type = metadata.file_type;

    // 必须是普通文件
    if file_type != FileType::File {
        return Err(SystemError::EACCES);
    }

    // 检查执行权限
    let cred = ProcessManager::current_pcb().cred();
    cred.inode_permission(&metadata, PermissionMask::MAY_EXEC.bits())?;

    // 同时需要读权限（用于读取ELF内容）
    cred.inode_permission(&metadata, PermissionMask::MAY_READ.bits())?;

    // 创建File对象，使用O_RDONLY | O_CLOEXEC
    let file = File::new(inode, FileFlags::O_RDONLY | FileFlags::O_CLOEXEC)?;

    Ok(Arc::new(file))
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

    // Linux semantics: if both timestamps are UTIME_OMIT, the call succeeds
    // without accessing the file at all (OmitNoop test).
    if let Some([atime, mtime]) = times {
        if atime.tv_nsec == UTIME_OMIT && mtime.tv_nsec == UTIME_OMIT {
            return Ok(0);
        }
    }

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
            // Linux-specific extension: pathname == NULL means operate on the
            // file referred to by dirfd (futimens). However, some combinations
            // are invalid and must return specific errors.

            // When dirfd is AT_FDCWD and pathname is NULL, Linux returns EFAULT.
            if dirfd == AtFlags::AT_FDCWD.bits() {
                return Err(SystemError::EFAULT);
            }

            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.write();
            let file = fd_table_guard
                .get_file_by_fd(dirfd)
                .ok_or(SystemError::EBADF)?;

            // If the file descriptor was opened with O_PATH, futimesat must fail
            // with EBADF instead of operating on it.
            if file.flags().contains(FileFlags::O_PATH) {
                return Err(SystemError::EBADF);
            }

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
