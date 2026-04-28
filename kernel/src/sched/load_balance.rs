//! 多核负载均衡模块
//!
//! 该模块实现了CPU之间的负载均衡，包括：
//! - 选择唤醒任务时的目标CPU（select_task_rq）
//! - 周期性负载均衡（rebalance_domains → load_balance）
//! - 任务迁移（detach_tasks / attach_tasks）

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{collections::LinkedList, sync::Arc};
use log::warn;

use crate::{
    libs::cpumask::CpuMask,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
};

use super::{
    cpu_rq,
    sched_domain::{GroupType, MigrationType, SchedDomain, SchedGroup, SdLbStats, SgLbStats},
    CpuRunQueue, EnqueueFlag, SchedPolicy, WakeupFlags, SCHED_CAPACITY_SCALE,
};

/// 任务迁移的 cache-hot 阈值（纳秒）。
const SYSCTL_SCHED_MIGRATION_COST: u64 = 500_000;

bitflags! {
    /// 负载均衡标志位
    pub struct LbfFlags: u32 {
        /// 所有候选任务都被钉住（无法迁移到 dst_cpu）  — LBF_ALL_PINNED 0x01
        const ALL_PINNED = 1 << 0;
        /// 需要中断本次遍历（已处理过多任务）        — LBF_NEED_BREAK  0x02
        const NEED_BREAK = 1 << 1;
        /// dst_cpu 被钉住，需要寻找新的目标 CPU       — LBF_DST_PINNED  0x04
        const DST_PINNED = 1 << 2;
        /// 至少有一个任务因 cpus_allowed 限制无法迁移 — LBF_SOME_PINNED 0x08
        const SOME_PINNED = 1 << 3;
        /// 主动均衡（强制迁移 cache-hot 任务）       — LBF_ACTIVE_LB   0x10
        const ACTIVE_LB = 1 << 4;
    }
}

/// ## 负载均衡是否已启用
/// 在SMP初始化完成后才启用负载均衡
static LOAD_BALANCE_ENABLED: AtomicBool = AtomicBool::new(false);

/// ## 启用负载均衡
/// 应该在SMP初始化完成后调用
pub fn enable_load_balance() {
    LOAD_BALANCE_ENABLED.store(true, Ordering::Release);
}

/// ## 检查负载均衡是否已启用
#[inline]
fn is_load_balance_enabled() -> bool {
    LOAD_BALANCE_ENABLED.load(Ordering::Acquire)
}

/// ## 负载均衡器
///
/// 目前以 ZST 形式存在，所有状态通过外部静态变量或运行队列维护。
/// 未来若需封装可迁移为真正的单例。
pub struct LoadBalancer;

