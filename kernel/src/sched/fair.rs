use core::intrinsics::likely;
use core::intrinsics::unlikely;
use core::mem::swap;
use core::sync::atomic::fence;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::libs::rbtree::RBTree;
use crate::libs::spinlock::SpinLock;
use crate::process::ProcessControlBlock;
use crate::process::ProcessFlags;
use crate::sched::clock::ClockUpdataFlag;
use crate::sched::{cpu_rq, SchedFeature, SCHED_FEATURES};
use crate::smp::core::smp_get_processor_id;
use crate::time::jiffies::TICK_NESC;
use crate::time::timer::clock;
use crate::time::NSEC_PER_MSEC;
use alloc::sync::{Arc, Weak};

use super::idle::IdleScheduler;
use super::pelt::{add_positive, sub_positive, SchedulerAvg, UpdateAvgFlags, PELT_MIN_DIVIDER};
use super::{
    CpuRunQueue, DequeueFlag, EnqueueFlag, LoadWeight, OnRq, SchedPolicy, Scheduler, TaskGroup,
    WakeupFlags, SCHED_CAPACITY_SHIFT,
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
            load: Default::default(),
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
        // TODO: 调度组
        true
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        if self.is_task() {
            return self.pcb().sched_info().policy() == SchedPolicy::IDLE;
        }

        return self.cfs_rq().is_idle();
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
        if unlikely(self.load.weight != LoadWeight::NICE_0_LOAD_SHIFT as u64) {
            return self
                .force_mut()
                .load
                .calculate_delta(delta, LoadWeight::NICE_0_LOAD_SHIFT as u64);
        };

        delta
    }

    /// 更新组内的权重信息
    pub fn update_cfs_group(&self) {
        if self.my_cfs_rq.is_none() {
            return;
        }

        let group_cfs = self.my_cfs_rq.clone().unwrap();

        let shares = group_cfs.task_group().shares;

        if unlikely(self.load.weight != shares) {
            // TODO: reweight
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
            return self.on_rq as u64;
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
    runtime_remaining: u64,

    /// 存放调度实体的红黑树
    pub(super) entities: RBTree<u64, Arc<FairSchedEntity>>,

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
            entities: RBTree::new(),
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
        }
    }

    #[inline]
    pub fn rq(&self) -> Arc<CpuRunQueue> {
        self.rq.upgrade().unwrap()
    }

    #[inline]
    pub fn set_rq(&mut self, rq: Weak<CpuRunQueue>) {
        self.rq = rq;
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
    #[allow(dead_code)]
    pub fn check_preempt_tick(&mut self, curr: Arc<FairSchedEntity>) {
        // 计算理想状态下该调度实体的理想运行时间
        let ideal_runtime = self.sched_slice(curr.clone());

        let delta_exec = curr.sum_exec_runtime - curr.prev_sum_exec_runtime;

        if delta_exec > ideal_runtime {
            // 表明实际运行时间长于理想运行时间
            self.rq().resched_current();

            self.clear_buddies(&curr);
            return;
        }

        if delta_exec < SYSCTL_SHCED_MIN_GRANULARITY.load(Ordering::SeqCst) {
            return;
        }

        todo!()
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

        // 根据实际运行时长加权增加虚拟运行时长
        curr.vruntime += curr.calculate_delta_fair(delta_exec);
        fence(Ordering::SeqCst);
        self.update_deadline(&curr.self_arc());
        self.update_min_vruntime();

        self.account_cfs_rq_runtime(delta_exec);
    }

    /// 计算当前cfs队列的运行时间是否到期
    fn account_cfs_rq_runtime(&mut self, delta_exec: u64) {
        if likely(self.runtime_remaining > delta_exec) {
            self.runtime_remaining -= delta_exec;
            // error!("runtime_remaining {}", self.runtime_remaining);
            return;
        }

        // warn!(
        //     "runtime_remaining {} delta exec {delta_exec} nr_running {}",
        //     self.runtime_remaining,
        //     self.nr_running
        // );
        // fixme: 目前只是简单分配一个时间片
        self.runtime_remaining = 5000 * NSEC_PER_MSEC as u64;

        if likely(self.current().is_some()) && self.nr_running > 1 {
            // error!("account_cfs_rq_runtime");
            self.rq().resched_current();
        }
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

        if curr.is_some() {
            let curr = curr.as_ref().unwrap();
            if curr.on_rq() {
                vruntime = curr.vruntime;
            } else {
                self.set_current(Weak::default());
            }
        }

        // 找到最小虚拟运行时间的调度实体
        let leftmost = self.entities.get_first();
        if let Some(leftmost) = leftmost {
            let se = leftmost.1;

            if curr.is_none() {
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

        // 如果se在队列中
        if se.on_rq() {
            // 如果是当前任务
            if is_curr {
                self.update_current();
            } else {
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
            self.reweight_eevdf(&se, weight);
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
    fn reweight_eevdf(&mut self, se: &Arc<FairSchedEntity>, weight: u64) {
        let old_weight = se.load.weight;
        let avg_vruntime = self.avg_vruntime();
        let mut vlag;
        if avg_vruntime != se.vruntime {
            vlag = avg_vruntime as i64 - se.vruntime as i64;
            vlag = vlag * old_weight as i64 / weight as i64;
            se.force_mut().vruntime = (avg_vruntime as i64 - vlag) as u64;
        }

        let mut vslice = se.deadline as i64 - avg_vruntime as i64;
        vslice = vslice * old_weight as i64 / weight as i64;
        se.force_mut().deadline = avg_vruntime + vslice as u64;
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

        return self.min_vruntime + avg as u64;
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
        let vruntime = self.avg_vruntime();
        let mut lag = 0;

        let se = se.force_mut();
        se.slice = SYSCTL_SHCED_BASE_SLICE.load(Ordering::SeqCst);

        let mut vslice = se.calculate_delta_fair(se.slice);

        if self.nr_running > 0 {
            let curr = self.current();

            lag = se.vlag;

            let mut load = self.avg_load;

            if let Some(curr) = curr {
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

        se.vruntime = vruntime - lag as u64;

        if flags.contains(EnqueueFlag::ENQUEUE_INITIAL) {
            vslice /= 2;
        }

        se.deadline = se.vruntime + vslice;
    }

    /// 更新负载均值
    fn update_load_avg(&mut self, se: &Arc<FairSchedEntity>, flags: UpdateAvgFlags) {
        let now = self.cfs_rq_clock_pelt();

        if se.avg.last_update_time > 0 && !flags.contains(UpdateAvgFlags::SKIP_AGE_LOAD) {
            se.force_mut().update_load_avg(self, now);
        }

        let mut decayed = self.update_self_load_avg(now);
        decayed |= se.force_mut().propagate_entity_load_avg() as u32;

        if se.avg.last_update_time > 0 && flags.contains(UpdateAvgFlags::DO_ATTACH) {
            todo!()
        } else if flags.contains(UpdateAvgFlags::DO_ATTACH) {
            self.detach_entity_load_avg(se);
        } else if decayed > 0 {
            // cfs_rq_util_change

            todo!()
        }
    }

    /// 将实体的负载均值与对应cfs分离
    fn detach_entity_load_avg(&mut self, se: &Arc<FairSchedEntity>) {
        self.dequeue_load_avg(se);

        sub_positive(&mut self.avg.util_avg, se.avg.util_avg);
        sub_positive(&mut (self.avg.util_sum as usize), se.avg.util_sum as usize);
        self.avg.util_sum = self
            .avg
            .util_sum
            .max((self.avg.util_avg * PELT_MIN_DIVIDER) as u64);

        sub_positive(&mut self.avg.runnable_avg, se.avg.runnable_avg);
        sub_positive(
            &mut (self.avg.runnable_sum as usize),
            se.avg.runnable_sum as usize,
        );
        self.avg.runnable_sum = self
            .avg
            .runnable_sum
            .max((self.avg.runnable_avg * PELT_MIN_DIVIDER) as u64);

        self.propagate = 1;
        self.prop_runnable_sum += se.avg.load_sum as isize;
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
            sub_positive(&mut (self.avg.load_sum as usize), r * divider);

            self.avg.load_sum = self
                .avg
                .load_sum
                .max((self.avg.load_avg * PELT_MIN_DIVIDER) as u64);

            r = removed_util;
            sub_positive(&mut self.avg.util_avg, r);
            sub_positive(&mut (self.avg.util_sum as usize), r * divider);
            self.avg.util_sum = self
                .avg
                .util_sum
                .max((self.avg.util_avg * PELT_MIN_DIVIDER) as u64);

            r = removed_runnable;
            sub_positive(&mut self.avg.runnable_avg, r);
            sub_positive(&mut (self.avg.runnable_sum as usize), r * divider);
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
            LoadWeight::scale_load_down(self.load.weight) as u32,
            self.h_nr_running as u32,
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

        if se.is_task() && se.on_rq == OnRq::Migrating {
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
            se.force_mut().vlag = se.deadline as i64;
        }

        self.set_current(Arc::downgrade(se));

        se.force_mut().prev_sum_exec_runtime = se.sum_exec_runtime;
    }

    fn update_idle_clock_pelt(&mut self) {
        let throttled = if unlikely(self.throttled_count > 0) {
            u64::MAX
        } else {
            self.throttled_clock_pelt_time
        };

        self.throttled_clock_pelt = throttled;
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
        self.load.update_load_add(se.load.weight);

        if se.is_task() {
            let rq = self.rq();
            let (rq, _guard) = rq.self_lock();
            // TODO:numa
            rq.cfs_tasks.push_back(se.clone());
        }
        self.nr_running += 1;
        if se.is_idle() {
            self.idle_nr_running += 1;
        }
    }

    fn account_entity_dequeue(&mut self, se: &Arc<FairSchedEntity>) {
        self.load.update_load_sub(se.load.weight);

        if se.is_task() {
            let rq = self.rq();
            let (rq, _guard) = rq.self_lock();

            // TODO:numa
            let _ = rq.cfs_tasks.extract_if(|x| Arc::ptr_eq(x, se));
        }

        self.nr_running -= 1;
        if se.is_idle() {
            self.idle_nr_running -= 1;
        }
    }

    pub fn inner_enqueue_entity(&mut self, se: &Arc<FairSchedEntity>) {
        self.avg_vruntime_add(se);
        se.force_mut().min_deadline = se.deadline;
        self.entities.insert(se.vruntime, se.clone());
        // warn!(
        //     "enqueue pcb {:?} cfsrq {:?}",
        //     se.pcb().pid(),
        //     self.entities
        //         .iter()
        //         .map(|x| (x.0, x.1.pcb().pid()))
        //         .collect::<Vec<_>>()
        // );
        // send_to_default_serial8250_port(
        //     format!(
        //         "enqueue pcb {:?} cfsrq {:?}\n",
        //         se.pcb().pid(),
        //         self.entities
        //             .iter()
        //             .map(|x| (x.0, x.1.pcb().pid()))
        //             .collect::<Vec<_>>()
        //     )
        //     .as_bytes(),
        // );
    }

    fn inner_dequeue_entity(&mut self, se: &Arc<FairSchedEntity>) {
        // warn!(
        //     "before dequeue pcb {:?} cfsrq {:?}",
        //     se.pcb().pid(),
        //     self.entities
        //         .iter()
        //         .map(|x| (x.0, x.1.pcb().pid()))
        //         .collect::<Vec<_>>()
        // );

        // send_to_default_serial8250_port(
        //     format!(
        //         "before dequeue pcb {:?} cfsrq {:?}\n",
        //         se.pcb().pid(),
        //         self.entities
        //             .iter()
        //             .map(|x| (x.0, x.1.pcb().pid()))
        //             .collect::<Vec<_>>()
        //     )
        //     .as_bytes(),
        // );

        let mut i = 1;
        while let Some(rm) = self.entities.remove(&se.vruntime) {
            if Arc::ptr_eq(&rm, se) {
                break;
            }
            rm.force_mut().vruntime += i;
            self.entities.insert(rm.vruntime, rm);

            i += 1;
        }
        // send_to_default_serial8250_port(
        //     format!(
        //         "after dequeue pcb {:?}(real: {:?}) cfsrq {:?}\n",
        //         se.pcb().pid(),
        //         remove.pcb().pid(),
        //         self.entities
        //             .iter()
        //             .map(|x| (x.0, x.1.pcb().pid()))
        //             .collect::<Vec<_>>()
        //     )
        //     .as_bytes(),
        // );
        // warn!(
        //     "after dequeue pcb {:?}(real: {:?}) cfsrq {:?}",
        //     se.pcb().pid(),
        //     remove.pcb().pid(),
        //     self.entities
        //         .iter()
        //         .map(|x| (x.0, x.1.pcb().pid()))
        //         .collect::<Vec<_>>()
        // );
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
        let mut delta_sum = gcfs_rq.avg.load_avg as isize - se.avg.load_avg as isize;
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

        add_positive(&mut (self.avg.util_avg as isize), delta_avg);
        add_positive(&mut (self.avg.util_sum as isize), delta_sum);

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
        let new_sum = se.avg.runnable_sum * divider as u64;
        delta_sum = new_sum as isize - se.avg.runnable_sum as isize;

        se.avg.runnable_sum = new_sum;

        add_positive(&mut (self.avg.runnable_avg as isize), delta_avg);
        add_positive(&mut (self.avg.runnable_sum as isize), delta_sum);

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

        add_positive(&mut (self.avg.load_avg as isize), delta_avg);
        add_positive(&mut (self.avg.util_sum as isize), delta_sum);

        self.avg.load_sum = self
            .avg
            .load_sum
            .max((self.avg.load_avg * PELT_MIN_DIVIDER) as u64);
    }

    /// pick下一个运行的task
    pub fn pick_next_entity(&self) -> Option<Arc<FairSchedEntity>> {
        if SCHED_FEATURES.contains(SchedFeature::NEXT_BUDDY)
            && self.next().is_some()
            && self.entity_eligible(&self.next().unwrap())
        {
            return self.next();
        }
        self.entities.get_first().map(|val| val.1.clone())
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
pub struct CompletelyFairScheduler;

impl CompletelyFairScheduler {
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
        let mut se = pcb.sched_info().sched_entity();
        let mut idle_h_nr_running = pcb.sched_info().policy() == SchedPolicy::IDLE;
        let (should_continue, se) = FairSchedEntity::for_each_in_group(&mut se, |se| {
            if se.on_rq() {
                return (false, true);
            }

            let binding = se.cfs_rq();
            let cfs_rq = binding.force_mut();
            cfs_rq.enqueue_entity(&se, flags);

            cfs_rq.h_nr_running += 1;
            cfs_rq.idle_h_nr_running += idle_h_nr_running as u64;

            if cfs_rq.is_idle() {
                idle_h_nr_running = true;
            }

            // TODO: cfs_rq_throttled

            flags = EnqueueFlag::ENQUEUE_WAKEUP;

            return (true, true);
        });

        if !should_continue {
            return;
        }

        if let Some(mut se) = se {
            FairSchedEntity::for_each_in_group(&mut se, |se| {
                let binding = se.cfs_rq();
                let cfs_rq = binding.force_mut();

                cfs_rq.update_load_avg(&se, UpdateAvgFlags::UPDATE_TG);

                let se = se.force_mut();
                se.update_runnable();

                se.update_cfs_group();

                cfs_rq.h_nr_running += 1;
                cfs_rq.idle_h_nr_running += idle_h_nr_running as u64;

                if cfs_rq.is_idle() {
                    idle_h_nr_running = true;
                }

                // TODO: cfs_rq_throttled

                return (true, true);
            });
        }

        rq.add_nr_running(1);
    }

    fn dequeue(
        rq: &mut CpuRunQueue,
        pcb: Arc<crate::process::ProcessControlBlock>,
        mut flags: DequeueFlag,
    ) {
        let mut se = pcb.sched_info().sched_entity();
        let mut idle_h_nr_running = pcb.sched_info().policy() == SchedPolicy::IDLE;
        let task_sleep = flags.contains(DequeueFlag::DEQUEUE_SLEEP);
        let was_sched_idle = rq.sched_idle_rq();

        let (should_continue, se) = FairSchedEntity::for_each_in_group(&mut se, |se| {
            let binding = se.cfs_rq();
            let cfs_rq = binding.force_mut();
            cfs_rq.dequeue_entity(&se, flags);

            cfs_rq.h_nr_running -= 1;
            cfs_rq.idle_h_nr_running -= idle_h_nr_running as u64;

            if cfs_rq.is_idle() {
                idle_h_nr_running = true;
            }

            // TODO: cfs_rq_throttled

            if cfs_rq.load.weight > 0 {
                let sep = se.parent();

                if task_sleep && sep.is_some() {
                    todo!()
                }
            }

            flags |= DequeueFlag::DEQUEUE_SLEEP;

            return (true, true);
        });

        if !should_continue {
            return;
        }

        if let Some(mut se) = se {
            FairSchedEntity::for_each_in_group(&mut se, |se| {
                let binding = se.cfs_rq();
                let cfs_rq = binding.force_mut();

                cfs_rq.update_load_avg(&se, UpdateAvgFlags::UPDATE_TG);

                let se = se.force_mut();
                se.update_runnable();

                se.update_cfs_group();

                cfs_rq.h_nr_running -= 1;
                cfs_rq.idle_h_nr_running -= idle_h_nr_running as u64;

                if cfs_rq.is_idle() {
                    idle_h_nr_running = true;
                }

                // TODO: cfs_rq_throttled

                return (true, true);
            });
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

        se.force_mut().deadline += se.calculate_delta_fair(se.slice);
    }

    fn check_preempt_currnet(
        rq: &mut CpuRunQueue,
        pcb: &Arc<crate::process::ProcessControlBlock>,
        wake_flags: WakeupFlags,
    ) {
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

        if unlikely(curr.sched_info().policy() == SchedPolicy::IDLE)
            && likely(pcb.sched_info().policy() != SchedPolicy::IDLE)
        {
            rq.resched_current();
            return;
        }

        if unlikely(pcb.sched_info().policy() != SchedPolicy::CFS)
            || !SCHED_FEATURES.contains(SchedFeature::WAKEUP_PREEMPTION)
        {
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
        cfs_rq.force_mut().update_current();

        if let Some((_, pick_se)) = cfs_rq.entities.get_first() {
            if Arc::ptr_eq(pick_se, &pse) {
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
            let curr = cfs.current();
            if let Some(curr) = curr {
                if curr.on_rq() {
                    cfs.update_current();
                } else {
                    cfs.set_current(Weak::default());
                }
            }

            se = cfs.pick_next_entity();
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

    fn tick(_rq: &mut CpuRunQueue, pcb: Arc<crate::process::ProcessControlBlock>, queued: bool) {
        let mut se = pcb.sched_info().sched_entity();

        FairSchedEntity::for_each_in_group(&mut se, |se| {
            let binding = se.clone();
            let binding = binding.cfs_rq();
            let cfs_rq = binding.force_mut();

            cfs_rq.entity_tick(se, queued);
            (true, true)
        });
    }

    fn task_fork(pcb: Arc<ProcessControlBlock>) {
        let rq = cpu_rq(smp_get_processor_id().data() as usize);
        let se = pcb.sched_info().sched_entity();

        let (rq, _guard) = rq.self_lock();

        rq.update_rq_clock();

        let binding = se.cfs_rq();
        let cfs_rq = binding.force_mut();

        if cfs_rq.current().is_some() {
            cfs_rq.update_current();
        }

        cfs_rq.place_entity(se.clone(), EnqueueFlag::ENQUEUE_INITIAL);
    }

    fn pick_next_task(
        rq: &mut CpuRunQueue,
        prev: Option<Arc<ProcessControlBlock>>,
    ) -> Option<Arc<ProcessControlBlock>> {
        let mut cfs_rq = rq.cfs_rq();
        if rq.nr_running == 0 {
            return None;
        }

        if prev.is_none()
            || (prev.is_some() && prev.as_ref().unwrap().sched_info().policy() != SchedPolicy::CFS)
        {
            if let Some(prev) = prev {
                match prev.sched_info().policy() {
                    SchedPolicy::RT => todo!(),
                    SchedPolicy::FIFO => todo!(),
                    SchedPolicy::CFS => todo!(),
                    SchedPolicy::IDLE => IdleScheduler::put_prev_task(rq, prev),
                }
            }
            let mut se;
            loop {
                match cfs_rq.pick_next_entity() {
                    Some(s) => se = s,
                    None => return None,
                }

                cfs_rq.force_mut().set_next_entity(&se);

                match &se.my_cfs_rq {
                    Some(q) => cfs_rq = q.clone(),
                    None => break,
                }
            }

            return Some(se.pcb());
        }

        let prev = prev.unwrap();
        let se = cfs_rq.pick_next_entity();

        if let Some(mut se) = se {
            loop {
                let curr = cfs_rq.current();
                if let Some(current) = curr {
                    if current.on_rq() {
                        cfs_rq.force_mut().update_current()
                    } else {
                        cfs_rq.force_mut().set_current(Weak::default());
                    }
                }

                match cfs_rq.pick_next_entity() {
                    Some(e) => se = e,
                    None => break,
                }

                if let Some(q) = se.my_cfs_rq.clone() {
                    cfs_rq = q;
                } else {
                    break;
                }
            }

            let p = se.pcb();

            if !Arc::ptr_eq(&prev, &p) {
                let mut pse = prev.sched_info().sched_entity();

                while !(Arc::ptr_eq(&se.cfs_rq(), &pse.cfs_rq())
                    && Arc::ptr_eq(&se.cfs_rq(), &cfs_rq))
                {
                    let se_depth = se.depth;
                    let pse_depth = pse.depth;

                    if se_depth <= pse_depth {
                        pse.cfs_rq().force_mut().put_prev_entity(pse.clone());
                        pse = pse.parent().unwrap();
                    }

                    if se_depth >= pse_depth {
                        se.cfs_rq().force_mut().set_next_entity(&se);
                        se = se.parent().unwrap();
                    }
                }

                cfs_rq.force_mut().put_prev_entity(pse);
                cfs_rq.force_mut().set_next_entity(&se);
            }

            return Some(p);
        } else {
            return None;
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
