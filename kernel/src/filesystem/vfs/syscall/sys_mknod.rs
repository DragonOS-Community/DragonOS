use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        syscall::ModeType, utils::rsplit_path, IndexNode, MAX_PATHLEN, ROOT_INODE,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::check_and_clone_cstr,
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
        let flags = Self::flags(args);
        let dev_t = Self::dev_t(args);
        let flags: ModeType = ModeType::from_bits_truncate(flags as u32);
        let dev_t = DeviceNumber::from(dev_t as u32);

        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.as_str().trim();

        let inode: Result<Arc<dyn IndexNode>, SystemError> =
            ROOT_INODE().lookup_follow_symlink(path, VFS_MAX_FOLLOW_SYMLINK_TIMES);

        if inode.is_ok() {
            return Err(SystemError::EEXIST);
        }

        let (filename, parent_path) = rsplit_path(path);

        // 查找父目录
        let parent_inode: Arc<dyn IndexNode> = ROOT_INODE()
            .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        // 创建nod
        parent_inode.mknod(filename, flags, dev_t)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("dev_t", format!("{:#x}", Self::dev_t(args))),
        ]
    }
}

impl SysMknodHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn flags(args: &[usize]) -> usize {
        args[1]
    }

    fn dev_t(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MKNOD, SysMknodHandle);
