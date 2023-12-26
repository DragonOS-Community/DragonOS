use system_error::SystemError;

use crate::{
    arch::interrupt::ipi::send_ipi,
    exception::ipi::{IpiKind, IpiTarget},
};

pub mod c_adapter;
pub mod core;
pub mod cpu;

pub fn kick_cpu(cpu_id: u32) -> Result<(), SystemError> {
    // todo: 增加对cpu_id的有效性检查

    send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu_id as usize));
    return Ok(());
}
