use crate::alloc::string::ToString;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PIDFD_OPEN;
use crate::filesystem::vfs::file::FileFlags;
use crate::process::pidfd::PidFd;
use crate::process::{ProcessManager, RawPid};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysPidFdOpen;

impl SysPidFdOpen {
    #[inline(always)]
    fn pid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn flags(args: &[usize]) -> u32 {
        args[1] as u32
    }
}

impl Syscall for SysPidFdOpen {
    fn num_args(&self) -> usize {
        2
    }

    /// 没实现完全, 见https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/pid.c#pidfd_create
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let flags = Self::flags(args);

        if pid <= 0 {
            return Err(SystemError::EINVAL);
        }

        let valid_flags = FileFlags::O_NONBLOCK.bits();
        if flags & !valid_flags != 0 {
            return Err(SystemError::EINVAL);
        }

        let mut file_flags = FileFlags::empty();
        if flags & FileFlags::O_NONBLOCK.bits() != 0 {
            file_flags.insert(FileFlags::O_NONBLOCK);
        }
        let pid = ProcessManager::find_vpid(RawPid::new(pid as usize)).ok_or(SystemError::ESRCH)?;
        let current = ProcessManager::current_pcb();
        let prepared = PidFd::prepare(&current, pid, file_flags, true)?;

        // 存入pcb
        // Linux 的 __pidfd_prepare() 无条件对 pidfd 设置 O_CLOEXEC，
        // 无论用户传入什么 flags，pidfd 始终是 close-on-exec 的。
        let reservation = prepared.reservation;
        let file = prepared.file;
        let fd_table = current.fd_table();
        let mut fd_table = fd_table.write();
        match fd_table.install_reserved_fd(reservation, file) {
            Ok(fd) => Ok(fd as usize),
            Err(err) => {
                fd_table.release_reserved_fd(reservation);
                Err(err)
            }
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args))),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PIDFD_OPEN, SysPidFdOpen);
