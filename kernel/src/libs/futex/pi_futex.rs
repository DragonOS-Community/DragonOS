//! PI ownership graph. The registry serializes graph changes before task.pi/rq;
//! it never waits for user memory or task exit while held. Only contended locks
//! have state: the last frontend reference retires it, allowing libc fastpaths.
use crate::{
    libs::{
        futex::{
            constant::*,
            futex::{Futex, FutexAccess, FutexData, FutexHashBucket, FutexKey},
        },
        prepared_queue::{Node, Queue},
        wait_queue::{Waiter, Waker},
    },
    mm::VirtAddr,
    process::{ProcessControlBlock, ProcessManager, RawPid},
    time::{
        absolute_timer::{AbsoluteClock, AbsoluteTimer, PreparedAbsoluteTimer},
        timer::TimerFunction,
        PosixTimeSpec,
    },
};
use alloc::{boxed::Box, sync::Arc};
use core::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicI32, AtomicU8, Ordering},
};
use hashbrown::HashMap;
use system_error::SystemError;

const ALIVE: u8 = 0;
const CLEANING: u8 = 1;
const DEAD: u8 = 2;
const PENDING: u8 = 0;
const ACQUIRED: u8 = 1;
const NEEDS_FIXUP: u8 = 2;
const MAX_CHAIN: usize = 1024; // Linux rt_mutex max_lock_depth.

type TaskId = usize;
fn identity(task: &Arc<ProcessControlBlock>) -> TaskId {
    Arc::as_ptr(task) as usize
}

pub(crate) struct PiTask {
    task: Arc<ProcessControlBlock>,
    blocked: Option<FutexKey>,
    owned: Queue<FutexKey>,
    upstream_depth: usize,
}
impl PiTask {
    fn prepare(task: Arc<ProcessControlBlock>) -> Result<Box<Node<Self>>, SystemError> {
        Node::try_new(Self {
            task,
            blocked: None,
            owned: Queue::new(),
            upstream_depth: 0,
        })
    }
}

