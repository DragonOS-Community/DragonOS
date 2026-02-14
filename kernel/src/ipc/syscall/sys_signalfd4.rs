use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SIGNALFD4;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::ipc::signalfd::{read_user_sigset, SignalFdFlags, SignalFdInode};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSignalFd4Handle;

impl SysSignalFd4Handle {
    #[inline(always)]
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn mask_ptr(args: &[usize]) -> usize {
        args[1]
    }

    #[inline(always)]
    fn mask_size(args: &[usize]) -> usize {
        args[2]
    }

    #[inline(always)]
    fn flags(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

impl Syscall for SysSignalFd4Handle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let mask_ptr = Self::mask_ptr(args);
        let mask_size = Self::mask_size(args);
        let flags = Self::flags(args);

        let flags = SignalFdFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        let mask = read_user_sigset(mask_ptr, mask_size)?;

        if fd == -1 {
            let inode = Arc::new(SignalFdInode::new(mask, flags));
            let mut file_flags = FileFlags::O_RDONLY;
            if flags.contains(SignalFdFlags::SFD_NONBLOCK) {
                file_flags |= FileFlags::O_NONBLOCK;
            }
            if flags.contains(SignalFdFlags::SFD_CLOEXEC) {
                file_flags |= FileFlags::O_CLOEXEC;
            }

            let file = File::new(inode, file_flags)?;
            let cloexec = flags.contains(SignalFdFlags::SFD_CLOEXEC);
            let binding = ProcessManager::current_pcb().fd_table();
            let mut fd_table_guard = binding.write();
            let fd = fd_table_guard.alloc_fd(file, None, cloexec)?;
            return Ok(fd as usize);
        }

        // 更新已存在的 signalfd
        let pcb = ProcessManager::current_pcb();
        let fd_table = pcb.fd_table();
        let file = fd_table
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        let inode = file.inode();
        let sfd = inode
            .as_any_ref()
            .downcast_ref::<SignalFdInode>()
            .ok_or(SystemError::EINVAL)?;

        sfd.set_mask_and_flags(mask, flags);

        // 同步 NONBLOCK/CLOEXEC 到 file flags
        let mut new_flags = file.flags();
        if flags.contains(SignalFdFlags::SFD_NONBLOCK) {
            new_flags |= FileFlags::O_NONBLOCK;
        } else {
            new_flags.remove(FileFlags::O_NONBLOCK);
        }
        let _ = file.set_flags(new_flags);

        // close_on_exec 是 per-fd 属性，需要通过 fd 表设置
        let fd_table = pcb.fd_table();
        let mut fd_table_guard = fd_table.write();
        fd_table_guard.set_cloexec(fd, flags.contains(SignalFdFlags::SFD_CLOEXEC));

        Ok(fd as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{}", Self::fd(args))),
            FormattedSyscallParam::new("mask", format!("{:#x}", Self::mask_ptr(args))),
            FormattedSyscallParam::new("mask_size", format!("{}", Self::mask_size(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SIGNALFD4, SysSignalFd4Handle);
