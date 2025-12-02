use crate::arch::syscall::nr::SYS_FCNTL;
use crate::filesystem::vfs::FileType;
use crate::ipc::pipe::LockedPipeInode;
use crate::process::RawPid;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{
        fcntl::{FcntlCommand, FD_CLOEXEC},
        file::FileMode,
        syscall::dup2::{do_dup2, do_dup3},
    },
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use log::warn;
use num_traits::FromPrimitive;
use system_error::SystemError;

// Only allow changing these flags
const SETFL_MASK: u32 = FileMode::O_APPEND.bits()
    | FileMode::O_NONBLOCK.bits()
    | FileMode::O_DSYNC.bits()
    | FileMode::FASYNC.bits()
    | FileMode::O_DIRECT.bits()
    | FileMode::O_NOATIME.bits();

struct SysFcntlHandle;

impl Syscall for SysFcntlHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let cmd = Self::cmd(args);
        let arg = Self::arg(args);

        let cmd: Option<FcntlCommand> = <FcntlCommand as FromPrimitive>::from_u32(cmd);
        let res = if let Some(cmd) = cmd {
            Self::do_fcntl(fd, cmd, arg)
        } else {
            Err(SystemError::EINVAL)
        };

        // debug!("FCNTL: fd: {}, cmd: {:?}, arg: {}, res: {:?}", fd, cmd, arg, res);
        res
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("cmd", format!("{:#x}", Self::cmd(args))),
            FormattedSyscallParam::new("arg", format!("{:#x}", Self::arg(args))),
        ]
    }
}

impl SysFcntlHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn cmd(args: &[usize]) -> u32 {
        args[1] as u32
    }

    fn arg(args: &[usize]) -> usize {
        args[2]
    }

    /// # fcntl
    ///
    /// ## 参数
    ///
    /// - `fd`：文件描述符
    /// - `cmd`：命令
    /// - `arg`：参数（对于某些命令，这是一个 64 位值）
    pub fn do_fcntl(fd: i32, cmd: FcntlCommand, arg: usize) -> Result<usize, SystemError> {
        // debug!("fcntl ({cmd:?}) fd: {fd}, arg={arg}");
        match cmd {
            FcntlCommand::DupFd | FcntlCommand::DupFdCloexec => {
                // RLIMIT_NOFILE 检查
                let nofile = ProcessManager::current_pcb()
                    .get_rlimit(crate::process::resource::RLimitID::Nofile)
                    .rlim_cur as usize;
                let arg_i32 = arg as i32;
                if arg_i32 < 0 || arg >= nofile {
                    return Err(SystemError::EBADF);
                }
                let binding = ProcessManager::current_pcb().fd_table();
                let mut fd_table_guard = binding.write();

                // 在RLIMIT_NOFILE范围内查找可用的文件描述符
                for i in arg..nofile {
                    if fd_table_guard.get_file_by_fd(i as i32).is_none() {
                        if cmd == FcntlCommand::DupFd {
                            return do_dup2(fd, i as i32, &mut fd_table_guard);
                        } else {
                            return do_dup3(fd, i as i32, FileMode::O_CLOEXEC, &mut fd_table_guard);
                        }
                    }
                }
                return Err(SystemError::EMFILE);
            }
            FcntlCommand::GetFd => {
                // Get file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);

                    if file.close_on_exec() {
                        return Ok(FD_CLOEXEC as usize);
                    } else {
                        return Ok(0);
                    }
                }
                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFd => {
                // Set file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    let arg = arg as u32;
                    if arg & FD_CLOEXEC != 0 {
                        file.set_close_on_exec(true);
                    } else {
                        file.set_close_on_exec(false);
                    }
                    return Ok(0);
                }
                return Err(SystemError::EBADF);
            }

            FcntlCommand::GetFlags => {
                // Get file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    return Ok(file.mode().bits() as usize);
                }

                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFlags => {
                // Set file status flags.
                // According to Linux man page, F_SETFL can only change:
                // O_APPEND, O_ASYNC, O_DIRECT, O_NOATIME, and O_NONBLOCK
                // File access mode (O_RDONLY, O_WRONLY, O_RDWR) and file creation flags
                // (O_CREAT, O_EXCL, O_NOCTTY, O_TRUNC) in arg are ignored.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    let arg = arg as u32;

                    // Get current mode
                    let current_mode = file.mode();
                    // Preserve access mode and other non-changeable flags
                    let preserved = current_mode.bits() & !SETFL_MASK;
                    // Apply new flags (only the ones allowed to change)
                    let new_bits = preserved | (arg & SETFL_MASK);
                    let mode = FileMode::from_bits_truncate(new_bits);
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    file.set_mode(mode)?;
                    return Ok(0);
                }

                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetOwn => {
                // arg 作为 pid_t（有符号整数）处理
                let arg_i32 = arg as i32;
                let pid = arg_i32.unsigned_abs();
                if pid > i32::MAX as u32 {
                    return Err(SystemError::EINVAL);
                }
                let pb = if pid == 0 {
                    None
                } else {
                    let pb = ProcessManager::find_task_by_vpid(RawPid::from(pid as _))
                        .ok_or(SystemError::ESRCH)?;
                    Some(pb)
                };
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                file.set_owner(pb)?;
                Ok(0)
            }

            FcntlCommand::GetOwn => {
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                let owner = file.owner().unwrap_or(RawPid::from(0));

                return Ok(owner.data());
            }
            FcntlCommand::GetPipeSize => {
                // F_GETPIPE_SZ: 获取管道缓冲区大小
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;

                // 检查是否是管道
                let metadata = file.metadata()?;
                if metadata.file_type != FileType::Pipe {
                    return Err(SystemError::EBADF);
                }

                // 获取 pipe inode 并返回实际大小
                let inode = file.inode();
                let pipe_inode = inode
                    .as_any_ref()
                    .downcast_ref::<LockedPipeInode>()
                    .ok_or(SystemError::EBADF)?;

                return Ok(pipe_inode.get_pipe_size());
            }
            FcntlCommand::SetPipeSize => {
                // F_SETPIPE_SZ: 设置管道缓冲区大小
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;

                // 检查是否是管道
                let metadata = file.metadata()?;
                if metadata.file_type != FileType::Pipe {
                    return Err(SystemError::EBADF);
                }

                // 获取 pipe inode 并设置大小
                let inode = file.inode();
                let pipe_inode = inode
                    .as_any_ref()
                    .downcast_ref::<LockedPipeInode>()
                    .ok_or(SystemError::EBADF)?;

                // set_pipe_size 内部会验证大小是否合法
                return pipe_inode.set_pipe_size(arg);
            }
            _ => {
                // TODO: unimplemented
                // 未实现的命令，返回0，不报错。
                warn!("fcntl: unimplemented command: {:?}, defaults to 0.", cmd);
                return Ok(0);
            }
        }
    }
}

syscall_table_macros::declare_syscall!(SYS_FCNTL, SysFcntlHandle);
