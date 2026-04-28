use alloc::sync::Arc;
use system_error::SystemError;

#[cfg(target_arch = "x86_64")]
use crate::arch::driver::apic::{CurrentApic, LocalAPIC};

use crate::{
    sched::{cpu_rq, cpu_wakequeue, task_cpu, EnqueueFlag, WakeupFlags},
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
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

        let cpu = smp_get_processor_id();
        let wq = cpu_wakequeue(cpu.data() as usize);
        for pcb in wq.drain() {
            let state = pcb.sched_info().inner_lock_read_irqsave().state();
            // 已退出进程不可被唤醒，跳过过期条目以防重新入队。
            if state.is_exited() {
                continue;
            }

            let rq = cpu_rq(cpu.data() as usize);
            let (rq, _guard) = rq.self_lock();
            rq.update_rq_clock();

            let prev_cpu = task_cpu(&pcb);
            let migrated = prev_cpu != cpu;
            // nr_iowait 在 __set_task_cpu 之前在 source rq 递减。
            if pcb
                .flags()
                .contains(crate::process::ProcessFlags::IN_IOWAIT)
            {
                if migrated {
                    cpu_rq(prev_cpu.data() as usize).dec_nr_iowait();
                } else {
                    rq.dec_nr_iowait();
                }
            }
            if migrated {
                crate::sched::__set_task_cpu(&pcb, cpu);
            }
            let mut flags = EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK;
            if migrated {
                flags |= EnqueueFlag::ENQUEUE_MIGRATED;
            }

            // dec nr_uninterruptible 后必须清除标志，防止后续 stop/resume 路径重复递减。
            if pcb
                .flags()
                .contains(crate::process::ProcessFlags::SCHED_CONTRIBUTES_TO_LOAD)
            {
                rq.dec_nr_uninterruptible();
                pcb.flags()
                    .remove(crate::process::ProcessFlags::SCHED_CONTRIBUTES_TO_LOAD);
            }

            rq.activate_task(&pcb, flags);
            rq.check_preempt_currnet(&pcb, WakeupFlags::WF_MIGRATED);
        }

        // 不直接调用 __schedule；让 check_preempt_currnet 或 NEED_SCHEDULE 标志
        // 在从中断返回时由正常路径处理
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
