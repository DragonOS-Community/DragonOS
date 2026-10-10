use core::intrinsics::unlikely;
use core::mem::swap;
use core::sync::atomic::fence;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::libs::spinlock::SpinLock;
use crate::process::ProcessControlBlock;
use crate::process::ProcessFlags;
use crate::sched::clock::ClockUpdataFlag;
use crate::sched::fair_tree::FairTimeline;
use crate::sched::{SchedFeature, SCHED_FEATURES};
use crate::time::jiffies::TICK_NESC;
use crate::time::timer::clock;
use alloc::sync::{Arc, Weak};

use super::pelt::{sub_positive, SchedulerAvg, UpdateAvgFlags, PELT_MIN_DIVIDER};
use super::{
    CpuRunQueue, DequeueFlag, EnqueueFlag, LoadWeight, OnRq, Scheduler, TaskGroup, WakeupFlags,
    SCHED_CAPACITY_SHIFT,
};

/// 用于设置 CPU-bound 任务的最小抢占粒度的参数。
/// 默认值为 0.75 毫秒乘以（1 加上 CPU 数量的二进制对数），单位为纳秒。
/// 这个值影响到任务在 CPU-bound 情况下的抢占行为。
static SYSCTL_SHCED_MIN_GRANULARITY: AtomicU64 = AtomicU64::new(750000);
/// 规范化最小抢占粒度参数
#[allow(dead_code)]
static NORMALIZED_SYSCTL_SCHED_MIN_GRANULARITY: AtomicU64 = AtomicU64::new(750000);

static SYSCTL_SHCED_BASE_SLICE: AtomicU64 = AtomicU64::new(750000);
#[allow(dead_code)]
static NORMALIZED_SYSCTL_SHCED_BASE_SLICE: AtomicU64 = AtomicU64::new(750000);

/// 预设的调度延迟任务数量
static SCHED_NR_LATENCY: AtomicU64 = AtomicU64::new(8);

fn add_signed(value: u64, delta: isize) -> u64 {
    if delta >= 0 {
        value.saturating_add(delta as u64)
    } else {
        value.saturating_sub(delta.unsigned_abs() as u64)
    }
}

/// 调度实体单位，一个调度实体可以是一个进程、一个进程组或者是一个用户等等划分
#[derive(Debug)]
pub struct FairSchedEntity {
    /// 负载相关
    pub load: LoadWeight,
    pub deadline: u64,
    pub min_deadline: u64,

    /// 是否在运行队列中
    pub on_rq: OnRq,
    /// 当前调度实体的开始执行时间
    pub exec_start: u64,
    /// 总运行时长
    pub sum_exec_runtime: u64,
    /// 虚拟运行时间
    pub vruntime: u64,
    /// 进程的调度延迟 它等于进程的权重（weight）乘以（V - v_i），其中V是系统当前的时间，v_i是进程的运行时间
    pub vlag: i64,
    // 运行时间片
    pub slice: u64,
    /// 上一个调度实体运行总时间
    pub prev_sum_exec_runtime: u64,

    pub avg: SchedulerAvg,

    /// 父节点
    parent: Weak<FairSchedEntity>,

    pub depth: u32,

    /// 指向自身
    self_ref: Weak<FairSchedEntity>,

    /// 所在的CFS运行队列
    cfs_rq: Weak<CfsRunQueue>,

    /// group持有的私有cfs队列
    my_cfs_rq: Option<Arc<CfsRunQueue>>,

    runnable_weight: u64,

    pcb: Weak<ProcessControlBlock>,
}

impl FairSchedEntity {
    pub fn new() -> Arc<Self> {
        let ret = Arc::new(Self {
            parent: Weak::new(),
            self_ref: Weak::new(),
            pcb: Weak::new(),
            cfs_rq: Weak::new(),
            my_cfs_rq: None,
            on_rq: OnRq::None,
            slice: SYSCTL_SHCED_BASE_SLICE.load(Ordering::SeqCst),
            load: LoadWeight {
                weight: LoadWeight::NICE_0_LOAD,
                inv_weight: 0,
            },
            deadline: Default::default(),
            min_deadline: Default::default(),
            exec_start: Default::default(),
            sum_exec_runtime: Default::default(),
            vruntime: Default::default(),
            vlag: Default::default(),
            prev_sum_exec_runtime: Default::default(),
            avg: Default::default(),
            depth: Default::default(),
            runnable_weight: Default::default(),
        });

        ret.force_mut().self_ref = Arc::downgrade(&ret);

        ret
    }
}

impl FairSchedEntity {
    pub fn self_arc(&self) -> Arc<FairSchedEntity> {
        self.self_ref.upgrade().unwrap()
    }

    #[inline]
    pub fn on_rq(&self) -> bool {
        self.on_rq != OnRq::None
    }

    pub fn pcb(&self) -> Arc<ProcessControlBlock> {
        self.pcb.upgrade().unwrap()
    }

    pub fn set_pcb(&mut self, pcb: Weak<ProcessControlBlock>) {
        self.pcb = pcb
    }

    #[inline]
    pub fn cfs_rq(&self) -> Arc<CfsRunQueue> {
        self.cfs_rq.upgrade().unwrap()
    }

    pub fn set_cfs(&mut self, cfs: Weak<CfsRunQueue>) {
        self.cfs_rq = cfs;
    }

    pub(crate) fn init_group(
        &mut self,
        leaf: Arc<CfsRunQueue>,
        parent_rq: &Arc<CfsRunQueue>,
        parent: Option<&Arc<FairSchedEntity>>,
    ) {
        self.my_cfs_rq = Some(leaf);
        self.cfs_rq = Arc::downgrade(parent_rq);
        self.parent = parent.map_or_else(Weak::new, Arc::downgrade);
        self.depth = parent.map_or(0, |parent| parent.depth + 1);
    }

    pub(crate) fn bind_task(
        &mut self,
        leaf: &Arc<CfsRunQueue>,
        parent: Option<&Arc<FairSchedEntity>>,
    ) {
        debug_assert!(self.is_task());
        self.cfs_rq = Arc::downgrade(leaf);
        self.parent = parent.map_or_else(Weak::new, Arc::downgrade);
        self.depth = parent.map_or(0, |parent| parent.depth + 1);
    }

    pub fn parent(&self) -> Option<Arc<FairSchedEntity>> {
        self.parent.upgrade()
    }

    #[allow(clippy::mut_from_ref)]
    pub fn force_mut(&self) -> &mut Self {
        unsafe {
            let p = self as *const Self as usize;
            (p as *mut Self).as_mut().unwrap()
        }
    }

    /// 判断是否是进程持有的调度实体
    #[inline]
    pub fn is_task(&self) -> bool {
        self.my_cfs_rq.is_none()
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        if self.is_task() {
            // SCHED_IDLE is not a supported task policy yet. Group idle is
            // independent and represented by its parent scheduling entity.
            return false;
        }

        self.my_cfs_rq.as_ref().is_some_and(|rq| rq.is_idle())
    }

    pub fn clear_buddies(&self) {
        let mut se = self.self_arc();

        Self::for_each_in_group(&mut se, |se| {
            let binding = se.cfs_rq();
            let cfs_rq = binding.force_mut();

            if let Some(next) = cfs_rq.next.upgrade() {
                if !Arc::ptr_eq(&next, &se) {
                    return (false, true);
                }
            }
            cfs_rq.next = Weak::new();
            return (true, true);
        });
    }

    pub fn calculate_delta_fair(&self, delta: u64) -> u64 {
        if unlikely(self.load.weight != LoadWeight::NICE_0_LOAD) {
            return self
                .force_mut()
                .load
                .calculate_delta(delta, LoadWeight::NICE_0_LOAD);
        };

        delta
    }

    /// 更新组内的权重信息
    pub fn update_cfs_group(&self) {
        if self.my_cfs_rq.is_none() {
            return;
        }

        let group_cfs = self.my_cfs_rq.clone().unwrap();

        if group_cfs.throttled_count > 0 {
            return;
        }
        let shares = group_cfs.group_shares();

        if unlikely(self.load.weight != shares) {
            self.cfs_rq()
                .force_mut()
                .reweight_entity(self.self_arc(), shares);
        }
    }

    /// 遍历se组，如果返回false则需要调用的函数return，
    /// 会将se指向其顶层parent
    /// 该函数会改变se指向
    /// 参数：
    /// - se: 对应调度实体
    /// - f: 对调度实体执行操作的闭包，返回值对应(no_break,should_continue),no_break为假时，退出循环，should_continue为假时表示需要将调用者return
    ///
    /// 返回值：
    /// - bool: 是否需要调度者return
    /// - Option<Arc<FairSchedEntity>>：最终se的指向
    pub fn for_each_in_group(
        se: &mut Arc<FairSchedEntity>,
        mut f: impl FnMut(Arc<FairSchedEntity>) -> (bool, bool),
    ) -> (bool, Option<Arc<FairSchedEntity>>) {
        let mut should_continue;
        let ret;
        // 这一步是循环计算,直到根节点
        // 比如有任务组 A ，有进程B，B属于A任务组，那么B的时间分配依赖于A组的权重以及B进程自己的权重
        loop {
            let (no_break, flag) = f(se.clone());
            should_continue = flag;
            if !no_break || !should_continue {
                ret = Some(se.clone());
                break;
            }

            let parent = se.parent();
            if parent.is_none() {
                ret = None;
                break;
            }

            *se = parent.unwrap();
        }

        (should_continue, ret)
    }

