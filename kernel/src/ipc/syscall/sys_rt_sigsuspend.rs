use syscall_table_macros::declare_syscall;
use system_error::SystemError;

use crate::arch::ipc::signal::SigSet;
use crate::arch::syscall::nr::SYS_RT_SIGSUSPEND;
use crate::ipc::signal::{set_sigprocmask, SigHow};
use crate::process::ProcessManager;
use crate::sched::{schedule, SchedMode};
use crate::{
    mm::VirtAddr,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
};

/// See <https://man7.org/linux/man-pages/man2/rt_sigsuspend.2.html>
pub struct SysRtSigSuspend;
impl Syscall for SysRtSigSuspend {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        let sigsetsize = args[1];
        if sigsetsize != size_of::<SigSet>() {
            return Err(SystemError::EFAULT);
        }

        let reader = UserBufferReader::new(
            VirtAddr::new(args[0]).as_ptr::<u64>(),
            core::mem::size_of::<u64>(),
            true,
        )?;
        let mask = reader.read_one_from_user::<u64>(0)?;
        let mut mask = SigSet::from_bits_truncate(*mask);
        // It is not possible to block SIGKILL or SIGSTOP; specifying these
        // signals in mask, has no effect on the thread's signal mask.
        mask -= SigSet::SIGKILL;
        mask -= SigSet::SIGSTOP;

        let pcb = ProcessManager::current_pcb();
        let old_mask = *pcb.sig_info_irqsave().sig_blocked();

        set_sigprocmask(SigHow::SetMask, mask).unwrap();
        log::trace!("Process enter rt_sigsuspend, new mask: {mask:?}, old mask: {old_mask:?}");
        loop {
            if pcb.has_pending_signal_fast() && pcb.has_pending_not_masked_signal() {
                set_sigprocmask(SigHow::SetMask, old_mask).unwrap();
                return Err(SystemError::EINTR);
            }
            schedule(SchedMode::SM_NONE);
        }
        // unreachable!("rt_sigsuspend always return EINTR");
    }

    fn entry_format(
        &self,
        args: &[usize],
    ) -> alloc::vec::Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "mask",
            format!("{:#x}", args[0]),
        )]
    }
}

declare_syscall!(SYS_RT_SIGSUSPEND, SysRtSigSuspend);
