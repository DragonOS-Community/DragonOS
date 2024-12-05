use riscv::register::sstatus::{FS, SPP};
use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    mm::VirtAddr,
    process::exec::{BinaryLoaderResult, ExecParam},
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
        // debug!("write proc_init_info to user stack done");

        regs.a0 = param.init_info().args.len();
        regs.a1 = argv_ptr.data();

        // 设置系统调用返回时的寄存器状态
        regs.sp = user_sp.data();

        regs.epc = load_result.entry_point().data();
        regs.status.update_spp(SPP::User);
        regs.status.update_fs(FS::Clean);
        regs.status.update_sum(true);

        return Ok(());
    }

    /// ## 用于控制和查询与体系结构相关的进程特定选项
    #[allow(dead_code)]
    pub fn arch_prctl(_option: usize, _arg2: usize) -> Result<usize, SystemError> {
        unimplemented!("Syscall::arch_prctl")
    }
}
