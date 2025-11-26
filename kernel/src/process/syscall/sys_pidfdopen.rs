use crate::alloc::string::ToString;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PIDFD_OPEN;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::file::FilePrivateData;
use crate::filesystem::vfs::syscall::InodeMode;
use crate::filesystem::vfs::FileType;
use crate::process::pid::PidPrivateData;
use crate::process::ProcessManager;
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

        let mode = InodeMode::from_bits(flags).ok_or_else(|| {
            log::error!("SysPidFdOpen: failed to get mode!");
            SystemError::EINVAL
        })?;
        let file_type = FileType::from(mode);
        let file_mode = FileFlags::from_bits(flags).ok_or_else(|| {
            log::error!("SysPidFdOpen: failed to get file_mode!");
            SystemError::EINVAL
        })?;

        let root_inode = ProcessManager::current_mntns().root_inode();
        let name = format!(
            "Pidfd(from {} to {})",
            ProcessManager::current_pcb().raw_pid().data(),
            pid
        );
        let new_inode = root_inode.create(&name, file_type, mode)?;
        let file = File::new(new_inode, file_mode)?;
        {
            let mut guard = file.private_data.lock();
            *guard = FilePrivateData::Pid(PidPrivateData::new(pid));
        }

        // 存入pcb
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(file, None)
            .map(|fd| fd as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args))),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PIDFD_OPEN, SysPidFdOpen);
