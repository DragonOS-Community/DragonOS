//! One-shot absolute timers. Queue keys remain in their original clock domain.
//! Publication, cancellation and expiry consume only previously prepared storage.
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU8, Ordering};

use system_error::SystemError;

use super::{
    timekeeping,
    timer::{clock, relative_timer_jiffies, TimerFunction},
    PosixTimeSpec,
};
use crate::{
    exception::softirq::{softirq_vectors, SoftirqNumber},
    libs::{
        rbtree::{PreparedNode, RBTree},
        spinlock::SpinLock,
    },
    smp::cpu::ProcessorId,
};

use super::deadline::{self, DeadlineCallback, DeadlineEvent};

const PREPARED: u8 = 0;
const QUEUED: u8 = 1;
const FIRED: u8 = 2;
const CANCELLED: u8 = 3;
const CLOCK_SET_BUDGET: usize = 20;
type Key = (u64, usize);
type TimerTree = RBTree<Key, Arc<AbsoluteTimer>>;
type TimerNode = PreparedNode<Key, Arc<AbsoluteTimer>>;

struct AbsoluteBase {
    monotonic: TimerTree,
    realtime: TimerTree,
    // Installed once and never removed: event.cancel cannot retire its last Arc.
    driver: Option<Arc<DeadlineEvent>>,
}

impl AbsoluteBase {
    fn new() -> Self {
        Self {
            monotonic: RBTree::new(),
            realtime: RBTree::new(),
            driver: None,
        }
    }

    fn tree(&self, domain: AbsoluteClock) -> &TimerTree {
        match domain {
            AbsoluteClock::Monotonic => &self.monotonic,
            AbsoluteClock::Realtime => &self.realtime,
        }
    }

    fn head_deadline(&self, domain: AbsoluteClock) -> Option<u64> {
        self.tree(domain).get_first().map(|(key, _)| key.0)
    }

    fn tree_mut(&mut self, domain: AbsoluteClock) -> &mut TimerTree {
        match domain {
            AbsoluteClock::Monotonic => &mut self.monotonic,
            AbsoluteClock::Realtime => &mut self.realtime,
        }
    }
}

lazy_static! {
    static ref BASE: SpinLock<AbsoluteBase> = SpinLock::new(AbsoluteBase::new());
}

#[derive(Debug)]
struct TimerNotification;

impl DeadlineCallback for TimerNotification {
    fn expire(&self, _now_ns: u64) -> Option<u64> {
        softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        None
    }
}

/// Called once, after registering TIMER softirq and outside resource locks.
/// Failure leaves the existing periodic service available.
pub(super) fn init_deadline_driver() -> Result<(), SystemError> {
    let callback = Arc::try_new(TimerNotification).map_err(|_| SystemError::ENOMEM)?;
    let event =
        DeadlineEvent::try_new(ProcessorId::new(0), callback).map_err(|_| SystemError::ENOMEM)?;
    let mut base = BASE.lock_irqsave();
    assert!(
        base.driver.is_none(),
        "absolute timer driver initialized twice"
    );
    base.driver = Some(event);
    drop(base);
    refresh_deadline_driver();
    Ok(())
}

/// Callers must release timekeeper/owner queue locks before acquiring BASE.
pub(crate) fn refresh_deadline_driver() {
    let base = BASE.lock_irqsave();
    let raise = refresh_locked(&base);
    drop(base);
    if raise {
        softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
    }
}

