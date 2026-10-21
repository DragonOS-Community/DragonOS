use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    libs::{
        futex::{
            constant::{FutexFlag, FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS},
            futex::{Futex, FutexAccess, FutexData},
        },
        prepared_queue::{Node, Queue},
        wait_queue::{Waiter, Waker},
    },
    mm::VirtAddr,
    process::ProcessManager,
    time::{
        timer::{clock, next_n_us_timer_jiffies, PreparedTimer, Timer, TimerFunction},
        PosixTimeSpec,
    },
};

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

/// PI's waiter owns its handoff result. The original user address may be
/// unmapped or reused while sleeping; successful acquisition must not depend on
/// reading that new mapping. Normal futex objects need none of this state.
pub(super) struct PiWaiter {
    pub(super) waker: Arc<Waker>,
    pub(super) tid: u32,
    // Zero is pending; one records acquisition and preserves OWNER_DIED.
    handoff: AtomicU32,
}

impl PiWaiter {
    pub(super) fn hand_off(&self, owner_died: u32) {
        self.handoff
            .store(1 | (owner_died & FUTEX_OWNER_DIED), Ordering::Release);
    }

    fn acquired(&self) -> Option<Result<usize, SystemError>> {
        let result = self.handoff.load(Ordering::Acquire);
        (result != 0).then(|| Futex::pi_acquired(result))
    }
}

/// All fallible ownership is prepared before changing the word or publishing a
/// PI waiter. The node and timer activation are consumed exactly once.
struct PreparedPiWait {
    waiter: Waiter,
    object: Arc<PiWaiter>,
    node: Option<Box<Node<Arc<PiWaiter>>>>,
    activation: Option<PreparedTimer>,
    timer: Option<Arc<Timer>>,
}

impl PreparedPiWait {
    fn try_new(tid: u32, deadline: Option<u64>) -> Result<Self, SystemError> {
        let (waiter, waker) = Waiter::try_new_pair()?;
        let object = Arc::try_new(PiWaiter {
            waker: waker.clone(),
            tid,
            handoff: AtomicU32::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let node = Node::try_new(object.clone())?;
        let activation = if let Some(deadline) = deadline {
            let callback = Box::try_new(WakerTimer { waker }).map_err(|_| SystemError::ENOMEM)?;
            Some(Timer::try_prepare(callback, deadline)?)
        } else {
            None
        };
        let timer = activation.as_ref().map(PreparedTimer::timer);
        Ok(Self {
            waiter,
            object,
            node: Some(node),
            activation,
            timer,
        })
    }

    /// Called only after the word CAS commits. No allocation or user access can
    /// fail between linking the node and activating its prepared timeout.
    fn publish(&mut self, bucket: &mut super::futex::FutexHashBucket, owner: u32) {
        let node = self.node.take().expect("unpublished PI node");
        bucket.pi_owner = owner;
        bucket.pi_waiters.push_node(node);
        if let Some(activation) = self.activation.take() {
            activation.activate();
        }
    }
}

impl Drop for PreparedPiWait {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.cancel();
        }
    }
}

#[cfg(feature = "mm_selftest")]
#[path = "pi_selftest.rs"]
mod selftest;

impl Futex {
    #[cfg(feature = "mm_selftest")]
    pub(crate) fn failed_pi_registration_selftest() -> Result<bool, SystemError> {
        selftest::run()
    }

    /// LOCK_PI and LOCK_PI2 have absolute deadlines. Capture the remaining
    /// budget once, outside all retries; a remapping/refault must not renew it.
    fn pi_timer_deadline(
        flags: FutexFlag,
        timeout: Option<PosixTimeSpec>,
    ) -> Result<Option<u64>, SystemError> {
        let Some(deadline) = timeout else {
            return Ok(None);
        };
        if !deadline.is_valid_timeout() {
            return Err(SystemError::EINVAL);
        }
        let now = if flags.contains(FutexFlag::FLAGS_CLOCKRT) {
            crate::time::timekeeping::realtime_now()
        } else {
            crate::time::timekeeping::monotonic_now()
        };
        let remaining = deadline.saturating_sub_timespec(&now);
        Ok(Some(if remaining.is_empty() {
            clock()
        } else {
            next_n_us_timer_jiffies(remaining.to_ktime_ns().div_ceil(1000))
        }))
    }