    pub fn runnable(&self) -> u64 {
        if self.is_task() {
            return self.on_rq() as u64;
        } else {
            self.runnable_weight
        }
    }

    /// 更新task和其cfsrq的负载均值
    pub fn propagate_entity_load_avg(&mut self) -> bool {
        if self.is_task() {
            return false;
        }

        let binding = self.my_cfs_rq.clone().unwrap();
        let gcfs_rq = binding.force_mut();

        if gcfs_rq.propagate == 0 {
            return false;
        }

        gcfs_rq.propagate = 0;

        let binding = self.cfs_rq();
        let cfs_rq = binding.force_mut();

        cfs_rq.add_task_group_propagate(gcfs_rq.prop_runnable_sum);

        cfs_rq.update_task_group_util(self.self_arc(), gcfs_rq);
        cfs_rq.update_task_group_runnable(self.self_arc(), gcfs_rq);
        cfs_rq.update_task_group_load(self.self_arc(), gcfs_rq);

        return true;
    }

    /// 更新runnable_weight
    pub fn update_runnable(&mut self) {
        if !self.is_task() {
            self.runnable_weight = self.my_cfs_rq.clone().unwrap().h_nr_running;
        }
    }

    /// 初始化实体运行均值
    pub fn init_entity_runnable_average(&mut self) {
        self.avg = SchedulerAvg::default();

        if self.is_task() {
            self.avg.load_avg = LoadWeight::scale_load_down(self.load.weight) as usize;
        }
    }
}

/// CFS的运行队列，这个队列需确保是percpu的
#[allow(dead_code)]
#[derive(Debug)]
pub struct CfsRunQueue {
    load: LoadWeight,

    /// 全局运行的调度实体计数器，用于负载均衡
    nr_running: u64,
    /// 针对特定 CPU 核心的任务计数器
    pub h_nr_running: u64,
    /// 运行时间
    exec_clock: u64,
    /// 最少虚拟运行时间
    min_vruntime: u64,
    /// remain runtime
    pub(crate) runtime_remaining: i64,
    pub(crate) runtime_enabled: bool,

    /// 按 vruntime 排序并维护子树最小 deadline 的 EEVDF timeline。
    pub(super) entities: FairTimeline,

    /// IDLE
    idle: usize,

    idle_nr_running: u64,

    pub idle_h_nr_running: u64,

    /// 当前运行的调度实体
    current: Weak<FairSchedEntity>,
    /// 下一个调度的实体
    next: Weak<FairSchedEntity>,
    /// 最后的调度实体
    last: Weak<FairSchedEntity>,
    /// 跳过运行的调度实体
    skip: Weak<FairSchedEntity>,

    avg_load: i64,
    avg_vruntime: i64,

    last_update_time_copy: u64,

    pub avg: SchedulerAvg,

    rq: Weak<CpuRunQueue>,
    /// 拥有此队列的taskgroup
    task_group: Weak<TaskGroup>,
    tg_load_avg_contrib: u64,
    on_pelt_list: bool,

    pub throttled_clock: u64,
    pub throttled_clock_pelt: u64,
    pub throttled_clock_pelt_time: u64,
    pub throttled_pelt_idle: u64,

    pub throttled: bool,
    pub throttled_count: u64,

    pub removed: SpinLock<CfsRemoved>,

    pub propagate: isize,
    pub prop_runnable_sum: isize,
}

#[derive(Debug, Default)]
pub struct CfsRemoved {
    pub nr: u32,
    pub load_avg: usize,
    pub util_avg: usize,
    pub runnable_avg: usize,
}

impl CfsRunQueue {
    pub fn new() -> Self {
        Self {
            load: LoadWeight::default(),
            nr_running: 0,
            h_nr_running: 0,
            exec_clock: 0,
            min_vruntime: 1 << 20,
            entities: FairTimeline::default(),
            idle: 0,
            idle_nr_running: 0,
            idle_h_nr_running: 0,
            current: Weak::new(),
            next: Weak::new(),
            last: Weak::new(),
            skip: Weak::new(),
            avg_load: 0,
            avg_vruntime: 0,
            last_update_time_copy: 0,
            avg: SchedulerAvg::default(),
            rq: Weak::new(),
            task_group: Weak::new(),
            tg_load_avg_contrib: 0,
            on_pelt_list: false,
            throttled_clock: 0,
            throttled_clock_pelt: 0,
            throttled_clock_pelt_time: 0,
            throttled_pelt_idle: 0,
            throttled: false,
            throttled_count: 0,
            removed: SpinLock::new(CfsRemoved::default()),
            propagate: 0,
            prop_runnable_sum: 0,
            runtime_remaining: 0,
            runtime_enabled: false,
        }
    }

    #[inline]
    pub fn rq(&self) -> Arc<CpuRunQueue> {
        self.rq.upgrade().unwrap()
    }

    /// Number of runnable fair tasks on this runqueue, the currently running
    /// one included. Linux spells this `cfs_rq->nr_running`.
    ///
    /// This is the counter that tells a priority change whether there is any
    /// other fair task it could hand the CPU to; `h_nr_running` adds the
    /// children of a task group, which DragonOS does not implement.
    #[inline]
    pub fn nr_running(&self) -> u64 {
        self.nr_running
    }

    #[inline]
    pub fn set_rq(&mut self, rq: Weak<CpuRunQueue>) {
        self.rq = rq;
    }

    pub(crate) fn set_task_group(&mut self, group: Weak<TaskGroup>) {
        self.task_group = group;
    }

    fn register_pelt_queue(&mut self) {
        if self.on_pelt_list {
            return;
        }
        let binding = self.rq();
        let rq = binding.force_mut_locked();
        let leaf = self
            .task_group_optional()
            .map_or_else(|| rq.cfs_rq(), |group| group.cfs_rq(rq.cpu()));
        rq.register_cfs_pelt_queue(leaf);
    }

    fn pelt_is_decayed(&self) -> bool {
        self.nr_running == 0
            && self.load.weight == 0
            && self.avg.load_sum == 0
            && self.avg.load_avg == 0
            && self.avg.util_sum == 0
            && self.avg.util_avg == 0
            && self.avg.runnable_sum == 0
            && self.avg.runnable_avg == 0
    }

    fn update_task_group_load_avg(&mut self) {
        let Some(group) = self.task_group.upgrade() else {
            return;
        };
        let load = self.avg.load_avg as u64;
        let old = self.tg_load_avg_contrib;
        if load.abs_diff(old) <= old / 64 {
            return;
        }
        if load >= old {
            group.load_avg.fetch_add(load - old, Ordering::Relaxed);
        } else {
            group.load_avg.fetch_sub(old - load, Ordering::Relaxed);
        }
        self.tg_load_avg_contrib = load;
    }

    fn group_shares(&self) -> u64 {
        let group = self.task_group();
        let shares = group.shares();
        let load = LoadWeight::scale_load_down(self.load.weight).max(self.avg.load_avg as u64);
        let total = group
            .load_avg
            .load(Ordering::Relaxed)
            .saturating_sub(self.tg_load_avg_contrib)
            .saturating_add(load);
        let local = if total != 0 {
            (shares as u128 * load as u128 / total as u128) as u64
        } else {
            shares
        };
        local.clamp(2, shares)
    }

    pub(crate) fn refresh_group_entity(&mut self, se: &Arc<FairSchedEntity>) {
        self.update_load_avg(se, UpdateAvgFlags::UPDATE_TG);
        se.force_mut().update_runnable();
        se.update_cfs_group();
    }

    pub(crate) fn set_group_idle(&mut self, entity: &Arc<FairSchedEntity>, idle: bool) {
        self.idle = usize::from(idle);
        if entity.on_rq() {
            let parent = entity.cfs_rq();
            if idle {
                parent.force_mut().idle_nr_running += 1;
            } else {
                parent.force_mut().idle_nr_running -= 1;
            }
        }
        let delta = self.h_nr_running - self.idle_h_nr_running;
        let mut se = entity.clone();
        FairSchedEntity::for_each_in_group(&mut se, |se| {
            if !se.on_rq() {
                return (false, true);
            }
            let binding = se.cfs_rq();
            let parent = binding.force_mut();
            if idle {
                parent.idle_h_nr_running += delta;
            } else {
                parent.idle_h_nr_running -= delta;
            }
            (!parent.is_idle(), true)
        });
    }

