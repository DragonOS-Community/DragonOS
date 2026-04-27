//! 周期性 CPU 负载均衡的间隔与互斥控制
//! 实现 `get_sd_balance_interval` 和 `should_we_balance`，
use alloc::sync::Arc;

use crate::{
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::clocksource::HZ,
    time::timer::clock,
};

use super::{
    cpu_rq,
    load_balance::LbEnv,
    sched_domain::{SchedDomain, SchedGroup},
    topology::for_each_domain,
};

#[inline]
pub fn max_load_balance_interval() -> u64 {
    (HZ * smp_cpu_manager().present_cpus_count() as u64) / 10
}

/// CPU 空闲类型，用于负载均衡决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuIdleType {
    /// CPU 非空闲（正在运行任务）
    NotIdle,
    /// CPU 完全空闲
    Idle,
    /// CPU 刚刚进入空闲状态（占位，当前未使用）
    NewlyIdle,
}

/// 将毫秒转换为 jiffies。
/// 使用 `ms * HZ / 1000` 近似。
#[inline]
fn msecs_to_jiffies(ms: u64) -> u64 {
    ms.saturating_mul(HZ) / 1000
}

/// 计算指定调度域的负载均衡间隔（jiffies）。
/// 1. `interval = sd->balance_interval`（ms）
/// 2. 若 `busy`，`interval *= sd->busy_factor`
/// 3. `interval = msecs_to_jiffies(interval)`
/// 4. 若 `busy`，`interval -= 1`（避免不同层级的 lock-step）
/// 5. `interval = clamp(interval, 1, MAX_LOAD_BALANCE_INTERVAL)`
pub fn get_sd_balance_interval(sd: &SchedDomain, busy: bool) -> u64 {
    let mut interval = sd
        .balance_interval
        .load(core::sync::atomic::Ordering::Relaxed);

    if busy {
        interval = interval.saturating_mul(sd.busy_factor as u64);
    }

    interval = msecs_to_jiffies(interval);

    if busy {
        interval = interval.saturating_sub(1);
    }

    interval.clamp(1, max_load_balance_interval())
}

/// 判断当前 CPU 是否应该执行负载均衡。
///
/// should_we_balance() 语义，针对单层模型简化：
/// - `NewlyIdle` 时直接允许（占位，未来可扩展检查 `nr_running`）
/// - 在 `sd.groups` 的 cpumask 中找第一个空闲 CPU，若是当前 CPU 则允许
/// - 若无空闲 CPU，仅 designated balancer CPU（group 中第一个 CPU）执行均衡
pub fn should_we_balance(env: &LbEnv) -> bool {
    if env.idle == CpuIdleType::NewlyIdle {
        return true;
    }

    let Some(ref sd) = env.sd else {
        return false;
    };

    let Some(ref group) = sd.groups else {
        return false;
    };

    if !env.cpus.get(env.dst_cpu).unwrap_or(false) {
        return false;
    }

    // 在 group's cpumask 中寻找第一个空闲 CPU
    for cpu in group.cpumask.iter_cpu() {
        if is_idle_cpu(cpu) {
            return cpu == env.dst_cpu;
        }
    }

    // 无空闲 CPU，仅 designated balancer 执行均衡
    group_balance_cpu(group) == env.dst_cpu
}

/// 返回调度组中的 designated balancer CPU。
/// 取 `sg.cpumask` 中第一个被置位的 CPU
pub fn group_balance_cpu(sg: &SchedGroup) -> ProcessorId {
    sg.cpumask
        .first()
        .expect("SchedGroup cpumask must not be empty")
}

/// 判断指定 CPU 是否空闲（`nr_running == 0`）。
#[inline]
fn is_idle_cpu(cpu: ProcessorId) -> bool {
    cpu_rq(cpu.data() as usize).nr_running_lockless() == 0
}

/// 检查指定 CPU 是否只运行 SCHED_IDLE 任务。
#[inline]
fn sched_idle_cpu(cpu: ProcessorId) -> bool {
    if cpu == ProcessorId::INVALID {
        return false;
    }
    let rq = cpu_rq(cpu.data() as usize);
    rq.sched_idle_rq()
}

/// 执行负载均衡。
/// 调用 `load_balance.rs` 中的完整实现。
fn load_balance(
    cpu: ProcessorId,
    sd: &Arc<SchedDomain>,
    idle: CpuIdleType,
    continue_balancing: &mut bool,
) -> bool {
    super::load_balance::load_balance(cpu, sd, idle, continue_balancing)
}

/// 周期性负载均衡核心循环。
pub fn rebalance_domains(cpu: ProcessorId, mut idle: CpuIdleType) {
    let rq_ref = cpu_rq(cpu.data() as usize);
    let mut continue_balancing = true;
    let mut busy = idle != CpuIdleType::Idle && !sched_idle_cpu(cpu);
    let mut next_balance = u64::MAX;
    let mut update_next_balance = false;
    let jiffies = clock();

    for_each_domain(cpu, |sd| {
        if !continue_balancing {
            return;
        }

        let mut interval = get_sd_balance_interval(sd, busy);

        if jiffies >= sd.last_balance.load(core::sync::atomic::Ordering::Relaxed) + interval {
            if load_balance(cpu, sd, idle, &mut continue_balancing) {
                idle = if is_idle_cpu(cpu) {
                    CpuIdleType::Idle
                } else {
                    CpuIdleType::NotIdle
                };
                busy = idle != CpuIdleType::Idle && !sched_idle_cpu(cpu);
            }
            sd.last_balance
                .store(jiffies, core::sync::atomic::Ordering::Relaxed);
            interval = get_sd_balance_interval(sd, busy);
        }

        let candidate = sd.last_balance.load(core::sync::atomic::Ordering::Relaxed) + interval;
        if candidate < next_balance {
            next_balance = candidate;
            update_next_balance = true;
        }
    });

    if update_next_balance {
        rq_ref
            .next_balance
            .store(next_balance, core::sync::atomic::Ordering::Relaxed);
    }
}
