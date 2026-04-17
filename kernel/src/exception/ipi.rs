use alloc::sync::Arc;
use system_error::SystemError;

#[cfg(target_arch = "x86_64")]
use crate::arch::driver::apic::{CurrentApic, LocalAPIC};

use crate::{
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
    /// TLB shootdown IPI.
    ///
    /// Do NOT directly `send_ipi(IpiKind::FlushTLB, ..)` in new code:
    ///
    /// - On x86_64 this IPI is issued by [`crate::mm::tlb::flush_tlb_multi`] via the per-CPU CSD
    ///   protocol; the receiving end [`FlushTLBIpiHandler`] reads `FlushTlbInfo` from the CSD
    ///   and performs a synchronous ack. A bare send provides no context and won't be waited
    ///   on by the initiator, inevitably breaking the "shootdown before free" ordering.
    /// - On RISC-V `send_ipi(IpiKind::FlushTLB, ..)` is completed synchronously via SBI
    ///   `remote_sfence_vma` without going through `FlushTLBIpiHandler`; it is still
    ///   restricted to be called only from the `mm::tlb` layer to keep the interface
    ///   consistent with future per-mm range flush support.
    ///
    /// All subsystems (mmap/munmap/mprotect/mremap/madvise/zap_file_mappings/...) must go
    /// through [`crate::mm::mmu_gather::MmuGather`] + [`crate::mm::ucontext::AddressSpace::flush_tlb_range`].
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

/// IPI handler for TLB flushing.
///
/// This handler is only invoked via the per-CPU CSD protocol by `crate::mm::tlb::flush_tlb_multi`.
/// Direct bare sends of `IpiKind::FlushTLB` are forbidden.
#[derive(Debug)]
pub struct FlushTLBIpiHandler;

impl IrqHandler for FlushTLBIpiHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        // Read the FlushTlbInfo context written by the initiator from the per-CPU CSD slot,
        // perform TLB invalidation for the corresponding range, and finally set `done` so
        // the initiator can synchronously wait.
        crate::mm::tlb::remote_flush_tlb_on_ipi();

        Ok(IrqReturn::Handled)
    }
}
