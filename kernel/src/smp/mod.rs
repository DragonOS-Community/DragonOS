use system_error::SystemError;

use crate::{
    arch::interrupt::ipi::send_ipi,
    exception::ipi::{IpiKind, IpiTarget},
};

use self::{
    core::smp_get_processor_id,
    cpu::{smp_cpu_manager_init, ProcessorId},
};

pub mod c_adapter;
pub mod core;
pub mod cpu;

pub fn kick_cpu(cpu_id: ProcessorId) -> Result<(), SystemError> {
    // todo: 增加对cpu_id的有效性检查

    send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu_id));
    return Ok(());
}

pub trait SMPArch {
    /// 准备SMP初始化所需的cpu拓扑数据。
    ///
    /// 该函数需要标记为 `#[inline(never)]`
    fn prepare_cpus() -> Result<(), SystemError>;

    /// 初始化SMP
    ///
    /// 该函数需要标记为 `#[inline(never)]`
    fn init() -> Result<(), SystemError>;
}

/// 早期SMP初始化
#[inline(never)]
pub fn early_smp_init() -> Result<(), SystemError> {
    smp_cpu_manager_init(smp_get_processor_id());

    return Ok(());
}