    pub(crate) fn retire_group(&mut self, entity: &Arc<FairSchedEntity>) {
        // Membership can disappear before the exiting task's terminal dequeue.
        // Its stable css reference keeps this queue alive until task_dead_fair.
        if self.h_nr_running != 0 || entity.on_rq() {
            return;
        }
        if entity.avg.last_update_time != 0 {
            let parent = entity.cfs_rq();
            let parent = parent.force_mut();
            parent.update_load_avg(entity, UpdateAvgFlags::empty());
            parent.detach_entity_load_avg(entity);
            entity.force_mut().avg.last_update_time = 0;
            let mut next = entity.parent();
            while let Some(se) = next {
                se.cfs_rq().force_mut().refresh_group_entity(&se);
                next = se.parent();
            }
        }
        if let Some(group) = self.task_group.upgrade() {
            group
                .load_avg
                .fetch_sub(self.tg_load_avg_contrib, Ordering::Relaxed);
        }
        self.tg_load_avg_contrib = 0;
        let binding = self.rq();
        binding
            .force_mut_locked()
            .cfs_pelt_queues
            .retain(|entry| !core::ptr::eq(entry.as_ptr(), self));
        self.on_pelt_list = false;
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn force_mut(&self) -> &mut Self {
        unsafe {
            (self as *const Self as usize as *mut Self)
                .as_mut()
                .unwrap()
        }
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        self.idle > 0
    }

    #[inline]
    pub fn current(&self) -> Option<Arc<FairSchedEntity>> {
        self.current.upgrade()
    }

    #[inline]
    pub fn set_current(&mut self, curr: Weak<FairSchedEntity>) {
        self.current = curr
    }

    #[inline]
    pub fn next(&self) -> Option<Arc<FairSchedEntity>> {
        self.next.upgrade()
    }

    pub fn task_group(&self) -> Arc<TaskGroup> {
        self.task_group.upgrade().unwrap()
    }

    pub(crate) fn task_group_optional(&self) -> Option<Arc<TaskGroup>> {
        self.task_group.upgrade()
    }

    /// Remove this group's representation while retaining its internal tasks.
    /// Caller owns rq and has settled current execution; budget lock is free.
    pub(crate) fn throttle_bandwidth(&mut self) -> bool {
        if self.throttled {
            return true;
        }
        let Some(group) = self.task_group_optional() else {
            return false;
        };
        let binding = self.rq();
        let rq = binding.force_mut_locked();
        let cpu = rq.cpu();
        let now = crate::time::deadline::now_ns();
        if !group
            .bandwidth
            .try_throttle(cpu, &mut self.runtime_remaining, now)
        {
            return false;
        }
        self.throttled = true;
        rq.resched_current();
        if self.throttled_count == 0 {
            self.throttled_clock_pelt = rq.rq_clock_pelt();
            if self.nr_running != 0 {
                group.bandwidth.freeze_local(cpu, now);
            }
        }
        self.throttled_count += 1;
        let own_queue = self as *const Self;
        group.for_each_descendant(cpu, &mut |descendant| {
            let leaf = descendant.cfs_rq(cpu);
            if Arc::as_ptr(&leaf) == own_queue {
                return;
            }
            let leaf = leaf.force_mut();
            if leaf.throttled_count == 0 {
                leaf.throttled_clock_pelt = rq.rq_clock_pelt();
                if leaf.nr_running != 0 {
                    descendant.bandwidth.freeze_local(cpu, now);
                }
            }
            leaf.throttled_count += 1;
        });

        let tasks = self.h_nr_running;
        let mut idle_tasks = self.idle_h_nr_running;
        let mut cursor = Some(group.entity(cpu));
        while let Some(entity) = cursor {
            if !entity.on_rq() {
                return true;
            }
            let parent = entity.cfs_rq();
            let parent = parent.force_mut();
            parent.dequeue_entity(&entity, DequeueFlag::DEQUEUE_SLEEP);
            if entity.my_cfs_rq.as_ref().is_some_and(|leaf| leaf.is_idle()) {
                idle_tasks = tasks;
            }
            parent.h_nr_running -= tasks;
            parent.idle_h_nr_running -= idle_tasks;
            cursor = entity.parent();
            if parent.load.weight != 0 {
                break;
            }
        }
        while let Some(entity) = cursor {
            if !entity.on_rq() {
                return true;
            }
            let parent = entity.cfs_rq();
            let parent = parent.force_mut();
            parent.refresh_group_entity(&entity);
            if entity.my_cfs_rq.as_ref().is_some_and(|leaf| leaf.is_idle()) {
                idle_tasks = tasks;
            }
            parent.h_nr_running -= tasks;
            parent.idle_h_nr_running -= idle_tasks;
            cursor = entity.parent();
        }
        rq.sub_nr_running(tasks as usize);
        true
    }

    /// Restore a refilled group's representation, stopping at throttled parents.
    pub(crate) fn unthrottle_bandwidth(&mut self) {
        if !self.throttled {
            return;
        }
        let Some(group) = self.task_group_optional() else {
            return;
        };
        let binding = self.rq();
        let rq = binding.force_mut_locked();
        let cpu = rq.cpu();
        let now = crate::time::deadline::now_ns();
        self.throttled = false;
        group.bandwidth.mark_unthrottled(cpu, now);
        assert!(
            self.throttled_count != 0,
            "CFS inherited throttle underflow"
        );
        self.throttled_count -= 1;
        if self.throttled_count == 0 {
            self.throttled_clock_pelt_time +=
                rq.rq_clock_pelt().saturating_sub(self.throttled_clock_pelt);
            group.bandwidth.thaw_local(cpu, now);
        }
        let own_queue = self as *const Self;
        group.for_each_descendant(cpu, &mut |descendant| {
            let leaf = descendant.cfs_rq(cpu);
            if Arc::as_ptr(&leaf) == own_queue {
                return;
            }
            let leaf = leaf.force_mut();
            assert!(
                leaf.throttled_count != 0,
                "CFS inherited throttle underflow"
            );
            leaf.throttled_count -= 1;
            if leaf.throttled_count == 0 {
                leaf.throttled_clock_pelt_time +=
                    rq.rq_clock_pelt().saturating_sub(leaf.throttled_clock_pelt);
                descendant.bandwidth.thaw_local(cpu, now);
            }
        });
        if self.load.weight == 0 {
            return;
        }
        let tasks = self.h_nr_running;
        let mut idle_tasks = self.idle_h_nr_running;
        let mut cursor = Some(group.entity(cpu));
        while let Some(entity) = &cursor {
            if entity.on_rq() {
                break;
            }
            let parent = entity.cfs_rq();
            let parent = parent.force_mut();
            parent.enqueue_entity(entity, EnqueueFlag::ENQUEUE_WAKEUP);
            if entity.my_cfs_rq.as_ref().is_some_and(|leaf| leaf.is_idle()) {
                idle_tasks = tasks;
            }
            parent.h_nr_running += tasks;
            parent.idle_h_nr_running += idle_tasks;
            if parent.throttled {
                return;
            }
            cursor = entity.parent();
        }
        while let Some(entity) = cursor {
            let parent = entity.cfs_rq();
            let parent = parent.force_mut();
            parent.refresh_group_entity(&entity);
            if entity.my_cfs_rq.as_ref().is_some_and(|leaf| leaf.is_idle()) {
                idle_tasks = tasks;
            }
            parent.h_nr_running += tasks;
            parent.idle_h_nr_running += idle_tasks;
            if parent.throttled {
                return;
            }
            cursor = entity.parent();
        }
        rq.add_nr_running(tasks as usize);
        rq.resched_current();
    }

    #[allow(dead_code)]
    #[inline]
    pub const fn bandwidth_used() -> bool {
        false
    }

    /// ## 计算调度周期，基本思想是在一个周期内让每个任务都至少运行一次。
    /// 这样可以确保所有的任务都能够得到执行，而且可以避免某些任务被长时间地阻塞。
    pub fn sched_period(nr_running: u64) -> u64 {
        if unlikely(nr_running > SCHED_NR_LATENCY.load(Ordering::SeqCst)) {
            // 如果当前活跃的任务数量超过了预设的调度延迟任务数量
            // 调度周期的长度将直接设置为活跃任务数量乘以最小抢占粒度
            return nr_running * SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst);
        } else {
            // 如果活跃任务数量未超过预设的延迟任务数量，那么调度周期的长度将设置为SCHED_NR_LATENCY
            return SCHED_NR_LATENCY.load(Ordering::SeqCst);
        }
    }

    /// ## 计算调度任务的虚拟运行时间片大小
    ///
    /// vruntime = runtime / weight
    #[allow(dead_code)]
    pub fn sched_vslice(&self, entity: Arc<FairSchedEntity>) -> u64 {
        let slice = self.sched_slice(entity.clone());
        return entity.calculate_delta_fair(slice);
    }

    /// ## 计算调度任务的实际运行时间片大小
    #[allow(dead_code)]
    pub fn sched_slice(&self, mut entity: Arc<FairSchedEntity>) -> u64 {
        let mut nr_running = self.nr_running;
        if SCHED_FEATURES.contains(SchedFeature::ALT_PERIOD) {
            nr_running = self.h_nr_running;
        }

        // 计算一个调度周期的整个slice
        let mut slice = Self::sched_period(nr_running + (!entity.on_rq()) as u64);

        // 这一步是循环计算,直到根节点
        // 比如有任务组 A ，有进程B，B属于A任务组，那么B的时间分配依赖于A组的权重以及B进程自己的权重
        FairSchedEntity::for_each_in_group(&mut entity, |se| {
            if unlikely(!se.on_rq()) {
                se.cfs_rq().force_mut().load.update_load_add(se.load.weight);
            }
            slice = se
                .cfs_rq()
                .force_mut()
                .load
                .calculate_delta(slice, se.load.weight);

            (true, true)
        });

        if SCHED_FEATURES.contains(SchedFeature::BASE_SLICE) {
            // TODO: IDLE？
            let min_gran = SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst);

            slice = min_gran.max(slice)
        }