// TK read is released before event.arm/cancel: owner queue programming itself
// reads TK. Never carry a TK guard into the deadline owner queue.
fn refresh_locked(base: &AbsoluteBase) -> bool {
    let driver = &base.driver;
    let mono_head = base.monotonic.get_first().map(|(key, _)| key.0);
    let real_head = base.realtime.get_first().map(|(key, _)| key.0);
    if mono_head.is_none() && real_head.is_none() {
        if let Some(event) = driver.as_ref() {
            event.cancel();
        }
        return false;
    }

    let (mono_now, real_now) = timekeeping::timer_clock_snapshot();
    let due = mono_head.is_some_and(|head| head <= mono_now)
        || real_head.is_some_and(|head| i128::from(head) <= real_now);
    if due {
        // Never turn an expired frontier into a stream of 1ns hardware IRQs.
        // The softirq's running owner rechecks this frontier after releasing
        // its gate, including when another CPU consumed a pending notification.
        if let Some(event) = driver.as_ref() {
            event.cancel();
        }
        return true;
    }

    let Some(event) = driver.as_ref() else {
        return false;
    };
    if !deadline::global_timer_capable() {
        event.cancel();
        return false;
    }
    let real_projected = real_head.map(|head| {
        let remaining = (i128::from(head) - real_now).min(i128::from(u64::MAX)) as u64;
        mono_now.saturating_add(remaining)
    });
    let next = match (mono_head, real_projected) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => unreachable!(),
    };
    // A capability/initialization transition can race admission. Keep the
    // original queue published and periodically serviced if arming fails.
    if event.arm(next).is_err() {
        event.cancel();
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbsoluteClock {
    Monotonic,
    Realtime,
}

impl AbsoluteClock {
    pub(crate) fn now_ns(self) -> u64 {
        match self {
            Self::Monotonic => timekeeping::monotonic_now(),
            Self::Realtime => timekeeping::realtime_now(),
        }
        .to_ktime_ns()
    }
}

#[derive(Debug)]
pub(crate) struct AbsoluteTimer {
    clock: AbsoluteClock,
    deadline_ns: u64,
    state: AtomicU8,
    callback: SpinLock<Option<Box<dyn TimerFunction>>>,
}

#[derive(Debug)]
pub(crate) struct PreparedAbsoluteTimer {
    timer: Arc<AbsoluteTimer>,
    node: PreparedNode<Key, Arc<AbsoluteTimer>>,
}

impl PreparedAbsoluteTimer {
    pub(crate) fn timer(&self) -> Arc<AbsoluteTimer> {
        self.timer.clone()
    }

    /// Infallible, one-shot publication. An earlier cancellation stays cancelled.
    pub(crate) fn activate(self) -> Arc<AbsoluteTimer> {
        let timer = self.timer;
        let mut base = BASE.lock_irqsave();
        let before = base.head_deadline(timer.clock);
        let rejected = publish(base.tree_mut(timer.clock), &timer, self.node);
        let raise = before != base.head_deadline(timer.clock) && refresh_locked(&base);
        drop(base);
        drop(rejected);
        if raise {
            softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        }
        timer
    }
}

impl AbsoluteTimer {
    /// The callback must be nonblocking and must not enter a futex registry;
    /// expiry may run in either TIMER softirq or clock-set syscall context.
    pub(crate) fn try_prepare(
        clock: AbsoluteClock,
        deadline: PosixTimeSpec,
        callback: Box<dyn TimerFunction>,
    ) -> Result<PreparedAbsoluteTimer, SystemError> {
        if !deadline.is_valid_timeout() {
            return Err(SystemError::EINVAL);
        }
        let timer = Arc::try_new(Self {
            clock,
            deadline_ns: deadline.to_ktime_ns(),
            state: AtomicU8::new(PREPARED),
            callback: SpinLock::new(Some(callback)),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let node =
            PreparedNode::try_new(timer.key(), timer.clone()).map_err(|_| SystemError::ENOMEM)?;
        Ok(PreparedAbsoluteTimer { timer, node })
    }

    /// Expiry is sticky, including after cancellation or a later backward step.
    pub(crate) fn fired(&self) -> bool {
        self.state.load(Ordering::Acquire) == FIRED
    }

    /// A callback already selected for expiry may still run after cancellation.
    pub(crate) fn cancel(&self) -> bool {
        let mut base = BASE.lock_irqsave();
        let before = base.head_deadline(self.clock);
        let (cancelled, retired) = cancel_in(base.tree_mut(self.clock), self);
        let raise = cancelled && before != base.head_deadline(self.clock) && refresh_locked(&base);
        drop(base);
        drop(retired);
        if raise {
            softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        }
        cancelled
    }

    fn key(&self) -> Key {
        (self.deadline_ns, self as *const Self as usize)
    }

    fn run(&self) {
        let callback = self.callback.lock_irqsave().take();
        if let Some(mut callback) = callback {
            // Only nonblocking callbacks are valid, as for the TIMER softirq.
            let _ = callback.run();
        }
    }
}

// Caller retires returned storage outside the base lock.
fn publish(tree: &mut TimerTree, timer: &Arc<AbsoluteTimer>, node: TimerNode) -> Option<TimerNode> {
    if timer.state.load(Ordering::Relaxed) == PREPARED {
        timer.state.store(QUEUED, Ordering::Release);
        tree.insert_prepared(node);
        None
    } else {
        Some(node)
    }
}

fn cancel_in(tree: &mut TimerTree, timer: &AbsoluteTimer) -> (bool, Option<TimerNode>) {
    let state = timer.state.load(Ordering::Relaxed);
    if state == FIRED || state == CANCELLED {
        return (false, None);
    }
    timer.state.store(CANCELLED, Ordering::Release);
    (true, tree.extract(&timer.key()))
}

fn pin_head(domain: AbsoluteClock) -> Result<Option<Arc<AbsoluteTimer>>, SystemError> {
    for _ in 0..10 {
        if let Ok(base) = BASE.try_lock_irqsave() {
            return Ok(base
                .tree(domain)
                .get_first()
                .map(|(_, timer)| timer.clone()));
        }
    }
    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
}

// Validate and sample under the same base guard. The detached node must be
// consumed after unlocking, including its allocation and last callback owner.
fn take_sampled_due(
    tree: &mut TimerTree,
    candidate: &Arc<AbsoluteTimer>,
    sample_clock: impl FnOnce() -> u64,
) -> Option<TimerNode> {
    if tree
        .get_first()
        .is_none_or(|(_, timer)| !Arc::ptr_eq(timer, candidate))
    {
        return None;
    }
    if candidate.deadline_ns > sample_clock() {
        return None;
    }
    candidate.state.store(FIRED, Ordering::Release);
    tree.extract(&candidate.key())
}

pub(super) fn run_one_due(domain: AbsoluteClock) -> bool {
    let Ok(Some(candidate)) = pin_head(domain) else {
        return false;
    };
    let retired = {
        let Ok(mut base) = BASE.try_lock_irqsave() else {
            return false;
        };
        take_sampled_due(base.tree_mut(domain), &candidate, || domain.now_ns())
    };
    let Some(node) = retired else {
        return false;
    };
    let (_, timer) = node.into_pair();
    timer.run();
    true
}

pub(super) fn first_expire(domain: AbsoluteClock) -> Result<Option<u64>, SystemError> {
    let Some(candidate) = pin_head(domain)? else {
        return Ok(None);
    };
    let remaining = candidate.deadline_ns.saturating_sub(domain.now_ns());
    Ok(Some(relative_timer_jiffies(clock(), remaining as u128)))
}

/// Called without the timekeeper write lock. No callback takes a PI registry
/// lock; expiry is serialized by removing a one-shot node, not by the ordinary
/// softirq's running gate, so concurrent clock-set/softirq drains are safe.
pub(super) fn clock_was_set() {
    for _ in 0..CLOCK_SET_BUDGET {
        if !run_one_due(AbsoluteClock::Realtime) {
            break;
        }
    }
    if first_expire(AbsoluteClock::Realtime)
        .is_ok_and(|deadline| deadline.is_some_and(|deadline| deadline <= clock()))
    {
        softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
    }
    refresh_deadline_driver();
}

/// Isolated clocks and queues: these checks never set wall time, publish a real
/// timer, raise a softirq, or rely on the machine's rate of progress.
#[cfg(feature = "mm_selftest")]
pub(super) fn queue_selftest() -> Result<bool, SystemError> {
    use core::sync::atomic::AtomicU32;

    #[derive(Debug)]
    struct Count(Arc<AtomicU32>);
    impl TimerFunction for Count {
        fn run(&mut self) -> Result<(), SystemError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    fn prepare(
        deadline: i64,
        counter: &Arc<AtomicU32>,
    ) -> Result<PreparedAbsoluteTimer, SystemError> {
        let callback = Box::try_new(Count(counter.clone())).map_err(|_| SystemError::ENOMEM)?;
        AbsoluteTimer::try_prepare(
            AbsoluteClock::Realtime,
            PosixTimeSpec::new(0, deadline),
            callback,
        )
    }
    type Queue = SpinLock<TimerTree>;
    fn activate_in(queue: &Queue, prepared: PreparedAbsoluteTimer) -> Arc<AbsoluteTimer> {
        let PreparedAbsoluteTimer { timer, node } = prepared;
        let rejected = super::absolute_timer::publish(&mut queue.lock_irqsave(), &timer, node);
        drop(rejected);
        timer
    }
    fn cancel_in(queue: &Queue, timer: &AbsoluteTimer) -> bool {
        let (changed, retired) = super::absolute_timer::cancel_in(&mut queue.lock_irqsave(), timer);
        drop(retired);
        changed
    }
    fn pin_head(queue: &Queue) -> Result<Option<Arc<AbsoluteTimer>>, SystemError> {
        Ok(queue
            .lock_irqsave()
            .get_first()
            .map(|(_, timer)| timer.clone()))
    }
    fn take_sampled_due(
        queue: &Queue,
        timer: &Arc<AbsoluteTimer>,
        now: impl FnOnce() -> u64,
    ) -> Option<Arc<AbsoluteTimer>> {
        let node = super::absolute_timer::take_sampled_due(&mut queue.lock_irqsave(), timer, now);
        node.map(|node| node.into_pair().1)
    }
    let queue = SpinLock::new(RBTree::new());
    let counter = Arc::try_new(AtomicU32::new(0)).map_err(|_| SystemError::ENOMEM)?;
    let old = activate_in(&queue, prepare(100, &counter)?);
    let sampled = pin_head(&queue)?.ok_or(SystemError::EINVAL)?;
    if !cancel_in(&queue, &old) || old.fired() {
        return Ok(false);
    }
    // A clock step and a newly published earlier event invalidate the old
    // candidate: its old sample of 100 cannot expire the new deadline of 75.
    let new = activate_in(&queue, prepare(75, &counter)?);
    if take_sampled_due(&queue, &sampled, || 100).is_some() || new.fired() {
        return Ok(false);
    }
    let sampled = pin_head(&queue)?.ok_or(SystemError::EINVAL)?;
    if take_sampled_due(&queue, &sampled, || 50).is_some() {
        return Ok(false);
    }
    let due = take_sampled_due(&queue, &sampled, || 75).ok_or(SystemError::EINVAL)?;
    if !due.fired() || cancel_in(&queue, &due) || pin_head(&queue)?.is_some() {
        return Ok(false);
    }
    // Firing is irreversible even if the wall clock subsequently steps back.
    due.run();
    due.run();
    if !due.fired() || counter.load(Ordering::Relaxed) != 1 {
        return Ok(false);
    }
    let cancelled = prepare(90, &counter)?;
    let cancelled_handle = cancelled.timer();
    if !cancel_in(&queue, &cancelled_handle) {
        return Ok(false);
    }
    activate_in(&queue, cancelled);
    if pin_head(&queue)?.is_some() {
        return Ok(false);
    }

    // Publication, removal and callback execution remain usable when the heap
    // rejects allocations. All callback/Arc/node owners were prepared earlier.
    let cancelled = prepare(80, &counter)?;
    let fired = prepare(70, &counter)?;
    let _deny = crate::mm::oom::denial::Scope::enter(true);
    let cancelled = activate_in(&queue, cancelled);
    let fired = activate_in(&queue, fired);
    if !cancel_in(&queue, &cancelled) {
        return Ok(false);
    }
    let sampled = pin_head(&queue)?.ok_or(SystemError::EINVAL)?;
    let due = take_sampled_due(&queue, &sampled, || 100).ok_or(SystemError::EINVAL)?;
    due.run();
    Ok(Arc::ptr_eq(&due, &fired)
        && due.fired()
        && counter.load(Ordering::Relaxed) == 2
        && pin_head(&queue)?.is_none()
        && crate::mm::oom::denial::probe_denied())
}