impl LoadBalancer {
    /// 选择任务唤醒时的目标CPU
    ///
    /// 这个函数在任务被唤醒时调用，用于选择最适合运行该任务的CPU。
    /// 目前仅处理 cpus_allowed 掩码、WF_CURRENT_CPU 和粗略负载比较，
    /// 未实现 Linux 的 wake_affine、LLC 域扫描及 sched_domain 层级逻辑。
    pub fn select_task_rq(
        pcb: &Arc<ProcessControlBlock>,
        prev_cpu: ProcessorId,
        wake_flags: u8,
    ) -> ProcessorId {
        // 如果负载均衡未启用，保持在原CPU（与原有行为一致）
        if !is_load_balance_enabled() {
            return prev_cpu;
        }

        let current_cpu = smp_get_processor_id();
        let cpu_manager = smp_cpu_manager();
        let cpus_allowed = pcb.sched_info().cpus_allowed();

        if cpu_manager.present_cpus_count() <= 1 && cpus_allowed.get(current_cpu).unwrap_or(false) {
            return current_cpu;
        }

        let nr_cpus_allowed = pcb.sched_info().nr_cpus_allowed();
        if nr_cpus_allowed <= 1 {
            if let Some(cpu) = cpus_allowed.iter_cpu().next() {
                return cpu;
            }
            return prev_cpu;
        }

        // WF_CURRENT_CPU：如果唤醒者请求将任务放到当前 CPU，优先满足
        if wake_flags & WakeupFlags::WF_CURRENT_CPU.bits() != 0
            && cpus_allowed.get(current_cpu).unwrap_or(false)
        {
            return current_cpu;
        }

        // 如果是IDLE策略，尝试找一个空闲CPU
        if pcb.sched_info().policy() == SchedPolicy::IDLE {
            let target = Self::find_idlest_cpu_lockless(&cpus_allowed, current_cpu);
            return Self::fallback_if_not_allowed(target, prev_cpu, &cpus_allowed);
        }

        let current_rq = cpu_rq(current_cpu.data() as usize);
        let current_load = Self::get_rq_load_lockless(&current_rq);
        let current_nr = current_rq.nr_running_lockless() as u64;

        let sync = wake_flags & WakeupFlags::WF_SYNC.bits() != 0;

        // 如果有原CPU信息且在允许掩码中
        if prev_cpu != ProcessorId::INVALID
            && prev_cpu != current_cpu
            && cpus_allowed.get(prev_cpu).unwrap_or(false)
        {
            let prev_rq = cpu_rq(prev_cpu.data() as usize);
            let prev_load = Self::get_rq_load_lockless(&prev_rq);
            let prev_nr = prev_rq.nr_running_lockless();

            if current_load < prev_load {
                let target = current_cpu;
                if cpus_allowed.get(target).unwrap_or(false) {
                    return target;
                }
            }

            if prev_nr == 0 && !Self::wake_wide(pcb) {
                return prev_cpu;
            }
        }

        if sync && current_nr == 1 && cpus_allowed.get(current_cpu).unwrap_or(false) {
            return current_cpu;
        }

        // select_task_rq_fair 没有 current_nr == 0 的短路检查。
        // 保留该检查会导致从 idle 线程唤醒时永远返回 current_cpu（CPU0），
        // 因为 idle 线程本身不计入 nr_running。正确的做法是让 find_idlest_cpu_lockless
        // 或 select_idle_sibling 在所有候选 CPU 中做决策。

        let target = Self::find_idlest_cpu_lockless(&cpus_allowed, current_cpu);
        let target = Self::fallback_if_not_allowed(target, prev_cpu, &cpus_allowed);
        Self::select_idle_sibling(prev_cpu, target, &cpus_allowed)
    }

    /// 如果选中的 CPU 不在任务允许掩码中，回退到 `prev_cpu`；
    /// 若 `prev_cpu` 也不允许，则在允许掩码中任选一个。
    /// 如果 `cpus_allowed` 为空（异常情况），返回当前 CPU 并记录警告。
    #[inline]
    fn fallback_if_not_allowed(
        target: ProcessorId,
        prev_cpu: ProcessorId,
        cpus_allowed: &CpuMask,
    ) -> ProcessorId {
        if cpus_allowed.get(target).unwrap_or(false) {
            return target;
        }
        if prev_cpu != ProcessorId::INVALID && cpus_allowed.get(prev_cpu).unwrap_or(false) {
            return prev_cpu;
        }
        if let Some(cpu) = cpus_allowed.iter_cpu().next() {
            return cpu;
        }
        warn!(
            "fallback_if_not_allowed: empty cpus_allowed, target={:?}, prev_cpu={:?}, defaulting to current CPU",
            target, prev_cpu
        );
        smp_get_processor_id()
    }

    /// ## 找到负载最低的CPU（不加锁）
    /// 遍历所有候选 CPU，比较 CFS load_avg，返回 load 最低的 CPU。
    /// 不因为 nr_running == 0 提前 break，否则在遍历顺序中先遇到 idle CPU 就会漏掉后续更优的候选。
    fn find_idlest_cpu_lockless(possible_cpus: &CpuMask, fallback: ProcessorId) -> ProcessorId {
        let mut min_load = u64::MAX;
        let mut idlest_cpu = fallback;

        for cpu in possible_cpus.iter_cpu() {
            let rq = cpu_rq(cpu.data() as usize);
            let load = Self::get_rq_load_lockless(&rq);

            if load < min_load {
                min_load = load;
                idlest_cpu = cpu;
            }
        }

        idlest_cpu
    }

    /// 负载估算
    /// Linux 优先检查 sched_idle_cpu / available_idle_cpu，再比较 load_avg。
    /// 优先使用 PELT load_avg；当 PELT 为 0 时回退到 nr_running，
    /// 避免系统刚启动时 PELT 全为 0 导致无法区分 CPU 的问题。
    #[inline]
    fn get_rq_load_lockless(rq: &Arc<CpuRunQueue>) -> u64 {
        let pelt = rq.cfs_rq().load_avg_lockless() as u64;
        if pelt > 0 {
            pelt
        } else {
            rq.nr_running_lockless() as u64
        }
    }

