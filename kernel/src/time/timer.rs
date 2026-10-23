use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{compiler_fence, AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use alloc::{boxed::Box, sync::Arc};
use log::{error, info, warn};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{
        softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
        InterruptArch,
    },
    libs::{
        rbtree::{PreparedNode, RBTree},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{ProcessControlBlock, ProcessManager},
    sched::{schedule, SchedMode},
};

use super::{
    absolute_timer::{self, AbsoluteClock},
    jiffies::NSEC_PER_JIFFY,
    timekeeping::update_wall_time,
};

const MAX_TIMEOUT: i64 = i64::MAX;
const TIMER_RUN_CYCLE_THRESHOLD: usize = 20;
static TIMER_JIFFIES: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    pub static ref TIMER_LIST: SpinLock<RBTree<(u64, usize), Arc<Timer>>> =
        SpinLock::new(RBTree::new());
}

/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync + Debug {
    fn run(&mut self) -> Result<(), SystemError>;
}
// # Jiffies结构体（注意这是一段时间的jiffies数而不是某一时刻的定时器时间片）

int_like!(Jiffies, u64);

impl Jiffies {
    /// ## 返回接下来的n_jiffies对应的定时器时间片
    pub fn timer_jiffies(&self) -> u64 {
        let result = TIMER_JIFFIES.load(Ordering::SeqCst) + self.data();
        result
    }
}

impl From<Jiffies> for Duration {
    /// # Jiffies转Duration
    ///
    /// ## 参数
    ///
    /// jiffies： 一段时间的jiffies数
    ///
    /// ### 返回值
    ///
    /// Duration： 这段时间的Duration形式
    fn from(jiffies: Jiffies) -> Self {
        let ms = jiffies.data() / 1_000_000 * NSEC_PER_JIFFY as u64;
        let result = Duration::from_millis(ms);
        result
    }
}

impl From<Duration> for Jiffies {
    /// # Duration 转 Jiffies
    ///
    /// ## 参数
    ///
    /// ms： 表示一段时间的Duration类型
    ///
    /// ### 返回值
    ///
    /// Jiffies结构体： 这段时间的Jiffies数
    fn from(ms: Duration) -> Self {
        let jiffies = ms.as_millis() as u64 * 1_000_000 / NSEC_PER_JIFFY as u64;
        let result = Jiffies::new(jiffies);
        result
    }
}

#[derive(Debug)]
/// WakeUpHelper函数对应的结构体
pub struct WakeUpHelper {
    pcb: Arc<ProcessControlBlock>,
}

impl WakeUpHelper {
    pub fn new(pcb: Arc<ProcessControlBlock>) -> Box<WakeUpHelper> {
        return Box::new(WakeUpHelper { pcb });
    }
}

impl TimerFunction for WakeUpHelper {
    fn run(&mut self) -> Result<(), SystemError> {
        ProcessManager::wakeup(&self.pcb).ok();
        return Ok(());
    }
}

#[derive(Debug)]
pub struct Timer {
    expire_jiffies: u64,
    inner: SpinLock<InnerTimer>,
}

/// Owns every allocation required to publish a timer. Kept outside Timer to
/// avoid a self-cycle; dropping an unpublished activation releases everything.
#[derive(Debug)]
pub(crate) struct PreparedTimer {
    timer: Arc<Timer>,
    node: PreparedNode<(u64, usize), Arc<Timer>>,
}

impl PreparedTimer {
    pub(crate) fn timer(&self) -> Arc<Timer> {
        self.timer.clone()
    }

    pub(crate) fn activate(self) {
        let mut timers = TIMER_LIST.lock_irqsave();
        // This token exclusively owns the unpublished activation.
        debug_assert!(timers.get(&self.timer.queue_key()).is_none());
        timers.insert_prepared(self.node);
    }
}

impl Timer {
    /// @brief 创建一个定时器（单位：ms）
    ///
    /// @param timer_func 定时器需要执行的函数对应的结构体
    ///
    /// @param expire_jiffies 定时器结束时刻
    ///
    /// @return 定时器结构体
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self> {
        Self::try_new(timer_func, expire_jiffies).expect("timer allocation failed")
    }

    fn try_new(
        timer_func: Box<dyn TimerFunction>,
        expire_jiffies: u64,
    ) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Timer {
            expire_jiffies,
            inner: SpinLock::new(InnerTimer {
                timer_func: Some(timer_func),
                triggered: false,
            }),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn try_prepare(
        timer_func: Box<dyn TimerFunction>,
        expire_jiffies: u64,
    ) -> Result<PreparedTimer, SystemError> {
        let timer = Self::try_new(timer_func, expire_jiffies)?;
        let node = PreparedNode::try_new(timer.queue_key(), timer.clone())
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(PreparedTimer { timer, node })
    }

    pub fn inner(&self) -> SpinLockGuard<'_, InnerTimer> {
        return self.inner.lock_irqsave();
    }

