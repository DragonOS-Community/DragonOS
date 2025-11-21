use core::sync::atomic::{compiler_fence, AtomicUsize, Ordering};

use crate::{
    arch::{ipc::signal::Signal, CurrentIrqArch},
    exception::InterruptArch,
    ipc::kill::kill_process_by_pcb,
    process::ProcessControlBlock,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::jiffies::TICK_NESC,
};
use alloc::sync::Arc;

use super::{clock::SchedClock, cpu_irq_time};

pub fn irq_time_read(cpu: ProcessorId) -> u64 {
    compiler_fence(Ordering::SeqCst);
    let irqtime = cpu_irq_time(cpu);

    let mut total;

    loop {
        let seq = irqtime.sync.load(Ordering::SeqCst);
        total = irqtime.total;

        if seq == irqtime.sync.load(Ordering::SeqCst) {
            break;
        }
    }
    compiler_fence(Ordering::SeqCst);
    total
}

#[derive(Debug, Default)]
pub struct IrqTime {
    pub total: u64,
    pub tick_delta: u64,
    pub irq_start_time: u64,
    pub sync: AtomicUsize,
}

impl IrqTime {
    pub fn account_delta(&mut self, delta: u64) {
        // 开始更改时增加序列号
        self.sync.fetch_add(1, Ordering::SeqCst);
        self.total += delta;
        self.tick_delta += delta;
    }

    pub fn irqtime_tick_accounted(&mut self, max: u64) -> u64 {
        let delta = self.tick_delta.min(max);
        self.tick_delta -= delta;
        return delta;
    }

    pub fn irqtime_start() {
        let cpu = smp_get_processor_id();
        let irq_time = cpu_irq_time(cpu);
        compiler_fence(Ordering::SeqCst);
        irq_time.irq_start_time = SchedClock::sched_clock_cpu(cpu) as u64;
        compiler_fence(Ordering::SeqCst);
    }

    pub fn irqtime_account_irq(_pcb: Arc<ProcessControlBlock>) {
        compiler_fence(Ordering::SeqCst);
        let cpu = smp_get_processor_id();
        let irq_time = cpu_irq_time(cpu);
        compiler_fence(Ordering::SeqCst);
        let delta = SchedClock::sched_clock_cpu(cpu) as u64 - irq_time.irq_start_time;
        compiler_fence(Ordering::SeqCst);

        irq_time.account_delta(delta);
        compiler_fence(Ordering::SeqCst);
    }
}

pub struct CpuTimeFunc;
impl CpuTimeFunc {
    pub fn irqtime_account_process_tick(
        pcb: &Arc<ProcessControlBlock>,
        user_tick: bool,
        ticks: u64,
    ) {
        let cputime = TICK_NESC as u64 * ticks;

        let other = Self::account_other_time(u64::MAX);

        if other >= cputime {
            return;
        }

        if user_tick {
            pcb.account_utime(cputime);
        } else {
            pcb.account_stime(cputime);
        }
        pcb.add_sum_exec_runtime(cputime);

        let cputime_ns = TICK_NESC as u64 * ticks;
        let accounted_cputime = cputime_ns - other;
        // 检查并处理CPU时间定时器
        let mut itimers = pcb.itimers_irqsave();
        // 处理 ITIMER_VIRTUAL (仅在用户态tick时消耗时间)
        if user_tick && itimers.virt.is_active {
            if itimers.virt.value <= accounted_cputime {
                kill_process_by_pcb(pcb.clone(), Signal::SIGVTALRM).ok();
                if itimers.virt.interval > 0 {
                    // 周期性定时器：在旧的剩余时间上增加间隔时间
                    itimers.virt.value += itimers.virt.interval;
                } else {
                    // 一次性定时器：禁用
                    itimers.virt.is_active = false;
                    itimers.virt.value = 0;
                }
            } else {
                itimers.virt.value -= accounted_cputime;
            }
        }

        // 处理 ITIMER_PROF (在用户态和内核态tick时都消耗时间)
        if itimers.prof.is_active {
            if itimers.prof.value <= accounted_cputime {
                kill_process_by_pcb(pcb.clone(), Signal::SIGPROF).ok();
                if itimers.prof.interval > 0 {
                    itimers.prof.value += itimers.prof.interval;
                } else {
                    itimers.prof.is_active = false;
                    itimers.prof.value = 0;
                }
            } else {
                itimers.prof.value -= accounted_cputime;
            }
        }
    }

    pub fn account_other_time(max: u64) -> u64 {
        assert!(!CurrentIrqArch::is_irq_enabled());

        let mut accounted = Self::steal_account_process_time(max);

        if accounted < max {
            let irqtime = cpu_irq_time(smp_get_processor_id());
            accounted += irqtime.irqtime_tick_accounted(max - accounted);
        }

        accounted
    }

    pub fn steal_account_process_time(_max: u64) -> u64 {
        // 这里未考虑虚拟机时间窃取
        0
    }
}