pub(crate) struct FutexRegistry {
    buckets: HashMap<FutexKey, FutexHashBucket>,
    tasks: HashMap<TaskId, Box<Node<PiTask>>>,
    // Nodes keep the final task references alive until the registry guard drops.
    pub(super) retired: Queue<PiTask>,
}
impl FutexRegistry {
    pub(crate) fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            tasks: HashMap::new(),
            retired: Queue::new(),
        }
    }
    fn task(&self, task: &Arc<ProcessControlBlock>) -> Option<&PiTask> {
        self.tasks.get(&identity(task)).map(|node| node.value())
    }
    fn task_mut(&mut self, task: &Arc<ProcessControlBlock>) -> Option<&mut PiTask> {
        self.tasks
            .get_mut(&identity(task))
            .map(|node| node.value_mut())
    }
    fn retire_task_if_idle(&mut self, task: &Arc<ProcessControlBlock>) {
        if self
            .task(task)
            .is_some_and(|entry| entry.blocked.is_none() && entry.owned.is_empty())
        {
            self.retired
                .push_node(self.tasks.remove(&identity(task)).unwrap());
        }
    }
    fn top(&self, key: &FutexKey) -> Option<Arc<PiWaiter>> {
        let bucket = self.buckets.get(key)?;
        // min_by_key retains the first FIFO member at equal priority.
        bucket
            .pi_waiters
            .iter()
            .min_by_key(|w| w.order_priority())
            .cloned()
    }
    fn donor(&self, task: &Arc<ProcessControlBlock>) -> Option<i32> {
        self.task(task)?
            .owned
            .iter()
            .filter_map(|key| self.top(key))
            .min_by_key(|waiter| waiter.order_priority())
            .map(|waiter| waiter.task.sched_info().prio())
    }
    fn refresh_donors(&mut self, task: &Arc<ProcessControlBlock>) {
        // An owner's donor tree is represented by its already prepared links.
        // Reinsert changed lock tops after existing equal-priority donors, just
        // as rt_mutex's FIFO tie ordering; never allocate during propagation.
        let mut owned = core::mem::take(&mut self.task_mut(task).unwrap().owned);
        let mut stable = Queue::new();
        let mut changed = Queue::new();
        while let Some(node) = owned.pop_node() {
            let key = node.value();
            let top = self
                .top(key)
                .map(|w| (Arc::as_ptr(&w) as usize, w.order_priority()));
            let bucket = self.buckets.get_mut(key).unwrap();
            if bucket.pi_top == top {
                stable.push_node(node);
            } else {
                bucket.pi_top = top;
                changed.push_node(node);
            }
        }
        stable.append(&mut changed);
        self.task_mut(task).unwrap().owned = stable;
    }
    fn refresh_depth(&mut self, task: &Arc<ProcessControlBlock>) {
        let depth = self
            .task(task)
            .unwrap()
            .owned
            .iter()
            .filter_map(|key| self.buckets.get(key))
            .flat_map(|bucket| bucket.pi_waiters.iter())
            .map(|waiter| self.task(&waiter.task).unwrap().upstream_depth + 1)
            .max()
            .unwrap_or(0);
        self.task_mut(task).unwrap().upstream_depth = depth;
    }
    fn refresh_blocked_waiter(&mut self, task: &Arc<ProcessControlBlock>) {
        let Some(key) = self.task(task).and_then(|entry| entry.blocked.clone()) else {
            return;
        };
        let priority = PiWaiter::task_order_priority(task);
        let bucket = self.buckets.get_mut(&key).unwrap();
        if bucket
            .pi_waiters
            .iter()
            .find(|waiter| Arc::ptr_eq(&waiter.task, task))
            .is_none_or(|waiter| waiter.order_priority() == priority)
        {
            return;
        }
        let mut changed = bucket.pi_waiters.extract_if(|waiter| {
            Arc::ptr_eq(&waiter.task, task) && waiter.order_priority() != priority
        });
        if let Some(node) = changed.pop_node() {
            node.value()
                .order_priority
                .store(priority, Ordering::Relaxed);
            bucket.pi_waiters.push_node(node);
        }
        debug_assert!(changed.is_empty());
    }
    fn validate_chain(
        &self,
        requester: &Arc<ProcessControlBlock>,
        mut owner: Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let mut depth = self.task(requester).map_or(0, |task| task.upstream_depth) + 1;
        loop {
            if depth > MAX_CHAIN {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            if Arc::ptr_eq(&owner, requester) {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            let Some(key) = self.task(&owner).and_then(|entry| entry.blocked.as_ref()) else {
                return Ok(());
            };
            let Some(next) = self
                .buckets
                .get(key)
                .and_then(|bucket| bucket.pi_owner.as_ref())
            else {
                return Ok(());
            };
            owner = next.clone();
            depth += 1;
        }
    }
    fn move_owner(
        &mut self,
        key: &FutexKey,
        next: Option<Arc<ProcessControlBlock>>,
        prepared: &mut Option<Box<Node<FutexKey>>>,
    ) {
        let old = self.buckets.get_mut(key).unwrap().pi_owner.take();
        let mut owned = if let Some(old) = &old {
            self.task_mut(old)
                .expect("PI owner indexed")
                .owned
                .extract_if(|owned| owned == key)
        } else {
            Queue::new()
        };
        if next.is_some() && owned.is_empty() {
            owned.push_node(prepared.take().expect("prepared PI owner link"));
        }
        if let Some(next) = &next {
            self.task_mut(next)
                .expect("PI successor indexed")
                .owned
                .append(&mut owned);
        }
        self.buckets.get_mut(key).unwrap().pi_owner = next.clone();
        if let Some(old) = old {
            refresh_priority_locked(self, &old);
            self.retire_task_if_idle(&old);
        }
        if let Some(next) = next {
            refresh_priority_locked(self, &next);
        }
    }
    fn put_state(&mut self, key: &FutexKey) {
        let bucket = self.buckets.get_mut(key).expect("referenced PI state");
        bucket.pi_refs -= 1;
        if bucket.pi_refs == 0 {
            debug_assert!(bucket.pi_waiters.is_empty());
            self.move_owner(key, None, &mut None);
            let retired = FutexData::remove_empty_locked(self, key);
            drop(retired);
        }
    }
}
impl Deref for FutexRegistry {
    type Target = HashMap<FutexKey, FutexHashBucket>;
    fn deref(&self) -> &Self::Target {
        &self.buckets
    }
}
impl DerefMut for FutexRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buckets
    }
}