    /// 此函数在 select_task_rq 路径中无锁调用，不能使用 current_ref()（要求 rq lock）。
    /// 使用 current_ptr_lockless()（AtomicPtr Acquire 加载）做指针比较，避免 Option<Arc>
    /// 非原子读取导致的数据竞争。
    #[inline]
    fn is_idle_cpu(cpu: ProcessorId) -> bool {
        if cpu == ProcessorId::INVALID {
            return false;
        }
        let rq = cpu_rq(cpu.data() as usize);
        let curr_ptr = rq.current_ptr_lockless();
        let idle = rq.idle();
        let is_idle_task = idle.upgrade().is_some_and(|i| Arc::as_ptr(&i) == curr_ptr);
        is_idle_task && rq.nr_running_lockless() == 0
    }

    /// `select_idle_sibling` 的高度简化实现
    /// 仅检查 nr_running == 0，未实现 LLC 域扫描、idle-core 检测及 SMT 感知。
    /// 在 `target`、`prev_cpu` 和 `cpus_allowed` 中优先寻找空闲 CPU，以利用缓存亲和性并减少任务迁移开销。
    fn select_idle_sibling(
        prev_cpu: ProcessorId,
        target: ProcessorId,
        cpus_allowed: &CpuMask,
    ) -> ProcessorId {
        if cpus_allowed.get(target).unwrap_or(false) && Self::is_idle_cpu(target) {
            return target;
        }
        if prev_cpu != ProcessorId::INVALID
            && prev_cpu != target
            && cpus_allowed.get(prev_cpu).unwrap_or(false)
            && Self::is_idle_cpu(prev_cpu)
        {
            return prev_cpu;
        }
        for cpu in cpus_allowed.iter_cpu() {
            if cpu != target && Self::is_idle_cpu(cpu) {
                return cpu;
            }
        }
        target
    }

    /// 注意：Linux 使用 `sd_llc_size`（LLC 共享的 CPU 数量）作为 factor；
    /// 当前实现使用 `present_cpus_count()` 代替，在 SMT 系统上阈值可能偏大。
    ///
    /// 当唤醒者与被唤醒者之间的唤醒链过宽时，返回 `true`，
    /// 提示调用者不要为了保持缓存亲和性而把任务留在原 CPU，
    /// 而应该打散到负载更低的 CPU 上。
    fn wake_wide(pcb: &Arc<ProcessControlBlock>) -> bool {
        let current = ProcessManager::current_pcb();
        let master = current.sched_info().wakee_flips.load(Ordering::Relaxed);
        let slave = pcb.sched_info().wakee_flips.load(Ordering::Relaxed);
        let factor = smp_cpu_manager().present_cpus_count().max(2) as usize;
        let max = master.max(slave);
        let min = master.min(slave);
        min >= factor && max >= min.saturating_mul(factor)
    }

    /// ## 检查是否需要进行负载均衡
    ///
    /// 目前仅检查负载均衡是否已启用。时间窗口判断（jiffies >= next_balance）
    /// 已在 trigger_load_balance 中完成。
    pub fn should_balance(_rq: &CpuRunQueue) -> bool {
        if !is_load_balance_enabled() {
            return false;
        }

        true
    }

    /// ## 执行负载均衡
    ///
    /// 已弃用：周期性负载均衡逻辑已迁移至 `load_balance()`，
    /// 由 `rebalance_domains()` 通过 workqueue 调用。
    #[allow(dead_code)]
    pub fn run_load_balance() {
        // 实际负载均衡逻辑在 load_balance() 中实现
    }
}

pub struct LbEnv {
    pub sd: Option<Arc<SchedDomain>>,
    pub dst_cpu: ProcessorId,
    pub src_cpu: ProcessorId,
    pub idle: super::rebalance::CpuIdleType,
    pub migration_type: MigrationType,
    pub imbalance: u64,
    pub flags: LbfFlags,
    pub tasks: LinkedList<Arc<ProcessControlBlock>>,
    pub new_dst_cpu: ProcessorId,
    pub loop_ctr: u32,
    pub loop_max: u32,
    pub loop_break: u32,
    pub cpus: CpuMask,
}

