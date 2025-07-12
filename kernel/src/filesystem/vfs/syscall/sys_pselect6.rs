use alloc::vec::Vec;

use system_error::SystemError;

use crate::{
    arch::{ipc::signal::SigSet, syscall::nr::SYS_PSELECT6},
    filesystem::vfs::syscall::sys_select::common_sys_select,
    ipc::signal::set_user_sigmask,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
};

pub struct SysPselect6;
impl Syscall for SysPselect6 {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let sigmask_ptr = args[5];
        let mut sigmask: Option<SigSet> = None;
        if sigmask_ptr != 0 {
            let sigmask_reader =
                UserBufferReader::new(sigmask_ptr as *const SigSet, size_of::<SigSet>(), true)?;
            sigmask.replace(*sigmask_reader.read_one_from_user(0)?);
        }
        if let Some(mut sigmask) = sigmask {
            set_user_sigmask(&mut sigmask);
        }
        common_sys_select(args[0], args[1], args[2], args[3], args[4])
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("nfds", format!("{}", args[0])),
            FormattedSyscallParam::new("readfds", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("writefds", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("exceptfds", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("timeout", format!("{:#x}", args[4])),
            FormattedSyscallParam::new("sigmask", format!("{:#x}", args[5])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PSELECT6, SysPselect6);