/// Base policy changes and futex changes share one stable dependency snapshot.
/// Boot policy initialization precedes the futex registry and has no PI edges.
pub(crate) fn with_pi_registry<R>(f: impl FnOnce(Option<&mut FutexRegistry>) -> R) -> R {
    if !FutexData::initialized() {
        return f(None);
    }
    f(Some(&mut FutexData::futex_map()))
}

pub(crate) fn refresh_priority_locked(
    registry: &mut FutexRegistry,
    start: &Arc<ProcessControlBlock>,
) {
    if registry.task(start).is_none() {
        return;
    }
    let mut task = start.clone();
    for _ in 0..=MAX_CHAIN {
        registry.refresh_depth(&task);
        registry.refresh_donors(&task);
        let donor = registry.donor(&task);
        // An unplaced fork child cannot own/await a PI futex; exit retains its rq.
        ProcessManager::set_pi_priority(&task, donor).expect("placed PI task");
        registry.refresh_blocked_waiter(&task);
        let Some(key) = registry.task(&task).and_then(|entry| entry.blocked.clone()) else {
            return;
        };
        let Some(owner) = registry
            .buckets
            .get(&key)
            .and_then(|bucket| bucket.pi_owner.clone())
        else {
            return;
        };
        task = owner;
    }
    unreachable!("PI registration validates acyclic bounded dependency graph")
}

#[derive(Debug)]
struct WakerTimer {
    waker: Arc<Waker>,
}
impl TimerFunction for WakerTimer {
    fn run(&mut self) -> Result<(), SystemError> {
        self.waker.wake();
        Ok(())
    }
}

pub(super) struct PiWaiter {
    pub(super) waker: Arc<Waker>,
    task: Arc<ProcessControlBlock>,
    handoff: AtomicU8,
    order_priority: AtomicI32,
}
impl PiWaiter {
    fn task_order_priority(task: &Arc<ProcessControlBlock>) -> i32 {
        if task.sched_info().prio() < 100 {
            task.sched_info().prio()
        } else {
            120
        }
    }
    fn order_priority(&self) -> i32 {
        self.order_priority.load(Ordering::Relaxed)
    }
}
struct PreparedPiWait {
    waiter: Waiter,
    object: Arc<PiWaiter>,
    node: Option<Box<Node<Arc<PiWaiter>>>>,
    task_node: Option<Box<Node<PiTask>>>,
    owner_node: Option<Box<Node<PiTask>>>,
    owned_node: Option<Box<Node<FutexKey>>>,
    activation: Option<PreparedAbsoluteTimer>,
    timer: Option<Arc<AbsoluteTimer>>,
}
impl PreparedPiWait {
    fn try_new(
        task: Arc<ProcessControlBlock>,
        owner: Arc<ProcessControlBlock>,
        key: &FutexKey,
        clock: AbsoluteClock,
        deadline: Option<PosixTimeSpec>,
    ) -> Result<Self, SystemError> {
        let (waiter, waker) = Waiter::try_new_pair()?;
        let object = Arc::try_new(PiWaiter {
            waker: waker.clone(),
            task: task.clone(),
            handoff: AtomicU8::new(PENDING),
            order_priority: AtomicI32::new(PiWaiter::task_order_priority(&task)),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let node = Some(Node::try_new(object.clone())?);
        let activation = if let Some(deadline) = deadline {
            Some(AbsoluteTimer::try_prepare(
                clock,
                deadline,
                Box::try_new(WakerTimer { waker }).map_err(|_| SystemError::ENOMEM)?,
            )?)
        } else {
            None
        };
        let timer = activation.as_ref().map(PreparedAbsoluteTimer::timer);
        Ok(Self {
            waiter,
            object,
            node,
            task_node: Some(PiTask::prepare(task)?),
            owner_node: Some(PiTask::prepare(owner)?),
            owned_node: Some(Node::try_new(key.clone())?),
            activation,
            timer,
        })
    }
}
impl Drop for PreparedPiWait {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.cancel();
        }
    }
}

