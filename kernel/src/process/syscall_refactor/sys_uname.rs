use crate::arch::syscall::nr::SYS_UNAME;
use crate::process::syscall::PosixOldUtsName;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysUname;

impl SysUname {
    fn name(args: &[usize]) -> *mut PosixOldUtsName {
        args[0] as *mut PosixOldUtsName
    }
}

impl Syscall for SysUname {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let name = Self::name(args);
        let mut writer =
            UserBufferWriter::new(name, core::mem::size_of::<PosixOldUtsName>(), true)?;
        writer.copy_one_to_user(&PosixOldUtsName::new(), 0)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "name",
            format!("{:#x}", Self::name(args) as usize),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_UNAME, SysUname);
