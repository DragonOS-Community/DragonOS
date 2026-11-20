use crate::arch::syscall::nr::SYS_FUTIMESAT;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{open::do_utimensat, syscall::UtimensFlags},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::{syscall::PosixTimeval, PosixTimeSpec},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) fn do_sys_futimesat(
    dirfd: i32,
    pathname: *const u8,
    times: *const PosixTimeval,
) -> Result<usize, SystemError> {
    let times = if times.is_null() {
        None
    } else {
        let reader = UserBufferReader::new(times, size_of::<[PosixTimeval; 2]>(), true)?;
        let values = reader.read_one_from_user::<[PosixTimeval; 2]>(0)?;
        // Legacy timeval syscalls validate microseconds before path lookup.
        if values
            .iter()
            .any(|time| !(0..1_000_000).contains(&time.tv_usec))
        {
            return Err(SystemError::EINVAL);
        }
        Some(values.map(PosixTimeSpec::from))
    };
    let path = super::utimensat::read_timestamp_path(pathname)?;
    do_utimensat(dirfd, path, times, UtimensFlags::empty())
}

pub struct SysFutimesatHandle;

impl Syscall for SysFutimesatHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_sys_futimesat(Self::dirfd(args), Self::pathname(args), Self::times(args))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("times", format!("{:#x}", Self::times(args) as usize)),
        ]
    }
}

impl SysFutimesatHandle {
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn pathname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn times(args: &[usize]) -> *const PosixTimeval {
        args[2] as *const PosixTimeval
    }
}

syscall_table_macros::declare_syscall!(SYS_FUTIMESAT, SysFutimesatHandle);