pub(crate) fn begin_task_cleanup(task: &Arc<ProcessControlBlock>) {
    let _registry = FutexData::futex_map();
    task.futex_lifecycle.store(CLEANING, Ordering::Release);
}
pub(crate) fn finish_task_cleanup(task: &Arc<ProcessControlBlock>) {
    // Each transferred waiter node doubles as a deferred wake node; no exit allocation.
    let mut wakes = Queue::new();
    let mut registry = FutexData::futex_map();
    loop {
        let Some(key) = registry
            .task(task)
            .and_then(|entry| entry.owned.front().cloned())
        else {
            break;
        };
        let next = registry.top(&key);
        if let Some(next) = &next {
            let mut removed = registry
                .buckets
                .get_mut(&key)
                .unwrap()
                .pi_waiters
                .extract_if(|w| Arc::ptr_eq(w, next));
            wakes.append(&mut removed);
            registry.task_mut(&next.task).unwrap().blocked = None;
        }
        registry.move_owner(&key, next.as_ref().map(|next| next.task.clone()), &mut None);
        if let Some(next) = next {
            next.handoff.store(NEEDS_FIXUP, Ordering::Release);
        }
    }
    registry.retire_task_if_idle(task);
    task.futex_lifecycle.store(DEAD, Ordering::Release);
    drop(registry);
    task.futex_cleanup_wait.notify_all();
    while let Some(node) = wakes.pop_node() {
        node.value().waker.wake();
    }
}
pub(crate) fn resume_after_exec(task: &Arc<ProcessControlBlock>) {
    let _registry = FutexData::futex_map();
    task.futex_lifecycle.store(ALIVE, Ordering::Release);
}

#[cfg(feature = "mm_selftest")]
#[path = "pi_selftest.rs"]
mod selftest;

impl Futex {
    #[cfg(feature = "mm_selftest")]
    pub(crate) fn failed_pi_registration_selftest() -> Result<bool, SystemError> {
        selftest::run()
    }

