use core::sync::atomic::{compiler_fence, AtomicU64, AtomicUsize, Ordering};

use crate::{
    arch::{ipc::signal::Signal, CurrentIrqArch},
    exception::InterruptArch,
    ipc::kill::send_signal_to_pcb,
    libs::lazy_init::Lazy,
    mm::percpu::PerCpuVar,
    process::{ProcessControlBlock, ProcessState},
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::jiffies::TICK_NESC,
};
use alloc::sync::Arc;

use super::{clock::SchedClock, cpu_irq_time, cpu_rq, prio::PrioUtil, SchedPolicy};

/// CPU 时间类型枚举（对齐 Linux kernel_stat.h 的 cpu_usage_stat）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum CpuUsageStat {
    User = 0,
    Nice = 1,
    System = 2,
    Softirq = 3,
    Irq = 4,
    Idle = 5,
    IoWait = 6,
    Steal = 7,
    Guest = 8,
    GuestNice = 9,
}

pub const NR_CPU_STATS: usize = 10;

/// Linux 用户空间时间单位（ticks per second）
pub const USER_HZ: u64 = 100;

/// 将纳秒转换为 USER_HZ 单位的 ticks
#[inline]
pub const fn ns_to_clock_t(ns: u64) -> u64 {
    ns * USER_HZ / 1_000_000_000
}

/// per-CPU 的内核 CPU 统计（对齐 Linux kernel_cpustat）
#[derive(Debug)]
pub struct KernelCpuStat {
    /// 各类 CPU 时间累计（单位：纳秒）
    pub cpustat: [AtomicU64; NR_CPU_STATS],
}

impl KernelCpuStat {
    pub const fn new() -> Self {
        Self {
            cpustat: [const { AtomicU64::new(0) }; NR_CPU_STATS],
        }
    }

    /// 累加指定类型的 CPU 时间（单位：ns）
    #[inline]
    pub fn account(&self, stat_type: CpuUsageStat, delta_ns: u64) {
        self.cpustat[stat_type as usize].fetch_add(delta_ns, Ordering::Relaxed);
    }

    /// 读取指定类型的累计时间（单位：ns）
    #[inline]
    #[allow(dead_code)]
    pub fn get(&self, stat_type: CpuUsageStat) -> u64 {
        self.cpustat[stat_type as usize].load(Ordering::Relaxed)
    }

    /// 获取所有统计的快照（用于 /proc/stat 导出）
    pub fn snapshot(&self) -> [u64; NR_CPU_STATS] {
        let mut result = [0u64; NR_CPU_STATS];
        for (item, stat) in result.iter_mut().zip(self.cpustat.iter()) {
            *item = stat.load(Ordering::Relaxed);
        }
        result
    }
}

static KERNEL_CPU_STAT: Lazy<PerCpuVar<KernelCpuStat>> = PerCpuVar::define_lazy();

/// 获取当前 CPU 的统计结构
#[inline]
pub fn kcpustat_this_cpu() -> &'static KernelCpuStat {
    KERNEL_CPU_STAT.ensure();
    KERNEL_CPU_STAT.get().get()
}

/// 获取指定 CPU 的统计结构
#[inline]
pub fn kcpustat_cpu(cpu: ProcessorId) -> &'static KernelCpuStat {
    KERNEL_CPU_STAT.ensure();
    unsafe { KERNEL_CPU_STAT.get().force_get(cpu) }
}

/// 初始化 per-CPU 统计（应在调度器初始化时调用）
pub fn init_kernel_cpu_stat() {
    use crate::mm::percpu::PerCpu;

    let mut cpu_stats = alloc::vec::Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
    for _ in 0..PerCpu::MAX_CPU_NUM as usize {
        cpu_stats.push(KernelCpuStat::new());
    }

    KERNEL_CPU_STAT.init(PerCpuVar::new(cpu_stats).unwrap());
}

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
    pub hardirq_delta: u64,
    pub softirq_delta: u64,
    pub irq_start_time: u64,
    pub sync: AtomicUsize,
}

impl IrqTime {
    pub fn account_delta(&mut self, delta: u64, is_hardirq: bool) {
        // 开始更改时增加序列号
        self.sync.fetch_add(1, Ordering::SeqCst);
        self.total += delta;
        self.tick_delta += delta;

        // 根据中断类型分别记录
        if is_hardirq {
            self.hardirq_delta += delta;
        } else {
            self.softirq_delta += delta;
        }
    }

    pub fn irqtime_tick_accounted(&mut self, max: u64) -> (u64, u64, u64) {
        let total_delta = self.tick_delta.min(max);
        let hardirq_delta = self.hardirq_delta.min(total_delta);
        let softirq_delta = self.softirq_delta.min(total_delta - hardirq_delta);

        self.tick_delta -= total_delta;
        self.hardirq_delta -= hardirq_delta;
        self.softirq_delta -= softirq_delta;

        (total_delta, hardirq_delta, softirq_delta)
    }