        slice
    }

    /// ## 在时间片到期时检查当前任务是否需要被抢占，
    /// 如果需要，则抢占当前任务，并确保不会由于与其他任务的“好友偏爱（buddy favours）”而重新选举为下一个运行的任务。
    pub fn check_preempt_tick(&mut self, curr: Arc<FairSchedEntity>) {
        let delta_exec = curr
            .sum_exec_runtime
            .saturating_sub(curr.prev_sum_exec_runtime);

        if delta_exec < SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst) {
            return;
        }

        if self.nr_running <= 1 {
            // rseq critical sections need a bounded preempt notification even
            // when the scheduler ultimately has no other CFS entity to select.
            if curr.is_task() && curr.pcb().rseq_state().is_registered() {
                self.rq().resched_current();
            }
            return;
        }

        let Some(next) = self.pick_eevdf_entity(Some(&curr)) else {
            return;
        };

        if !Arc::ptr_eq(&next, &curr) {
            self.rq().resched_current();
            self.clear_buddies(&curr);
        }
    }

    pub fn clear_buddies(&mut self, se: &Arc<FairSchedEntity>) {
        if let Some(next) = self.next.upgrade() {
            if Arc::ptr_eq(&next, se) {
                se.clear_buddies();
            }
        }
    }

    /// 处理调度实体的时间片到期事件
    pub fn entity_tick(&mut self, curr: Arc<FairSchedEntity>, queued: bool) {
        // 更新当前调度实体的运行时间统计信息
        self.update_current();

        self.update_load_avg(&curr, UpdateAvgFlags::UPDATE_TG);

        // 更新组调度相关
        curr.update_cfs_group();

        if queued {
            self.rq().resched_current();
            return;
        }

        self.check_preempt_tick(curr);
    }

    /// 更新当前调度实体的运行时间统计信息
    pub fn update_current(&mut self) {
        let curr = self.current();
        if unlikely(curr.is_none()) {
            return;
        }

        let now = self.rq().clock_task();
        let curr = curr.unwrap();

        fence(Ordering::SeqCst);
        if unlikely(now <= curr.exec_start) {
            // warn!(
            //     "update_current return now <= curr.exec_start now {now} execstart {}",
            //     curr.exec_start
            // );
            return;
        }

        fence(Ordering::SeqCst);
        let delta_exec = now - curr.exec_start;

        let curr = curr.force_mut();

        curr.exec_start = now;

        curr.sum_exec_runtime += delta_exec;
        if curr.is_task() {
            let task = curr.pcb();
            task.add_sum_exec_runtime(delta_exec);
            task.account_cgroup_runtime(delta_exec);
        }

        // 根据实际运行时长加权增加虚拟运行时长
        curr.vruntime += curr.calculate_delta_fair(delta_exec);
        fence(Ordering::SeqCst);
        self.update_deadline(&curr.self_arc());
        self.update_min_vruntime();

        self.account_cfs_rq_runtime(delta_exec);
    }

    /// 计算当前cfs队列的运行时间是否到期
    fn account_cfs_rq_runtime(&mut self, delta_exec: u64) {
        if !self.runtime_enabled {
            return;
        }
        self.runtime_remaining = self
            .runtime_remaining
            .saturating_sub(delta_exec.min(i64::MAX as u64) as i64);
        if self.runtime_remaining > 0 || self.throttled {
            return;
        }
        if !self
            .task_group()
            .bandwidth
            .assign_slice(&mut self.runtime_remaining)
            && self.current().is_some()
        {
            self.rq().resched_current();
        }
    }

    pub(crate) fn check_runtime(&mut self) -> bool {
        if !self.runtime_enabled {
            return true;
        }
        if self.throttled {
            return false;
        }
        if self.runtime_remaining > 0 {
            return true;
        }
        if self
            .task_group()
            .bandwidth
            .assign_slice(&mut self.runtime_remaining)
        {
            return true;
        }
        !self.throttle_bandwidth()
    }

    /// 计算deadline，如果vruntime到期会重调度
    pub fn update_deadline(&mut self, se: &Arc<FairSchedEntity>) {
        // error!("vruntime {} deadline {}", se.vruntime, se.deadline);
        if se.vruntime < se.deadline {
            return;
        }

        se.force_mut().slice = SYSCTL_SHCED_BASE_SLICE.load(Ordering::SeqCst);

        se.force_mut().deadline = se.vruntime + se.calculate_delta_fair(se.slice);

        if self.nr_running > 1 {
            self.rq().resched_current();
            self.clear_buddies(se);
        }
    }

    /// ## 更新最小虚拟运行时间
    pub fn update_min_vruntime(&mut self) {
        let curr = self.current();

        let mut vruntime = self.min_vruntime;
        let mut curr_on_rq = false;

        if let Some(curr) = curr.as_ref() {
            if curr.on_rq() {
                vruntime = curr.vruntime;
                curr_on_rq = true;
            }
        }

        // 找到最小虚拟运行时间的调度实体
        if let Some(se) = self.entities.leftmost() {
            if !curr_on_rq {
                vruntime = se.vruntime;
            } else {
                vruntime = vruntime.min(se.vruntime);
            }
        }

        self.min_vruntime = self.__update_min_vruntime(vruntime);
    }

    fn __update_min_vruntime(&mut self, vruntime: u64) -> u64 {
        let mut min_vruntime = self.min_vruntime;

        let delta = vruntime as i64 - min_vruntime as i64;
        if delta > 0 {
            self.avg_vruntime -= self.avg_load * delta;
            min_vruntime = vruntime;
        }

        return min_vruntime;
    }

    // 判断是否为当前任务
    pub fn is_curr(&self, se: &Arc<FairSchedEntity>) -> bool {
        if self.current().is_none() {
            false
        } else {
            // 判断当前和传入的se是否相等
            Arc::ptr_eq(se, self.current().as_ref().unwrap())
        }
    }

    // 修改后
    pub fn reweight_entity(&mut self, se: Arc<FairSchedEntity>, weight: u64) {
        // 判断是否为当前任务
        let is_curr = self.is_curr(&se);
        let mut avruntime = 0;

        // 如果se在队列中
        if se.on_rq() {
            // 如果是当前任务
            self.update_current();
            avruntime = self.avg_vruntime();
            if !is_curr {
                // 否则，出队
                self.inner_dequeue_entity(&se);
            }

            // 减去该权重
            self.load.update_load_sub(se.load.weight);
        }

        self.dequeue_load_avg(&se);

        if !se.on_rq() {
            se.force_mut().vlag = se.vlag * se.load.weight as i64 / weight as i64;
        } else {
            self.reweight_eevdf(&se, avruntime, weight);
        }
        se.force_mut().load.update_load_set(weight);

        // SMP
        let divider = se.avg.get_pelt_divider();
        se.force_mut().avg.load_avg = LoadWeight::scale_load_down(se.load.weight) as usize
            * se.avg.load_sum as usize
            / divider;

        self.enqueue_load_avg(se.clone());

        if se.on_rq() {
            self.load.update_load_add(se.load.weight);
            if !is_curr {
                self.inner_enqueue_entity(&se);
            }

            self.update_min_vruntime();
        }
    }

    /// 用于重新计算调度实体（sched_entity）的权重（weight）和虚拟运行时间（vruntime）
    fn reweight_eevdf(&mut self, se: &Arc<FairSchedEntity>, avg_vruntime: u64, weight: u64) {
        let old_weight = se.load.weight;
        let mut vlag;
        if avg_vruntime != se.vruntime {
            vlag = avg_vruntime as i64 - se.vruntime as i64;
            vlag = vlag * old_weight as i64 / weight as i64;
            se.force_mut().vruntime = (avg_vruntime as i64 - vlag) as u64;
        }

        let mut vslice = se.deadline as i64 - avg_vruntime as i64;
        vslice = vslice * old_weight as i64 / weight as i64;
        se.force_mut().deadline = avg_vruntime.wrapping_add(vslice as u64);
    }

    fn avg_vruntime(&self) -> u64 {
        let curr = self.current();
        let mut avg = self.avg_vruntime;
        let mut load = self.avg_load;

        if let Some(curr) = curr {
            if curr.on_rq() {
                let weight = LoadWeight::scale_load_down(curr.load.weight);
                avg += self.entity_key(&curr) * weight as i64;
                load += weight as i64;
            }
        }

        if load > 0 {
            if avg < 0 {
                avg -= load - 1;
            }

            avg /= load;
        }

        return self.min_vruntime.wrapping_add(avg as u64);
    }

    #[inline]
    pub fn entity_key(&self, se: &Arc<FairSchedEntity>) -> i64 {
        return se.vruntime as i64 - self.min_vruntime as i64;
    }

    pub fn avg_vruntime_add(&mut self, se: &Arc<FairSchedEntity>) {
        let weight = LoadWeight::scale_load_down(se.load.weight);

        let key = self.entity_key(se);

        let avg_vruntime = self.avg_vruntime + key * weight as i64;

        self.avg_vruntime = avg_vruntime;
        self.avg_load += weight as i64;
    }

    pub fn avg_vruntime_sub(&mut self, se: &Arc<FairSchedEntity>) {
        let weight = LoadWeight::scale_load_down(se.load.weight);

        let key = self.entity_key(se);

        let avg_vruntime = self.avg_vruntime - key * weight as i64;

        self.avg_vruntime = avg_vruntime;
        self.avg_load -= weight as i64;
    }

    /// 为调度实体计算初始vruntime等信息
    fn place_entity(&mut self, se: Arc<FairSchedEntity>, flags: EnqueueFlag) {
        let current_entity = self.current();
        let vruntime = self.avg_vruntime();
        let mut lag = 0;

        let se = se.force_mut();
        se.slice = SYSCTL_SHCED_BASE_SLICE.load(Ordering::SeqCst);

        let mut vslice = se.calculate_delta_fair(se.slice);

        if SCHED_FEATURES.contains(SchedFeature::PLACE_LAG) && self.nr_running > 0 {
            lag = se.vlag;

            let mut load = self.avg_load;

            if let Some(curr) = current_entity {
                if curr.on_rq() {
                    load += LoadWeight::scale_load_down(curr.load.weight) as i64;
                }
            }

            lag *= load + LoadWeight::scale_load_down(se.load.weight) as i64;

            if load == 0 {
                load = 1;
            }

            lag /= load;
        }

        se.vruntime = vruntime.wrapping_sub(lag as u64);

        if SCHED_FEATURES.contains(SchedFeature::PLACE_DEADLINE_INITIAL)
            && flags.contains(EnqueueFlag::ENQUEUE_INITIAL)
        {
            vslice /= 2;
        }

        se.deadline = se.vruntime.wrapping_add(vslice);
    }

    /// 更新负载均值
    fn update_load_avg(&mut self, se: &Arc<FairSchedEntity>, flags: UpdateAvgFlags) {
        let now = self.cfs_rq_clock_pelt();

        if se.avg.last_update_time > 0 && !flags.contains(UpdateAvgFlags::SKIP_AGE_LOAD) {
            se.force_mut().update_load_avg(self, now);
        }

        let mut _decayed = self.update_self_load_avg(now);
        _decayed |= se.force_mut().propagate_entity_load_avg() as u32;

        if se.avg.last_update_time == 0 && flags.contains(UpdateAvgFlags::DO_ATTACH) {
            self.attach_entity_load_avg(se);
        } else if flags.contains(UpdateAvgFlags::DO_DETACH) {
            self.detach_entity_load_avg(se);
        }
        if flags.contains(UpdateAvgFlags::UPDATE_TG) {
            self.update_task_group_load_avg();
        }
    }

    /// Attach an entity's PELT contribution to this CFS runqueue.
    ///
    /// The CFS rq average must be current before this method is called.
    fn attach_entity_load_avg(&mut self, se: &Arc<FairSchedEntity>) {
        self.register_pelt_queue();
        let divider = self.avg.get_pelt_divider();
        let scaled_weight = LoadWeight::scale_load_down(se.load.weight);

        let se_mut = se.force_mut();
        se_mut.avg.last_update_time = self.avg.last_update_time;
        se_mut.avg.period_contrib = self.avg.period_contrib;
        se_mut.avg.util_sum = (se_mut.avg.util_avg * divider) as u64;
        se_mut.avg.runnable_sum = (se_mut.avg.runnable_avg * divider) as u64;
        se_mut.avg.load_sum = (se_mut.avg.load_avg * divider) as u64;
        se_mut.avg.load_sum = if scaled_weight < se_mut.avg.load_sum {
            se_mut.avg.load_sum / scaled_weight
        } else {
            1
        };

        self.enqueue_load_avg(se.clone());
        self.avg.util_avg += se.avg.util_avg;
        self.avg.util_sum += se.avg.util_sum;
        self.avg.runnable_avg += se.avg.runnable_avg;
        self.avg.runnable_sum += se.avg.runnable_sum;
        self.propagate = 1;
        self.prop_runnable_sum += se.avg.load_sum as isize;
    }

    /// 将实体的负载均值与对应cfs分离
    fn detach_entity_load_avg(&mut self, se: &Arc<FairSchedEntity>) {
        self.dequeue_load_avg(se);

        sub_positive(&mut self.avg.util_avg, se.avg.util_avg);
        self.avg.util_sum = self.avg.util_sum.saturating_sub(se.avg.util_sum);
        self.avg.util_sum = self
            .avg
            .util_sum
            .max((self.avg.util_avg * PELT_MIN_DIVIDER) as u64);

        sub_positive(&mut self.avg.runnable_avg, se.avg.runnable_avg);
        self.avg.runnable_sum = self.avg.runnable_sum.saturating_sub(se.avg.runnable_sum);
        self.avg.runnable_sum = self
            .avg
            .runnable_sum
            .max((self.avg.runnable_avg * PELT_MIN_DIVIDER) as u64);

        self.propagate = 1;
        self.prop_runnable_sum -= se.avg.load_sum as isize;
    }

    fn update_self_load_avg(&mut self, now: u64) -> u32 {
        let mut removed_load = 0;
        let mut removed_util = 0;
        let mut removed_runnable = 0;

        let mut decayed = 0;

        if self.removed.lock().nr > 0 {
            let mut removed_guard = self.removed.lock();
            let divider = self.avg.get_pelt_divider();

            swap::<usize>(&mut removed_guard.util_avg, &mut removed_util);
            swap::<usize>(&mut removed_guard.load_avg, &mut removed_load);
            swap::<usize>(&mut removed_guard.runnable_avg, &mut removed_runnable);

            removed_guard.nr = 0;

            let mut r = removed_load;

            sub_positive(&mut self.avg.load_avg, r);
            self.avg.load_sum = self.avg.load_sum.saturating_sub((r * divider) as u64);

            self.avg.load_sum = self
                .avg
                .load_sum
                .max((self.avg.load_avg * PELT_MIN_DIVIDER) as u64);

            r = removed_util;
            sub_positive(&mut self.avg.util_avg, r);
            self.avg.util_sum = self.avg.util_sum.saturating_sub((r * divider) as u64);
            self.avg.util_sum = self
                .avg
                .util_sum
                .max((self.avg.util_avg * PELT_MIN_DIVIDER) as u64);

            r = removed_runnable;
            sub_positive(&mut self.avg.runnable_avg, r);
            self.avg.runnable_sum = self.avg.runnable_sum.saturating_sub((r * divider) as u64);
            self.avg.runnable_sum = self
                .avg
                .runnable_sum
                .max((self.avg.runnable_avg * PELT_MIN_DIVIDER) as u64);

            drop(removed_guard);
            self.add_task_group_propagate(
                -(removed_runnable as isize * divider as isize) >> SCHED_CAPACITY_SHIFT,
            );

            decayed = 1;
        }

        decayed |= self.__update_load_avg(now) as u32;

        self.last_update_time_copy = self.avg.last_update_time;

        return decayed;
    }

    fn __update_load_avg(&mut self, now: u64) -> bool {
        if self.avg.update_load_sum(
            now,
            LoadWeight::scale_load_down(self.load.weight),
            self.h_nr_running,
            self.current().is_some() as u32,
        ) {
            self.avg.update_load_avg(1);
            return true;
        }

        return false;
    }

    fn add_task_group_propagate(&mut self, runnable_sum: isize) {
        self.propagate = 1;
        self.prop_runnable_sum += runnable_sum;
    }

    /// 将实体加入队列
    pub fn enqueue_entity(&mut self, se: &Arc<FairSchedEntity>, flags: EnqueueFlag) {
        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        if flags.contains(EnqueueFlag::ENQUEUE_MIGRATED) {
            assert_eq!(
                se.avg.last_update_time, 0,
                "a migrated Fair entity must be detached before destination enqueue"
            );
        }

        let is_curr = self.is_curr(se);

        if is_curr {
            self.place_entity(se.clone(), flags);
        }

        self.update_current();

        self.update_load_avg(se, UpdateAvgFlags::UPDATE_TG | UpdateAvgFlags::DO_ATTACH);

        se.force_mut().update_runnable();

        se.update_cfs_group();

        if !is_curr {
            self.place_entity(se.clone(), flags);
        }

        self.account_entity_enqueue(se);

        if flags.contains(EnqueueFlag::ENQUEUE_MIGRATED) {
            se.force_mut().exec_start = 0;
        }

        if !is_curr {
            self.inner_enqueue_entity(se);
        }

        se.force_mut().on_rq = OnRq::Queued;

        if self.nr_running == 1 {
            // 只有上面加入的
            // TODO: throttle
        }
    }

    pub fn dequeue_entity(&mut self, se: &Arc<FairSchedEntity>, flags: DequeueFlag) {
        let mut action = UpdateAvgFlags::UPDATE_TG;

        if se.is_task() && *se.pcb().sched_info().on_rq.lock_irqsave() == OnRq::Migrating {
            action |= UpdateAvgFlags::DO_DETACH;
        }

        self.update_current();

        self.update_load_avg(se, action);

        se.force_mut().update_runnable();

        self.clear_buddies(se);

        self.update_entity_lag(se);

        if let Some(curr) = self.current() {
            if !Arc::ptr_eq(&curr, se) {
                self.inner_dequeue_entity(se);
            }
        } else {
            self.inner_dequeue_entity(se);
        }

        se.force_mut().on_rq = OnRq::None;

        self.account_entity_dequeue(se);

        // return_cfs_rq_runtime

        se.update_cfs_group();

        if flags & (DequeueFlag::DEQUEUE_SAVE | DequeueFlag::DEQUEUE_MOVE)
            != DequeueFlag::DEQUEUE_SAVE
        {
            self.update_min_vruntime();
        }

        if self.nr_running == 0 {
            self.update_idle_clock_pelt()
        }
    }

    /// 将前一个调度的task放回队列
    pub fn put_prev_entity(&mut self, prev: Arc<FairSchedEntity>) {
        if prev.on_rq() {
            self.update_current();
        }

        if prev.on_rq() {
            self.inner_enqueue_entity(&prev);
        }

        self.set_current(Weak::default());
    }

    /// 将下一个运行的task设置为current
    pub fn set_next_entity(&mut self, se: &Arc<FairSchedEntity>) {
        self.clear_buddies(se);

        if se.on_rq() {
            self.inner_dequeue_entity(se);
            self.update_load_avg(se, UpdateAvgFlags::UPDATE_TG);
            // Match Linux EEVDF: stash the picked deadline in vlag so
            // RUN_TO_PARITY can keep the selected entity running until it
            // becomes ineligible or receives a new slice.
            se.force_mut().vlag = se.deadline as i64;
        }

        se.force_mut().exec_start = self.rq().clock_task();
        self.set_current(Arc::downgrade(se));

        se.force_mut().prev_sum_exec_runtime = se.sum_exec_runtime;
    }

    fn update_idle_clock_pelt(&mut self) {
        let throttled = if unlikely(self.throttled_count > 0) {
            u64::MAX
        } else {
            self.throttled_clock_pelt_time
        };

        self.throttled_pelt_idle = throttled;
    }

    fn update_entity_lag(&mut self, se: &Arc<FairSchedEntity>) {
        let lag = self.avg_vruntime() as i64 - se.vruntime as i64;

        let limit = se.calculate_delta_fair((TICK_NESC as u64).max(2 * se.slice)) as i64;

        se.force_mut().vlag = if lag < -limit {
            -limit
        } else if lag > limit {
            limit
        } else {
            lag
        }
    }

    fn account_entity_enqueue(&mut self, se: &Arc<FairSchedEntity>) {
        self.register_pelt_queue();
        self.load.update_load_add(se.load.weight);

        // FairTimeline owns queued entities. Do not add a second owning task
        // list here: unlike Linux's SMP/NUMA list, it has no consumer in DragonOS.
        self.nr_running += 1;
        if self.nr_running == 1 && self.throttled_count != 0 {
            if let Some(group) = self.task_group_optional() {
                group
                    .bandwidth
                    .freeze_local(self.rq().cpu(), crate::time::deadline::now_ns());
            }
        }
        if se.is_idle() {
            self.idle_nr_running += 1;
        }
    }

    fn account_entity_dequeue(&mut self, se: &Arc<FairSchedEntity>) {
        self.load.update_load_sub(se.load.weight);

        self.nr_running -= 1;
        if self.nr_running == 0 && self.runtime_enabled {
            self.task_group()
                .bandwidth
                .return_slack(&mut self.runtime_remaining);
        }
        if self.nr_running == 0 && self.throttled_count != 0 {
            if let Some(group) = self.task_group_optional() {
                group
                    .bandwidth
                    .thaw_local(self.rq().cpu(), crate::time::deadline::now_ns());
            }
        }
        debug_assert!(
            self.nr_running < i64::MAX as u64,
            "cfs_rq nr_running underflow"
        );
        if se.is_idle() {
            self.idle_nr_running -= 1;
        }
    }

    pub fn inner_enqueue_entity(&mut self, se: &Arc<FairSchedEntity>) {
        self.avg_vruntime_add(se);
        self.entities.insert(se.clone());
    }

    fn inner_dequeue_entity(&mut self, se: &Arc<FairSchedEntity>) {
        if self.entities.remove(se).is_none() {
            panic!("dequeue entity that is not present in CFS timeline");
        }
        self.avg_vruntime_sub(se);
    }

    pub fn enqueue_load_avg(&mut self, se: Arc<FairSchedEntity>) {
        self.avg.load_avg += se.avg.load_avg;
        self.avg.load_sum += LoadWeight::scale_load_down(se.load.weight) * se.avg.load_sum;
    }

    pub fn dequeue_load_avg(&mut self, se: &Arc<FairSchedEntity>) {
        if self.avg.load_avg > se.avg.load_avg {
            self.avg.load_avg -= se.avg.load_avg;
        } else {
            self.avg.load_avg = 0;
        };

        let se_load = LoadWeight::scale_load_down(se.load.weight) * se.avg.load_sum;

        if self.avg.load_sum > se_load {
            self.avg.load_sum -= se_load;
        } else {
            self.avg.load_sum = 0;
        }

        self.avg.load_sum = self
            .avg
            .load_sum
            .max((self.avg.load_avg * PELT_MIN_DIVIDER) as u64)
    }

    pub fn update_task_group_util(&mut self, se: Arc<FairSchedEntity>, gcfs_rq: &CfsRunQueue) {
        let mut delta_sum = gcfs_rq.avg.util_avg as isize - se.avg.util_avg as isize;
        let delta_avg = delta_sum;

        if delta_avg == 0 {
            return;
        }

        let divider = self.avg.get_pelt_divider();

        let se = se.force_mut();
        se.avg.util_avg = gcfs_rq.avg.util_avg;
        let new_sum = se.avg.util_avg * divider;
        delta_sum = new_sum as isize - se.avg.util_sum as isize;

        se.avg.util_sum = new_sum as u64;

        self.avg.util_avg = add_signed(self.avg.util_avg as u64, delta_avg) as usize;
        self.avg.util_sum = add_signed(self.avg.util_sum, delta_sum);

        self.avg.util_sum = self
            .avg
            .util_sum
            .max((self.avg.util_avg * PELT_MIN_DIVIDER) as u64);
    }

    pub fn update_task_group_runnable(&mut self, se: Arc<FairSchedEntity>, gcfs_rq: &CfsRunQueue) {
        let mut delta_sum = gcfs_rq.avg.runnable_avg as isize - se.avg.runnable_avg as isize;
        let delta_avg = delta_sum;

        if delta_avg == 0 {
            return;
        }

        let divider = self.avg.get_pelt_divider();

        let se = se.force_mut();
        se.avg.runnable_avg = gcfs_rq.avg.runnable_avg;
        let new_sum = se.avg.runnable_avg as u64 * divider as u64;
        delta_sum = new_sum as isize - se.avg.runnable_sum as isize;

        se.avg.runnable_sum = new_sum;

        self.avg.runnable_avg = add_signed(self.avg.runnable_avg as u64, delta_avg) as usize;
        self.avg.runnable_sum = add_signed(self.avg.runnable_sum, delta_sum);

        self.avg.runnable_sum = self
            .avg
            .runnable_sum
            .max((self.avg.runnable_avg * PELT_MIN_DIVIDER) as u64);
    }

    pub fn update_task_group_load(&mut self, se: Arc<FairSchedEntity>, gcfs_rq: &mut CfsRunQueue) {
        let mut runnable_sum = gcfs_rq.prop_runnable_sum;

        let mut load_sum = 0;

        if runnable_sum == 0 {
            return;
        }

        gcfs_rq.prop_runnable_sum = 0;

        let divider = self.avg.get_pelt_divider();

        if runnable_sum >= 0 {
            runnable_sum += se.avg.load_sum as isize;
            runnable_sum = runnable_sum.min(divider as isize);
        } else {
            if LoadWeight::scale_load_down(gcfs_rq.load.weight) > 0 {
                load_sum = gcfs_rq.avg.load_sum / LoadWeight::scale_load_down(gcfs_rq.load.weight);
            }

            runnable_sum = se.avg.load_sum.min(load_sum) as isize;
        }

        let running_sum = se.avg.util_sum as isize >> SCHED_CAPACITY_SHIFT;
        runnable_sum = runnable_sum.max(running_sum);

        load_sum = LoadWeight::scale_load_down(se.load.weight) * runnable_sum as u64;
        let load_avg = load_sum / divider as u64;

        let delta_avg = load_avg as isize - se.avg.load_avg as isize;
        if delta_avg == 0 {
            return;
        }

        let delta_sum = load_sum as isize
            - LoadWeight::scale_load_down(se.load.weight) as isize * se.avg.load_sum as isize;

        let se = se.force_mut();
        se.avg.load_sum = runnable_sum as u64;
        se.avg.load_avg = load_avg as usize;

        self.avg.load_avg = add_signed(self.avg.load_avg as u64, delta_avg) as usize;
        self.avg.load_sum = add_signed(self.avg.load_sum, delta_sum);

        self.avg.load_sum = self
            .avg
            .load_sum
            .max((self.avg.load_avg * PELT_MIN_DIVIDER) as u64);
    }

    fn pick_eevdf_entity(
        &self,
        curr: Option<&Arc<FairSchedEntity>>,
    ) -> Option<Arc<FairSchedEntity>> {
        if SCHED_FEATURES.contains(SchedFeature::RUN_TO_PARITY) {
            if let Some(curr) = curr.filter(|se| se.on_rq() && self.entity_eligible(se)) {
                if curr.vlag == curr.deadline as i64 {
                    return Some(curr.clone());
                }
            }
        }

        self.entities
            .pick_eevdf(curr.filter(|se| se.on_rq()), |se| self.entity_eligible(se))
    }

    pub fn pick_next_entity_with_curr(
        &self,
        curr: Option<&Arc<FairSchedEntity>>,
    ) -> Option<Arc<FairSchedEntity>> {
        if SCHED_FEATURES.contains(SchedFeature::NEXT_BUDDY) {
            if let Some(next) = self.next() {
                if self.entity_eligible(&next) {
                    return Some(next);
                }
            }
        }

        let picked = self
            .pick_eevdf_entity(curr)
            .or_else(|| self.entities.leftmost())
            .or_else(|| curr.cloned().filter(|se| se.on_rq()));
        picked
    }

    /// pick下一个运行的task
    pub fn pick_next_entity(&self) -> Option<Arc<FairSchedEntity>> {
        let curr = self.current();
        self.pick_next_entity_with_curr(curr.as_ref().filter(|se| se.on_rq()))
    }

    pub fn entity_eligible(&self, se: &Arc<FairSchedEntity>) -> bool {
        let curr = self.current();
        let mut avg = self.avg_vruntime;
        let mut load = self.avg_load;

        if let Some(curr) = curr {
            if curr.on_rq() {
                let weight = LoadWeight::scale_load_down(curr.load.weight);

                avg += self.entity_key(&curr) * weight as i64;
                load += weight as i64;
            }
        }

        return avg >= self.entity_key(se) * load;
    }
}