    /// PI allocation/publication protocol. Queue storage is shared with normal
    /// futexes; PI ownership, handoff and cancellation remain separate policy.
    pub fn futex_lock_pi(
        uaddr: VirtAddr,
        flags: FutexFlag,
        timeout: Option<PosixTimeSpec>,
    ) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;
        let deadline = Self::pi_timer_deadline(flags, timeout)?;
        // Declared before any map guard: every unpublished owner is retired
        // after the final registration guard on every error/return path.
        let mut prepared: Option<PreparedPiWait> = None;
        loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;
            let mut map = FutexData::futex_map();
            let value = match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(map);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let owner = value & FUTEX_TID_MASK;
            if owner == current_tid {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            if owner == 0 {
                let queued = map
                    .get(&key)
                    .is_some_and(|bucket| !bucket.pi_waiters.is_empty());
                let desired = current_tid
                    | (value & FUTEX_OWNER_DIED)
                    | if queued { FUTEX_WAITERS } else { 0 };
                match Self::futex_cmpxchg_user(uaddr, value, desired) {
                    Ok(observed) if observed == value => {
                        if let Some(bucket) = map.get_mut(&key) {
                            bucket.pi_owner = current_tid;
                        }
                        drop(map);
                        return Self::pi_acquired(value);
                    }
                    Ok(_) => continue,
                    Err(SystemError::EFAULT) => {
                        drop(map);
                        Self::fault_in_user_writeable_u32(uaddr)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            // As in Linux, an immediately available lock can be acquired with
            // an already expired deadline; only a contended lock times out.
            if deadline.is_some_and(|deadline| deadline <= clock()) {
                return Err(SystemError::ETIMEDOUT);
            }
            if prepared.is_none() {
                drop(map);
                prepared = Some(PreparedPiWait::try_new(current_tid, deadline)?);
                continue;
            }
            if !map.contains_key(&key) {
                map.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            }
            // Even if WAITERS was already set, use a nofault CAS: it validates
            // write access and the exact owner observed before publication.
            match Self::futex_cmpxchg_user(uaddr, value, value | FUTEX_WAITERS) {
                Ok(observed) if observed == value => {}
                Ok(_) => continue,
                Err(SystemError::EFAULT) => {
                    drop(map);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let prepared = prepared.as_mut().expect("prepared PI wait");
            let bucket = map
                .entry(key.clone())
                .or_insert_with(super::futex::FutexHashBucket::new);
            prepared.publish(bucket, owner);
            drop(map);
            return Self::wait_pi_registered(&key, current_tid, deadline, prepared);
        }
    }

    /// Keep the original registration key after publication: the mapping at
    /// uaddr can change while asleep, but cancellation must retire our old node.
    fn wait_pi_registered(
        key: &super::futex::FutexKey,
        tid: u32,
        deadline: Option<u64>,
        prepared: &PreparedPiWait,
    ) -> Result<usize, SystemError> {
        loop {
            let waited = prepared.waiter.wait(true);
            let expired = deadline.is_some_and(|deadline| deadline <= clock())
                || prepared.timer.as_ref().is_some_and(|timer| timer.timeout());
            let interrupted = waited.is_err() || ProcessManager::current_pcb().has_pending_signal();
            let mut map = FutexData::futex_map();
            let bucket = map.get_mut(key).expect("registered PI bucket");
            // A successful owner handoff wins over timeout/signal. The unlock
            // path already detached our node before waking us.
            if let Some(acquired) = prepared.object.acquired() {
                debug_assert_eq!(bucket.pi_owner, tid);
                drop(map);
                return acquired;
            }
            if !expired && !interrupted {
                drop(map);
                continue;
            }
            let removed = bucket
                .pi_waiters
                .extract_if(|object| Arc::ptr_eq(object, &prepared.object));
            // WAITERS is only an advisory user fastpath hint. Leave it set on
            // cancellation until the owner unlocks, like Linux PI cleanup can;
            // do not access a potentially remapped user word to clear it.
            drop(map);
            drop(removed);
            return Err(if expired {
                SystemError::ETIMEDOUT
            } else {
                SystemError::EINTR
            });
        }
    }

    // Preserve the existing owner-died ABI here. Full Linux PI donation and
    // raw owner-death/restart semantics are tracked independently from MM-234.
    fn pi_acquired(value: u32) -> Result<usize, SystemError> {
        if value & FUTEX_OWNER_DIED != 0 {
            Err(SystemError::EOWNERDEAD)
        } else {
            Ok(0)
        }
    }

    pub fn futex_unlock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;
        loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;
            let mut map = FutexData::futex_map();
            let value = match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(map);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if value & FUTEX_TID_MASK != current_tid {
                return Err(SystemError::EPERM);
            }
            let mut retired = Queue::new();
            let next = if let Some(bucket) = map.get_mut(&key) {
                retired = bucket
                    .pi_waiters
                    .extract_if(|object| object.waker.is_closed());
                bucket.pi_waiters.front().cloned()
            } else {
                None
            };
            let desired = if let Some(next) = &next {
                next.tid
                    | (value & FUTEX_OWNER_DIED)
                    | if map.get(&key).unwrap().pi_waiters.len() > 1 {
                        FUTEX_WAITERS
                    } else {
                        0
                    }
            } else {
                value & FUTEX_OWNER_DIED
            };
            match Self::futex_cmpxchg_user(uaddr, value, desired) {
                Ok(observed) if observed == value => {}
                result => {
                    drop(map);
                    drop(retired);
                    match result {
                        Ok(_) => continue,
                        Err(SystemError::EFAULT) => {
                            Self::fault_in_user_writeable_u32(uaddr)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            let handed_off = if let Some(bucket) = map.get_mut(&key) {
                bucket.pi_owner = next.as_ref().map_or(0, |next| next.tid);
                if next.is_some() {
                    bucket.pi_waiters.pop_node()
                } else {
                    None
                }
            } else {
                None
            };
            let empty = FutexData::remove_empty_locked(&mut map, &key);
            if let Some(next) = &next {
                next.hand_off(value & FUTEX_OWNER_DIED);
            }
            drop(map);
            drop(retired);
            drop(empty);
            if let Some(next) = next {
                next.waker.wake();
            }
            drop(handed_off);
            return Ok(0);
        }
    }

    /// A busy trylock does not register a waiter or need bucket metadata.
    pub fn futex_trylock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;
        loop {
            let key = Self::get_futex_key(
                uaddr,
                flags.contains(FutexFlag::FLAGS_SHARED),
                FutexAccess::FutexWrite,
            )?;
            let mut map = FutexData::futex_map();
            let value = match Self::read_user_u32_nofault(uaddr) {
                Ok(value) => value,
                Err(SystemError::EFAULT) => {
                    drop(map);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let owner = value & FUTEX_TID_MASK;
            if owner == current_tid {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            if owner != 0 {
                // Linux validates writable access even for a busy trylock.
                // An unchanged CAS checks that without publishing a waiter hint
                // or creating an otherwise unused PI registry entry.
                match Self::futex_cmpxchg_user(uaddr, value, value) {
                    Ok(observed) if observed == value => {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    }
                    Ok(_) => continue,
                    Err(SystemError::EFAULT) => {
                        drop(map);
                        Self::fault_in_user_writeable_u32(uaddr)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            let queued = map
                .get(&key)
                .is_some_and(|bucket| !bucket.pi_waiters.is_empty());
            let desired =
                current_tid | (value & FUTEX_OWNER_DIED) | if queued { FUTEX_WAITERS } else { 0 };
            match Self::futex_cmpxchg_user(uaddr, value, desired) {
                Ok(observed) if observed == value => {
                    if let Some(bucket) = map.get_mut(&key) {
                        bucket.pi_owner = current_tid;
                    }
                    drop(map);
                    return Self::pi_acquired(value);
                }
                Ok(_) => continue,
                Err(SystemError::EFAULT) => {
                    drop(map);
                    Self::fault_in_user_writeable_u32(uaddr)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}
