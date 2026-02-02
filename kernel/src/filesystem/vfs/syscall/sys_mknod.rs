use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        fcntl::AtFlags,
        utils::{rsplit_path, user_path_at},
        vcore::resolve_parent_inode,
        IndexNode, InodeMode, MAX_PATHLEN, NAME_MAX, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::syscall::nr::SYS_MKNOD;

pub struct SysMknodHandle;

impl Syscall for SysMknodHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let mode_val = Self::mode(args) as u32;
        let dev = DeviceNumber::from(Self::dev(args) as u32);

        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.as_str().trim();

        // 解析 mode：提取文件类型和权限位
        // "Zero file type is equivalent to type S_IFREG." - mknod(2)
        let file_type_bits = mode_val & InodeMode::S_IFMT.bits();
        let perm_bits = mode_val & !InodeMode::S_IFMT.bits();

        let file_type = if file_type_bits == 0 {
            InodeMode::S_IFREG
        } else {
            InodeMode::from_bits(file_type_bits).ok_or(SystemError::EINVAL)?
        };

        // 应用 umask 到权限位
        // "In the absence of a default ACL, the permissions of the created node
        //  are (mode & ~umask)." - mknod(2)
        let pcb = ProcessManager::current_pcb();
        let umask = pcb.fs_struct().umask();
        let masked_perm = InodeMode::from_bits_truncate(perm_bits) & !umask;

        // 组合文件类型和 umask 后的权限
        let mode = file_type | masked_perm;

        let (inode_begin, remain_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), path)?;

        if inode_begin
            .lookup_follow_symlink(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)
            .is_ok()
        {
            return Err(SystemError::EEXIST);
        }

        let (filename, parent_path) = rsplit_path(&remain_path);

        // 检查文件名长度
        if filename.len() > NAME_MAX {
            return Err(SystemError::ENAMETOOLONG);
        }

        // 查找父目录
        let parent_inode: Arc<dyn IndexNode> = resolve_parent_inode(inode_begin, parent_path)?;

        // 创建节点
        parent_inode.mknod(filename, mode, dev)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#o}", Self::mode(args))),
            FormattedSyscallParam::new("dev", format!("{:#x}", Self::dev(args))),
        ]
    }
}

impl SysMknodHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn mode(args: &[usize]) -> usize {
        args[1]
    }

    fn dev(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MKNOD, SysMknodHandle);
