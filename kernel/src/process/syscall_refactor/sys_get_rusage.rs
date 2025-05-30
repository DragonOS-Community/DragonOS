use crate::arch::syscall::nr::SYS_GETRUSAGE;
use crate::process::resource::RUsageWho;
use crate::process::{resource::RUsage, ProcessManager};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use core::ffi::c_int;
use system_error::SystemError;

pub struct SysGetRusage;

impl SysGetRusage {
    fn who(args: &[usize]) -> c_int {
        args[0] as c_int
    }

    fn rusage(args: &[usize]) -> *mut RUsage {
        args[1] as *mut RUsage
    }
}

impl Syscall for SysGetRusage {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let who = Self::who(args);
        let rusage = Self::rusage(args);

        let who = RUsageWho::try_from(who)?;
        let mut writer = UserBufferWriter::new(rusage, core::mem::size_of::<RUsage>(), true)?;
        let pcb = ProcessManager::current_pcb();
        let rusage = pcb.get_rusage(who).ok_or(SystemError::EINVAL)?;

        let ubuf = writer.buffer::<RUsage>(0).unwrap();
        ubuf.copy_from_slice(&[rusage]);

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("who", format!("{:#x}", Self::who(args))),
            FormattedSyscallParam::new("rusage", format!("{:#x}", Self::rusage(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETRUSAGE, SysGetRusage);
