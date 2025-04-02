/// 系统调用号
pub mod nr;
use system_error::SystemError;

use crate::{exception::InterruptArch, process::ProcessManager, syscall::Syscall};

use super::{interrupt::TrapFrame, CurrentIrqArch};

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    return Ok(());
}

macro_rules! syscall_return {
    ($val:expr, $regs:expr, $show:expr) => {{
        let ret = $val;
        $regs.a0 = ret;

        if $show {
            let pid = ProcessManager::current_pcb().pid();
            log::debug!("syscall return:pid={:?},ret= {:?}\n", pid, ret as isize);
        }

        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
        return;
    }};
}

pub(super) fn syscall_handler(syscall_num: usize, frame: &mut TrapFrame) -> () {
    // debug!("syscall_handler: syscall_num: {}", syscall_num);
    unsafe {
        CurrentIrqArch::interrupt_enable();
    }

    let args = [frame.a0, frame.a1, frame.a2, frame.a3, frame.a4, frame.a5];
    let mut syscall_handle = || -> usize {
        Syscall::catch_handle(syscall_num, &args, frame)
            .unwrap_or_else(|e| e.to_posix_errno() as usize)
    };
    syscall_return!(syscall_handle(), frame, false);
}
