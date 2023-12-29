use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    driver::base::block::SeekFrom, process::ProcessManager,
    syscall::user_access::check_and_clone_cstr,
};

use super::{
    fcntl::AtFlags,
    file::{File, FileMode},
    syscall::{ModeType, OpenHow, OpenHowResolve},
    utils::{rsplit_path, user_path_at},
    FileType, IndexNode, MAX_PATHLEN, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

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

    if path.len() == 0 {
        return Err(SystemError::EINVAL);
    }

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, &path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // todo: 接着完善（可以借鉴linux 6.1.9的do_faccessat）
    return Ok(0);
}

pub fn do_fchmodat(dirfd: i32, path: *const u8, _mode: ModeType) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;

    if path.len() == 0 {
        return Err(SystemError::EINVAL);
    }

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, &path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    kwarn!("do_fchmodat: not implemented yet\n");
    // todo: 真正去改变文件的权限

    return Ok(0);
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
    // kdebug!("open: path: {}, mode: {:?}", path, mode);
    // 文件名过长
    if path.len() > MAX_PATHLEN as usize {
        return Err(SystemError::ENAMETOOLONG);
    }
    let (inode_begin, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;
    let inode: Result<Arc<dyn IndexNode>, SystemError> = inode_begin.lookup_follow_symlink(
        &path,
        if follow_symlink {
            VFS_MAX_FOLLOW_SYMLINK_TIMES
        } else {
            0
        },
    );

    let inode: Arc<dyn IndexNode> = if inode.is_err() {
        let errno = inode.unwrap_err();
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
    } else {
        inode.unwrap()
    };

    let file_type: FileType = inode.metadata()?.file_type;
    // 如果要打开的是文件夹，而目标不是文件夹
    if how.o_flags.contains(FileMode::O_DIRECTORY) && file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 创建文件对象

    let mut file: File = File::new(inode, how.o_flags)?;

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
