pub mod clock;
pub mod completion;
pub mod cputime;
pub mod fair;
pub mod idle;
pub mod pelt;
pub mod prio;
pub mod syscall;

use core::{
    intrinsics::{likely, unlikely},
    sync::atomic::{compiler_fence, fence, AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::{interrupt::ipi::send_ipi, CurrentIrqArch},
    exception::{
        ipi::{IpiKind, IpiTarget},
        InterruptArch,
    },
    libs::{
        lazy_init::Lazy,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::percpu::{PerCpu, PerCpuVar},
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, SchedInfo},
    sched::idle::IdleScheduler,
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
    time::{clocksource::HZ, timer::clock},
};

use self::{
    clock::{ClockUpdataFlag, SchedClock},
    cputime::{irq_time_read, CpuTimeFunc, IrqTime},
    fair::{CfsRunQueue, CompletelyFairScheduler, FairSchedEntity},
    prio::PrioUtil,
};

static mut CPU_IRQ_TIME: Option<Vec<&'static mut IrqTime>> = None;

// 这里虽然rq是percpu的，但是在负载均衡的时候需要修改对端cpu的rq，所以仍需加锁
static CPU_RUNQUEUE: Lazy<PerCpuVar<Arc<CpuRunQueue>>> = PerCpuVar::define_lazy();

/// 用于记录系统中所有 CPU 的可执行进程数量的总和。
static CALCULATE_LOAD_TASKS: AtomicUsize = AtomicUsize::new(0);

const LOAD_FREQ: usize = HZ as usize * 5 + 1;

pub const SCHED_FIXEDPOINT_SHIFT: u64 = 10;
#[allow(dead_code)]
pub const SCHED_FIXEDPOINT_SCALE: u64 = 1 << SCHED_FIXEDPOINT_SHIFT;
#[allow(dead_code)]
pub const SCHED_CAPACITY_SHIFT: u64 = SCHED_FIXEDPOINT_SHIFT;
#[allow(dead_code)]
pub const SCHED_CAPACITY_SCALE: u64 = 1 << SCHED_CAPACITY_SHIFT;

#[inline]
pub fn cpu_irq_time(cpu: ProcessorId) -> &'static mut IrqTime {
    unsafe { CPU_IRQ_TIME.as_mut().unwrap()[cpu.data() as usize] }
}

#[inline]
pub fn cpu_rq(cpu: usize) -> Arc<CpuRunQueue> {
    CPU_RUNQUEUE.ensure();
    unsafe {
        CPU_RUNQUEUE
            .get()
            .force_get(ProcessorId::new(cpu as u32))
            .clone()
    }
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
    fn check_preempt_currnet(
        rq: &mut CpuRunQueue,
        pcb: &Arc<ProcessControlBlock>,
        flags: WakeupFlags,
    );

    /// ## 选择接下来最适合运行的任务
    #[allow(dead_code)]
    fn pick_task(rq: &mut CpuRunQueue) -> Option<Arc<ProcessControlBlock>>;

    /// ## 选择接下来最适合运行的任务
    fn pick_next_task(
        rq: &mut CpuRunQueue,
        pcb: Option<Arc<ProcessControlBlock>>,
    ) -> Option<Arc<ProcessControlBlock>>;

    /// ## 被时间滴答函数调用，它可能导致进程切换。驱动了运行时抢占。
    fn tick(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, queued: bool);

    /// ## 在进程fork时，如需加入cfs，则调用
    fn task_fork(pcb: Arc<ProcessControlBlock>);

    fn put_prev_task(rq: &mut CpuRunQueue, prev: Arc<ProcessControlBlock>);
}

/// 调度策略
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchedPolicy {
    /// 实时进程
    RT,
    /// 先进先出调度
    FIFO,
    /// 完全公平调度
    CFS,
    /// IDLE
    IDLE,
}

