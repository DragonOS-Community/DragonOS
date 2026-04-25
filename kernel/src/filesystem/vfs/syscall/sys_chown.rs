use crate::arch::syscall::nr::SYS_CHOWN;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{fcntl::AtFlags, open::do_fchownat, MAX_PATHLEN},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access,
    },
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SyschownHandle;

impl Syscall for SyschownHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pathname = Self::pathname(args);
        let uid = Self::uid(args);
        let gid = Self::gid(args);

        let pathname = user_access::vfs_check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_fchownat(
            AtFlags::AT_FDCWD.bits(),
            &pathname,
            uid,
            gid,
            AtFlags::AT_STATX_SYNC_AS_STAT,
        );
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pathname", format!("{:#x}", Self::pathname(args) as usize)),
            FormattedSyscallParam::new("uid", format!("{:#x}", Self::uid(args))),
            FormattedSyscallParam::new("gid", format!("{:#x}", Self::gid(args))),
        ]
    }
}

impl SyschownHandle {
    fn pathname(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn uid(args: &[usize]) -> usize {
        args[1]
    }

    fn gid(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_CHOWN, SyschownHandle);