    fn queue_key(&self) -> (u64, usize) {
        (self.expire_jiffies, self as *const Self as usize)
    }

    pub fn expire_jiffies(&self) -> u64 {
        self.expire_jiffies
    }

    /// @brief 将定时器插入到定时器队列中
    pub fn activate(self: &Arc<Self>) {
        let node = PreparedNode::try_new(self.queue_key(), self.clone())
            .expect("timer queue allocation failed");
        let mut timers = TIMER_LIST.lock_irqsave();
        if timers.get(&self.queue_key()).is_some() {
            drop(timers);
            warn!("Timer already in list");
            return;
        }
        timers.insert_prepared(node);
    }

    #[inline]
    fn run(&self) {
        let mut timer = self.inner();
        timer.triggered = true;
        let func = timer.timer_func.take();
        drop(timer);
        let r = func.map(|mut f| f.run()).unwrap_or(Ok(()));
        if unlikely(r.is_err()) {
            error!(
                "Failed to run timer function: {self:?} {:?}",
                r.as_ref().err().unwrap()
            );
        }
    }

    /// ## 判断定时器是否已经触发
    pub fn timeout(&self) -> bool {
        self.inner().triggered
    }

    /// ## 取消定时器任务
    pub fn cancel(&self) -> bool {
        let removed = TIMER_LIST.lock_irqsave().remove(&self.queue_key());
        drop(removed);
        true
    }
}

/// 定时器类型
#[derive(Debug)]
pub struct InnerTimer {
    /// 定时器需要执行的函数结构体
    pub timer_func: Option<Box<dyn TimerFunction>>,
    /// 判断该计时器是否触发
    triggered: bool,
}

#[derive(Debug)]
pub struct DoTimerSoftirq {
    running: AtomicBool,
}

impl DoTimerSoftirq {
    pub fn new() -> Self {
        return DoTimerSoftirq {
            running: AtomicBool::new(false),
        };
    }

    fn set_run(&self) -> bool {
        let x = self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        return x.is_ok();
    }

    fn clear_run(&self) {
        self.running.store(false, Ordering::Release);
    }
}
impl SoftirqVec for DoTimerSoftirq {
    fn run(&self) {
        if !self.set_run() {
            return;
        }
        super::clocksource::run_due_watchdog();
        // Round-robin all three domains. An empty ordinary queue must not
        // suppress absolute timers, and one busy domain must not starve others.
        let mut completed = 0;
        let mut idle = 0;
        let mut domain = 0;
        while completed < TIMER_RUN_CYCLE_THRESHOLD && idle < 3 {
            let ran = match domain {
                0 => run_one_jiffies_timer(),
                1 => absolute_timer::run_one_due(AbsoluteClock::Monotonic),
                _ => absolute_timer::run_one_due(AbsoluteClock::Realtime),
            };
            domain = (domain + 1) % 3;
            if ran {
                completed += 1;
                idle = 0;
            } else {
                idle += 1;
            }
        }
        self.clear_run();
        if completed == TIMER_RUN_CYCLE_THRESHOLD {
            try_raise_timer_softirq();
        }
    }
}

/// 初始化系统定时器
#[inline(never)]
pub fn timer_init() {
    // FIXME 调用register_trap
    let do_timer_softirq = Arc::new(DoTimerSoftirq::new());
    softirq_vectors()
        .register_softirq(SoftirqNumber::TIMER, do_timer_softirq)
        .expect("Failed to register timer softirq");
    info!("timer initialized successfully");
}

/// 计算接下来n毫秒对应的定时器时间片
#[allow(dead_code)]
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64 {
    relative_timer_jiffies(clock(), u128::from(expire_ms) * 1_000_000)
}
/// 计算接下来n微秒对应的定时器时间片
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64 {
    relative_timer_jiffies(clock(), u128::from(expire_us) * 1_000)
}

pub(super) fn relative_timer_jiffies(now: u64, duration_ns: u128) -> u64 {
    if duration_ns == 0 {
        return now;
    }
    let ticks = duration_ns
        .div_ceil(NSEC_PER_JIFFY as u128)
        .min(u64::MAX as u128) as u64;
    // `now` identifies a tick already in progress. Include that partial tick
    // so a positive real duration cannot expire before its requested deadline.
    now.saturating_add(ticks.saturating_add(1))
}

