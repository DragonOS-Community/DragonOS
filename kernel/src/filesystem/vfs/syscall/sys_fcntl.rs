use crate::arch::ipc::signal::Signal;
use crate::arch::syscall::nr::SYS_FCNTL;
use crate::filesystem::vfs::fasync::{set_file_fasync, FAsyncHandlerPolicy};
use crate::filesystem::vfs::FileType;
use crate::filesystem::vfs::InodeFlags;
use crate::process::pid::PidType;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{
        fcntl::{FcntlCommand, FileOwnerEx, PosixFlock, FD_CLOEXEC, F_UNLCK},
        file::FileFlags,
        posix_lock::{get_posix_lock, set_posix_lock},
    },
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use alloc::vec::Vec;
use core::mem::size_of;
use log::warn;
use num_traits::FromPrimitive;
use system_error::SystemError;

// Only allow changing these flags
const SETFL_MASK: u32 = FileFlags::O_APPEND.bits()
    | FileFlags::O_NONBLOCK.bits()
    | FileFlags::O_DSYNC.bits()
    | FileFlags::FASYNC.bits()
    | FileFlags::O_DIRECT.bits()
    | FileFlags::O_NOATIME.bits();

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
                let current = ProcessManager::current_pcb();
                let nofile = current.nofile_soft_limit();
                let table = current.fd_table();
                let cloexec = cmd == FcntlCommand::DupFdCloexec;
                let source = table.get_file_by_fd(fd).ok_or(SystemError::EBADF)?;
                let minimum = arg as i32;
                if minimum < 0 || minimum as usize >= nofile {
                    return Err(SystemError::EINVAL);
                }
                let reservation = crate::filesystem::vfs::file::File::recover_io(|| {
                    table.reserve::<1>(
                        nofile,
                        minimum as usize,
                        cloexec,
                        &crate::cgroup::memory::object_for_task(&current),
                    )
                })?;
                Ok(reservation.commit_arc(source) as usize)
            }
            FcntlCommand::GetFd => {
                // Get file descriptor flags (close_on_exec is per-fd).
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if fd_table_guard.get_file_by_fd(fd).is_some() {
                    if fd_table_guard.get_cloexec(fd) {
                        return Ok(FD_CLOEXEC as usize);
                    } else {
                        return Ok(0);
                    }
                }
                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFd => {
                // Set file descriptor flags (close_on_exec is per-fd).
                let arg = arg as u32;
                ProcessManager::current_pcb()
                    .fd_table()
                    .set_cloexec(fd, arg & FD_CLOEXEC != 0)?;
                Ok(0)
            }

            FcntlCommand::GetFlags => {
                // Get file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    return Ok(file.flags().bits() as usize);
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
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
                    let arg = arg as u32;

                    // Get current mode
                    let current_flags = file.flags();
                    // Preserve access mode and other non-changeable flags
                    let preserved = current_flags.bits() & !SETFL_MASK;
                    // Apply new flags (only the ones allowed to change)
                    let new_bits = preserved | (arg & SETFL_MASK);
                    let new_flags = FileFlags::from_bits_truncate(new_bits);
                    // drop guard 以避免无法调度的问题
                    drop(fd_table_guard);
                    let inode_flags = file.get_inode_flags()?;
                    if inode_flags.contains(InodeFlags::S_APPEND)
                        && !new_flags.contains(FileFlags::O_APPEND)
                    {
                        return Err(SystemError::EPERM);
                    }
                    if new_flags.contains(FileFlags::O_NOATIME)
                        && !current_flags.contains(FileFlags::O_NOATIME)
                    {
                        let metadata = file.inode().metadata()?;
                        crate::filesystem::vfs::permission::check_noatime_permission(&metadata)?;
                    }
                    let old_fasync = current_flags.contains(FileFlags::FASYNC);
                    let new_fasync = new_flags.contains(FileFlags::FASYNC);

                    // Apply every potentially failing non-FASYNC flag update
                    // before invoking the fasync handler. Linux likewise
                    // validates flags first and lets the handler own FASYNC.
                    let mut non_fasync_flags = new_flags;
                    if old_fasync {
                        non_fasync_flags.insert(FileFlags::FASYNC);
                    } else {
                        non_fasync_flags.remove(FileFlags::FASYNC);
                    }
                    file.set_flags(non_fasync_flags)?;

                    if old_fasync != new_fasync {
                        if let Err(err) =
                            set_file_fasync(&file, fd, new_fasync, FAsyncHandlerPolicy::Optional)
                        {
                            // A failed fasync transition must not commit the
                            // other status flags from this F_SETFL request.
                            file.set_flags(current_flags)?;
                            return Err(err);
                        }
                    }

                    return Ok(0);
                }

                return Err(SystemError::EBADF);
            }
            FcntlCommand::GetLock => {
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();
                let file = fd_table_guard
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                let owner_id = fd_table_guard.lock_owner_id();
                drop(fd_table_guard);

                let reader =
                    UserBufferReader::new(arg as *const PosixFlock, size_of::<PosixFlock>(), true)?;
                let mut flock = reader.buffer_protected(0)?.read_one::<PosixFlock>(0)?;

                get_posix_lock(&file, owner_id, &mut flock)?;

                let mut writer =
                    UserBufferWriter::new(arg as *mut PosixFlock, size_of::<PosixFlock>(), true)?;
                writer.buffer_protected(0)?.write_one(0, &flock)?;
                Ok(0)
            }
            FcntlCommand::SetLock | FcntlCommand::SetLockWait => {
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();
                let file = fd_table_guard
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                let owner_id = fd_table_guard.lock_owner_id();
                drop(fd_table_guard);

                let reader =
                    UserBufferReader::new(arg as *const PosixFlock, size_of::<PosixFlock>(), true)?;
                let flock = reader.buffer_protected(0)?.read_one::<PosixFlock>(0)?;

                let owner_pid = ProcessManager::current_pcb().raw_tgid().data() as i32;
                let blocking = cmd == FcntlCommand::SetLockWait;
                let flock = set_posix_lock(&file, owner_id, owner_pid, &flock, blocking)?;

                // Linux 语义：检测 close/fcntl 并发竞态。
                // 如果加锁成功后发现 fd 已不再指向同一 file，需要回滚刚加的锁并返回 EBADF。
                if flock.l_type != F_UNLCK {
                    let binding = ProcessManager::current_pcb().fd_table();
                    let fd_table_guard = binding.read();
                    let same_file = fd_table_guard
                        .get_file_by_fd(fd)
                        .is_some_and(|f| alloc::sync::Arc::ptr_eq(&f, &file));
                    drop(fd_table_guard);
                    if !same_file {
                        let mut unlock = flock;
                        unlock.l_type = F_UNLCK;
                        let _ = set_posix_lock(&file, owner_id, owner_pid, &unlock, false);
                        return Err(SystemError::EBADF);
                    }
                }
                Ok(0)
            }
            FcntlCommand::SetOwn
            | FcntlCommand::GetOwn
            | FcntlCommand::SetOwnEx
            | FcntlCommand::GetOwnEx => {
                let file = ProcessManager::current_pcb()
                    .fd_table()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                if file.flags().contains(FileFlags::O_PATH) {
                    return Err(SystemError::EBADF);
                }
                match cmd {
                    FcntlCommand::SetOwn => {
                        file.set_owner(arg as i32)?;
                        Ok(0)
                    }
                    FcntlCommand::GetOwn => Ok(file.owner() as isize as usize),
                    FcntlCommand::SetOwnEx => {
                        let reader = UserBufferReader::new(
                            arg as *const FileOwnerEx,
                            size_of::<FileOwnerEx>(),
                            true,
                        )?;
                        let owner = reader.buffer_protected(0)?.read_one::<FileOwnerEx>(0)?;
                        let kind = match owner.kind {
                            0 => PidType::PID,
                            1 => PidType::TGID,
                            2 => PidType::PGID,
                            _ => return Err(SystemError::EINVAL),
                        };
                        file.set_owner_identity(kind, owner.pid)?;
                        Ok(0)
                    }
                    FcntlCommand::GetOwnEx => {
                        let (kind, pid) = file.owner_identity();
                        let kind = match kind {
                            PidType::PID => 0,
                            PidType::TGID => 1,
                            PidType::PGID => 2,
                            _ => unreachable!("invalid asynchronous file owner type"),
                        };
                        let owner = FileOwnerEx { kind, pid };
                        let mut writer = UserBufferWriter::new(
                            arg as *mut FileOwnerEx,
                            size_of::<FileOwnerEx>(),
                            true,
                        )?;
                        writer.buffer_protected(0)?.write_one(0, &owner)?;
                        Ok(0)
                    }
                    _ => unreachable!(),
                }
            }

            FcntlCommand::SetSig => {
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                if arg > Signal::SIGRTMAX as usize {
                    return Err(SystemError::EINVAL);
                }
                file.set_owner_signum(arg as i32);
                return Ok(0);
            }
            FcntlCommand::GetSig => {
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                return Ok(file.owner_signum() as usize);
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
                let pipe_inode = file.pipe_runtime()?;

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
                let pipe_inode = file.pipe_runtime()?;

                // set_pipe_size 内部会验证大小是否合法
                return crate::filesystem::vfs::file::File::recover_io(|| {
                    pipe_inode.set_pipe_size(arg)
                });
            }
            FcntlCommand::GetSeals | FcntlCommand::AddSeals => {
                let binding = ProcessManager::current_pcb().fd_table();
                let file = binding
                    .read()
                    .get_file_by_fd(fd)
                    .ok_or(SystemError::EBADF)?;
                if cmd == FcntlCommand::GetSeals {
                    return file.inode().get_seals().map(|bits| bits as usize);
                }
                if !file
                    .mode()
                    .contains(crate::filesystem::vfs::file::FileMode::FMODE_WRITE)
                {
                    return Err(SystemError::EPERM);
                }
                file.inode().add_seals(arg as u32)?;
                return Ok(0);
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
