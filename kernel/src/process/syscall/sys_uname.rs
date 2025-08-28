use core::ops::Deref;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UNAME;
use crate::process::namespace::uts_namespace::PosixNewUtsName;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysUname;

impl SysUname {
    fn name(args: &[usize]) -> *mut PosixNewUtsName {
        args[0] as *mut PosixNewUtsName
    }
}

impl Syscall for SysUname {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let name = Self::name(args);
        let mut writer =
            UserBufferWriter::new(name, core::mem::size_of::<PosixNewUtsName>(), true)?;

        let uts_ns = ProcessManager::current_utsns();
        let uts_wrapper = uts_ns.utsname();
        writer.copy_one_to_user(&PosixNewUtsName::from(uts_wrapper.deref()), 0)?;

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
