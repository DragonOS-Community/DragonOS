use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_FSTATFS;
use crate::filesystem::vfs::syscall::PosixStatfs;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysFstatfsHandle;

impl Syscall for SysFstatfsHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let user_statfs = Self::statfs(args);
        let mut writer = UserBufferWriter::new(user_statfs, size_of::<PosixStatfs>(), true)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);
        let inode = file.inode();
        let sb = inode.fs().statfs(&inode)?;
        let statfs = PosixStatfs::from(sb);
        writer.copy_one_to_user(&statfs, 0)?;
        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", format!("{:#x}", Self::fd(args) as usize)),
            FormattedSyscallParam::new("statfs", format!("{:#x}", Self::statfs(args) as usize)),
        ]
    }
}

impl SysFstatfsHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn statfs(args: &[usize]) -> *mut PosixStatfs {
        args[1] as *mut PosixStatfs
    }
}

syscall_table_macros::declare_syscall!(SYS_FSTATFS, SysFstatfsHandle);
