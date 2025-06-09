use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    mm::VirtAddr,
    process::{
        exec::{BinaryLoaderResult, ExecParam},
        ProcessManager,
    },
    syscall::Syscall,
};

impl Syscall {
    pub fn arch_do_execve(
        regs: &mut TrapFrame,
        param: &ExecParam,
        load_result: &BinaryLoaderResult,
        user_sp: VirtAddr,
        argv_ptr: VirtAddr,
    ) -> Result<(), SystemError> {
        todo!("la64:arch_do_execve not unimplemented");
    }

    /// ## 用于控制和查询与体系结构相关的进程特定选项
    pub fn arch_prctl(option: usize, arg2: usize) -> Result<usize, SystemError> {
        todo!("la64:arch_prctl")
    }
}
