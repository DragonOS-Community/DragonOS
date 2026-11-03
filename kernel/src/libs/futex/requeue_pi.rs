//! Source-to-PI ownership transfer. The existing registry lock serializes both
//! queue locations, cancellation and the PI graph; no second locking protocol.
use super::*;
use crate::libs::{futex::futex::FutexObj, spinlock::SpinLock};
use crate::syscall::user_access::UserBufferReader;
use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Source,
    Target,
    DirectAcquired,
}
struct ProxyResources {
    phase: Phase,
    registration: PiRegistration,
}
pub(crate) struct RequeuePiWaiter {
    target: FutexKey,
    pi: Arc<PiWaiter>,
    resources: SpinLock<ProxyResources>,
}
impl fmt::Debug for RequeuePiWaiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequeuePiWaiter")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Futex {
    fn fault_in_requeue_source(uaddr: VirtAddr) -> Result<(), SystemError> {
        let reader =
            UserBufferReader::new(uaddr.as_ptr::<u32>(), core::mem::size_of::<u32>(), true)?;
        reader.read_one_from_user::<u32>(0).map(|_| ())
    }

    pub fn futex_wait_requeue_pi(
        source: VirtAddr,
        flags: FutexFlag,
        expected: u32,
        deadline: Option<PosixTimeSpec>,
        target: VirtAddr,
    ) -> Result<usize, SystemError> {
        if deadline.is_some_and(|time| !time.is_valid_timeout()) || source == target {
            return Err(SystemError::EINVAL);
        }
        let shared = flags.contains(FutexFlag::FLAGS_SHARED);
        // Linux resolves the writable target identity before the source value.
        // Private keys intentionally do not fault in the target at this stage.
        let target_key = Self::get_futex_key(target, shared, FutexAccess::FutexWrite)?;
        loop {
            let key = Self::get_futex_key(source, shared, FutexAccess::FutexRead)?;
            let registry = FutexData::futex_map();
            match Self::read_user_u32_nofault(source) {
                Ok(value) if value != expected => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                Ok(_) => {}
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_requeue_source(source)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            if key == target_key {
                return Err(SystemError::EINVAL);
            }
            break;
        }
        let task = ProcessManager::current_pcb();
        let mut prepared =
            PreparedPiWait::for_task(task.clone(), &target_key, Self::pi_clock(flags), deadline)?;
        let proxy = Arc::try_new(RequeuePiWaiter {
            target: target_key.clone(),
            pi: prepared.object.clone(),
            resources: SpinLock::new(ProxyResources {
                phase: Phase::Source,
                registration: PiRegistration {
                    node: prepared.registration.node.take(),
                    task_node: prepared.registration.task_node.take(),
                    owner_node: None,
                    owned_node: prepared.registration.owned_node.take(),
                },
            }),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let object = Arc::try_new(FutexObj {
            waker: prepared.object.waker.clone(),
            key: SpinLock::new(None),
            bitset: FUTEX_BITSET_MATCH_ANY,
            source_priority: AtomicI32::new(100),
            requeue_pi: Some(proxy.clone()),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let node = Node::try_new(object.clone())?;
        loop {
            let key = Self::get_futex_key(source, shared, FutexAccess::FutexRead)?;
            let mut registry = FutexData::futex_map();
            match Self::read_user_u32_nofault(source) {
                Ok(value) if value != expected => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                Ok(_) => {}
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_requeue_source(source)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            if key == target_key {
                return Err(SystemError::EINVAL);
            }
            if registry.get(&key).is_some_and(|b| b.pi_refs != 0) {
                return Err(SystemError::EINVAL);
            }
            registry.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            object
                .source_priority
                .store(task.sched_info().normal_prio().min(100), Ordering::Relaxed);
            *object.key.lock() = Some(key.clone());
            registry
                .entry(key)
                .or_insert_with(FutexHashBucket::new)
                .enqueue_node(node);
            if let Some(activation) = prepared.activation.take() {
                activation.activate();
            }
            break;
        }
        let waited = prepared.waiter.wait(true);
        let mut registry = FutexData::futex_map();
        let phase = proxy.resources.lock().phase;
        match phase {
            Phase::DirectAcquired => Ok(0),
            Phase::Target => {
                drop(registry);
                // The source wake may have consumed the timer notification;
                // PI waiting must still inspect its timer/signal before sleeping.
                Self::wait_pi_registered(target, &target_key, &prepared).map_err(|error| {
                    if error == SystemError::ERESTARTNOINTR {
                        SystemError::EAGAIN_OR_EWOULDBLOCK
                    } else {
                        error
                    }
                })
            }
            Phase::Source => {
                let expired = prepared.timer.as_ref().is_some_and(|timer| timer.fired());
                let interrupted =
                    waited.is_err() || Signal::signal_pending_state(true, false, &task);
                let (_, retired, location) = Self::unqueue_waiter(&mut registry, &object)?;
                drop(registry);
                drop(retired);
                drop(location);
                Err(if expired {
                    SystemError::ETIMEDOUT
                } else if interrupted {
                    SystemError::ERESTARTNOINTR
                } else {
                    SystemError::EAGAIN_OR_EWOULDBLOCK
                })
            }
        }
    }

    pub fn futex_cmp_requeue_pi(
        source: VirtAddr,
        flags: FutexFlag,
        target: VirtAddr,
        wake: i32,
        requeue: i32,
        expected: u32,
    ) -> Result<usize, SystemError> {
        if wake != 1 || requeue < 0 || source == target {
            return Err(SystemError::EINVAL);
        }
        let shared = flags.contains(FutexFlag::FLAGS_SHARED);
        let limit = 1 + requeue as usize;
        let mut owner_prepared: Option<Box<Node<PiTask>>> = None;
        loop {
            let source_key = Self::get_futex_key(source, shared, FutexAccess::FutexRead)?;
            let target_key = Self::get_futex_key(target, shared, FutexAccess::FutexWrite)?;
            if source_key == target_key {
                return Err(SystemError::EINVAL);
            }
            let mut registry = FutexData::futex_map();
            match Self::read_user_u32_nofault(source) {
                Ok(value) if value != expected => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
                Ok(_) => {}
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_requeue_source(source)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            // Linux performs this nofault read even when source has no waiters.
            let value = match Self::read_user_u32_nofault(target) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(target)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if registry.get(&source_key).is_some_and(|b| b.pi_refs != 0) {
                return Err(SystemError::EINVAL);
            }
            let Some(first) = registry
                .get(&source_key)
                .and_then(|b| b.chain.front())
                .cloned()
            else {
                return Ok(0);
            };
            let first_proxy = first.requeue_pi.as_ref().ok_or(SystemError::EINVAL)?;
            if first_proxy.target != target_key {
                return Err(SystemError::EINVAL);
            }
            let first_task = &first_proxy.pi.task;
            let tid = value & FUTEX_TID_MASK;
            if tid == first_task.task_pid_vnr().data() as u32 {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            Self::validate_pi_state(&registry, &target_key, value)?;
            let free = tid == 0
                && registry
                    .get(&target_key)
                    .is_none_or(|b| b.pi_owner.is_none());
            let owner = if free {
                first_task.clone()
            } else if let Some(owner) = registry.get(&target_key).and_then(|b| b.pi_owner.clone()) {
                owner
            } else {
                match Self::lookup_pi_owner(tid) {
                    Ok(owner) => owner,
                    Err(error) => match Self::futex_cmpxchg_user(target, value, value) {
                        Ok(observed) if observed == value => return Err(error),
                        Ok(_) => continue,
                        Err(SystemError::EFAULT) => {
                            drop(registry);
                            Self::fault_in_user_writeable_u32(target)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    },
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
            if !free {
                registry.validate_chain(first_task, owner.clone())?;
            }
            if !free
                && registry.task(&owner).is_none()
                && owner_prepared
                    .as_ref()
                    .is_none_or(|n| !Arc::ptr_eq(&n.value().task, &owner))
            {
                drop(registry);
                owner_prepared = Some(PiTask::prepare(owner)?);
                continue;
            }
            if !free || requeue > 0 {
                let additional = registry.get(&source_key).unwrap().chain.len().min(limit) + 1;
                registry
                    .tasks
                    .try_reserve(additional)
                    .map_err(|_| SystemError::ENOMEM)?;
                registry
                    .buckets
                    .try_reserve(1)
                    .map_err(|_| SystemError::ENOMEM)?;
            }
            let desired = if free {
                first_task.task_pid_vnr().data() as u32
                    | (value & FUTEX_OWNER_DIED)
                    | if requeue > 0 { FUTEX_WAITERS } else { 0 }
            } else {
                value | FUTEX_WAITERS
            };
            match Self::futex_cmpxchg_user(target, value, desired) {
                Ok(observed) if observed == value => {}
                Ok(_) => continue,
                Err(SystemError::EFAULT) => {
                    drop(registry);
                    Self::fault_in_user_writeable_u32(target)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let mut retired_nodes = Queue::new();
            let result = (|| {
                let mut count = 0;
                while count < limit {
                    let Some(object) = registry
                        .get(&source_key)
                        .and_then(|b| b.chain.front())
                        .cloned()
                    else {
                        break;
                    };
                    let proxy = object.requeue_pi.as_ref().ok_or(SystemError::EINVAL)?;
                    if proxy.target != target_key {
                        return Err(SystemError::EINVAL);
                    }
                    if count != 0 || !free {
                        registry.validate_chain(&proxy.pi.task, owner.clone())?;
                    }
                    let mut resources = proxy.resources.lock();
                    debug_assert!(resources.phase == Phase::Source);
                    if count == 0 && free && requeue == 0 {
                        resources.phase = Phase::DirectAcquired;
                        proxy.pi.handoff.store(ACQUIRED, Ordering::Release);
                    } else {
                        if count == 0 && !free {
                            resources.registration.owner_node = owner_prepared.take();
                        }
                        Self::register_pi_waiter(
                            &mut registry,
                            &target_key,
                            &owner,
                            &proxy.pi,
                            &mut resources.registration,
                            count == 0 && free,
                        );
                        resources.phase = Phase::Target;
                    }
                    let node = registry
                        .get_mut(&source_key)
                        .unwrap()
                        .chain
                        .pop_node()
                        .unwrap();
                    let old_location = object.key.lock().take();
                    drop(old_location);
                    retired_nodes.push_node(node);
                    count += 1;
                }
                Ok(count)
            })();
            // Source waiters have no PI blocked edge yet; each new edge to the
            // same owner is cycle-checked above against the published graph.
            // Publish donation once for the whole batch, even when a later
            // waiter failed validation after an already committed prefix.
            if !retired_nodes.is_empty() {
                refresh_priority_locked(&mut registry, &owner);
            }
            let retired_bucket = FutexData::remove_empty_locked(&mut registry, &source_key);
            drop(registry);
            // Also drain after a partial-error prefix: an already transferred
            // owner must be allowed to run and unlock even if a later node fails.
            while let Some(node) = retired_nodes.pop_node() {
                let proxy = node.value().requeue_pi.as_ref().unwrap();
                if proxy.pi.handoff.load(Ordering::Acquire) != PENDING {
                    proxy.pi.waker.wake();
                }
            }
            drop(retired_bucket);
            return result;
        }
    }
}
