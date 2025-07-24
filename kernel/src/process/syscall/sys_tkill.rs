use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::syscall::nr::SYS_TKILL;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysTkill;

impl SysTkill {
    fn thread_id(args: &[usize]) -> usize {
        args[0]
    }

    fn signal(args: &[usize]) -> usize {
        args[1] as usize
    }
}

impl Syscall for SysTkill {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _tf: &mut TrapFrame) -> Result<usize, SystemError> {
        let tid = Self::thread_id(args);
        let sig = Self::signal(args);
        // 当前只支持向当前进程的线程发送信号
        let current = ProcessManager::current_pcb();
        // 检查线程ID是否有效（目前简化处理）
        if tid != 0 && tid != current.raw_pid().data() {
            return Err(SystemError::ESRCH);
        }
        if sig > Signal::SIGRTMAX as usize {
            return Err(SystemError::EINVAL);
        }
        // current.stop_process(sig.into());
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("tid", format!("{}", Self::thread_id(args))),
            FormattedSyscallParam::new("sig", format!("{}", Self::signal(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_TKILL, SysTkill);
