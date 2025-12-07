use crate::arch::{ipc::signal::Signal, syscall::nr::SYS_TRUNCATE};
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{
        fcntl::AtFlags, permission::PermissionMask, utils::user_path_at, FileType, IndexNode,
        MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    ipc::kill::send_signal_to_pid,
    process::{resource::RLimitID, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::check_and_clone_cstr,
    },
};

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::filesystem::vfs::vcore::vfs_truncate;

/// # truncate(path, length)
///
/// 基于路径调整文件大小：
/// - 跟随符号链接定位最终 inode。
/// - 目录返回 EISDIR；非普通文件返回 EINVAL。
/// - 只读挂载点返回 EROFS。
/// - 调用 inode.resize(length)。
pub struct SysTruncateHandle;

impl Syscall for SysTruncateHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = args[0] as *const u8;
        let length = Self::len(args)?;
        // 复制并校验用户态路径
        let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?;
        let path = path.to_str().map_err(|_| SystemError::EINVAL)?;

        // 解析起始 inode 与剩余路径
        let (begin_inode, remain_path) = user_path_at(
            &ProcessManager::current_pcb(),
            AtFlags::AT_FDCWD.bits(),
            path,
        )?;

        // 跟随符号链接解析最终目标 inode
        let target: Arc<dyn IndexNode> = begin_inode
            .lookup_follow_symlink(remain_path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        let md = target.metadata()?;
        // DAC write permission check
        let cred = ProcessManager::current_pcb().cred();
        cred.inode_permission(&md, PermissionMask::MAY_WRITE.bits())?;

        // RLIMIT_FSIZE enforcement for regular files
        if md.file_type == FileType::File {
            let fsize_limit = ProcessManager::current_pcb().get_rlimit(RLimitID::Fsize);
            if fsize_limit.rlim_cur != u64::MAX && length as u64 > fsize_limit.rlim_cur {
                let _ =
                    send_signal_to_pid(ProcessManager::current_pcb().raw_pid(), Signal::SIGXFSZ);
                return Err(SystemError::EFBIG);
            }
        }

        vfs_truncate(target, length)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("length", format!("{:#x}", args[1])),
        ]
    }
}

impl SysTruncateHandle {
    fn len(args: &[usize]) -> Result<usize, SystemError> {
        let len = args[1] as isize;
        if len < 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(len as usize)
    }
}

syscall_table_macros::declare_syscall!(SYS_TRUNCATE, SysTruncateHandle);