impl Default for CfsRunQueue {
    fn default() -> Self {
        Self::new()
    }
}
impl CpuRunQueue {
    /// Register only queues which have become active or acquired blocked
    /// history. Connect the branch top-down, inserting each child immediately
    /// before its parent; the resulting traversal remains bottom-up.
    fn register_cfs_pelt_queue(&mut self, leaf: Arc<CfsRunQueue>) {
        if leaf.on_pelt_list {
            return;
        }
        let mut branch = alloc::vec::Vec::new();
        let mut cursor = leaf;
        loop {
            if cursor.on_pelt_list {
                break;
            }
            branch.push(cursor.clone());
            let Some(group) = cursor.task_group_optional() else {
                break;
            };
            cursor = group.entity(self.cpu()).cfs_rq();
        }
        for queue in branch.into_iter().rev() {
            let parent = queue
                .task_group_optional()
                .map(|group| group.entity(self.cpu()).cfs_rq());
            let index = parent
                .as_ref()
                .and_then(|parent| {
                    self.cfs_pelt_queues
                        .iter()
                        .position(|entry| entry.as_ptr() == Arc::as_ptr(parent))
                })
                .unwrap_or(self.cfs_pelt_queues.len());
            queue.force_mut().on_pelt_list = true;
            self.cfs_pelt_queues.insert(index, Arc::downgrade(&queue));
        }
    }
}