    fn pi_clock(flags: FutexFlag) -> AbsoluteClock {
        if flags.contains(FutexFlag::FLAGS_CLOCKRT) {
            AbsoluteClock::Realtime
        } else {
            AbsoluteClock::Monotonic
        }
    }
    fn lookup_pi_owner(tid: u32) -> Result<Arc<ProcessControlBlock>, SystemError> {
        let task = ProcessManager::find_task_by_vpid(RawPid::new(tid as usize))
            .ok_or(SystemError::ESRCH)?;
        if task.is_kthread() {
            return Err(SystemError::EPERM);
        }
        Ok(task)
    }
    fn validate_pi_state(
        registry: &FutexRegistry,
        key: &FutexKey,
        value: u32,
    ) -> Result<(), SystemError> {
        if let Some(bucket) = registry.buckets.get(key) {
            if !bucket.chain.is_empty() {
                return Err(SystemError::EINVAL);
            }
            if let Some(owner) = &bucket.pi_owner {
                let visible = owner.task_pid_vnr().data() as u32;
                let word_tid = value & FUTEX_TID_MASK;
                if word_tid != visible && !(value & FUTEX_OWNER_DIED != 0 && word_tid == 0) {
                    return Err(SystemError::EINVAL);
                }
            }
        }
        Ok(())
    }
    pub fn futex_lock_pi(
        uaddr: VirtAddr,
        flags: FutexFlag,
        deadline: Option<PosixTimeSpec>,
    ) -> Result<usize, SystemError> {
        Self::lock_pi(uaddr, flags, deadline, false)
    }
    pub fn futex_trylock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        Self::lock_pi(uaddr, flags, None, true)
    }
    fn lock_pi(
        uaddr: VirtAddr,
        flags: FutexFlag,
        deadline: Option<PosixTimeSpec>,
        try_only: bool,
    ) -> Result<usize, SystemError> {
        if deadline.is_some_and(|deadline| !deadline.is_valid_timeout()) {
            return Err(SystemError::EINVAL);
        }
        let task = ProcessManager::current_pcb();
        let tid = task.task_pid_vnr().data() as u32;
        let clock = Self::pi_clock(flags);
        let mut prepared: Option<PreparedPiWait> = None;
        loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;
            let mut registry = FutexData::futex_map();
            let value = match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let owner_tid = value & FUTEX_TID_MASK;
            if owner_tid == tid {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            Self::validate_pi_state(&registry, &key, value)?;
            if owner_tid == 0
                && registry
                    .buckets
                    .get(&key)
                    .is_none_or(|b| b.pi_owner.is_none())
            {
                match Self::futex_cmpxchg_user(uaddr, value, tid | (value & FUTEX_OWNER_DIED)) {
                    Ok(observed) if observed == value => return Ok(0),
                    Ok(_) => continue,
                    Err(SystemError::EFAULT) => {
                        drop(registry);
                        Self::fault_in_user_writeable_u32(uaddr)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            let owner = if let Some(owner) = registry
                .buckets
                .get(&key)
                .and_then(|bucket| bucket.pi_owner.clone())
            {
                owner
            } else {
                match Self::lookup_pi_owner(owner_tid) {
                    Ok(owner) => owner,
                    Err(error) => {
                        // Reject a stale lookup only if the word is still the one sampled.
                        match Self::futex_cmpxchg_user(uaddr, value, value) {
                            Ok(observed) if observed == value => return Err(error),
                            Ok(_) => continue,
                            Err(SystemError::EFAULT) => {
                                drop(registry);
                                Self::fault_in_user_writeable_u32(uaddr)?;
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            };
            match owner.futex_lifecycle.load(Ordering::Acquire) {
                CLEANING => {
                    drop(registry);
                    owner.futex_cleanup_wait.wait_uninterruptible(|| {
                        (owner.futex_lifecycle.load(Ordering::Acquire) != CLEANING).then_some(())
                    })?;
                    continue;
                }
                DEAD => return Err(SystemError::ESRCH),
                _ => {}
            }
            if try_only {
                match Self::futex_cmpxchg_user(uaddr, value, value) {
                    Ok(observed) if observed == value => {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    }
                    Ok(_) => continue,
                    Err(SystemError::EFAULT) => {
                        drop(registry);
                        Self::fault_in_user_writeable_u32(uaddr)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            registry.validate_chain(&task, owner.clone())?;
            if prepared
                .as_ref()
                .and_then(|p| p.timer.as_ref())
                .is_some_and(|timer| timer.fired())
                || deadline.is_some_and(|deadline| deadline.to_ktime_ns() <= clock.now_ns())
            {
                return Err(SystemError::ETIMEDOUT);
            }
            let valid_preparation = prepared.as_ref().is_some_and(|prepared| {
                prepared
                    .owner_node
                    .as_ref()
                    .is_some_and(|node| Arc::ptr_eq(&node.value().task, &owner))
                    && prepared
                        .owned_node
                        .as_ref()
                        .is_some_and(|node| node.value() == &key)
            });
            if !valid_preparation {
                drop(registry);
                prepared = Some(PreparedPiWait::try_new(
                    task.clone(),
                    owner,
                    &key,
                    clock,
                    deadline,
                )?);
                continue;
            }
            registry
                .buckets
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            registry
                .tasks
                .try_reserve(2)
                .map_err(|_| SystemError::ENOMEM)?;
            match Self::futex_cmpxchg_user(uaddr, value, value | FUTEX_WAITERS) {
                Ok(observed) if observed == value => {}
                Ok(_) => continue,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let prepared = prepared.as_mut().unwrap();
            registry
                .tasks
                .entry(identity(&owner))
                .or_insert_with(|| prepared.owner_node.take().unwrap());
            registry
                .tasks
                .entry(identity(&task))
                .or_insert_with(|| prepared.task_node.take().unwrap());
            registry
                .buckets
                .entry(key.clone())
                .or_insert_with(FutexHashBucket::new);
            if registry.buckets.get(&key).unwrap().pi_owner.is_none() {
                registry.move_owner(&key, Some(owner.clone()), &mut prepared.owned_node);
            }
            registry.task_mut(&task).unwrap().blocked = Some(key.clone());
            let bucket = registry.buckets.get_mut(&key).unwrap();
            bucket.pi_refs += 1;
            prepared
                .object
                .order_priority
                .store(PiWaiter::task_order_priority(&task), Ordering::Relaxed);
            bucket.pi_waiters.push_node(prepared.node.take().unwrap());
            refresh_priority_locked(&mut registry, &owner);
            if let Some(activation) = prepared.activation.take() {
                activation.activate();
            }
            drop(registry);
            return Self::wait_pi_registered(uaddr, &key, prepared);
        }
    }
    fn wait_pi_registered(
        uaddr: VirtAddr,
        key: &FutexKey,
        prepared: &PreparedPiWait,
    ) -> Result<usize, SystemError> {
        loop {
            let waited = if prepared.object.handoff.load(Ordering::Acquire) == PENDING {
                prepared.waiter.wait(true)
            } else {
                Ok(())
            };
            let expired = prepared.timer.as_ref().is_some_and(|timer| timer.fired());
            let interrupted = waited.is_err() || ProcessManager::current_pcb().has_pending_signal();
            let mut registry = FutexData::futex_map();
            match prepared.object.handoff.load(Ordering::Acquire) {
                ACQUIRED => {
                    registry.put_state(key);
                    return Ok(0);
                }
                NEEDS_FIXUP => {
                    // Keep the actual kernel owner on EFAULT. A remaining waiter
                    // can still inherit from it and its eventual exit can hand off.
                    let result = Self::fixup_pi_owner(uaddr, key, &mut registry, &prepared.object);
                    if result == Err(SystemError::EFAULT) {
                        drop(registry);
                        if Self::fault_in_user_writeable_u32(uaddr).is_ok() {
                            continue;
                        }
                        let mut registry = FutexData::futex_map();
                        registry.put_state(key);
                        return Err(SystemError::EFAULT);
                    }
                    registry.put_state(key);
                    return result;
                }
                _ => {}
            }
            if !expired && !interrupted {
                drop(registry);
                continue;
            }
            let bucket = registry.buckets.get_mut(key).unwrap();
            let retired = bucket
                .pi_waiters
                .extract_if(|w| Arc::ptr_eq(w, &prepared.object));
            let owner = bucket.pi_owner.clone();
            registry.task_mut(&prepared.object.task).unwrap().blocked = None;
            if let Some(owner) = owner {
                refresh_priority_locked(&mut registry, &owner);
            }
            registry.put_state(key);
            registry.retire_task_if_idle(&prepared.object.task);
            drop(registry);
            drop(retired);
            return Err(if expired {
                SystemError::ETIMEDOUT
            } else {
                SystemError::ERESTARTNOINTR
            });
        }
    }
    fn fixup_pi_owner(
        uaddr: VirtAddr,
        key: &FutexKey,
        registry: &mut FutexRegistry,
        waiter: &PiWaiter,
    ) -> Result<usize, SystemError> {
        let owner = registry
            .buckets
            .get(key)
            .and_then(|b| b.pi_owner.as_ref())
            .ok_or(SystemError::EINVAL)?;
        if !Arc::ptr_eq(owner, &waiter.task) {
            return Err(SystemError::EINVAL);
        }
        loop {
            let value = Self::read_user_u32_nofault(uaddr)?;
            let desired =
                waiter.task.task_pid_vnr().data() as u32 | FUTEX_OWNER_DIED | FUTEX_WAITERS;
            if Self::futex_cmpxchg_user(uaddr, value, desired)? == value {
                waiter.handoff.store(ACQUIRED, Ordering::Release);
                return Ok(0);
            }
        }
    }
    pub fn futex_unlock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        let task = ProcessManager::current_pcb();
        let tid = task.task_pid_vnr().data() as u32;
        loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;
            let mut registry = FutexData::futex_map();
            let value = match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if value & FUTEX_TID_MASK != tid {
                return Err(SystemError::EPERM);
            }
            Self::validate_pi_state(&registry, &key, value)?;
            if registry
                .buckets
                .get(&key)
                .and_then(|b| b.pi_owner.as_ref())
                .is_some_and(|owner| !Arc::ptr_eq(owner, &task))
            {
                return Err(SystemError::EINVAL);
            }
            let next = registry.top(&key);
            let desired = next.as_ref().map_or(0, |next| {
                next.task.task_pid_vnr().data() as u32 | FUTEX_WAITERS
            });
            match Self::futex_cmpxchg_user(uaddr, value, desired) {
                Ok(observed) if observed == value => {}
                Ok(_) => continue,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let mut retired = Queue::new();
            if let Some(next) = &next {
                retired = registry
                    .buckets
                    .get_mut(&key)
                    .unwrap()
                    .pi_waiters
                    .extract_if(|w| Arc::ptr_eq(w, next));
                registry.task_mut(&next.task).unwrap().blocked = None;
                registry.move_owner(&key, Some(next.task.clone()), &mut None);
                next.handoff.store(ACQUIRED, Ordering::Release);
            } else if registry
                .buckets
                .get(&key)
                .is_some_and(|b| b.pi_owner.is_some())
            {
                registry.move_owner(&key, None, &mut None);
            }
            let empty = FutexData::remove_empty_locked(&mut registry, &key);
            drop(registry);
            drop(empty);
            if let Some(next) = next {
                next.waker.wake();
            }
            drop(retired);
            return Ok(0);
        }
    }
}
