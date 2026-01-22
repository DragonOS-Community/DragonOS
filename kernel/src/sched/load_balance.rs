//! 多核负载均衡模块
//!
//! 该模块实现了CPU之间的负载均衡，包括：
//! - 选择唤醒任务时的目标CPU
//! - 周期性负载均衡检查
//! - 任务迁移

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;

use crate::{
    libs::cpumask::CpuMask,
    process::ProcessControlBlock,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    time::timer::clock,
};

use super::{cpu_rq, CpuRunQueue, DequeueFlag, EnqueueFlag, SchedPolicy};

/// ## 负载均衡间隔（单位：jiffies），执行一次负载均衡检查
const LOAD_BALANCE_INTERVAL: u64 = 100;

/// ## 负载不均衡阈值
/// 当两个CPU的负载差距超过这个比例时，触发负载均衡
const LOAD_IMBALANCE_THRESHOLD: u64 = 25;

/// ## 上次负载均衡时间（全局）
static LAST_BALANCE_TIME: AtomicU64 = AtomicU64::new(0);

/// ## 负载均衡是否已启用
/// 在SMP初始化完成后才启用负载均衡
static LOAD_BALANCE_ENABLED: AtomicBool = AtomicBool::new(false);

/// ## 启用负载均衡
/// 应该在SMP初始化完成后调用
pub fn enable_load_balance() {
    LOAD_BALANCE_ENABLED.store(true, Ordering::SeqCst);
}

/// ## 检查负载均衡是否已启用
#[inline]
fn is_load_balance_enabled() -> bool {
    LOAD_BALANCE_ENABLED.load(Ordering::Relaxed)
}

/// ## 负载均衡器
pub struct LoadBalancer;

impl LoadBalancer {
    /// 选择任务唤醒时的目标CPU
    ///
    /// 这个函数在任务被唤醒时调用，用于选择最适合运行该任务的CPU。
    /// 选择策略：
    /// 1. 如果负载均衡未启用，保持在原CPU（不改变行为）
    /// 2. 如果当前CPU负载较低，选择当前CPU（缓存亲和性）
    /// 3. 如果原CPU负载较低，选择原CPU
    /// 4. 否则选择负载最低的CPU
    pub fn select_task_rq(
        pcb: &Arc<ProcessControlBlock>,
        prev_cpu: ProcessorId,
        _wake_flags: u8,
    ) -> ProcessorId {
        // 如果负载均衡未启用，保持在原CPU（与原有行为一致）
        if !is_load_balance_enabled() {
            return prev_cpu;
        }

        let current_cpu = smp_get_processor_id();
        let cpu_manager = smp_cpu_manager();

        let present_cpus = cpu_manager.present_cpus();

        if cpu_manager.present_cpus_count() <= 1 {
            return current_cpu;
        }

        // 如果是IDLE策略，尝试找一个空闲CPU
        if pcb.sched_info().policy() == SchedPolicy::IDLE {
            return Self::find_idlest_cpu_lockless(present_cpus, current_cpu);
        }

        let current_rq = cpu_rq(current_cpu.data() as usize);
        let current_load = Self::get_rq_load_lockless(&current_rq);

        // 如果有原CPU信息且在present_cpus中
        if prev_cpu != ProcessorId::INVALID
            && prev_cpu != current_cpu
            && present_cpus.get(prev_cpu).unwrap_or(false)
        {
            let prev_rq = cpu_rq(prev_cpu.data() as usize);
            let prev_load = Self::get_rq_load_lockless(&prev_rq);

            // 如果当前CPU负载低于原CPU，选择当前CPU
            if current_load < prev_load {
                return current_cpu;
            }

            // 如果原CPU负载不高，保持缓存亲和性
            if prev_load <= 2 {
                return prev_cpu;
            }
        }

        // 如果当前CPU负载低，直接使用当前cpu即可
        if current_load <= 1 {
            return current_cpu;
        }

        Self::find_idlest_cpu_lockless(present_cpus, current_cpu)
    }

    /// ## 找到负载最低的CPU（不加锁）
    fn find_idlest_cpu_lockless(possible_cpus: &CpuMask, fallback: ProcessorId) -> ProcessorId {
        let mut min_load = u64::MAX;
        let mut idlest_cpu = fallback;

        for cpu in possible_cpus.iter_cpu() {
            let rq = cpu_rq(cpu.data() as usize);
            let load = Self::get_rq_load_lockless(&rq);

            if load < min_load {
                min_load = load;
                idlest_cpu = cpu;

                // 如果找到完全空闲的CPU，直接返回
                if load == 0 {
                    break;
                }
            }
        }

        idlest_cpu
    }

