//! System call handler for sys_umount.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_UMOUNT2},
    filesystem::vfs::{fcntl::AtFlags, utils::user_path_at, MountFS, MAX_PATHLEN},
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
/// - _flag: UmountFlag - 卸载标志，目前未使用。
///
/// ## 返回值
///
/// - Ok(Arc<MountFS>): 成功时返回文件系统的 Arc 引用。
/// - Err(SystemError): 出错时返回系统错误。
///
/// ## 错误处理
///
/// 如果指定的路径没有对应的文件系统，或者在尝试卸载时发生错误，将返回错误。
pub fn do_umount2(
    dirfd: i32,
    target: &str,
    _flag: UmountFlag,
) -> Result<Arc<MountFS>, SystemError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let (work, rest) = user_path_at(&ProcessManager::current_pcb(), dirfd, target)?;

    // user_path_at 已经保证：
    // - 绝对路径：rest 以 '/' 开头
    // - 相对路径：rest 不以 '/' 开头，work 为 dirfd/cwd inode
    //
    // 因此这里不能无脑拼接 work.absolute_path()+rest，否则绝对路径会被重复前缀化，导致找不到挂载记录。
    let path = if rest.starts_with('/') {
        rest
    } else {
        let mut base = work.absolute_path()?;
        if !base.ends_with('/') {
            base.push('/');
        }
        base.push_str(&rest);
        base
    };

    let result = ProcessManager::current_mntns().remove_mount(&path);
    if let Some(fs) = result {
        // Todo: 占用检测
        fs.umount()?;
        return Ok(fs);
    }
    return Err(SystemError::EINVAL);
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
