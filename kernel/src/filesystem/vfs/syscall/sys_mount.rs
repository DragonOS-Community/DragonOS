//! System call handler for sys_mount.

use crate::{
    arch::syscall::nr::SYS_MOUNT,
    filesystem::vfs::{
        fcntl::AtFlags, file::FileMode, mount::MOUNT_LIST, utils::user_path_at, vcore::do_mkdir_at,
        FileSystem, FileSystemMakerData, MountFS, FSMAKER, MAX_PATHLEN,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    process::ProcessManager,
    producefs,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

/// #挂载文件系统
///
/// 用于挂载文件系统,目前仅支持ramfs挂载
///
/// ## 参数:
///
/// - source       挂载设备(目前只支持ext4格式的硬盘)
/// - target       挂载目录
/// - filesystemtype   文件系统
/// - mountflags     挂载选项（暂未实现）
/// - data        带数据挂载
///
/// ## 返回值
/// - Ok(0): 挂载成功
/// - Err(SystemError) :挂载过程中出错
pub struct SysMountHandle;

impl Syscall for SysMountHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let target = Self::target(args);
        let filesystemtype = Self::filesystemtype(args);
        let data = Self::data(args);
        let source = Self::source(args);

        let target = user_access::check_and_clone_cstr(target, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let source = user_access::check_and_clone_cstr(source, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let source = source.as_str();

        let fstype_str = user_access::check_and_clone_cstr(filesystemtype, Some(MAX_PATHLEN))?;
        let fstype_str = fstype_str.to_str().map_err(|_| SystemError::EINVAL)?;

        let fstype = producefs!(FSMAKER, fstype_str, data, source)?;

        do_mount(fstype, &target)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("source", format!("{:#x}", Self::source(args) as usize)),
            FormattedSyscallParam::new("target", format!("{:#x}", Self::target(args) as usize)),
            FormattedSyscallParam::new(
                "filesystemtype",
                format!("{:#x}", Self::filesystemtype(args) as usize),
            ),
            FormattedSyscallParam::new("mountflags", format!("{:#x}", Self::mountflags(args))),
            FormattedSyscallParam::new("data", format!("{:#x}", Self::data(args) as usize)),
        ]
    }
}

impl SysMountHandle {
    fn source(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    fn target(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    fn filesystemtype(args: &[usize]) -> *const u8 {
        args[2] as *const u8
    }
    fn mountflags(args: &[usize]) -> usize {
        args[3]
    }
    fn data(args: &[usize]) -> *const u8 {
        args[4] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_MOUNT, SysMountHandle);

/// # do_mount - 挂载文件系统
///
/// 将给定的文件系统挂载到指定的挂载点。
///
/// 此函数会检查是否已经挂载了相同的文件系统，如果已经挂载，则返回错误。
/// 它还会处理符号链接，并确保挂载点是有效的。
///
/// ## 参数
///
/// - `fs`: Arc<dyn FileSystem>，要挂载的文件系统。
/// - `mount_point`: &str，挂载点路径。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 挂载成功后返回挂载的文件系统。
/// - `Err(SystemError)`: 挂载失败时返回错误。
pub fn do_mount(fs: Arc<dyn FileSystem>, mount_point: &str) -> Result<Arc<MountFS>, SystemError> {
    let (current_node, rest_path) = user_path_at(
        &ProcessManager::current_pcb(),
        AtFlags::AT_FDCWD.bits(),
        mount_point,
    )?;
    let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    if let Some((_, rest, _fs)) = MOUNT_LIST().get_mount_point(mount_point) {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    // 移至IndexNode.mount()来记录
    return inode.mount(fs);
}

/// # do_mount_mkdir - 在指定挂载点创建目录并挂载文件系统
///
/// 在指定的挂载点创建一个目录，并将其挂载到文件系统中。如果挂载点已经存在，并且不是空的，
/// 则会返回错误。成功时，会返回一个新的挂载文件系统的引用。
///
/// ## 参数
///
/// - `fs`: FileSystem - 文件系统的引用，用于创建和挂载目录。
/// - `mount_point`: &str - 挂载点路径，用于创建和挂载目录。
///
/// ## 返回值
///
/// - `Ok(Arc<MountFS>)`: 成功挂载文件系统后，返回挂载文件系统的共享引用。
/// - `Err(SystemError)`: 挂载失败时，返回系统错误。
pub fn do_mount_mkdir(
    fs: Arc<dyn FileSystem>,
    mount_point: &str,
) -> Result<Arc<MountFS>, SystemError> {
    let inode = do_mkdir_at(
        AtFlags::AT_FDCWD.bits(),
        mount_point,
        FileMode::from_bits_truncate(0o755),
    )?;
    if let Some((_, rest, _fs)) = MOUNT_LIST().get_mount_point(mount_point) {
        if rest.is_empty() {
            return Err(SystemError::EBUSY);
        }
    }
    return inode.mount(fs);
}
