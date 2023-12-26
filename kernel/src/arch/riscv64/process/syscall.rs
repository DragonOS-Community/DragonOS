use alloc::{string::String, vec::Vec};
use system_error::SystemError;

use crate::{arch::interrupt::TrapFrame, syscall::Syscall};

impl Syscall {
    pub fn do_execve(
        path: String,
        argv: Vec<String>,
        envp: Vec<String>,
        regs: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        unimplemented!("Syscall::do_execve")
    }

    /// ## 用于控制和查询与体系结构相关的进程特定选项
    pub fn arch_prctl(option: usize, arg2: usize) -> Result<usize, SystemError> {
        unimplemented!("Syscall::arch_prctl")
    }
}