/// @brief 让pcb休眠timeout个jiffies
///
/// @param timeout 需要休眠的时间(单位：jiffies)
///
/// @return Ok(i64) 剩余需要休眠的时间(单位：jiffies)
///
/// @return Err(SystemError) 错误码
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError> {
    // debug!("schedule_timeout");
    if timeout == MAX_TIMEOUT {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).ok();
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
        return Ok(MAX_TIMEOUT);
    } else if timeout < 0 {
        error!("timeout can't less than 0");
        return Err(SystemError::EINVAL);
    } else {
        // 禁用中断，防止在这段期间发生调度，造成死锁
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        timeout += TIMER_JIFFIES.load(Ordering::SeqCst) as i64;
        let timer = Timer::new(
            WakeUpHelper::new(ProcessManager::current_pcb()),
            timeout as u64,
        );
        ProcessManager::mark_sleep(true).ok();
        timer.activate();

        drop(irq_guard);

        schedule(SchedMode::SM_NONE);

        // 如果定时器没有超时（被信号中断或其他原因唤醒），则取消定时器
        if !timer.timeout() {
            timer.cancel();
        }

        let time_remaining: i64 = timeout - TIMER_JIFFIES.load(Ordering::SeqCst) as i64;
        if time_remaining >= 0 {
            // 被提前唤醒，返回剩余时间
            return Ok(time_remaining);
        } else {
            return Ok(0);
        }
    }
}

/// Return the earliest deadline, distinguishing an empty queue from deadline 0.
/// Contention remains bounded because the caller runs in timer IRQ context.
pub fn timer_get_first_expire() -> Result<Option<u64>, SystemError> {
    earliest_expire([
        first_expire_in(&TIMER_LIST),
        absolute_timer::first_expire(AbsoluteClock::Monotonic),
        absolute_timer::first_expire(AbsoluteClock::Realtime),
    ])
}

fn earliest_expire(
    candidates: [Result<Option<u64>, SystemError>; 3],
) -> Result<Option<u64>, SystemError> {
    let mut earliest: Option<u64> = None;
    let mut contended = false;
    for candidate in candidates {
        match candidate {
            Ok(Some(deadline)) => {
                earliest = Some(earliest.map_or(deadline, |old| old.min(deadline)));
            }
            Ok(None) => {}
            Err(_) => contended = true,
        }
    }
    if earliest.is_none() && contended {
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    } else {
        Ok(earliest)
    }
}

fn run_one_jiffies_timer() -> bool {
    let Ok(mut queue) = TIMER_LIST.try_lock_irqsave() else {
        return false;
    };
    let timer = pop_expired(&mut queue, clock());
    drop(queue);
    let Some(timer) = timer else {
        return false;
    };
    timer.run();
    true
}

fn first_expire_in(
    timers: &SpinLock<RBTree<(u64, usize), Arc<Timer>>>,
) -> Result<Option<u64>, SystemError> {
    for _ in 0..10 {
        if let Ok(timer_list) = timers.try_lock_irqsave() {
            return Ok(timer_list.get_first().map(|(key, _)| key.0));
        }
    }
    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
}

/// Select and retire one due timer while the caller holds the queue lock.
fn pop_expired(timers: &mut RBTree<(u64, usize), Arc<Timer>>, now: u64) -> Option<Arc<Timer>> {
    if timers.get_first().is_none_or(|(key, _)| key.0 > now) {
        return None;
    }
    timers.pop_first().map(|(_, timer)| timer)
}

/// 检查是否需要触发定时器软中断，如果需要则触发
pub fn try_raise_timer_softirq() {
    if super::clocksource::watchdog_due(clock()) {
        softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        return;
    }
    if let Ok(Some(first_expire)) = timer_get_first_expire() {
        if first_expire <= clock() {
            softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        }
    }
}

/// 处理本地定时器中断
pub fn run_local_timer() {
    assert!(!CurrentIrqArch::is_irq_enabled());
    try_raise_timer_softirq();
}

/// 更新系统时间片
pub fn update_timer_jiffies(add_jiffies: u64) -> u64 {
    let prev = TIMER_JIFFIES.fetch_add(add_jiffies, Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    update_wall_time();

    compiler_fence(Ordering::SeqCst);
    let now = prev + add_jiffies;
    crate::mm::page::reclaim_wait::tick(now);
    crate::mm::fault_recovery::tick();
    return now;
}

pub fn clock() -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst);
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn heap_denied_activation_selftest() -> Result<bool, SystemError> {
    use crate::mm::oom::denial;
    #[derive(Debug)]
    struct Noop;
    impl TimerFunction for Noop {
        fn run(&mut self) -> Result<(), SystemError> {
            Ok(())
        }
    }
    let callback = Box::try_new(Noop).map_err(|_| SystemError::ENOMEM)?;
    let timer = Timer::try_new(callback, clock().saturating_add(u64::MAX / 2))?;
    let prepared =
        PreparedNode::try_new(timer.queue_key(), timer.clone()).map_err(|_| SystemError::ENOMEM)?;
    let rejected = {
        let _deny = denial::Scope::enter(true);
        PreparedNode::try_new(timer.queue_key(), timer.clone()).is_err()
    };
    let token = PreparedTimer {
        timer: timer.clone(),
        node: prepared,
    };
    let _deny = denial::Scope::enter(true);
    let before = denial::probe_denied();
    token.activate();
    let registered = TIMER_LIST.lock_irqsave().get(&timer.queue_key()).is_some();
    timer.cancel();
    let removed = TIMER_LIST.lock_irqsave().get(&timer.queue_key()).is_none();
    Ok(before && rejected && registered && removed && denial::probe_denied())
}

