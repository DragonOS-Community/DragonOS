//! SchedDomain / SchedGroup 数据结构设计
//!
//! 用于周期性 CPU 负载均衡。
//! 当前未实现 NUMA、cgroups、sched-debug 相关字段。

#![allow(dead_code)]

use alloc::sync::{Arc, Weak};

use crate::libs::cpumask::CpuMask;

// ============================================================================
// SD Flags
// ============================================================================

/// `SD_LOAD_BALANCE` - 允许在此调度域进行负载均衡
pub const SD_LOAD_BALANCE: u64 = 1 << 0;
/// `SD_BALANCE_WAKE` - 任务唤醒时进行均衡
pub const SD_BALANCE_WAKE: u64 = 1 << 1;
/// `SD_BALANCE_FORK` - fork/clone 时进行均衡
pub const SD_BALANCE_FORK: u64 = 1 << 2;
/// `SD_BALANCE_EXEC` - exec 时进行均衡
pub const SD_BALANCE_EXEC: u64 = 1 << 3;
/// `SD_WAKE_AFFINE` - 考虑将唤醒的任务放在唤醒 CPU 上
pub const SD_WAKE_AFFINE: u64 = 1 << 4;
/// `SD_SHARE_CPUCAPACITY` - 域成员共享 CPU 容量（如 SMT）
pub const SD_SHARE_CPUCAPACITY: u64 = 1 << 5;
/// `SD_SHARE_PKG_RESOURCES` - 域成员共享 CPU 封装资源（如缓存）
pub const SD_SHARE_PKG_RESOURCES: u64 = 1 << 6;
/// `SD_ASYM_CPUCAPACITY` - 域成员具有不同的 CPU 容量
pub const SD_ASYM_CPUCAPACITY: u64 = 1 << 7;
/// `SD_ASYM_CPUCAPACITY_FULL` - 域成员覆盖所有唯一的 CPU 容量值
pub const SD_ASYM_CPUCAPACITY_FULL: u64 = 1 << 8;
/// `SD_ASYM_PACKING` - 将繁忙任务放在域中更靠前的位置
pub const SD_ASYM_PACKING: u64 = 1 << 9;

// ============================================================================
// Enums
// ============================================================================

/// 描述调度组在负载均衡时的状态
/// 按拉取优先级排序，优先级最低的在前，以便直接比较 `group_type` 来选择最忙的组。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum GroupType {
    /// 组有剩余容量可运行更多任务
    #[default]
    HasSpare = 0,
    /// 组已满负荷，任务不竞争更多 CPU 周期
    FullyBusy,
    /// 任务亲和性约束阻止了负载均衡
    Imbalanced,
    /// CPU 过载，无法为所有任务提供预期周期
    Overloaded,
}

/// 迁移类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationType {
    /// `migrate_load` - 按负载迁移
    Load = 0,
    /// `migrate_util` - 按利用率迁移
    Util,
    /// `migrate_task` - 按任务迁移
    Task,
}

// ============================================================================
// Core Structures
// ============================================================================

/// 调度组容量
/// 未实现 `min_capacity` / `max_capacity` / `next_update` / `imbalance` 等字段。
#[derive(Debug, Clone)]
pub struct SchedGroupCapacity {
    /// `capacity` - 该组的 CPU 容量，SCHED_CAPACITY_SCALE 为单 CPU 最大容量
    pub capacity: u64,
    /// `capacity_orig` - 原始容量（简化表示，对应 DragonOS 设计需求）
    pub capacity_orig: u64,
}

/// 调度域中的调度组
#[derive(Debug, Clone)]
pub struct SchedGroup {
    /// `next` - 必须是循环链表
    pub next: Weak<SchedGroup>,
    /// `sgc` - 指向该组的容量信息
    pub sgc: Arc<SchedGroupCapacity>,
    /// `cpumask` - 该组覆盖的 CPU 集合
    pub cpumask: CpuMask,
}