pub struct CompletelyFairScheduler;

impl CompletelyFairScheduler {
    /// Ordinary tick service, including idle CPUs. Only the owner rq is
    /// touched: blocked history must keep decaying after tasks leave a CPU.
    pub(crate) fn update_blocked_averages(rq: &mut CpuRunQueue) {
        let mut index = 0;
        let mut previous: Option<Arc<CfsRunQueue>> = None;
        while index < rq.cfs_pelt_queues.len() {
            let Some(binding) = rq.cfs_pelt_queues[index].upgrade() else {
                rq.cfs_pelt_queues.remove(index);
                continue;
            };
            let leaf = binding.force_mut();
            if leaf.throttled_count == 0 {
                leaf.update_self_load_avg(leaf.cfs_rq_clock_pelt());
                leaf.update_task_group_load_avg();
                if leaf.nr_running == 0 {
                    leaf.update_idle_clock_pelt();
                }
                if let Some(group) = leaf.task_group_optional() {
                    let entity = group.entity(rq.cpu());
                    entity
                        .cfs_rq()
                        .force_mut()
                        .update_load_avg(&entity, UpdateAvgFlags::UPDATE_TG);
                }
            }
            // In branch postorder, a still-listed direct child immediately
            // precedes its parent. Retain the parent for pending propagation.
            let child_is_listed = previous.as_ref().is_some_and(|child| {
                child
                    .task_group_optional()
                    .is_some_and(|group| Arc::ptr_eq(&group.entity(rq.cpu()).cfs_rq(), &binding))
            });
            if leaf.throttled_count == 0 && leaf.pelt_is_decayed() && !child_is_listed {
                leaf.on_pelt_list = false;
                rq.cfs_pelt_queues.remove(index);
                continue;
            }
            previous = Some(binding);
            index += 1;
        }
    }

