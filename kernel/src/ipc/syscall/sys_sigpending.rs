use crate::{
    alloc::vec::Vec,
    arch::ipc::signal::SigSet,
    arch::syscall::nr::SYS_RT_SIGPENDING,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
};
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSigpendingHandle;
#[inline(never)]
pub(super) fn do_kernel_rt_sigpending(
    user_sigset_ptr: usize,
    sigsetsize: usize,
) -> Result<usize, SystemError> {
    if sigsetsize != size_of::<SigSet>() {
        return Err(SystemError::EINVAL);
    }

    let mut user_buffer_writer =
        UserBufferWriter::new(user_sigset_ptr as *mut SigSet, size_of::<SigSet>(), true)?;

    let pcb = ProcessManager::current_pcb();
    let siginfo_guard = pcb.sig_info_irqsave();
    let pending_set = siginfo_guard.sig_pending().signal();
    let shared_pending_set = siginfo_guard.sig_shared_pending().signal();
    let blocked_set = *siginfo_guard.sig_blocked();
    drop(siginfo_guard);

    let mut result = pending_set.union(shared_pending_set);
    result = result.difference(blocked_set);

    user_buffer_writer.copy_one_to_user(&result, 0)?;

    Ok(0)
}
impl SysSigpendingHandle {}

impl Syscall for SysSigpendingHandle {
    fn num_args(&self) -> usize {
        2 // sigpending(sigset_t *set)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("user_sigset_ptr", format!("{}", args[0])),
            FormattedSyscallParam::new("sigsetsize", format!("{}", args[1])),
        ]
    }
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        do_kernel_rt_sigpending(args[0], args[1])
    }
}

declare_syscall!(SYS_RT_SIGPENDING, SysSigpendingHandle);
