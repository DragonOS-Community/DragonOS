use crate::{
    arch::{
        asm::current::current_pcb,
        context::switch_process,
        interrupt::{cli, sti},
    },
    syscall::{Syscall, SystemError},
};

use super::core::do_sched;

impl Syscall {
    /// @brief 让系统立即运行调度器的系统调用
    /// 请注意，该系统调用不能由ring3的程序发起
    #[inline(always)]
    pub fn sched(from_user: bool) -> Result<usize, SystemError> {
        cli();
        // 进行权限校验，拒绝用户态发起调度
        if from_user {
            return Err(SystemError::EPERM);
        }
        // 根据调度结果统一进行切换
        let pcb = do_sched();

        if pcb.is_some() {
            switch_process(current_pcb(), pcb.unwrap());
        }
        sti();
        return Ok(0);
    }
}