    pub(crate) fn update_current_chain(task: &Arc<ProcessControlBlock>) {
        let mut entity = task.sched_info().sched_entity();
        FairSchedEntity::for_each_in_group(&mut entity, |entity| {
            entity.cfs_rq().force_mut().update_current();
            (true, true)
        });
    }

    pub(crate) fn task_throttled(task: &Arc<ProcessControlBlock>) -> bool {
        task.sched_info().sched_entity().cfs_rq().throttled_count > 0
    }
    fn detach_task_load_avg(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        let se = pcb.sched_info().sched_entity();
        debug_assert_eq!(se.cfs_rq().rq().cpu(), rq.cpu());
        if se.avg.last_update_time == 0 {
            return;
        }

        let cfs = se.cfs_rq();
        let cfs = cfs.force_mut();
        cfs.update_load_avg(&se, UpdateAvgFlags::empty());
        cfs.detach_entity_load_avg(&se);
        cfs.update_task_group_load_avg();
        se.force_mut().avg.last_update_time = 0;
        let mut parent = se.parent();
        while let Some(entity) = parent {
            entity.cfs_rq().force_mut().refresh_group_entity(&entity);
            parent = entity.parent();
        }
    }

    /// Remove a task's PELT contribution when it leaves the fair class.
    pub fn switched_from_fair(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        Self::detach_task_load_avg(rq, pcb);
    }

    fn attach_task_load_avg(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        let se = pcb.sched_info().sched_entity();
        debug_assert_eq!(se.cfs_rq().rq().cpu(), rq.cpu());
        let cfs = se.cfs_rq();
        let cfs = cfs.force_mut();
        // Linux enables ATTACH_AGE_LOAD by default: age a detached entity to
        // the destination rq clock before restoring its contribution.
        cfs.update_load_avg(&se, UpdateAvgFlags::empty());
        cfs.attach_entity_load_avg(&se);
        cfs.update_task_group_load_avg();
        let mut parent = se.parent();
        while let Some(entity) = parent {
            entity.cfs_rq().force_mut().refresh_group_entity(&entity);
            parent = entity.parent();
        }
    }

    /// Attach a task's PELT contribution before it enters the fair class.
    pub fn switched_to_fair(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        Self::attach_task_load_avg(rq, pcb);
    }

    /// Prepare a Fair entity before changing its CPU/CFS runqueue binding.
    ///
    /// A runnable migration has already detached under the source rq lock.
    /// Sleeping tasks remain attached on Linux, so a wakeup migration must
    /// synchronize and detach that contribution from the retained source rq.
    pub(crate) fn prepare_task_rq_migration(pcb: &Arc<ProcessControlBlock>) {
        let se = pcb.sched_info().sched_entity();
        if se.avg.last_update_time == 0 {
            return;
        }

        if *pcb.sched_info().on_rq.lock_irqsave() != OnRq::Migrating {
            let old_rq = se.cfs_rq().rq();
            let (old_rq, _old_rq_guard) = old_rq.self_lock();
            old_rq.update_rq_clock();
            Self::detach_task_load_avg(old_rq, pcb);
        }

        // A zero timestamp tells destination enqueue_entity() to attach the
        // migrated entity to its new CFS runqueue.
        se.force_mut().avg.last_update_time = 0;
    }

    /// Restore the source PELT attachment when an asynchronous stop cancels
    /// a current-task migration after source dequeue.
    pub(crate) fn cancel_task_rq_migration(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        Self::attach_task_load_avg(rq, pcb);
    }

    /// Remove the final PELT contribution after a Fair task exits and leaves
    /// its runqueue. Sleeping tasks deliberately remain attached.
    pub(crate) fn task_dead_fair(rq: &mut CpuRunQueue, pcb: &Arc<ProcessControlBlock>) {
        Self::detach_task_load_avg(rq, pcb);
        let mut cursor = pcb.sched_info().cpu_group();
        while let Some(group) = cursor {
            if group.retired.load(Ordering::Acquire) {
                group
                    .cfs_rq(rq.cpu())
                    .force_mut()
                    .retire_group(&group.entity(rq.cpu()));
            }
            cursor = group.parent();
        }
    }

    pub fn set_next_task(rq: &mut CpuRunQueue, next: Arc<ProcessControlBlock>) {
        let mut se = next.sched_info().sched_entity();
        FairSchedEntity::for_each_in_group(&mut se, |se| {
            let cfs = se.cfs_rq();
            cfs.force_mut().set_next_entity(&se);
            (true, true)
        });
        super::cfs_bandwidth::refresh_runtime_event(rq, &next);
    }

    /// 寻找到最近公共组长
    fn find_matching_se(se: &mut Arc<FairSchedEntity>, pse: &mut Arc<FairSchedEntity>) {
        let mut se_depth = se.depth;
        let mut pse_depth = pse.depth;

        while se_depth > pse_depth {
            se_depth -= 1;
            *se = se.parent().unwrap();
        }

        while pse_depth > se_depth {
            pse_depth -= 1;
            *pse = pse.parent().unwrap();
        }

        while !Arc::ptr_eq(&se.cfs_rq(), &pse.cfs_rq()) {
            *se = se.parent().unwrap();
            *pse = pse.parent().unwrap();
        }
    }
}

