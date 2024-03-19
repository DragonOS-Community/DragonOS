pub mod clock;
pub mod cputime;
pub mod fair;
pub mod pelt;

use core::{
    intrinsics::{likely, unlikely},
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{boxed::Box, collections::LinkedList, sync::Arc, vec::Vec};

use crate::{
    include::bindings::bindings::MAX_CPU_NUM,
    libs::{
        lazy_init::Lazy,
        rwlock::{RwLock, RwLockReadGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::percpu::PerCpu,
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, SchedInfo},
    smp::core::smp_get_processor_id,
    time::{clocksource::HZ, timer::clock},
};

use self::{
    clock::{ClockUpdataFlag, SchedClock},
    cputime::{irq_time_read, IrqTime},
    fair::{CfsRunQueue, CompletelyFairScheduler, FairSchedEntity},
};

static mut CPU_IQR_TIME: Option<Vec<&'static mut IrqTime>> = None;

// 这里虽然rq是percpu的，但是在负载均衡的时候需要修改对端cpu的rq，所以仍需加锁
static CPU_RUNQUEUE: Lazy<Vec<Arc<CpuRunQueue>>> = Lazy::new();

/// 用于记录系统中所有 CPU 的可执行进程数量的总和。
static CALCULATE_LOAD_TASKS: AtomicUsize = AtomicUsize::new(0);

const LOAD_FREQ: usize = HZ as usize * 5 + 1;

pub const SCHED_FIXEDPOINT_SHIFT: u64 = 10;
pub const SCHED_FIXEDPOINT_SCALE: u64 = 1 << SCHED_FIXEDPOINT_SHIFT;
pub const SCHED_CAPACITY_SHIFT: u64 = SCHED_FIXEDPOINT_SHIFT;
pub const SCHED_CAPACITY_SCALE: u64 = 1 << SCHED_CAPACITY_SHIFT;

#[inline]
pub fn cpu_irq_time(cpu: usize) -> &'static mut IrqTime {
    unsafe { CPU_IQR_TIME.as_mut().unwrap()[cpu] }
}

#[inline]
pub fn cpu_rq(cpu: usize) -> Arc<CpuRunQueue> {
    CPU_RUNQUEUE.ensure();
    CPU_RUNQUEUE.get()[cpu].clone()
}

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
    fn enqueue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, flags: EnqueueFlag);

    /// ## 当任务不再可运行时被调用，对应的调度实体被移出红黑树。它减少nr_running变量的值。
    fn dequeue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, flags: DequeueFlag);

    /// ## 主动让出cpu，这个函数的行为基本上是出队，紧接着入队
    fn yield_task(rq: &mut CpuRunQueue);

    /// ## 检查进入可运行状态的任务能否抢占当前正在运行的任务
    fn check_preempt_currnet(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, flags: u32);

    /// ## 选择接下来最适合运行的任务
    fn pick_task(rq: &mut CpuRunQueue) -> Option<Arc<ProcessControlBlock>>;

    /// ## 被时间滴答函数调用，它可能导致进程切换。驱动了运行时抢占。
    fn tick(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, queued: bool);

    /// ## 在进程fork时，如需加入cfs，则调用
    fn task_fork(pcb: Arc<ProcessControlBlock>);
}

/// 调度策略
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    /// 完全公平调度
    CFS,
    /// 先进先出调度
    FIFO,
    /// 轮转调度
    RR,
    /// IDLE
    IDLE,
}

pub struct TaskGroup {
    /// CFS管理的调度实体，percpu的
    entitys: Vec<Arc<FairSchedEntity>>,
    /// 每个CPU的CFS运行队列
    cfs: Vec<Arc<CfsRunQueue>>,
    /// 父节点
    parent: Option<Arc<TaskGroup>>,

    shares: u64,
}

