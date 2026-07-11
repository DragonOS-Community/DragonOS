use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_STATFS;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::syscall::PosixStatfs;
use crate::filesystem::vfs::utils::user_resolved_path_at;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysStatfsHandle;

impl Syscall for SysStatfsHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let user_statfs = Self::statfs(args);
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        if path.is_empty() {
            return Err(SystemError::ENOENT);
        }
        let pcb = ProcessManager::current_pcb();
        let (start_path, remain_path) =
            user_resolved_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
        let inode_begin = start_path.inode();
        let resolved = inode_begin.lookup_follow_symlink_owned(
            &start_path,
            &remain_path,
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
            true,
        )?;
        let inode = resolved.inode();
        let sb = inode.fs().statfs(&inode)?;
        let statfs = PosixStatfs::from(sb);
        writer.copy_one_to_user(&statfs, 0)?;
        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("statfs", format!("{:#x}", Self::statfs(args) as usize)),
        ]
    }
}

impl SysStatfsHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    fn statfs(args: &[usize]) -> *mut PosixStatfs {
        args[1] as *mut PosixStatfs
    }
}

syscall_table_macros::declare_syscall!(SYS_STATFS, SysStatfsHandle);
