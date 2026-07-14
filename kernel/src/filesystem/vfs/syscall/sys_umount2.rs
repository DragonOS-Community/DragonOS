//! System call handler for sys_umount.

use super::sys_mount::may_mount;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_UMOUNT2},
    filesystem::vfs::{
        fcntl::AtFlags, mount::is_mountpoint_root, utils::user_path_at, FileSystem, MountFS,
        MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::casting::DowncastArc,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

/// src/linux/mount.c `umount` & `umount2`
///
/// [umount(2) — Linux manual page](https://www.man7.org/linux/man-pages/man2/umount.2.html)
pub struct SysUmount2Handle;

impl Syscall for SysUmount2Handle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let target = Self::target(args);
        let flags = Self::flags(args);

        let target = user_access::vfs_check_and_clone_cstr(target, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        do_umount2(
            AtFlags::AT_FDCWD.bits(),
            &target,
            UmountFlag::from_bits(flags).ok_or(SystemError::EINVAL)?,
        )?;
        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("target", format!("{:#x}", Self::target(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysUmount2Handle {
    fn target(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn flags(args: &[usize]) -> i32 {
        args[1] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_UMOUNT2, SysUmount2Handle);

/// # do_umount2 - 执行卸载文件系统的函数
///
/// 这个函数用于卸载指定的文件系统。
///
/// ## 参数
///
/// - dirfd: i32 - 目录文件描述符，用于指定要卸载的文件系统的根目录。
/// - target: &str - 要卸载的文件系统的目标路径。
/// - flag: UmountFlag - 卸载模式；`MNT_DETACH` 执行 lazy detach。
///
/// ## 返回值
///
/// - Ok(Arc<MountFS>): 成功时返回文件系统的 Arc 引用。
/// - Err(SystemError): 出错时返回系统错误。
///
/// ## 错误处理
///
/// 如果指定的路径没有对应的文件系统，或者在尝试卸载时发生错误，将返回错误。
pub fn do_umount2(dirfd: i32, target: &str, flag: UmountFlag) -> Result<Arc<MountFS>, SystemError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(SystemError::ENOENT);
    }

    if flag.contains(UmountFlag::MNT_EXPIRE)
        && flag.intersects(UmountFlag::MNT_FORCE | UmountFlag::MNT_DETACH)
    {
        return Err(SystemError::EINVAL);
    }
    if flag.intersects(UmountFlag::MNT_EXPIRE | UmountFlag::MNT_FORCE) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let (work, rest) = user_path_at(&ProcessManager::current_pcb(), dirfd, target)?;
    let target_inode = work.lookup_follow_symlink2(
        &rest,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
        !flag.contains(UmountFlag::UMOUNT_NOFOLLOW),
    )?;
    if !may_mount() {
        return Err(SystemError::EPERM);
    }

    let current_mntns = ProcessManager::current_mntns();
    if !is_mountpoint_root(&target_inode) {
        return Err(SystemError::EINVAL);
    }
    let fs = target_inode
        .fs()
        .downcast_arc::<MountFS>()
        .ok_or(SystemError::EINVAL)?;
    if fs.self_mountpoint().is_none() || !fs.is_belongs_to_mntns(&current_mntns) {
        return Err(SystemError::EINVAL);
    }

    // The target lookup above is not an external mount pin. This is deliberate:
    // Linux discounts the syscall's own path reference from the busy check.
    // File/path owners acquire explicit MountExternalGuard values instead.
    let lazy = flag.contains(UmountFlag::MNT_DETACH);
    if let Err(err) = MountFS::umount_subtree_with_mode(&fs, lazy) {
        log::warn!(
            "do_umount2: fs.umount failed for fs='{}': {:?}",
            fs.name(),
            err
        );
        return Err(err);
    }
    Ok(fs)
}

bitflags! {
    pub struct UmountFlag: i32 {
        const DEFAULT = 0;          /* Default call to umount. */
        const MNT_FORCE = 1;        /* Force unmounting.  */
        const MNT_DETACH = 2;       /* Just detach from the tree.  */
        const MNT_EXPIRE = 4;       /* Mark for expiry.  */
        const UMOUNT_NOFOLLOW = 8;  /* Don't follow symlink on umount.  */
    }
}
