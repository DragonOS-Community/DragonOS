use system_error::SystemError;

use ::core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    arch::{interrupt::ipi::send_ipi, CurrentSMPArch},
    exception::ipi::{IpiKind, IpiTarget},
};

use self::{
    core::smp_get_processor_id,
    cpu::{
        smp_cpu_manager, smp_cpu_manager_init, smp_cpu_manager_initialized, CpuHpCpuState,
        ProcessorId,
    },
};

pub mod core;
pub mod cpu;
pub mod init;
mod syscall;

#[repr(align(64))]
struct KickCpuReceiveCounter(AtomicU64);

static KICK_CPU_RECEIVED: [KickCpuReceiveCounter; crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize] =
    [const { KickCpuReceiveCounter(AtomicU64::new(0)) };
        crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize];

/// Returns whether this build has a working generic KickCpu IPI path.
///
/// Keep this capability next to `kick_cpu()` so architecture bring-up cannot
/// accidentally make callers infer support from CPU count alone.
pub const fn kick_cpu_supported() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Records completed delivery of the generic reschedule IPI on this CPU.
pub(crate) fn note_kick_cpu_received() {
    let cpu = smp_get_processor_id();
    KICK_CPU_RECEIVED[cpu.data() as usize]
        .0
        .fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn kick_cpu_received(cpu: ProcessorId) -> u64 {
    KICK_CPU_RECEIVED[cpu.data() as usize]
        .0
        .load(Ordering::Relaxed)
}

pub fn kick_cpu(cpu_id: ProcessorId) -> Result<(), SystemError> {
    if !smp_cpu_manager_initialized()
        || smp_cpu_manager().possible_cpus().get(cpu_id) != Some(true)
        || !smp_cpu_manager().is_online_cpu(cpu_id)
    {
        return Err(SystemError::ENODEV);
    }

    #[cfg(target_arch = "x86_64")]
    {
        send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu_id));
        Ok(())
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = cpu_id;
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
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
