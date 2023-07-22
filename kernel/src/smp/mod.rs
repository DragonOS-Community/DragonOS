use crate::{
    arch::{asm::current::current_pcb, interrupt::ipi::send_ipi},
    exception::ipi::{IpiKind, IpiTarget},
    mm::INITIAL_PROCESS_ADDRESS_SPACE,
    syscall::SystemError,
};

pub mod c_adapter;
pub mod core;

pub fn kick_cpu(cpu_id: usize) -> Result<(), SystemError> {
    // todo: 增加对cpu_id的有效性检查

    send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu_id));
    return Ok(());
}

/// 初始化AP核的idle进程
pub unsafe fn init_smp_idle_process() {
    current_pcb().set_address_space(INITIAL_PROCESS_ADDRESS_SPACE());
}