impl Scheduler for CompletelyFairScheduler {
    fn enqueue(
        rq: &mut CpuRunQueue,
        pcb: Arc<crate::process::ProcessControlBlock>,
        mut flags: EnqueueFlag,
    ) {
        let se = pcb.sched_info().sched_entity();
        debug_assert_eq!(se.cfs_rq().rq().cpu(), rq.cpu());
        let mut idle_h_nr_running = se.is_idle();
        let mut cursor = Some(se);
        // First attach only missing representative entities.
        while let Some(entity) = &cursor {
            if entity.on_rq() {
                break;
            }
            let binding = entity.cfs_rq();
            let cfs = binding.force_mut();
            cfs.enqueue_entity(entity, flags);
            cfs.h_nr_running += 1;
            cfs.idle_h_nr_running += idle_h_nr_running as u64;
            idle_h_nr_running |= cfs.is_idle();
            if cfs.throttled {
                return;
            }
            cursor = entity.parent();
            flags = EnqueueFlag::ENQUEUE_WAKEUP;
        }
        // Already queued ancestors receive counts/PELT, not a second insert.
        while let Some(entity) = cursor {
            let binding = entity.cfs_rq();
            let cfs = binding.force_mut();
            cfs.refresh_group_entity(&entity);
            cfs.h_nr_running += 1;
            cfs.idle_h_nr_running += idle_h_nr_running as u64;
            idle_h_nr_running |= cfs.is_idle();
            if cfs.throttled {
                return;
            }
            cursor = entity.parent();
        }
        rq.add_nr_running(1);
    }

    fn dequeue(
        rq: &mut CpuRunQueue,
        pcb: Arc<crate::process::ProcessControlBlock>,
        mut flags: DequeueFlag,
    ) {
        let se = pcb.sched_info().sched_entity();
        let mut idle_h_nr_running = se.is_idle();
        let task_sleep = flags.contains(DequeueFlag::DEQUEUE_SLEEP);
        let was_sched_idle = rq.sched_idle_rq();

        let mut cursor = Some(se);
        while let Some(entity) = cursor {
            let binding = entity.cfs_rq();
            let cfs = binding.force_mut();
            cfs.dequeue_entity(&entity, flags);
            cfs.h_nr_running -= 1;
            cfs.idle_h_nr_running -= idle_h_nr_running as u64;
            idle_h_nr_running |= cfs.is_idle();
            if cfs.throttled {
                return;
            }
            cursor = entity.parent();
            if cfs.load.weight > 0 {
                if task_sleep {
                    if let Some(parent) = &cursor {
                        if cfs.throttled_count == 0 {
                            parent.cfs_rq().force_mut().next = Arc::downgrade(parent);
                        }
                    }
                }
                break;
            }
            flags |= DequeueFlag::DEQUEUE_SLEEP;
        }
        while let Some(entity) = cursor {
            let binding = entity.cfs_rq();
            let cfs = binding.force_mut();
            cfs.refresh_group_entity(&entity);
            cfs.h_nr_running -= 1;
            cfs.idle_h_nr_running -= idle_h_nr_running as u64;
            idle_h_nr_running |= cfs.is_idle();
            if cfs.throttled {
                return;
            }
            cursor = entity.parent();
        }
        rq.sub_nr_running(1);

        if unlikely(!was_sched_idle && rq.sched_idle_rq()) {
            rq.next_balance = clock();
        }
    }

    fn yield_task(rq: &mut CpuRunQueue) {
        let curr = rq.current();
        let se = curr.sched_info().sched_entity();
        let binding = se.cfs_rq();
        let cfs_rq = binding.force_mut();

        if unlikely(rq.nr_running == 1) {
            return;
        }

        cfs_rq.clear_buddies(&se);

        rq.update_rq_clock();

        cfs_rq.update_current();

        rq.clock_updata_flags |= ClockUpdataFlag::RQCF_REQ_SKIP;

        if cfs_rq.entity_eligible(&se) {
            let se_mut = se.force_mut();
            se_mut.vruntime = se_mut.deadline;
            se_mut.deadline += se_mut.calculate_delta_fair(se_mut.slice);
            cfs_rq.update_min_vruntime();
        }
    }

    fn check_preempt_current(
        rq: &mut CpuRunQueue,
        pcb: &Arc<crate::process::ProcessControlBlock>,
        wake_flags: WakeupFlags,
    ) {
        if Self::task_throttled(pcb) {
            return;
        }
        let curr = rq.current();
        let mut se = curr.sched_info().sched_entity();
        let mut pse = pcb.sched_info().sched_entity();

        if unlikely(Arc::ptr_eq(&se, &pse)) {
            return;
        }

        // TODO:https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sched/fair.c#8160

        let _next_buddy_mark = if SCHED_FEATURES.contains(SchedFeature::NEXT_BUDDY)
            && !wake_flags.contains(WakeupFlags::WF_FORK)
        {
            FairSchedEntity::for_each_in_group(&mut pse, |se| {
                if !se.on_rq() {
                    return (false, true);
                }

                if se.is_idle() {
                    return (false, true);
                }

                se.cfs_rq().force_mut().next = Arc::downgrade(&se);

                return (true, true);
            });
            true
        } else {
            false
        };

        if curr.flags().contains(ProcessFlags::NEED_SCHEDULE) {
            return;
        }

        if !SCHED_FEATURES.contains(SchedFeature::WAKEUP_PREEMPTION) {
            return;
        }

        Self::find_matching_se(&mut se, &mut pse);

        let cse_is_idle = se.is_idle();
        let pse_is_idle = pse.is_idle();

        if cse_is_idle && !pse_is_idle {
            rq.resched_current();
            return;
        }

        if cse_is_idle != pse_is_idle {
            return;
        }

        let cfs_rq = se.cfs_rq();
        let cfs_rq = cfs_rq.force_mut();
        cfs_rq.update_current();

        if let Some(pick_se) = cfs_rq.pick_eevdf_entity(Some(&se)) {
            if Arc::ptr_eq(&pick_se, &pse) {
                rq.resched_current();
                return;
            }
        }
    }

    fn pick_task(rq: &mut CpuRunQueue) -> Option<Arc<crate::process::ProcessControlBlock>> {
        let mut cfs_rq = Some(rq.cfs_rq());
        if cfs_rq.as_ref().unwrap().nr_running == 0 {
            return None;
        }

        let mut se;
        loop {
            let cfs = cfs_rq.unwrap();
            let cfs = cfs.force_mut();
            if cfs.throttled {
                return None;
            }
            let curr = cfs.current();
            let curr = if let Some(curr) = curr {
                if curr.on_rq() {
                    cfs.update_current();
                    Some(curr)
                } else {
                    None
                }
            } else {
                None
            };

            se = cfs.pick_next_entity_with_curr(curr.as_ref());
            match se.clone() {
                Some(val) => cfs_rq = val.my_cfs_rq.clone(),
                None => {
                    break;
                }
            }

            if cfs_rq.is_none() {
                break;
            }
        }

        se.map(|se| se.pcb())
    }

    fn tick(rq: &mut CpuRunQueue, pcb: Arc<crate::process::ProcessControlBlock>, queued: bool) {
        let mut se = pcb.sched_info().sched_entity();

        FairSchedEntity::for_each_in_group(&mut se, |se| {
            let binding = se.clone();
            let binding = binding.cfs_rq();
            let cfs_rq = binding.force_mut();

            cfs_rq.entity_tick(se, queued);
            (true, true)
        });
        super::cfs_bandwidth::refresh_runtime_event(rq, &pcb);
    }

    fn task_fork(pcb: Arc<ProcessControlBlock>) {
        let se = pcb.sched_info().sched_entity();
        let cfs_rq = se.cfs_rq();
        let rq = cfs_rq.rq();

        let (rq, _guard) = rq.self_lock();

        rq.update_rq_clock();

        let cfs_rq = cfs_rq.force_mut();

        if cfs_rq.current().is_some() {
            cfs_rq.update_current();
        }

        cfs_rq.place_entity(se.clone(), EnqueueFlag::ENQUEUE_INITIAL);
    }

    fn pick_next_task(
        rq: &mut CpuRunQueue,
        _prev: Option<Arc<ProcessControlBlock>>,
    ) -> Option<Arc<ProcessControlBlock>> {
        'retry: loop {
            let mut cfs_rq = rq.cfs_rq();
            if cfs_rq.nr_running() == 0 {
                return None;
            }
            loop {
                let cfs = cfs_rq.force_mut();
                let curr = cfs.current().filter(|se| se.on_rq());
                if curr.is_some() {
                    cfs.update_current();
                }
                // Throttling changes the parent timeline. Restart at root,
                // rather than skipping all of the group's runnable siblings.
                if !cfs.check_runtime() {
                    continue 'retry;
                }
                let winner = cfs.pick_next_entity_with_curr(curr.as_ref())?;
                if winner.is_task() {
                    return Some(winner.pcb());
                }
                cfs_rq = winner.my_cfs_rq.clone().unwrap();
            }
        }
    }

    fn put_prev_task(_rq: &mut CpuRunQueue, prev: Arc<ProcessControlBlock>) {
        let mut se = prev.sched_info().sched_entity();

        FairSchedEntity::for_each_in_group(&mut se, |se| {
            let cfs = se.cfs_rq();
            cfs.force_mut().put_prev_entity(se);

            return (true, true);
        });
    }
}