#[derive(Debug)]
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

    pub fn update_load_add(&mut self, inc: u64) {
        self.weight += inc;
        self.inv_weight = 0;
    }

    pub fn update_load_sub(&mut self, dec: u64) {
        self.weight -= dec;
        self.inv_weight = 0;
    }

    pub fn update_load_set(&mut self, weight: u64) {
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
#[derive(Debug)]
pub struct CpuRunQueue {
    lock: SpinLock<()>,
    lock_on_who: AtomicUsize,

    cpu: usize,
    clock_task: u64,
    clock: u64,
    prev_irq_time: u64,
    clock_updata_flags: ClockUpdataFlag,

    /// 过载
    overload: bool,

    next_balance: u64,

    /// 运行任务数
    nr_running: usize,

    /// 被阻塞的任务数量
    nr_uninterruptible: usize,

    /// 记录上次更新负载时间
    cala_load_update: usize,
    cala_load_active: usize,

    /// CFS调度器
    cfs: Arc<CfsRunQueue>,

    clock_pelt: u64,
    lost_idle_time: u64,
    clock_idle: u64,

    cfs_tasks: LinkedList<Arc<FairSchedEntity>>,

    /// 最近一次的调度信息
    sched_info: SchedInfo,

    /// 当前在运行队列上执行的进程
    current: Arc<ProcessControlBlock>,
}

impl CpuRunQueue {
    /// 获取到rq的可变引用，需要注意的是返回的第二个值需要确保其生命周期
    /// 所以可以说这个函数是unsafe的，需要确保正确性
    pub fn self_mut(&self) -> (&mut Self, Option<SpinLockGuard<()>>) {
        if self.lock.is_locked()
            && smp_get_processor_id().data() as usize == self.lock_on_who.load(Ordering::SeqCst)
        {
            // 在本cpu已上锁则可以直接拿
            (
                unsafe { &mut *(self as *const Self as usize as *mut Self) },
                None,
            )
        } else {
            // 否则先上锁再拿
            let guard = self.lock();
            (
                unsafe { &mut *(self as *const Self as usize as *mut Self) },
                Some(guard),
            )
        }
    }

    fn lock(&self) -> SpinLockGuard<()> {
        let guard = self.lock.lock_irqsave();

        // 更新在哪一个cpu上锁
        self.lock_on_who
            .store(smp_get_processor_id().data() as usize, Ordering::SeqCst);

        guard
    }

    pub fn enqueue_task(&mut self, pcb: Arc<ProcessControlBlock>, flags: EnqueueFlag) {
        if !flags.contains(EnqueueFlag::ENQUEUE_NOCLOCK) {
            self.update_rq_clock();
        }

        if !flags.contains(EnqueueFlag::ENQUEUE_RESTORE) {
            let sched_info = pcb.sched_info().sched_stat.upgradeable_read_irqsave();
            if sched_info.last_queued == 0 {
                sched_info.upgrade().last_queued = self.clock;
            }
        }

        match pcb.sched_info().policy() {
            SchedPolicy::CFS => CompletelyFairScheduler::enqueue(self, pcb, flags),
            SchedPolicy::FIFO => todo!(),
            SchedPolicy::RR => todo!(),
            SchedPolicy::IDLE => todo!(),
        }

        // TODO:https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sched/core.c#239
    }

    pub fn dequeue_task(&mut self, pcb: Arc<ProcessControlBlock>, flags: DequeueFlag) {
        // TODO:sched_core

        if !flags.contains(DequeueFlag::DEQUEUE_NOCLOCK) {
            self.update_rq_clock()
        }

        if !flags.contains(DequeueFlag::DEQUEUE_SAVE) {
            let sched_info = pcb.sched_info().sched_stat.upgradeable_read_irqsave();

            if sched_info.last_queued > 0 {
                let delta = self.clock - sched_info.last_queued;

                let mut sched_info = sched_info.upgrade();
                sched_info.last_queued = 0;
                sched_info.run_delay += delta as usize;

                self.sched_info.run_delay += delta as usize;
            }
        }

        match pcb.sched_info().policy() {
            SchedPolicy::CFS => CompletelyFairScheduler::dequeue(self, pcb, flags),
            SchedPolicy::FIFO => todo!(),
            SchedPolicy::RR => todo!(),
            SchedPolicy::IDLE => todo!(),
        }
    }

    pub fn activate_task(&mut self, pcb: Arc<ProcessControlBlock>, mut flags: EnqueueFlag) {
        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::OnRqMigrating {
            flags |= EnqueueFlag::ENQUEUE_MIGRATED;
        }

        if flags.contains(EnqueueFlag::ENQUEUE_MIGRATED) {
            todo!()
        }

        self.enqueue_task(pcb.clone(), flags);

        *pcb.sched_info().on_rq.lock_irqsave() = OnRq::OnRqQueued;
    }

    pub fn deactive_task(&mut self, pcb: Arc<ProcessControlBlock>, flags: DequeueFlag) {
        *pcb.sched_info().on_rq.lock_irqsave() = if flags.contains(DequeueFlag::DEQUEUE_SLEEP) {
            OnRq::NoOnRq
        } else {
            OnRq::OnRqMigrating
        };

        self.dequeue_task(pcb, flags);
    }

    #[inline]
    pub fn cfs_rq(&self) -> Arc<CfsRunQueue> {
        self.cfs.clone()
    }

    pub fn update_rq_clock(&mut self) {
        // 需要跳过这次时钟更新
        if self
            .clock_updata_flags
            .contains(ClockUpdataFlag::RQCF_ACT_SKIP)
        {
            return;
        }

        let clock = SchedClock::sched_clock_cpu(self.cpu);
        if clock < self.clock as u128 {
            return;
        }

        let delta = (clock - self.clock as u128) as u64;
        self.clock += delta;
        self.update_rq_clock_task(delta);
    }

    pub fn update_rq_clock_task(&mut self, mut delta: u64) {
        let mut irq_delta = irq_time_read(self.cpu) - self.prev_irq_time;

        if irq_delta > delta {
            irq_delta = delta;
        }

        self.prev_irq_time += irq_delta;

        delta -= irq_delta;

        // todo: psi?

        self.clock_task += delta;

        // todo: pelt?
    }

    /// 计算当前进程中的可执行数量
    fn calculate_load_fold_active(&mut self, adjust: usize) -> usize {
        let mut nr_active = self.nr_running - adjust;
        nr_active += self.nr_uninterruptible;
        let mut delta = 0;

        if nr_active != self.cala_load_active {
            delta = nr_active - self.cala_load_active;
            self.cala_load_active = nr_active;
        }

        delta
    }

    /// ## tick计算全局负载
    pub fn calculate_global_load_tick(&mut self) {
        if clock() < self.cala_load_update as u64 {
            // 如果当前时间在上次更新时间之前，则直接返回
            return;
        }

        let delta = self.calculate_load_fold_active(0);

        if delta != 0 {
            CALCULATE_LOAD_TASKS.fetch_add(delta, Ordering::SeqCst);
        }

        self.cala_load_update += LOAD_FREQ;
    }

    pub fn add_nr_running(&mut self, nr_running: usize) {
        let prev = self.nr_running;

        self.nr_running = prev + nr_running;

        if prev < 2 && self.nr_running >= 2 {
            if !self.overload {
                self.overload = true;
            }
        }
    }

    pub fn sched_idle_rq(&self) -> bool {
        return unlikely(
            self.nr_running == self.cfs.idle_h_nr_running as usize && self.nr_running > 0,
        );
    }

    #[inline]
    pub fn current(&self) -> Arc<ProcessControlBlock> {
        self.current.clone()
    }

    #[inline]
    pub fn clock_task(&self) -> u64 {
        self.clock_task
    }

    /// 重新调度当前进程
    pub fn resched_current(&self) {
        let current = self.current();

        // 又需要被调度？
        if unlikely(current.flags().contains(ProcessFlags::NEED_SCHEDULE)) {
            return;
        }

        let cpu = self.cpu;

        if cpu == smp_get_processor_id().data() as usize {
            // 设置需要调度
            current.flags().insert(ProcessFlags::NEED_SCHEDULE);
            return;
        }

        // 需要迁移到其他cpu
        todo!()
    }
}

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

    pub struct EnqueueFlag: u8 {
        const ENQUEUE_WAKEUP	= 0x01;
        const ENQUEUE_RESTORE	= 0x02;
        const ENQUEUE_MOVE	= 0x04;
        const ENQUEUE_NOCLOCK	= 0x08;

        const ENQUEUE_MIGRATED	= 0x40;

        const ENQUEUE_INITIAL	= 0x80;
    }

    pub struct DequeueFlag: u8 {
        const DEQUEUE_SLEEP		= 0x01;
        const DEQUEUE_SAVE		= 0x02; /* Matches ENQUEUE_RESTORE */
        const DEQUEUE_MOVE		= 0x04; /* Matches ENQUEUE_MOVE */
        const DEQUEUE_NOCLOCK		= 0x08; /* Matches ENQUEUE_NOCLOCK */
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum OnRq {
    OnRqQueued,
    OnRqMigrating,
    NoOnRq,
}

impl ProcessManager {
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sched/core.c#4852
    ///
    /// SMP TODO
    pub fn wakeup_new(pcb: Arc<ProcessControlBlock>) {}kkk
}

/// ## 时钟tick时调用此函数
pub fn scheduler_tick() {
    // 获取当前CPU索引
    let cpu_idx = smp_get_processor_id().data() as usize;

    // 获取当前CPU的请求队列
    let rq = cpu_rq(cpu_idx);
    let mut _rq_guard = rq.lock.lock_irqsave();

    let (rq, guard) = rq.self_mut();

    // 获取当前请求队列的当前请求
    let current = rq.current();

    // 更新请求队列时钟
    rq.update_rq_clock();

    match current.sched_info().policy() {
        SchedPolicy::CFS => CompletelyFairScheduler::tick(rq, current, false),
        SchedPolicy::FIFO => todo!(),
        SchedPolicy::RR => todo!(),
        SchedPolicy::IDLE => todo!(),
    }

    rq.calculate_global_load_tick();

    // TODO:处理负载均衡
}

#[inline(never)]
pub fn sched_init() {
    // 初始化percpu变量
    unsafe {
        CPU_IQR_TIME = Some(Vec::with_capacity(MAX_CPU_NUM as usize));
        CPU_IQR_TIME
            .as_mut()
            .unwrap()
            .resize_with(MAX_CPU_NUM as usize, || {
                Box::leak(Box::new(IrqTime::default()))
            });

        // CPU_RUNQUEUE = Some(Vec::with_capacity(MAX_CPU_NUM as usize));
        // CPU_RUNQUEUE
        //     .as_mut()
        //     .unwrap()
        //     .resize_with(MAX_CPU_NUM as usize, || {
        //         Box::leak(Box::new(CpuRunQueue::default()))
        //     });
    };
}