/// 更新调度组的负载均衡统计信息
///
/// 针对单层模型简化：
/// - 遍历 sg.cpumask 中的所有 CPU
/// - 累加 group_load, group_util, group_runnable, sum_h_nr_running, idle_cpus
/// - 计算 group_capacity 和 group_weight
pub fn update_sg_lb_stats(sg: &SchedGroup, sgs: &mut SgLbStats, env: &LbEnv) {
    *sgs = SgLbStats::default();

    for cpu in sg.cpumask.iter_cpu() {
        let rq = cpu_rq(cpu.data() as usize);
        let load = rq.cfs_load_avg_lockless() as u64;
        let nr_running = rq.nr_running_lockless() as u32;

        sgs.group_load += load;
        sgs.group_util += load; // 单层模型下 util 用 load 近似
        sgs.group_runnable += load;
        sgs.sum_nr_running += nr_running;
        sgs.sum_h_nr_running += nr_running; // 单层模型下 h_nr_running 用 nr_running 近似

        if nr_running == 0 {
            sgs.idle_cpus += 1;
        }
    }

    sgs.group_capacity = sg.sgc.capacity;
    sgs.group_weight = sg.cpumask.iter_cpu().count() as u32;

    sgs.group_type = group_classify(sg, sgs, env);

    if sgs.group_type == GroupType::Overloaded {
        sgs.avg_load = (sgs.group_load * SCHED_CAPACITY_SCALE) / sgs.group_capacity;
    }
}

/// 判断调度组是否过载
fn group_is_overloaded(imbalance_pct: u32, sgs: &SgLbStats) -> bool {
    if sgs.sum_nr_running <= sgs.group_weight {
        return false;
    }
    if (sgs.group_capacity * 100) < (sgs.group_util * imbalance_pct as u64) {
        return true;
    }
    if (sgs.group_capacity * imbalance_pct as u64) < (sgs.group_runnable * 100) {
        return true;
    }
    false
}

/// 判断调度组是否还有剩余容量
fn group_has_capacity(imbalance_pct: u32, sgs: &SgLbStats) -> bool {
    if sgs.sum_nr_running < sgs.group_weight {
        return true;
    }
    if (sgs.group_capacity * imbalance_pct as u64) < (sgs.group_runnable * 100) {
        return false;
    }
    if (sgs.group_capacity * 100) > (sgs.group_util * imbalance_pct as u64) {
        return true;
    }
    false
}

/// 对调度组进行分类
/// 单层模型下仅实现 HasSpare 和 Overloaded。
pub fn group_classify(_sg: &SchedGroup, sgs: &SgLbStats, env: &LbEnv) -> GroupType {
    let imbalance_pct = env.sd.as_ref().map(|sd| sd.imbalance_pct).unwrap_or(125);

    if group_is_overloaded(imbalance_pct, sgs) {
        return GroupType::Overloaded;
    }

    if !group_has_capacity(imbalance_pct, sgs) {
        return GroupType::FullyBusy;
    }

    GroupType::HasSpare
}

/// 更新调度域的负载均衡统计信息
///
/// 针对单层模型简化：
/// - 仅处理一个调度组（该组同时是 local 和 busiest）
/// - 不累加 NUMA 相关统计
pub fn update_sd_lb_stats(sd: &SchedDomain, env: &LbEnv) -> SdLbStats {
    let mut sds = SdLbStats::default();

    let Some(ref group) = sd.groups else {
        return sds;
    };

    let mut local_sgs = SgLbStats::default();
    update_sg_lb_stats(group, &mut local_sgs, env);

    sds.local = Some(group.clone());
    sds.total_load = local_sgs.group_load;
    sds.total_capacity = local_sgs.group_capacity;

    // 单层单组模型：该组同时是 local 和潜在 busiest
    if local_sgs.group_type == GroupType::Overloaded {
        sds.busiest = Some(group.clone());
        sds.busiest_stat = local_sgs.clone();
    }

    sds.local_stat = local_sgs;
    sds
}

