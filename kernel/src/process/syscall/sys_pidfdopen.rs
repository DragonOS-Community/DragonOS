use crate::alloc::string::ToString;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PIDFD_OPEN;
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::FileType;
use crate::process::pidfd::Pidfd;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::sync::Arc;
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

        let mode = ModeType::from_bits(flags).unwrap();
        let file_type = FileType::from(mode);
        let file_mode = FileMode::from_bits(flags).unwrap();

        let pidfd = Pidfd::new(pid, file_mode, file_type).unwrap();

        // 存入pcb
        let r = ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(Arc::new(pidfd), None)
            .map(|fd| fd as usize);
        r
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args))),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PIDFD_OPEN, SysPidFdOpen);
