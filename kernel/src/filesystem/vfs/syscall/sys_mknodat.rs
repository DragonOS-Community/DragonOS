use alloc::string::ToString;

use super::ModeType;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MKNODAT;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::FileType;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::syscall::user_access::check_and_clone_cstr;

pub struct SysMknodatHandle;

impl Syscall for SysMknodatHandle {
    /// Returns the number of arguments this syscall takes (4).
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the syscall
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let path = Self::path(args);
        let mode = Self::mode(args);
        let dev = DeviceNumber::from(Self::dev(args));

        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let mode = ModeType::from_bits(mode).ok_or(SystemError::EINVAL)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(dirfd)
            .ok_or(SystemError::EBADF)?;
        drop(fd_table_guard);

        if file.file_type() != FileType::Dir {
            return Err(SystemError::EBADF);
        }

        file.inode().mknod(&path, mode, dev)?;

        Ok(0)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", Self::dirfd(args).to_string()),
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", Self::mode(args).to_string()),
            FormattedSyscallParam::new("dev", Self::dev(args).to_string()),
        ]
    }
}

impl SysMknodatHandle {
    /// Extracts the dir descriptor (dirfd) argument from syscall parameters.
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }
    /// Extracts the dev_t argument from syscall parameters.
    fn dev(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_MKNODAT, SysMknodatHandle);