/// 单组模型下的负载不均衡计算
///
/// 单组模型中 local == busiest（同一组），无法使用 Linux 的组间 avg_load 比较
///
/// 替代策略：比较 dst_cpu 与组内 per-CPU 平均负载。
/// 若 dst_cpu 负载低于平均值且组内存在负载更高的 CPU，则允许迁移。
fn calculate_imbalance_single_group(env: &mut LbEnv, sds: &SdLbStats) {
    let local = &sds.local_stat;
    let group_weight = local.group_weight.max(1) as u64;

    // 计算组内 per-CPU 平均负载
    let per_cpu_avg = local.group_load / group_weight;

    // dst_cpu 的负载
    let dst_rq = cpu_rq(env.dst_cpu.data() as usize);
    let dst_load = dst_rq.cfs_load_avg_lockless() as u64;

    if local.group_type == GroupType::Overloaded {
        // 组过载：使用 MigrateLoad，计算 dst_cpu 低于平均值的差额
        env.migration_type = MigrationType::Load;
        if dst_load < per_cpu_avg {
            env.imbalance = per_cpu_avg.saturating_sub(dst_load);
        } else {
            // dst_cpu 已经高于平均，无需迁移
            env.imbalance = 0;
        }
    } else {
        // 组未过载但有可用任务：迁移一个任务即可
        env.migration_type = MigrationType::Task;
        if dst_load < per_cpu_avg || env.idle != super::rebalance::CpuIdleType::NotIdle {
            env.imbalance = 1;
        } else {
            env.imbalance = 0;
        }
    }
}

/// 查找最忙的调度组
///
/// 针对单层单组模型简化：
/// - 调用 update_sd_lb_stats 统计域信息
/// - 对唯一的组进行分类
/// - 若组为 Overloaded 或本地 CPU 空闲且组内有任务，则返回该组
pub fn find_busiest_group(env: &mut LbEnv) -> Option<Arc<SchedGroup>> {
    let sd = env.sd.clone()?;
    let sds = update_sd_lb_stats(&sd, env);

    let group = sd.groups.as_ref()?.clone();
    let local = &sds.local_stat;

    if local.group_type == GroupType::Overloaded {
        calculate_imbalance_single_group(env, &sds);
        if env.imbalance > 0 {
            return Some(group);
        }
    }

    if env.idle != super::rebalance::CpuIdleType::NotIdle && local.sum_h_nr_running > 0 {
        env.migration_type = MigrationType::Task;
        env.imbalance = 1;
        return Some(group);
    }

    None
}

/// 查找最忙的运行队列
///
/// 针对单层模型简化：
/// - 遍历 group.cpumask 中的 CPU
/// - 根据 env.migration_type 选择最忙的 CPU
/// - 返回最忙 CPU 的 ProcessorId
pub fn find_busiest_queue(env: &LbEnv, group: &SchedGroup) -> Option<ProcessorId> {
    let mut busiest_cpu = ProcessorId::INVALID;
    let mut busiest_load = 0u64;
    let mut busiest_util = 0u64;
    let mut busiest_nr = 0u32;
    let mut busiest_capacity = 1u64;

    for cpu in group.cpumask.iter_cpu() {
        if cpu == env.dst_cpu {
            continue;
        }

        let rq = cpu_rq(cpu.data() as usize);
        let nr_running = rq.nr_running_lockless() as u32;
        if nr_running == 0 {
            continue;
        }

        let load = rq.cfs_load_avg_lockless() as u64;
        let util = load; // 单层模型下 util 用 load 近似
        let capacity = SCHED_CAPACITY_SCALE;

        match env.migration_type {
            MigrationType::Load => {
                if load * busiest_capacity > busiest_load * capacity {
                    busiest_load = load;
                    busiest_capacity = capacity;
                    busiest_cpu = cpu;
                }
            }
            MigrationType::Util => {
                if nr_running <= 1 {
                    continue;
                }
                if busiest_util < util {
                    busiest_util = util;
                    busiest_cpu = cpu;
                }
            }
            MigrationType::Task => {
                if busiest_nr < nr_running {
                    busiest_nr = nr_running;
                    busiest_cpu = cpu;
                }
            }
        }
    }

    if busiest_cpu == ProcessorId::INVALID {
        None
    } else {
        Some(busiest_cpu)
    }
}

