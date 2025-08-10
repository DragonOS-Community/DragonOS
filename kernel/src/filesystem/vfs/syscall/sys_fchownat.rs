use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FCHOWNAT;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{MAX_PATHLEN, fcntl::AtFlags, open::do_fchownat},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::vec::Vec;

pub struct SysFchownatHandle;

impl Syscall for SysFchownatHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let pathname = Self::pathname(args);
        let uid = Self::uid(args);
        let gid = Self::gid(args);
        let flags = Self::flags(args);

        let pathname = user_access::check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let pathname = pathname.as_str().trim();
        let flags = AtFlags::from_bits_truncate(flags);
        return do_fchownat(dirfd, pathname, uid, gid, flags);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("uid", format!("{:#x}", Self::uid(args))),
            FormattedSyscallParam::new("gid", format!("{:#x}", Self::gid(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysFchownatHandle {
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn pathname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn uid(args: &[usize]) -> usize {
        args[2]
    }

    fn gid(args: &[usize]) -> usize {
        args[3]
    }

    fn flags(args: &[usize]) -> i32 {
        args[4] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_FCHOWNAT, SysFchownatHandle);
