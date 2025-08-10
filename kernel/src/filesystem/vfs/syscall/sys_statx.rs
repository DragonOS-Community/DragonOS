use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_STATX;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::stat::do_statx;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysStatxHandle;

impl Syscall for SysStatxHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dfd = SysStatxHandle::dfd(args);
        let filename_ptr = SysStatxHandle::filename_ptr(args);
        let flags = SysStatxHandle::flags(args);
        let mask = SysStatxHandle::mask(args);
        let user_kstat_ptr = SysStatxHandle::user_kstat_ptr(args);
        Self::statx(dfd, filename_ptr, flags, mask, user_kstat_ptr)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dfd", format!("{:#x}", Self::dfd(args))),
            FormattedSyscallParam::new("filename_ptr", format!("{:#x}", Self::filename_ptr(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("mask", format!("{:#x}", Self::mask(args))),
            FormattedSyscallParam::new(
                "user_kstat_ptr",
                format!("{:#x}", Self::user_kstat_ptr(args)),
            ),
        ]
    }
}

impl SysStatxHandle {
    #[inline(never)]
    fn statx(
        dfd: i32,
        filename_ptr: usize,
        flags: u32,
        mask: u32,
        user_kstat_ptr: usize,
    ) -> Result<usize, SystemError> {
        if user_kstat_ptr == 0 {
            return Err(SystemError::EFAULT);
        }

        let filename = check_and_clone_cstr(filename_ptr as *const u8, Some(MAX_PATHLEN))?;
        let filename_str = filename.to_str().map_err(|_| SystemError::EINVAL)?;

        do_statx(dfd, filename_str, flags, mask, user_kstat_ptr).map(|_| 0)
    }

    fn dfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn filename_ptr(args: &[usize]) -> usize {
        args[1]
    }
    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
    fn mask(args: &[usize]) -> u32 {
        args[3] as u32
    }
    fn user_kstat_ptr(args: &[usize]) -> usize {
        args[4]
    }
}

syscall_table_macros::declare_syscall!(SYS_STATX, SysStatxHandle);
