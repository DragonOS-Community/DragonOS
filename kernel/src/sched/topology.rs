//! 单层 sched_domain 构建
//!
//! 为所有 present CPU 构建一个 system-wide 的 SchedDomain。
//! 严格对齐 Linux 6.6 `build_sched_domains()` 语义，但大幅简化：
//! - 不解析真实 CPU 拓扑（SMT/MC/NUMA）
//! - 只有一个 system-wide domain
//! - 只有一个 SchedGroup，覆盖所有 present CPUs

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicU64};

use crate::{
    sched::{
        cpu_rq,
        sched_domain::{SchedDomain, SchedGroup, SchedGroupCapacity, SD_LOAD_BALANCE},
    },
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::timer::clock,
};

/// `SCHED_CAPACITY_SCALE` 定义在 `sched/mod.rs`，表示单 CPU 最大容量。
use super::SCHED_CAPACITY_SCALE;

/// 为每个 present CPU 构建单层 system-wide SchedDomain。
/// 简化版本：单层模型下，每个 CPU 的 domain 拥有相同的 span 和同一个 group。
pub fn build_sched_domains() {
    let cpu_manager = smp_cpu_manager();
    let present_cpus = cpu_manager.present_cpus().clone();
    let present_count = cpu_manager.present_cpus_count() as u64;

    if present_count == 0 {
        log::warn!("build_sched_domains: no present CPUs, skipping");
        return;
    }

    let total_capacity = present_count * SCHED_CAPACITY_SCALE;

    // 创建该组的容量信息
    let sgc = Arc::new(SchedGroupCapacity {
        capacity: total_capacity,
        capacity_orig: total_capacity,
    });

    // 创建唯一的 SchedGroup。
    // 单层模型下 group 链表只有一个节点，next 指向自身。
    let group = Arc::new_cyclic(|weak| SchedGroup {
        next: weak.clone(),
        sgc: sgc.clone(),
        cpumask: present_cpus.clone(),
    });

    let now = clock();

    for cpu in present_cpus.iter_cpu() {
        let sd = Arc::new(SchedDomain {
            parent: None,
            child: None,
            groups: Some(group.clone()),
            min_interval: core::sync::atomic::AtomicU64::new(1),
            max_interval: 32,
            balance_interval: core::sync::atomic::AtomicU64::new(1),
            imbalance_pct: 125,
            busy_factor: 1,
            flags: SD_LOAD_BALANCE,
            last_balance: AtomicU64::new(now),
            nr_balance_failed: AtomicU32::new(0),
            span: present_cpus.clone(),
        });

        let rq = cpu_rq(cpu.data() as usize);
        let (rq, _guard) = rq.self_lock();
        rq.set_sched_domain(Some(sd));
    }

    log::info!(
        "build_sched_domains: built single-layer domain for {} CPUs",
        present_count
    );
}

/// 获取指定 CPU 的顶层 sched_domain。
///
/// 单层模型下每个 CPU 只有一个 domain。
#[inline]
pub fn cpu_sched_domain(cpu: ProcessorId) -> Option<Arc<SchedDomain>> {
    cpu_rq(cpu.data() as usize).sched_domain()
}

/// 遍历指定 CPU 的 sched_domain 层级。
/// 单层模型下只遍历一个 domain。
pub fn for_each_domain<F>(cpu: ProcessorId, mut f: F)
where
    F: FnMut(&Arc<SchedDomain>),
{
    if let Some(sd) = cpu_sched_domain(cpu) {
        f(&sd);
    }
}