/// 判断指定任务是否可以迁移到 env.dst_cpu。
///
/// 1. 检查 dst_cpu 是否在 p.cpus_allowed 中；若不在，设置 LBF_SOME_PINNED。
/// 2. 检查任务是否正在 src_cpu 上运行（task_on_cpu）；若是，跳过。
/// 3. 检查 cache-hot：比较当前运行片段（delta_exec）与 sysctl_sched_migration_cost。
///    若 sd->nr_balance_failed > cache_nice_tries（简化为 1），则忽略 cache-hot。
fn can_migrate_task(pcb: &Arc<ProcessControlBlock>, env: &mut LbEnv) -> bool {
    let info = pcb.sched_info();

    if *info.on_rq.lock_irqsave() != super::OnRq::Queued {
        return false;
    }

    if !info.cpus_allowed().get(env.dst_cpu).unwrap_or(false) {
        env.flags |= LbfFlags::SOME_PINNED;
        // 在 group 中搜索可用的替代 dst_cpu
        if env.idle != super::rebalance::CpuIdleType::NewlyIdle
            && !env.flags.contains(LbfFlags::DST_PINNED)
            && !env.flags.contains(LbfFlags::ACTIVE_LB)
        {
            let sd = env.sd.as_ref();
            if let Some(sd) = sd {
                if let Some(ref group) = sd.groups {
                    for cpu in group.cpumask.iter_cpu() {
                        if info.cpus_allowed().get(cpu).unwrap_or(false) {
                            env.flags |= LbfFlags::DST_PINNED;
                            env.new_dst_cpu = cpu;
                            break;
                        }
                    }
                }
            }
        }

        return false;
    }

    env.flags &= !LbfFlags::ALL_PINNED;

    if info.on_cpu() == Some(env.src_cpu) {
        return false;
    }

    if env.flags.contains(LbfFlags::ACTIVE_LB) {
        return true;
    }

    let se = info.sched_entity();
    let src_rq = cpu_rq(env.src_cpu.data() as usize);
    let delta_exec = src_rq.clock_task().saturating_sub(se.exec_start);

    if delta_exec < SYSCTL_SCHED_MIGRATION_COST {
        let nr_failed = env
            .sd
            .as_ref()
            .map(|sd| sd.nr_balance_failed.load(Ordering::Relaxed))
            .unwrap_or(0);
        if nr_failed <= 1 {
            return false;
        }
    }

    true
}

/// 限制单次 detach 的任务数。
const SCHED_NR_MIGRATE_BREAK: u32 = 32;

#[inline]
fn shr_bound(val: u64, shift: u32) -> u64 {
    (val >> shift).max(1)
}

/// 从 src_rq 分离任务，直到满足 env.imbalance。
///
/// 返回实际分离的任务数量。
pub fn detach_tasks(src_rq: &mut CpuRunQueue, env: &mut LbEnv) -> u32 {
    let cfs_rq_arc = src_rq.cfs_rq();
    let cfs_rq = unsafe { cfs_rq_arc.force_mut() };

    if src_rq.nr_running_lockless() <= 1 {
        env.flags &= !LbfFlags::ALL_PINNED;
        return 0;
    }

    if env.imbalance == 0 {
        return 0;
    }

    env.flags |= LbfFlags::ALL_PINNED;

    let mut detached: u32 = 0;
    let nr_balance_failed = env
        .sd
        .as_ref()
        .map(|sd| sd.nr_balance_failed.load(Ordering::Relaxed))
        .unwrap_or(0);

    let mut retry = LinkedList::new();
    while let Some(se) = src_rq.cfs_tasks.pop_back() {
        if env.idle != super::rebalance::CpuIdleType::NotIdle && src_rq.nr_running_lockless() <= 1 {
            retry.push_front(se);
            break;
        }

        env.loop_ctr += 1;
        if env.loop_ctr > env.loop_max && !env.flags.contains(LbfFlags::ALL_PINNED) {
            retry.push_front(se);
            break;
        }

        if env.loop_ctr > env.loop_break {
            env.loop_break += SCHED_NR_MIGRATE_BREAK;
            env.flags |= LbfFlags::NEED_BREAK;
            retry.push_front(se);
            break;
        }

        let pcb = match se.try_pcb() {
            Some(p) => p,
            None => {
                retry.push_front(se);
                continue;
            }
        };

        if pcb.sched_info().policy() == SchedPolicy::IDLE {
            retry.push_front(se);
            continue;
        }

        if pcb.flags().contains(ProcessFlags::KTHREAD) {
            retry.push_front(se);
            continue;
        }

        if !can_migrate_task(&pcb, env) {
            retry.push_front(se);
            continue;
        }

        match env.migration_type {
            MigrationType::Load => {
                let load = se.load.weight.max(1);
                if shr_bound(load, nr_balance_failed) > env.imbalance {
                    retry.push_front(se);
                    continue;
                }
                env.imbalance -= load;
            }
            MigrationType::Util => {
                let util = se.avg.util_avg.max(1) as u64;
                if util > env.imbalance {
                    retry.push_front(se);
                    continue;
                }
                env.imbalance -= util;
            }
            MigrationType::Task => {
                if env.imbalance == 0 {
                    retry.push_front(se);
                    continue;
                }
                env.imbalance -= 1;
            }
        }

        cfs_rq.detach_task(&pcb, src_rq);
        // set_task_cpu(p, env->dst_cpu)
        // 在 dequeue 之后、push 之前更新 task_cpu 和 cfs_rq，确保后续
        // attach_task / activate_task 使用正确的目标 CPU 上下文。
        log::trace!(
            "detach_tasks: migrating pid={:?} src_cpu={:?} -> dst_cpu={:?}",
            pcb.raw_pid(),
            env.src_cpu,
            env.dst_cpu
        );
        crate::sched::__set_task_cpu(&pcb, env.dst_cpu);
        env.tasks.push_back(pcb);
        detached += 1;

        if env.imbalance == 0 {
            break;
        }
    }

    while let Some(se) = retry.pop_front() {
        src_rq.cfs_tasks.push_front(se);
    }

    detached
}

