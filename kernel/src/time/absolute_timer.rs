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
};

const PREPARED: u8 = 0;
const QUEUED: u8 = 1;
const FIRED: u8 = 2;
const CANCELLED: u8 = 3;
const CLOCK_SET_BUDGET: usize = 20;
type Key = (u64, usize);
type Queue = SpinLock<RBTree<Key, Arc<AbsoluteTimer>>>;

lazy_static! {
    static ref MONOTONIC: Queue = SpinLock::new(RBTree::new());
    static ref REALTIME: Queue = SpinLock::new(RBTree::new());
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

    fn queue(self) -> &'static Queue {
        match self {
            Self::Monotonic => &MONOTONIC,
            Self::Realtime => &REALTIME,
        }
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
        publish(timer.clock.queue(), &timer, self.node);
        // Sampling outside the queue lock prevents timekeeper/queue nesting.
        // A concurrent clock change is also covered by the next periodic tick.
        if timer.state.load(Ordering::Acquire) == QUEUED
            && timer.deadline_ns <= timer.clock.now_ns()
        {
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
        cancel_in(self.clock.queue(), self)
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

fn publish(queue: &Queue, timer: &Arc<AbsoluteTimer>, node: PreparedNode<Key, Arc<AbsoluteTimer>>) {
    let mut queue = queue.lock_irqsave();
    if timer.state.load(Ordering::Relaxed) == PREPARED {
        timer.state.store(QUEUED, Ordering::Release);
        queue.insert_prepared(node);
    } else {
        drop(queue);
        drop(node);
    }
}

fn cancel_in(queue: &Queue, timer: &AbsoluteTimer) -> bool {
    let mut queue = queue.lock_irqsave();
    let state = timer.state.load(Ordering::Relaxed);
    if state == FIRED || state == CANCELLED {
        return false;
    }
    timer.state.store(CANCELLED, Ordering::Release);
    let retired = queue.remove(&timer.key());
    drop(queue);
    drop(retired);
    true
}

fn pin_head(queue: &Queue) -> Result<Option<Arc<AbsoluteTimer>>, SystemError> {
    for _ in 0..10 {
        if let Ok(queue) = queue.try_lock_irqsave() {
            return Ok(queue.get_first().map(|(_, timer)| timer.clone()));
        }
    }
    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
}

/// The candidate must have existed before the caller sampled its domain clock.
/// Pinning it prevents address reuse; one-shot timers cannot rearm the same key.
fn take_sampled_due(
    queue: &Queue,
    candidate: &Arc<AbsoluteTimer>,
    now: u64,
) -> Option<Arc<AbsoluteTimer>> {
    if candidate.deadline_ns > now {
        return None;
    }
    let mut queue = queue.try_lock_irqsave().ok()?;
    if queue
        .get_first()
        .is_none_or(|(_, timer)| !Arc::ptr_eq(timer, candidate))
    {
        return None;
    }
    let (_, timer) = queue.pop_first().expect("pinned absolute timer head");
    timer.state.store(FIRED, Ordering::Release);
    Some(timer)
}

pub(super) fn run_one_due(domain: AbsoluteClock) -> bool {
    let Ok(Some(candidate)) = pin_head(domain.queue()) else {
        return false;
    };
    let now = domain.now_ns();
    let Some(timer) = take_sampled_due(domain.queue(), &candidate, now) else {
        return false;
    };
    // Both callback invocation and last references retire outside queue locks.
    timer.run();
    true
}

pub(super) fn first_expire(domain: AbsoluteClock) -> Result<Option<u64>, SystemError> {
    let Some(candidate) = pin_head(domain.queue())? else {
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
    fn activate_in(queue: &Queue, prepared: PreparedAbsoluteTimer) -> Arc<AbsoluteTimer> {
        let PreparedAbsoluteTimer { timer, node } = prepared;
        publish(queue, &timer, node);
        timer
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
    if take_sampled_due(&queue, &sampled, 100).is_some() || new.fired() {
        return Ok(false);
    }
    let sampled = pin_head(&queue)?.ok_or(SystemError::EINVAL)?;
    if take_sampled_due(&queue, &sampled, 50).is_some() {
        return Ok(false);
    }
    let due = take_sampled_due(&queue, &sampled, 75).ok_or(SystemError::EINVAL)?;
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
    let due = take_sampled_due(&queue, &sampled, 100).ok_or(SystemError::EINVAL)?;
    due.run();
    Ok(Arc::ptr_eq(&due, &fired)
        && due.fired()
        && counter.load(Ordering::Relaxed) == 2
        && pin_head(&queue)?.is_none()
        && crate::mm::oom::denial::probe_denied())
}
