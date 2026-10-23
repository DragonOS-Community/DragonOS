use core::hint::spin_loop;

use log::error;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    process::{ProcessFlags, ProcessManager},
    sched::{cpu_rq, SchedMode, __schedule},
};

impl ProcessManager {
    /// 每个核的idle进程
    pub fn arch_idle_func() -> ! {
        loop {
            let pcb = ProcessManager::current_pcb();
            if pcb.flags().contains(ProcessFlags::NEED_SCHEDULE) {
                __schedule(SchedMode::SM_NONE);
            }
            if CurrentIrqArch::is_irq_enabled() {
                // 对齐 Linux x86 default_idle() 的 safe_halt 语义：
                // 上面的 NEED_SCHEDULE 检查是在开中断下做的；IPI 若落在"检查
                // 之后、halt 之前"的窗口内，处理程序置位的 NEED_SCHEDULE 会被
                // 随后的 hlt 无视，已入队的任务要等到下一次任意 IRQ 才会被调度
                // （实测：wake 返回成功但 worker ~1s 不被执行，idle 在
                // nr_running>0 时仍进入 halt）。
                // 关中断后复查 NEED_SCHEDULE，再以 STI;HLT 原子地开中断+停机：
                // IPI 要么先于复查（标志已可见，走调度），要么在 STI;HLT 之后
                // 到达（立即唤醒核心），不存在丢失窗口。
                let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
                if pcb.flags().contains(ProcessFlags::NEED_SCHEDULE) {
                    drop(irq_guard);
                    continue;
                }
                // 远端入队的 nr_running 递增与 NEED_SCHEDULE 置位之间存在无锁
                // 读取窗口；此处再读一次，若仍有可运行任务则不进入 halt（最坏
                // 情况多循环一次）。
                {
                    let cpu = crate::smp::core::smp_get_processor_id().data();
                    if cpu_rq(cpu as usize).cfs_rq().nr_running() > 0 {
                        drop(irq_guard);
                        continue;
                    }
                }
                let rcu_idle = crate::rcu::idle_enter();
                // sti; hlt —— 与 Linux safe_halt() 等价（x86_64 crate 的
                // enable_and_hlt 处理了 STI 的中断影子）。
                unsafe { x86_64::instructions::interrupts::enable_and_hlt() };
                crate::rcu::idle_exit(rcu_idle);
                // 此时硬件 IF 已置位，irq_guard 保存的也是开中断状态，
                // 其 Drop 不会引入额外的关中断窗口。
                drop(irq_guard);
            } else {
                error!("Idle process should not be scheduled with IRQs disabled.");
                spin_loop();
            }
        }
    }
}
