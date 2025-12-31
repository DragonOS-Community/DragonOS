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
        user_access::vfs_check_and_clone_cstr,
    },
};

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::filesystem::notify::inotify::uapi::{InotifyCookie, InotifyMask};
use crate::filesystem::notify::inotify::{report, report_dir_entry, InodeKey};
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::vcore::{resolve_parent_inode, vfs_truncate_no_event};

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
        let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?;
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

        // Resolve parent before truncate
        let (filename, parent_path) = rsplit_path(&remain_path);
        let parent_key = if !filename.is_empty() {
            resolve_parent_inode(begin_inode.clone(), parent_path)
                .ok()
                .and_then(|p| p.metadata().ok())
                .map(|m| InodeKey::new(m.dev_id, m.inode_id.data()))
        } else {
            None
        };

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

        let old_size = vfs_truncate_no_event(target.clone(), length)?;

        // Report IN_MODIFY to parent directory
        if let (Some(parent), false) = (parent_key, filename.is_empty()) {
            let md = target.metadata()?;
            let is_dir = md.file_type == FileType::Dir;
            report_dir_entry(
                parent,
                InotifyMask::IN_MODIFY,
                InotifyCookie(0),
                filename,
                is_dir,
            );
        }

        if old_size != length as u64 {
            let md = target.metadata()?;
            report(
                InodeKey::new(md.dev_id, md.inode_id.data()),
                InotifyMask::IN_MODIFY,
            );
        }

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
        let len = args[1];
        if len > isize::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        Ok(len)
    }
}

syscall_table_macros::declare_syscall!(SYS_TRUNCATE, SysTruncateHandle);