#[allow(dead_code)]
pub struct TaskGroup {
    /// CFS管理的调度实体，percpu的
    entitys: Vec<Arc<FairSchedEntity>>,
    /// 每个CPU的CFS运行队列
    cfs: Vec<Arc<CfsRunQueue>>,
    /// 父节点
    parent: Option<Arc<TaskGroup>>,

    shares: u64,
}

#[derive(Debug, Default)]
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
        fact *= self.inv_weight as u64;

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
            weight >>= Self::SCHED_FIXEDPOINT_SHIFT;

            if weight < 2 {
                weight = 2;
            }
        }
        weight
    }

    #[allow(dead_code)]
    pub const fn scale_load(weight: u64) -> u64 {
        weight << Self::SCHED_FIXEDPOINT_SHIFT
    }
}

pub trait SchedArch {
    /// 开启当前核心的调度
    fn enable_sched_local();
    /// 关闭当前核心的调度
    #[allow(dead_code)]
    fn disable_sched_local();

    /// 在第一次开启调度之前，进行初始化工作。
    ///
    /// 注意区别于sched_init，这个函数只是做初始化时钟的工作等等。
    fn initial_setup_sched_local() {}
}

/// ## PerCpu的运行队列，其中维护了各个调度器对应的rq
#[allow(dead_code)]
#[derive(Debug)]
pub struct CpuRunQueue {
    lock: SpinLock<()>,
    lock_on_who: AtomicUsize,

    cpu: ProcessorId,
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
    current: Weak<ProcessControlBlock>,

    idle: Weak<ProcessControlBlock>,
}

impl CpuRunQueue {
    pub fn new(cpu: ProcessorId) -> Self {
        Self {
            lock: SpinLock::new(()),
            lock_on_who: AtomicUsize::new(usize::MAX),
            cpu,
            clock_task: 0,
            clock: 0,
            prev_irq_time: 0,
            clock_updata_flags: ClockUpdataFlag::empty(),
            overload: false,
            next_balance: 0,
            nr_running: 0,
            nr_uninterruptible: 0,
            cala_load_update: (clock() + (5 * HZ + 1)) as usize,
            cala_load_active: 0,
            cfs: Arc::new(CfsRunQueue::new()),
            clock_pelt: 0,
            lost_idle_time: 0,
            clock_idle: 0,
            cfs_tasks: LinkedList::new(),
            sched_info: SchedInfo::default(),
            current: Weak::new(),
            idle: Weak::new(),
        }
    }