    pub fn irqtime_start() {
        let cpu = smp_get_processor_id();
        let irq_time = cpu_irq_time(cpu);
        compiler_fence(Ordering::SeqCst);
        irq_time.irq_start_time = SchedClock::sched_clock_cpu(cpu) as u64;
        compiler_fence(Ordering::SeqCst);
    }

    pub fn irqtime_account_irq(_pcb: Arc<ProcessControlBlock>, is_hardirq: bool) {
        compiler_fence(Ordering::SeqCst);
        let cpu = smp_get_processor_id();
        let irq_time = cpu_irq_time(cpu);
        compiler_fence(Ordering::SeqCst);
        let delta = SchedClock::sched_clock_cpu(cpu) as u64 - irq_time.irq_start_time;
        compiler_fence(Ordering::SeqCst);

        irq_time.account_delta(delta, is_hardirq);
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

        let accounted_cputime = cputime - other;

        // 记账到全局 per-CPU 统计
        let kcpustat = kcpustat_this_cpu();

        // 判断是否是 idle 进程
        let policy = pcb.sched_info().policy();
        if policy == SchedPolicy::IDLE {
            // idle 进程：根据 nr_iowait 区分 IDLE 和 IOWAIT
            let cpu_id = smp_get_processor_id();
            let rq = cpu_rq(cpu_id.data() as usize);
            let nr_iowait = rq.nr_iowait();

            if nr_iowait > 0 {
                // 有任务因 IO 阻塞，记入 IOWAIT
                kcpustat.account(CpuUsageStat::IoWait, accounted_cputime);
            } else {
                // 纯空闲，记入 IDLE
                kcpustat.account(CpuUsageStat::Idle, accounted_cputime);
            }
        } else if user_tick {
            // 用户态时间：区分 NICE
            // 获取进程的 nice 值（通过 static_prio 计算）
            let prio_data = pcb.sched_info().prio_data();
            let static_prio = prio_data.static_prio;
            // 使用 PrioUtil 将 prio 转换为 nice 值
            let nice = PrioUtil::prio_to_nice(static_prio);

            if nice > 0 {
                // nice > 0 的进程记入 Nice
                kcpustat.account(CpuUsageStat::Nice, accounted_cputime);
            } else {
                // nice <= 0 的进程记入 User
                kcpustat.account(CpuUsageStat::User, accounted_cputime);
            }
            pcb.account_utime(accounted_cputime);
        } else {
            // 系统态时间：记入 SYSTEM
            // IRQ 和 SOFTIRQ 通过 account_other_time() 在 tick 中单独记账
            kcpustat.account(CpuUsageStat::System, accounted_cputime);
            pcb.account_stime(accounted_cputime);
        }

        // 只有非 idle 进程才累加 sum_exec_runtime
        if policy != SchedPolicy::IDLE {
            pcb.add_sum_exec_runtime(accounted_cputime);
        }

        // 唤醒可能在等待 CPU-time 时钟的线程（clock_nanosleep: PROCESS/THREAD_CPUTIME）。
        // 线程 CPU-time：仅在该线程运行时推进，因此唤醒该 PCB 的等待队列即可。
        if !pcb.cputime_wait_queue().is_empty() {
            pcb.cputime_wait_queue()
                .wakeup_all(Some(ProcessState::Blocked(true)));
        }

        // 进程 CPU-time：需要在任一线程推进时唤醒线程组组长上的等待队列。
        // 这样“主线程 sleep + 子线程 busy loop”的场景才能正确返回。
        if !pcb.is_thread_group_leader() {
            if let Some(leader) = pcb.threads_read_irqsave().group_leader() {
                if !leader.cputime_wait_queue().is_empty() {
                    leader
                        .cputime_wait_queue()
                        .wakeup_all(Some(ProcessState::Blocked(true)));
                }
            }
        }

        // 检查并处理CPU时间定时器
        let mut itimers = pcb.itimers_irqsave();
        // 处理 ITIMER_VIRTUAL (仅在用户态tick时消耗时间)
        if user_tick && itimers.virt.is_active {
            if itimers.virt.value <= accounted_cputime {
                send_signal_to_pcb(pcb.clone(), Signal::SIGVTALRM).ok();
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
                send_signal_to_pcb(pcb.clone(), Signal::SIGPROF).ok();
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
            let (total_delta, hardirq_delta, softirq_delta) =
                irqtime.irqtime_tick_accounted(max - accounted);

            // 分别记账硬中断和软中断时间
            let kcpustat = kcpustat_this_cpu();
            if hardirq_delta > 0 {
                kcpustat.account(CpuUsageStat::Irq, hardirq_delta);
            }
            if softirq_delta > 0 {
                kcpustat.account(CpuUsageStat::Softirq, softirq_delta);
            }

            accounted += total_delta;
        }

        accounted
    }

    pub fn steal_account_process_time(_max: u64) -> u64 {
        // 这里未考虑虚拟机时间窃取
        0
    }
}