/// 将 env.tasks 中已分离的任务附加到 dst_rq。
pub fn attach_tasks(dst_rq: &mut CpuRunQueue, env: &mut LbEnv) {
    while let Some(pcb) = env.tasks.pop_front() {
        dst_rq.activate_task(&pcb, EnqueueFlag::ENQUEUE_MIGRATED);
        dst_rq.check_preempt_currnet(&pcb, super::WakeupFlags::empty());
    }
}

/// 执行负载均衡。
///
/// DragonOS 必须同时持有两把 rq 锁（detach+attach 在同一锁域内），
/// 因为没有 TASK_ON_RQ_MIGRATING 保护机制
pub fn load_balance(
    cpu: ProcessorId,
    sd: &Arc<SchedDomain>,
    idle: super::rebalance::CpuIdleType,
    _continue_balancing: &mut bool,
) -> bool {
    let mut env = LbEnv {
        sd: Some(Arc::clone(sd)),
        dst_cpu: cpu,
        src_cpu: ProcessorId::INVALID,
        idle,
        migration_type: MigrationType::Load,
        imbalance: 0,
        flags: LbfFlags::empty(),
        tasks: LinkedList::new(),
        new_dst_cpu: ProcessorId::INVALID,
        loop_ctr: 0,
        loop_max: 0,
        loop_break: SCHED_NR_MIGRATE_BREAK,
        cpus: sd.span.clone(),
    };

    if !super::rebalance::should_we_balance(&env) {
        *_continue_balancing = false;
        return false;
    }

    let group = match find_busiest_group(&mut env) {
        Some(g) => g,
        None => {
            // out_balanced: 没找到不平衡的 group，倍增 balance_interval 退避（跳过 NEWLY_IDLE）。
            if idle != super::rebalance::CpuIdleType::NewlyIdle {
                let cur = sd.balance_interval.load(Ordering::Relaxed);
                if cur < sd.max_interval {
                    sd.balance_interval.store(cur * 2, Ordering::Relaxed);
                }
            }
            return false;
        }
    };

    let busiest_cpu = match find_busiest_queue(&env, &group) {
        Some(cpu) => cpu,
        None => {
            // out_balanced: 找到 group 但没有 busiest queue
            if idle != super::rebalance::CpuIdleType::NewlyIdle {
                let cur = sd.balance_interval.load(Ordering::Relaxed);
                if cur < sd.max_interval {
                    sd.balance_interval.store(cur * 2, Ordering::Relaxed);
                }
            }
            return false;
        }
    };

    if busiest_cpu == env.dst_cpu {
        return false;
    }

    env.src_cpu = busiest_cpu;

    env.flags |= LbfFlags::ALL_PINNED;

    let mut ld_moved: u32 = 0;

    loop {
        let (first_cpu, second_cpu) = if env.dst_cpu < busiest_cpu {
            (env.dst_cpu, busiest_cpu)
        } else {
            (busiest_cpu, env.dst_cpu)
        };

        // 短路检查：自迁移无意义，且会对同一把 rq 锁自旋死锁。
        if first_cpu == second_cpu {
            break;
        }

        let first_rq_arc = cpu_rq(first_cpu.data() as usize);
        let second_rq_arc = cpu_rq(second_cpu.data() as usize);

        let (mut first_rq, _g1) = first_rq_arc.self_lock();
        if first_cpu == env.dst_cpu {
            first_rq.update_rq_clock();
        }

        // 双锁顺序：低 CPU ID 先锁，防止 ABBA 死锁。
        // first_cpu < second_cpu 由上面保证。
        let (mut second_rq, _g2) = second_rq_arc.self_lock();
        if second_cpu == env.dst_cpu {
            second_rq.update_rq_clock();
        }

        let (src_rq, dst_rq) = if busiest_cpu == first_cpu {
            (&mut first_rq, &mut second_rq)
        } else {
            (&mut second_rq, &mut first_rq)
        };

        // loop_max 在 src_rq 锁内计算，避免 TOCTOU。
        env.loop_max = src_rq
            .nr_running_lockless()
            .min(SCHED_NR_MIGRATE_BREAK as usize) as u32;

        let cur_ld_moved = detach_tasks(src_rq, &mut env);

        if cur_ld_moved > 0 {
            attach_tasks(dst_rq, &mut env);
            ld_moved += cur_ld_moved;
        }

        drop(_g2);
        drop(_g1);

        // LBF_NEED_BREAK: DragonOS 缺少 TASK_ON_RQ_MIGRATING 保护，
        // 释放锁后重新获取会导致 TOCTOU 竞态。不再重试。
        if env.flags.contains(LbfFlags::NEED_BREAK) {
            env.flags -= LbfFlags::NEED_BREAK;
        }

        // LBF_DST_PINNED: 更换 dst_cpu 后重试
        if env.flags.contains(LbfFlags::DST_PINNED) && env.imbalance > 0 {
            env.dst_cpu = env.new_dst_cpu;
            env.flags -= LbfFlags::DST_PINNED;
            env.loop_ctr = 0;
            env.loop_break = SCHED_NR_MIGRATE_BREAK;
            continue;
        }

        break;
    }

    if ld_moved > 0 {
        sd.nr_balance_failed.store(0, Ordering::Relaxed);
        sd.balance_interval
            .store(sd.min_interval.load(Ordering::Relaxed), Ordering::Relaxed);
    } else {
        if env.idle != super::rebalance::CpuIdleType::NewlyIdle {
            sd.nr_balance_failed.fetch_add(1, Ordering::Relaxed);
        }

        // 发现不平衡但迁移失败时 reset to min_interval
        // 倍增仅在 "balanced / all_pinned / one_pinned" 路径中执行。
        sd.balance_interval
            .store(sd.min_interval.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    // out_balanced / out_all_pinned / out_one_pinned 路径:
    // 当没有找到 busiest (find_busiest_group → None) 或所有任务被 pin 时，
    // 通过倍增 balance_interval 退避。
    // DragonOS 的 find_busiest_group → None 直接 return false（不经过此处），
    // 因此这里的 ALL_PINNED 检查覆盖 "找到 busiest 但全部被 pin" 的情况。
    if ld_moved == 0
        && env.flags.contains(LbfFlags::ALL_PINNED)
        && env.idle != super::rebalance::CpuIdleType::NewlyIdle
    {
        let max_pinned = 512u64; // MAX_PINNED_INTERVAL — 对齐 Linux fair.c:10916
        let cur = sd.balance_interval.load(Ordering::Relaxed);
        if cur < max_pinned {
            sd.balance_interval.store(cur * 2, Ordering::Relaxed);
        }
    }

    ld_moved > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lb_env_creation() {
        let env = LbEnv {
            src_cpu: ProcessorId::new(0),
            dst_cpu: ProcessorId::new(1),
            imbalance: 42,
            sd: None,
            idle: super::super::rebalance::CpuIdleType::NotIdle,
            migration_type: MigrationType::Load,
            flags: LbfFlags::empty(),
            tasks: LinkedList::new(),
            new_dst_cpu: ProcessorId::INVALID,
            loop_ctr: 0,
            loop_max: 32,
            loop_break: 32,
            cpus: CpuMask::new(),
        };
        assert_eq!(env.src_cpu.data(), 0);
        assert_eq!(env.dst_cpu.data(), 1);
        assert_eq!(env.imbalance, 42);
    }
}