    /// 此函数只能在关中断的情况下使用！！！
    /// 获取到rq的可变引用，需要注意的是返回的第二个值需要确保其生命周期
    /// 所以可以说这个函数是unsafe的，需要确保正确性
    /// 在中断上下文，关中断的情况下，此函数是安全的
    pub fn self_lock(&self) -> (&mut Self, Option<SpinLockGuard<()>>) {
        if self.lock.is_locked()
            && smp_get_processor_id().data() as usize == self.lock_on_who.load(Ordering::SeqCst)
        {
            // 在本cpu已上锁则可以直接拿
            (
                unsafe {
                    (self as *const Self as usize as *mut Self)
                        .as_mut()
                        .unwrap()
                },
                None,
            )
        } else {
            // 否则先上锁再拿
            let guard = self.lock();
            (
                unsafe {
                    (self as *const Self as usize as *mut Self)
                        .as_mut()
                        .unwrap()
                },
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
            SchedPolicy::RT => todo!(),
            SchedPolicy::IDLE => IdleScheduler::enqueue(self, pcb, flags),
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
            SchedPolicy::RT => todo!(),
            SchedPolicy::IDLE => IdleScheduler::dequeue(self, pcb, flags),
        }
    }

    /// 启用一个任务，将加入队列
    pub fn activate_task(&mut self, pcb: &Arc<ProcessControlBlock>, mut flags: EnqueueFlag) {
        if *pcb.sched_info().on_rq.lock_irqsave() == OnRq::Migrating {
            flags |= EnqueueFlag::ENQUEUE_MIGRATED;
        }

        if flags.contains(EnqueueFlag::ENQUEUE_MIGRATED) {
            todo!()
        }

        self.enqueue_task(pcb.clone(), flags);

        *pcb.sched_info().on_rq.lock_irqsave() = OnRq::Queued;
        pcb.sched_info().set_on_cpu(Some(self.cpu));
    }

    /// 检查对应的task是否可以抢占当前运行的task
    #[allow(clippy::comparison_chain)]
    pub fn check_preempt_currnet(&mut self, pcb: &Arc<ProcessControlBlock>, flags: WakeupFlags) {
        if pcb.sched_info().policy() == self.current().sched_info().policy() {
            match self.current().sched_info().policy() {
                SchedPolicy::CFS => {
                    CompletelyFairScheduler::check_preempt_currnet(self, pcb, flags)
                }
                SchedPolicy::FIFO => todo!(),
                SchedPolicy::RT => todo!(),
                SchedPolicy::IDLE => IdleScheduler::check_preempt_currnet(self, pcb, flags),
            }
        } else if pcb.sched_info().policy() < self.current().sched_info().policy() {
            // 调度优先级更高
            self.resched_current();
        }

        if *self.current().sched_info().on_rq.lock_irqsave() == OnRq::Queued
            && self.current().flags().contains(ProcessFlags::NEED_SCHEDULE)
        {
            self.clock_updata_flags
                .insert(ClockUpdataFlag::RQCF_REQ_SKIP);
        }
    }

    /// 禁用一个任务，将离开队列
    pub fn deactivate_task(&mut self, pcb: Arc<ProcessControlBlock>, flags: DequeueFlag) {
        *pcb.sched_info().on_rq.lock_irqsave() = if flags.contains(DequeueFlag::DEQUEUE_SLEEP) {
            OnRq::None
        } else {
            OnRq::Migrating
        };

        self.dequeue_task(pcb, flags);
    }

    #[inline]
    pub fn cfs_rq(&self) -> Arc<CfsRunQueue> {
        self.cfs.clone()
    }

    /// 更新rq时钟
    pub fn update_rq_clock(&mut self) {
        // 需要跳过这次时钟更新
        if self
            .clock_updata_flags
            .contains(ClockUpdataFlag::RQCF_ACT_SKIP)
        {
            return;
        }

        let clock = SchedClock::sched_clock_cpu(self.cpu);
        if clock < self.clock {
            return;
        }

        let delta = clock - self.clock;
        self.clock += delta;
        // error!("clock {}", self.clock);
        self.update_rq_clock_task(delta);
    }

    /// 更新任务时钟
    pub fn update_rq_clock_task(&mut self, mut delta: u64) {
        let mut irq_delta = irq_time_read(self.cpu) - self.prev_irq_time;
        // if self.cpu == 0 {
        //     error!(
        //         "cpu 0 delta {delta} irq_delta {} irq_time_read(self.cpu) {} self.prev_irq_time {}",
        //         irq_delta,
        //         irq_time_read(self.cpu),
        //         self.prev_irq_time
        //     );
        // }
        compiler_fence(Ordering::SeqCst);

        if irq_delta > delta {
            irq_delta = delta;
        }

        self.prev_irq_time += irq_delta;

        delta -= irq_delta;

        // todo: psi?

        // send_to_default_serial8250_port(format!("\n{delta}\n",).as_bytes());
        compiler_fence(Ordering::SeqCst);
        self.clock_task += delta;
        compiler_fence(Ordering::SeqCst);
        // if self.cpu == 0 {
        //     error!("cpu {} clock_task {}", self.cpu, self.clock_task);
        // }
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
        if prev < 2 && self.nr_running >= 2 && !self.overload {
            self.overload = true;
        }
    }

    pub fn sub_nr_running(&mut self, count: usize) {
        self.nr_running -= count;
    }

    /// 在运行idle？
    pub fn sched_idle_rq(&self) -> bool {
        return unlikely(
            self.nr_running == self.cfs.idle_h_nr_running as usize && self.nr_running > 0,
        );
    }

    #[inline]
    pub fn current(&self) -> Arc<ProcessControlBlock> {
        self.current.upgrade().unwrap()
    }

    #[inline]
    pub fn set_current(&mut self, pcb: Weak<ProcessControlBlock>) {
        self.current = pcb;
    }

    #[inline]
    pub fn set_idle(&mut self, pcb: Weak<ProcessControlBlock>) {
        self.idle = pcb;
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

        if cpu == smp_get_processor_id() {
            // assert!(
            //     Arc::ptr_eq(&current, &ProcessManager::current_pcb()),
            //     "rq current name {} process current {}",
            //     current.basic().name().to_string(),
            //     ProcessManager::current_pcb().basic().name().to_string(),
            // );
            // 设置需要调度
            ProcessManager::current_pcb()
                .flags()
                .insert(ProcessFlags::NEED_SCHEDULE);
            return;
        }

        // 向目标cpu发送重调度ipi
        send_resched_ipi(cpu);
    }

    /// 选择下一个task
    pub fn pick_next_task(&mut self, prev: Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
        if likely(prev.sched_info().policy() >= SchedPolicy::CFS)
            && self.nr_running == self.cfs.h_nr_running as usize
        {
            let p = CompletelyFairScheduler::pick_next_task(self, Some(prev.clone()));

            if let Some(pcb) = p.as_ref() {
                return pcb.clone();
            } else {
                // error!(
                //     "pick idle cfs rq {:?}",
                //     self.cfs_rq()
                //         .entities
                //         .iter()
                //         .map(|x| x.1.pid)
                //         .collect::<Vec<_>>()
                // );
                match prev.sched_info().policy() {
                    SchedPolicy::FIFO => todo!(),
                    SchedPolicy::RT => todo!(),
                    SchedPolicy::CFS => CompletelyFairScheduler::put_prev_task(self, prev),
                    SchedPolicy::IDLE => IdleScheduler::put_prev_task(self, prev),
                }
                // 选择idle
                return self.idle.upgrade().unwrap();
            }
        }

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

    pub struct WakeupFlags: u8 {
        /* Wake flags. The first three directly map to some SD flag value */
        const WF_EXEC         = 0x02; /* Wakeup after exec; maps to SD_BALANCE_EXEC */
        const WF_FORK         = 0x04; /* Wakeup after fork; maps to SD_BALANCE_FORK */
        const WF_TTWU         = 0x08; /* Wakeup;            maps to SD_BALANCE_WAKE */

        const WF_SYNC         = 0x10; /* Waker goes to sleep after wakeup */
        const WF_MIGRATED     = 0x20; /* Internal use, task got migrated */
        const WF_CURRENT_CPU  = 0x40; /* Prefer to move the wakee to the current CPU. */
    }

    pub struct SchedMode: u8 {
        /*
        * Constants for the sched_mode argument of __schedule().
        *
        * The mode argument allows RT enabled kernels to differentiate a
        * preemption from blocking on an 'sleeping' spin/rwlock. Note that
        * SM_MASK_PREEMPT for !RT has all bits set, which allows the compiler to
        * optimize the AND operation out and just check for zero.
        */
        /// 在调度过程中不会再次进入队列，即需要手动唤醒
        const SM_NONE			= 0x0;
        /// 重新加入队列，即当前进程被抢占，需要时钟调度
        const SM_PREEMPT		= 0x1;
        /// rt相关
        const SM_RTLOCK_WAIT		= 0x2;
        /// 默认与SM_PREEMPT相同
        const SM_MASK_PREEMPT	= Self::SM_PREEMPT.bits;
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum OnRq {
    Queued,
    Migrating,
    None,
}

impl ProcessManager {
    pub fn update_process_times(user_tick: bool) {
        let pcb = Self::current_pcb();
        CpuTimeFunc::irqtime_account_process_tick(&pcb, user_tick, 1);

        scheduler_tick();
    }
}

/// ## 时钟tick时调用此函数
pub fn scheduler_tick() {
    fence(Ordering::SeqCst);
    // 获取当前CPU索引
    let cpu_idx = smp_get_processor_id().data() as usize;

    // 获取当前CPU的请求队列
    let rq = cpu_rq(cpu_idx);

    let (rq, guard) = rq.self_lock();

    // 获取当前请求队列的当前请求
    let current = rq.current();

    // 更新请求队列时钟
    rq.update_rq_clock();

    match current.sched_info().policy() {
        SchedPolicy::CFS => CompletelyFairScheduler::tick(rq, current, false),
        SchedPolicy::FIFO => todo!(),
        SchedPolicy::RT => todo!(),
        SchedPolicy::IDLE => IdleScheduler::tick(rq, current, false),
    }

    rq.calculate_global_load_tick();

    drop(guard);
    // TODO:处理负载均衡
}

/// ## 执行调度
/// 若preempt_count不为0则报错
#[inline]
pub fn schedule(sched_mod: SchedMode) {
    let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    assert_eq!(ProcessManager::current_pcb().preempt_count(), 0);
    __schedule(sched_mod);
}

/// ## 执行调度
/// 此函数与schedule的区别为，该函数不会检查preempt_count
/// 适用于时钟中断等场景
pub fn __schedule(sched_mod: SchedMode) {
    let cpu = smp_get_processor_id().data() as usize;
    let rq = cpu_rq(cpu);

    let mut prev = rq.current();
    if let ProcessState::Exited(_) = prev.clone().sched_info().inner_lock_read_irqsave().state() {
        // 从exit进的Schedule
        prev = ProcessManager::current_pcb();
    }

    // TODO: hrtick_clear(rq);

    let (rq, _guard) = rq.self_lock();

    rq.clock_updata_flags = ClockUpdataFlag::from_bits_truncate(rq.clock_updata_flags.bits() << 1);

    rq.update_rq_clock();
    rq.clock_updata_flags = ClockUpdataFlag::RQCF_UPDATE;

    // kBUG!(
    //     "before cfs rq pcbs {:?}\nvruntimes {:?}\n",
    //     rq.cfs
    //         .entities
    //         .iter()
    //         .map(|x| { x.1.pcb().pid() })
    //         .collect::<Vec<_>>(),
    //     rq.cfs
    //         .entities
    //         .iter()
    //         .map(|x| { x.1.vruntime })
    //         .collect::<Vec<_>>(),
    // );
    // warn!(
    //     "before cfs rq {:?} prev {:?}",
    //     rq.cfs
    //         .entities
    //         .iter()
    //         .map(|x| { x.1.pcb().pid() })
    //         .collect::<Vec<_>>(),
    //     prev.pid()
    // );

    // error!("prev pid {:?} {:?}", prev.pid(), prev.sched_info().policy());
    if !sched_mod.contains(SchedMode::SM_MASK_PREEMPT)
        && prev.sched_info().policy() != SchedPolicy::IDLE
        && prev.sched_info().inner_lock_read_irqsave().is_mark_sleep()
    {
        // warn!("deactivate_task prev {:?}", prev.pid());
        // TODO: 这里需要处理信号
        // https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sched/core.c?r=&mo=172979&fi=6578#6630
        rq.deactivate_task(
            prev.clone(),
            DequeueFlag::DEQUEUE_SLEEP | DequeueFlag::DEQUEUE_NOCLOCK,
        );
    }

    let next = rq.pick_next_task(prev.clone());

    // kBUG!(
    //     "after cfs rq pcbs {:?}\nvruntimes {:?}\n",
    //     rq.cfs
    //         .entities
    //         .iter()
    //         .map(|x| { x.1.pcb().pid() })
    //         .collect::<Vec<_>>(),
    //     rq.cfs
    //         .entities
    //         .iter()
    //         .map(|x| { x.1.vruntime })
    //         .collect::<Vec<_>>(),
    // );

    // error!("next {:?}", next.pid());

    prev.flags().remove(ProcessFlags::NEED_SCHEDULE);
    fence(Ordering::SeqCst);
    if likely(!Arc::ptr_eq(&prev, &next)) {
        rq.set_current(Arc::downgrade(&next));
        // warn!(
        //     "switch_process prev {:?} next {:?} sched_mode {sched_mod:?}",
        //     prev.pid(),
        //     next.pid()
        // );

        // send_to_default_serial8250_port(
        //     format!(
        //         "switch_process prev {:?} next {:?} sched_mode {sched_mod:?}\n",
        //         prev.pid(),
        //         next.pid()
        //     )
        //     .as_bytes(),
        // );

        // CurrentApic.send_eoi();
        compiler_fence(Ordering::SeqCst);

        unsafe { ProcessManager::switch_process(prev, next) };
    } else {
        assert!(
            Arc::ptr_eq(&ProcessManager::current_pcb(), &prev),
            "{}",
            ProcessManager::current_pcb().basic().name()
        );
    }
}

pub fn sched_fork(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    let mut prio_guard = pcb.sched_info().prio_data.write_irqsave();
    let current = ProcessManager::current_pcb();

    prio_guard.prio = current.sched_info().prio_data.read_irqsave().normal_prio;

    if PrioUtil::dl_prio(prio_guard.prio) {
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    } else if PrioUtil::rt_prio(prio_guard.prio) {
        let policy = &pcb.sched_info().sched_policy;
        *policy.write_irqsave() = SchedPolicy::RT;
    } else {
        let policy = &pcb.sched_info().sched_policy;
        *policy.write_irqsave() = SchedPolicy::CFS;
    }

    pcb.sched_info()
        .sched_entity()
        .force_mut()
        .init_entity_runnable_average();

    Ok(())
}

pub fn sched_cgroup_fork(pcb: &Arc<ProcessControlBlock>) {
    __set_task_cpu(pcb, smp_get_processor_id());
    match pcb.sched_info().policy() {
        SchedPolicy::RT => todo!(),
        SchedPolicy::FIFO => todo!(),
        SchedPolicy::CFS => CompletelyFairScheduler::task_fork(pcb.clone()),
        SchedPolicy::IDLE => todo!(),
    }
}

fn __set_task_cpu(pcb: &Arc<ProcessControlBlock>, cpu: ProcessorId) {
    // TODO: Fixme There is not implement group sched;
    let se = pcb.sched_info().sched_entity();
    let rq = cpu_rq(cpu.data() as usize);
    se.force_mut().set_cfs(Arc::downgrade(&rq.cfs));
}

#[inline(never)]
pub fn sched_init() {
    // 初始化percpu变量
    unsafe {
        CPU_IRQ_TIME = Some(Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize));
        CPU_IRQ_TIME
            .as_mut()
            .unwrap()
            .resize_with(PerCpu::MAX_CPU_NUM as usize, || Box::leak(Box::default()));

        let mut cpu_runqueue = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        for cpu in 0..PerCpu::MAX_CPU_NUM as usize {
            let rq = Arc::new(CpuRunQueue::new(ProcessorId::new(cpu as u32)));
            rq.cfs.force_mut().set_rq(Arc::downgrade(&rq));
            cpu_runqueue.push(rq);
        }

        CPU_RUNQUEUE.init(PerCpuVar::new(cpu_runqueue).unwrap());
    };
}

#[inline]
pub fn send_resched_ipi(cpu: ProcessorId) {
    send_ipi(IpiKind::KickCpu, IpiTarget::Specified(cpu));
}