/// Isolated queue checks invoked only by the existing feature selftest report.
/// No production queue membership, jiffies, pending bits or callbacks are changed.
#[cfg(feature = "mm_selftest")]
pub(crate) fn queue_frontier_selftest() -> Result<bool, SystemError> {
    if earliest_expire([Ok(None), Ok(None), Ok(Some(0))]) != Ok(Some(0))
        || earliest_expire([
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Ok(Some(0)),
            Ok(None),
        ]) != Ok(Some(0))
        || !absolute_timer::queue_selftest()?
    {
        return Ok(false);
    }
    #[derive(Debug)]
    struct Noop;
    impl TimerFunction for Noop {
        fn run(&mut self) -> Result<(), SystemError> {
            Ok(())
        }
    }

    let tick_ns = NSEC_PER_JIFFY as u128;
    let now = 7;
    if relative_timer_jiffies(now, 0) != now
        || relative_timer_jiffies(now, 1) != now + 2
        || relative_timer_jiffies(now, tick_ns - 1) != now + 2
        || relative_timer_jiffies(now, tick_ns) != now + 2
        || relative_timer_jiffies(now, tick_ns + 1) != now + 3
        || relative_timer_jiffies(u64::MAX - 1, 1) != u64::MAX
        || relative_timer_jiffies(now, u128::MAX) != u64::MAX
    {
        return Ok(false);
    }
    // Convert wide before multiplication: saturating a millisecond value to
    // u64 microseconds first would silently shorten a large legal timeout.
    let max_us = relative_timer_jiffies(now, u128::from(u64::MAX) * 1_000);
    let max_ms = relative_timer_jiffies(now, u128::from(u64::MAX) * 1_000_000);
    if max_us <= now || max_ms <= max_us || max_ms == u64::MAX {
        return Ok(false);
    }

    let timers = SpinLock::new(RBTree::new());
    if first_expire_in(&timers) != Ok(None) {
        return Ok(false);
    }
    let mut owners = alloc::vec::Vec::new();
    owners
        .try_reserve_exact(5)
        .map_err(|_| SystemError::ENOMEM)?;
    for deadline in [20, 10, 10, 0, u64::MAX] {
        let callback = Box::try_new(Noop).map_err(|_| SystemError::ENOMEM)?;
        let timer = Timer::try_new(callback, deadline)?;
        let node = PreparedNode::try_new(timer.queue_key(), timer.clone())
            .map_err(|_| SystemError::ENOMEM)?;
        timers.lock_irqsave().insert_prepared(node);
        owners.push(timer);
    }
    if first_expire_in(&timers) != Ok(Some(0)) {
        return Ok(false);
    }
    {
        let mut queue = timers.lock_irqsave();
        if first_expire_in(&timers) != Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            || !pop_expired(&mut queue, 0).is_some_and(|timer| Arc::ptr_eq(&timer, &owners[3]))
            || pop_expired(&mut queue, 9).is_some()
        {
            return Ok(false);
        }
        // Cancellation exposes the other timer at the same deadline.
        queue.remove(&owners[1].queue_key());
    }
    if first_expire_in(&timers) != Ok(Some(10)) {
        return Ok(false);
    }
    {
        let mut queue = timers.lock_irqsave();
        if !pop_expired(&mut queue, 10).is_some_and(|timer| Arc::ptr_eq(&timer, &owners[2]))
            || pop_expired(&mut queue, 19).is_some()
            || !pop_expired(&mut queue, 20).is_some_and(|timer| Arc::ptr_eq(&timer, &owners[0]))
            || pop_expired(&mut queue, u64::MAX - 1).is_some()
        {
            return Ok(false);
        }
    }
    if first_expire_in(&timers) != Ok(Some(u64::MAX)) {
        return Ok(false);
    }
    let last = pop_expired(&mut timers.lock_irqsave(), u64::MAX);
    Ok(last.is_some_and(|timer| Arc::ptr_eq(&timer, &owners[4]))
        && first_expire_in(&timers) == Ok(None))
}