/// 调度域
/// 未实现 NUMA、schedstats、sched-debug、cgroup 相关字段。
#[derive(Debug)]
pub struct SchedDomain {
    /// `parent` - 顶层域必须以 null 结尾
    pub parent: Option<Weak<SchedDomain>>,
    /// `child` - 底层域必须以 null 结尾
    pub child: Option<Arc<SchedDomain>>,
    /// `groups` - 该域的均衡组
    pub groups: Option<Arc<SchedGroup>>,
    /// `min_interval` - 最小均衡间隔（毫秒）
    pub min_interval: core::sync::atomic::AtomicU64,
    /// `max_interval` - 最大均衡间隔（毫秒）
    pub max_interval: u64,
    /// `balance_interval` - 初始化为 1，单位为毫秒。
    /// 成功迁移后重置为 min_interval
    pub balance_interval: core::sync::atomic::AtomicU64,
    /// `imbalance_pct` - 超过此水位线才进行均衡
    pub imbalance_pct: u32,
    /// `busy_factor` - 繁忙时按此因子减少均衡频率
    pub busy_factor: u32,
    /// `flags` - 见 SD_*
    pub flags: u64,
    /// `last_balance` - 初始化为 jiffies，单位为 jiffies
    pub last_balance: core::sync::atomic::AtomicU64,
    /// `nr_balance_failed` - 初始化为 0
    pub nr_balance_failed: core::sync::atomic::AtomicU32,
    /// `span` - 该域中所有 CPU 的跨度
    pub span: CpuMask,
}

// ============================================================================
// Load-Balance Statistics
// ============================================================================

/// 负载均衡所需的调度组统计
/// 未实现 NUMA balancing 相关字段。
#[derive(Debug, Default, Clone)]
pub struct SgLbStats {
    /// `avg_load` - 组内 CPU 的平均负载
    pub avg_load: u64,
    /// `group_load` - 组内所有 CPU 的总负载
    pub group_load: u64,
    /// `group_capacity` - 组容量
    pub group_capacity: u64,
    /// `group_util` - 组内所有 CPU 的总利用率
    pub group_util: u64,
    /// `group_runnable` - 组内所有 CPU 的总可运行时间
    pub group_runnable: u64,
    /// `sum_nr_running` - 组内运行的任务数
    pub sum_nr_running: u32,
    /// `sum_h_nr_running` - 组内运行的 CFS 任务数
    pub sum_h_nr_running: u32,
    /// `idle_cpus` - 组内空闲 CPU 数量
    pub idle_cpus: u32,
    /// `group_weight` - 组权重
    pub group_weight: u32,
    /// `group_type` - 组类型
    pub group_type: GroupType,
    /// `group_asym_packing` - 任务应迁移到首选 CPU
    pub group_asym_packing: u32,
    /// `group_smt_balance` - 繁忙 SMT 上的任务应被迁移
    pub group_smt_balance: u32,
    /// `group_misfit_task_load` - 某 CPU 上有任务超出其容量
    pub group_misfit_task_load: u64,
}

/// 负载均衡期间调度域的统计
#[derive(Debug)]
pub struct SdLbStats {
    /// `busiest` - 该域中最忙的组
    pub busiest: Option<Arc<SchedGroup>>,
    /// `local` - 该域中的本地组
    pub local: Option<Arc<SchedGroup>>,
    /// `total_load` - 该域中所有组的总负载
    pub total_load: u64,
    /// `total_capacity` - 该域中所有组的总容量
    pub total_capacity: u64,
    /// `avg_load` - 该域中所有组的平均负载
    pub avg_load: u64,
    /// `prefer_sibling` - 任务应优先进入兄弟节点
    pub prefer_sibling: u32,
    /// `busiest_stat` - 最忙组的统计信息
    pub busiest_stat: SgLbStats,
    /// `local_stat` - 本地组的统计信息
    pub local_stat: SgLbStats,
}

impl Default for SdLbStats {
    fn default() -> Self {
        Self {
            busiest: None,
            local: None,
            total_load: 0,
            total_capacity: 0,
            avg_load: 0,
            prefer_sibling: 0,
            busiest_stat: SgLbStats {
                idle_cpus: u32::MAX,
                group_type: GroupType::HasSpare,
                ..Default::default()
            },
            local_stat: SgLbStats::default(),
        }
    }
}
