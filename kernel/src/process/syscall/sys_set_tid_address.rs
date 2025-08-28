use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SET_TID_ADDRESS;
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetTidAddress;

impl SysSetTidAddress {
    fn ptr(args: &[usize]) -> usize {
        args[0]
    }
}

impl Syscall for SysSetTidAddress {
    fn num_args(&self) -> usize {
        1
    }

    /// # 函数的功能
    /// 设置线程地址
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let ptr = Self::ptr(args);

        let pcb = ProcessManager::current_pcb();
        pcb.thread.write_irqsave().clear_child_tid = Some(VirtAddr::new(ptr));
        Ok(pcb.pid.0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "ptr",
            format!("{:#x}", Self::ptr(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SET_TID_ADDRESS, SysSetTidAddress);
