use alloc::sync::Arc;
use system_error::SystemError;

#[cfg(target_arch = "x86_64")]
use crate::arch::driver::apic::{CurrentApic, LocalAPIC};

use crate::{
    arch::MMArch,
    mm::MemoryManagementArch,
    sched::{SchedMode, __schedule},
    smp::cpu::ProcessorId,
};

use super::{
    irqdata::IrqHandlerData,
    irqdesc::{IrqHandler, IrqReturn},
    HardwareIrqNumber, IrqNumber,
};

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IpiKind {
    KickCpu,
    FlushTLB,
    /// 指定中断向量号
    SpecVector(HardwareIrqNumber),
}

/// IPI投递目标
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[allow(dead_code)]
pub enum IpiTarget {
    /// 当前CPU
    Current,
    /// 所有CPU
    All,
    /// 除了当前CPU以外的所有CPU
    Other,
    /// 指定的CPU
    Specified(ProcessorId),
}

/// 处理跨核心CPU唤醒的IPI
#[derive(Debug)]
pub struct KickCpuIpiHandler;

impl IrqHandler for KickCpuIpiHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        #[cfg(target_arch = "x86_64")]
        CurrentApic.send_eoi();

        // 被其他cpu kick时应该是抢占调度
        __schedule(SchedMode::SM_PREEMPT);
        Ok(IrqReturn::Handled)
    }
}

/// 处理TLB刷新的IPI
#[derive(Debug)]
pub struct FlushTLBIpiHandler;

impl IrqHandler for FlushTLBIpiHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        unsafe { MMArch::invalidate_all() };

        Ok(IrqReturn::Handled)
    }
}
