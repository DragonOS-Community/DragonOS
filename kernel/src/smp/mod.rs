use system_error::SystemError;

use crate::{
    arch::{interrupt::ipi::send_ipi, CurrentSMPArch},
    exception::ipi::{IpiKind, IpiTarget},
};

use self::{
    core::smp_get_processor_id,
    cpu::{smp_cpu_manager, smp_cpu_manager_init, CpuHpCpuState, ProcessorId},
};

pub mod core;
pub mod cpu;
pub mod init;
mod syscall;

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

    /// 在smp初始化结束后，执行一些必要的操作
    ///
    /// 该函数需要标记为 `#[inline(never)]`
    fn post_init() -> Result<(), SystemError> {
        return Ok(());
    }

    /// 向目标CPU发送启动信号
    ///
    /// 如果目标CPU已经启动，返回Ok。
    fn start_cpu(cpu_id: ProcessorId, hp_state: &CpuHpCpuState) -> Result<(), SystemError>;
}

/// 早期SMP初始化
#[inline(never)]
pub fn early_smp_init() -> Result<(), SystemError> {
    smp_cpu_manager_init(smp_get_processor_id());

    return Ok(());
}

#[inline(never)]
pub fn smp_init() {
    smp_cpu_manager().bringup_nonboot_cpus();

    CurrentSMPArch::post_init().expect("SMP post init failed");
}
