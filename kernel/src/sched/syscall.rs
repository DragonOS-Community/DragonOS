use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch, exception::InterruptArch, process::ProcessManager,
    smp::core::smp_get_processor_id, syscall::Syscall,
};

use super::core::{do_sched, CPU_EXECUTING};

impl Syscall {
    /// @brief 让系统立即运行调度器的系统调用
    /// 请注意，该系统调用不能由ring3的程序发起
    #[inline(always)]
    pub fn sched(from_user: bool) -> Result<usize, SystemError> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 进行权限校验，拒绝用户态发起调度
        if from_user {
            return Err(SystemError::EPERM);
        }
        // 根据调度结果统一进行切换
        let pcb = do_sched();

        if pcb.is_some() {
            let next_pcb = pcb.unwrap();
            let current_pcb = ProcessManager::current_pcb();
            // kdebug!("sched: current_pcb: {:?}, next_pcb: {:?}\n", current_pcb, next_pcb);
            if current_pcb.pid() != next_pcb.pid() {
                CPU_EXECUTING.set(smp_get_processor_id(), next_pcb.pid());
                unsafe { ProcessManager::switch_process(current_pcb, next_pcb) };
            }
        }
        drop(irq_guard);
        return Ok(0);
    }
}