    /// ## 获取运行队列的负载（不加锁）
    #[inline]
    fn get_rq_load_lockless(rq: &Arc<CpuRunQueue>) -> u64 {
        // 使用 nr_running_lockless 方法，不需要锁定
        // 因为这只是用于负载均衡决策的估算值
        rq.nr_running_lockless() as u64
    }

    /// ## 检查是否需要进行负载均衡
    pub fn should_balance() -> bool {
        // 如果负载均衡未启用，直接返回false
        if !is_load_balance_enabled() {
            return false;
        }

        let now = clock();
        let last = LAST_BALANCE_TIME.load(Ordering::Relaxed);

        if now.saturating_sub(last) >= LOAD_BALANCE_INTERVAL {
            // 尝试更新时间戳，避免多个CPU同时进行负载均衡
            LAST_BALANCE_TIME
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
        } else {
            false
        }
    }

    /// ## 执行负载均衡
    ///
    /// 这个函数由scheduler_tick调用，检查并执行CPU之间的负载均衡
    pub fn run_load_balance() {
        // 如果负载均衡未启用，直接返回
        if !is_load_balance_enabled() {
            return;
        }

        let cpu_manager = smp_cpu_manager();

        if cpu_manager.present_cpus_count() <= 1 {
            return;
        }

        let current_cpu = smp_get_processor_id();
        let current_rq = cpu_rq(current_cpu.data() as usize);

        // 获取当前CPU的负载（不加锁）
        let current_load = Self::get_rq_load_lockless(&current_rq);

        // 如果当前CPU负载很高，不主动拉取任务
        if current_load > 2 {
            return;
        }

        let (busiest_cpu, busiest_load) =
            Self::find_busiest_cpu_lockless(cpu_manager.present_cpus());

        // 如果没有负载不均衡，返回
        if busiest_cpu == current_cpu || busiest_load <= current_load + 1 {
            return;
        }

        // 计算负载差距
        let load_diff = busiest_load.saturating_sub(current_load);
        let avg_load = (busiest_load + current_load) / 2;

        if avg_load == 0 {
            return;
        }

        // 检查负载不均衡是否超过阈值
        let imbalance_pct = (load_diff * 100) / avg_load;
        if imbalance_pct < LOAD_IMBALANCE_THRESHOLD {
            return;
        }

        // 尝试从最忙的CPU迁移任务
        Self::migrate_tasks(busiest_cpu, current_cpu, load_diff / 2);
    }

    /// ## 找到负载最高的CPU（不加锁）
    fn find_busiest_cpu_lockless(possible_cpus: &CpuMask) -> (ProcessorId, u64) {
        let mut max_load = 0u64;
        let mut busiest_cpu = smp_get_processor_id();

        for cpu in possible_cpus.iter_cpu() {
            let rq = cpu_rq(cpu.data() as usize);
            let load = Self::get_rq_load_lockless(&rq);

            if load > max_load {
                max_load = load;
                busiest_cpu = cpu;
            }
        }

        (busiest_cpu, max_load)
    }

    /// ## 从源CPU迁移任务到目标CPU
    ///
    /// 注意：当前版本暂时禁用任务迁移功能，因为需要更复杂的 CFS 队列引用更新逻辑。
    /// 目前只启用唤醒时的 CPU 选择功能。
    #[allow(dead_code)]
    fn migrate_tasks(_src_cpu: ProcessorId, _dst_cpu: ProcessorId, _nr_migrate: u64) {
        // TODO: 实现安全的任务迁移
        // 当前暂时禁用，因为直接修改 CFS 引用会破坏调度器状态
    }

    /// ## 执行单个任务的迁移
    ///
    /// 注意：当前未使用，因为任务迁移功能暂时禁用
    #[allow(dead_code)]
    fn do_migrate_task(
        pcb: &Arc<ProcessControlBlock>,
        src_rq: &mut CpuRunQueue,
        dst_rq: &mut CpuRunQueue,
        dst_cpu: ProcessorId,
    ) {
        // 从源队列出队
        src_rq.dequeue_task(pcb.clone(), DequeueFlag::DEQUEUE_NOCLOCK);

        // 更新任务的CPU信息
        pcb.sched_info().set_on_cpu(Some(dst_cpu));

        // 注意：不要直接修改 CFS 引用，让 enqueue_task 处理
        // 因为直接修改会破坏调度器的内部状态

        // 加入目标队列
        dst_rq.enqueue_task(
            pcb.clone(),
            EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_MIGRATED,
        );
    }
}
