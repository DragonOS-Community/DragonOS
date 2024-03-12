pub mod fair;

use core::intrinsics::{likely, unlikely};

use alloc::sync::Arc;
use bitmap::traits::BitOps;

use crate::process::ProcessControlBlock;

lazy_static! {
    pub static ref SCHED_FEATURES: SchedFeature = SchedFeature::GENTLE_FAIR_SLEEPERS
        | SchedFeature::START_DEBIT
        | SchedFeature::LAST_BUDDY
        | SchedFeature::CACHE_HOT_BUDDY
        | SchedFeature::WAKEUP_PREEMPTION
        | SchedFeature::NONTASK_CAPACITY
        | SchedFeature::TTWU_QUEUE
        | SchedFeature::SIS_UTIL
        | SchedFeature::RT_PUSH_IPI
        | SchedFeature::ALT_PERIOD
        | SchedFeature::BASE_SLICE
        | SchedFeature::UTIL_EST
        | SchedFeature::UTIL_EST_FASTUP;
}

pub trait Scheduler {
    /// ## 加入当任务进入可运行状态时调用。它将调度实体（任务）放到红黑树中，增加nr_running变量的值。
    fn enqueue(&self, pcb: Arc<ProcessControlBlock>, flags: u32);

    /// ## 当任务不再可运行时被调用，对应的调度实体被移出红黑树。它减少nr_running变量的值。
    fn dequeue(&self, pcb: Arc<ProcessControlBlock>, flags: u32);

    /// ## 主动让出cpu，这个函数的行为基本上是出队，紧接着入队
    fn yield_task(&self) -> bool;

    /// ## 检查进入可运行状态的任务能否抢占当前正在运行的任务
    fn check_preempt_currnet(&self, pcb: Arc<ProcessControlBlock>, flags: u32);

    /// ## 选择接下来最适合运行的任务
    fn pick_next_task(&self) -> Arc<ProcessControlBlock>;

    /// ## 被时间滴答函数调用，它可能导致进程切换。驱动了运行时抢占。
    fn tick(&self, pcb: Arc<ProcessControlBlock>, queued: bool);
}

pub struct LoadWeight {
    /// 负载权重
    pub weight: u64,
    /// weight的倒数，方便计算
    pub inv_weight: u32,
}

impl LoadWeight {
    /// 用于限制权重在一个合适的区域内
    pub const SCHED_FIXEDPOINT_SHIFT: u32 = 10;

    pub const WMULT_SHIFT: u32 = 32;
    pub const WMULT_CONST: u32 = !0;

    pub const NICE_0_LOAD_SHIFT: u32 = Self::SCHED_FIXEDPOINT_SHIFT + Self::SCHED_FIXEDPOINT_SHIFT;

    pub fn update_add(&mut self, inc: u64) {
        self.weight += inc;
        self.inv_weight = 0;
    }

    pub fn update_sub(&mut self, dec: u64) {
        self.weight -= dec;
        self.inv_weight = 0;
    }

    pub fn update_set(&mut self, weight: u64) {
        self.weight = weight;
        self.inv_weight = 0;
    }

    /// ## 更新负载权重的倒数
    pub fn update_inv_weight(&mut self) {
        // 已经更新
        if likely(self.inv_weight != 0) {
            return;
        }

        let w = Self::scale_load_down(self.weight);

        if unlikely(w >= Self::WMULT_CONST as u64) {
            // 高位有数据
            self.inv_weight = 1;
        } else if unlikely(w == 0) {
            // 倒数去最大
            self.inv_weight = Self::WMULT_CONST;
        } else {
            // 计算倒数
            self.inv_weight = Self::WMULT_CONST / w as u32;
        }
    }

    /// ## 计算任务的执行时间差
    ///
    /// 计算公式：(delta_exec * (weight * self.inv_weight)) >> WMULT_SHIFT
    pub fn calculate_delta(&mut self, delta_exec: u64, weight: u64) -> u64 {
        // 降低精度
        let mut fact = Self::scale_load_down(weight);

        // 记录fact高32位
        let mut fact_hi = (fact >> 32) as u32;
        // 用于恢复
        let mut shift = Self::WMULT_SHIFT;

        self.update_inv_weight();

        if unlikely(fact_hi != 0) {
            // 这里表示高32位还有数据
            // 需要计算最高位，然后继续调整fact
            let fs = 32 - fact_hi.leading_zeros();
            shift -= fs;

            // 确保高32位全为0
            fact >>= fs;
        }

        // 这里确定了fact已经在32位内
        fact = fact * self.inv_weight as u64;

        fact_hi = (fact >> 32) as u32;

        if fact_hi != 0 {
            // 这里表示高32位还有数据
            // 需要计算最高位，然后继续调整fact
            let fs = 32 - fact_hi.leading_zeros();
            shift -= fs;

            // 确保高32位全为0
            fact >>= fs;
        }

        return ((delta_exec as u128 * fact as u128) >> shift) as u64;
    }

    /// ## 将负载权重缩小到到一个小的范围中计算，相当于减小精度计算
    pub const fn scale_load_down(mut weight: u64) -> u64 {
        if weight != 0 {
            weight = weight >> Self::SCHED_FIXEDPOINT_SHIFT;

            if weight < 2 {
                weight = 2;
            }
        }
        weight
    }

    pub const fn scale_load(weight: u64) -> u64 {
        weight << Self::SCHED_FIXEDPOINT_SHIFT
    }
}

/// ## PerCpu的运行队列，其中维护了各个调度器对应的rq
pub struct CpuRunQueue {}

bitflags! {
    pub struct SchedFeature:u32 {
        /// 给予睡眠任务仅有 50% 的服务赤字。这意味着睡眠任务在被唤醒后会获得一定的服务，但不能过多地占用资源。
        const GENTLE_FAIR_SLEEPERS = 1 << 0;
        /// 将新任务排在前面，以避免已经运行的任务被饿死
        const START_DEBIT = 1 << 1;
        /// 在调度时优先选择上次唤醒的任务，因为它可能会访问之前唤醒的任务所使用的数据，从而提高缓存局部性。
        const NEXT_BUDDY = 1 << 2;
        /// 在调度时优先选择上次运行的任务，因为它可能会访问与之前运行的任务相同的数据，从而提高缓存局部性。
        const LAST_BUDDY = 1 << 3;
        /// 认为任务的伙伴（buddy）在缓存中是热点，减少缓存伙伴被迁移的可能性，从而提高缓存局部性。
        const CACHE_HOT_BUDDY = 1 << 4;
        /// 允许唤醒时抢占当前任务。
        const WAKEUP_PREEMPTION = 1 << 5;
        /// 基于任务未运行时间来减少 CPU 的容量。
        const NONTASK_CAPACITY = 1 << 6;
        /// 将远程唤醒排队到目标 CPU，并使用调度器 IPI 处理它们，以减少运行队列锁的争用。
        const TTWU_QUEUE = 1 << 7;
        /// 在唤醒时尝试限制对最后级联缓存（LLC）域的无谓扫描。
        const SIS_UTIL = 1 << 8;
        /// 在 RT（Real-Time）任务迁移时，通过发送 IPI 来减少 CPU 之间的锁竞争。
        const RT_PUSH_IPI = 1 << 9;
        /// 启用估计的 CPU 利用率功能，用于调度决策。
        const UTIL_EST = 1 << 10;
        const UTIL_EST_FASTUP = 1 << 11;
        /// 启用备选调度周期
        const ALT_PERIOD = 1 << 12;
        /// 启用基本时间片
        const BASE_SLICE = 1 << 13;
    }
}
