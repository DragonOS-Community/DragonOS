use crate::{
    arch::interrupt::ipi::send_ipi,
    exception::ipi::{IpiKind, IpiTarget},
    syscall::SystemError,
};

pub mod core;

pub fn kick_cpu(cpu_id: usize) -> Result<(), SystemError> {
    // todo: 增加对cpu_id的有效性检查

    send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu_id));
    return Ok(());
}

#[no_mangle]
pub extern "C" fn rs_kick_cpu(cpu_id: usize) -> usize {
    return kick_cpu(cpu_id)
        .map(|_| 0usize)
        .unwrap_or_else(|e| e.to_posix_errno() as usize);
}
