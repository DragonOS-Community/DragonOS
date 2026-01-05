use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_EXECVEAT},
    filesystem::vfs::{utils::user_path_at, FileType, VFS_MAX_FOLLOW_SYMLINK_TIMES},
    process::{syscall::sys_execve::SysExecve, ProcessManager},
    syscall::table::{FormattedSyscallParam, Syscall},
};

bitflags::bitflags! {
    struct OpenFlags: u32 {
        const AT_EMPTY_PATH = 0x1000;
        const AT_SYMLINK_NOFOLLOW = 0x100;
    }
}

/// See https://man7.org/linux/man-pages/man2/execveat.2.html
pub struct SysExecveAt;

impl Syscall for SysExecveAt {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = args[0];
        let path_ptr = args[1];
        let argv_ptr = args[2];
        let env_ptr = args[3];
        let flags = OpenFlags::from_bits(args[4] as u32).ok_or(SystemError::EINVAL)?;

        // 权限校验
        SysExecve::check_args(frame, path_ptr, argv_ptr, env_ptr)?;

        let (path, argv, envp) = SysExecve::basic_args(
            path_ptr as *const u8,
            argv_ptr as *const *const u8,
            env_ptr as *const *const u8,
        )
        .inspect_err(|e: &SystemError| {
            log::error!("Failed to execve: {:?}", e);
        })?;
        let path = path.into_string().map_err(|_| SystemError::EINVAL)?;

        // 空路径且未设置 AT_EMPTY_PATH 标志 -> 返回 ENOENT
        if path.is_empty() && !flags.contains(OpenFlags::AT_EMPTY_PATH) {
            return Err(SystemError::ENOENT);
        }

        let inode = if flags.contains(OpenFlags::AT_EMPTY_PATH) && path.is_empty() {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();

            let file = fd_table_guard
                .get_file_by_fd(dirfd as _)
                .ok_or(SystemError::EBADF)?;

            // 无法执行目录
            if file.file_type() == FileType::Dir {
                return Err(SystemError::EACCES);
            }

            file.inode()
        } else {
            let (inode_begin, path) =
                user_path_at(&ProcessManager::current_pcb(), dirfd as _, &path)?;

            let inode = if flags.contains(OpenFlags::AT_SYMLINK_NOFOLLOW) {
                // AT_SYMLINK_NOFOLLOW: 不跟随最终的符号链接
                // 使用 lookup_follow_symlink2 以便控制 follow_final_symlink 参数
                let result_inode = inode_begin.lookup_follow_symlink2(
                    &path,
                    VFS_MAX_FOLLOW_SYMLINK_TIMES,
                    false, // 不跟随最终符号链接
                )?;

                // Linux 语义：如果最终路径是符号链接且设置了 AT_SYMLINK_NOFOLLOW，返回 ELOOP
                if result_inode.metadata()?.file_type == FileType::SymLink {
                    return Err(SystemError::ELOOP);
                }

                result_inode
            } else {
                // AT_SYMLINK_NOFOLLOW 未设置：跟随所有符号链接
                inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?
            };
            inode
        };

        SysExecve::execve(inode, path, argv, envp, frame)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("path", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("argv", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("envp", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[4])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_EXECVEAT, SysExecveAt);
