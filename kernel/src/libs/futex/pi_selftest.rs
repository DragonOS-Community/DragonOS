//! PI birth failures use the production slowpath on prefaulted userspace words.
//! This is feature-only coverage reached through the existing futex selftest.
use super::*;
use crate::{
    arch::MMArch,
    libs::futex::futex::FutexKey,
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        oom::denial,
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
        MemoryManagementArch,
    },
    process::RawPid,
    syscall::user_access::UserBufferWriter,
};

pub(super) fn run() -> Result<bool, SystemError> {
    // Pin a live other task throughout the failed birth. This fixture never
    // publishes a PI owner/waiter and does not stop or alter the owner task.
    let owner = ProcessManager::find(RawPid::new(1)).ok_or(SystemError::ESRCH)?;
    let owner_tid = owner.task_pid_vnr().data() as u32;
    let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;
    if owner_tid == 0 || owner_tid == current_tid {
        return Err(SystemError::EINVAL);
    }
    let mm = AddressSpace::current()?;
    let address = mm
        .map_anonymous_wait(
            VirtAddr::new(0),
            MMArch::PAGE_SIZE,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            true,
            false,
        )?
        .virt_address();
    let result = (|| {
        let mut writer = UserBufferWriter::new(address.as_ptr::<u32>(), MMArch::PAGE_SIZE, true)?;
        writer.copy_one_to_user(&owner_tid, 0)?;
        let key = Futex::get_futex_key(address, false, FutexAccess::FutexWrite)?;
        let now = crate::time::timekeeping::realtime_now();
        let future = PosixTimeSpec {
            tv_sec: now.tv_sec.saturating_add(1),
            tv_nsec: now.tv_nsec,
        };
        {
            let _scope = denial::Scope::enter(true);
            if !denial::probe_denied() {
                return Ok(false);
            }
            for timeout in [None, Some(future)] {
                if Futex::futex_lock_pi(address, FutexFlag::FLAGS_CLOCKRT, timeout)
                    != Err(SystemError::ENOMEM)
                    || Futex::read_user_u32_nofault(address)? != owner_tid
                    || FutexData::futex_map().contains_key(&key)
                {
                    return Ok(false);
                }
            }
            if Futex::futex_trylock_pi(address, FutexFlag::empty())
                != Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                || FutexData::futex_map().contains_key(&key)
                || Futex::futex_cmpxchg_user(address, owner_tid, 0)? != owner_tid
                || Futex::futex_lock_pi(address, FutexFlag::empty(), None) != Ok(0)
                || Futex::futex_unlock_pi(address, FutexFlag::empty()) != Ok(0)
            {
                return Ok(false);
            }
        }
        // Test each later factory boundary with earlier owners already alive.
        // First-Waker failure alone cannot validate these allocation sites.
        let (_, waker) = Waiter::try_new_pair()?;
        let object = Arc::try_new(PiWaiter {
            waker: waker.clone(),
            tid: current_tid,
            handoff: AtomicU32::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let callback = Box::try_new(WakerTimer {
            waker: waker.clone(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut registry =
            hashbrown::HashMap::<FutexKey, super::super::futex::FutexHashBucket>::new();
        let denied = {
            let _scope = denial::Scope::enter(true);
            Arc::try_new(PiWaiter {
                waker: waker.clone(),
                tid: current_tid,
                handoff: AtomicU32::new(0),
            })
            .is_err()
                && Node::try_new(object.clone()).is_err()
                && Box::try_new(WakerTimer { waker }).is_err()
                && Timer::try_prepare(callback, clock().saturating_add(100)).is_err()
                && registry.try_reserve(1).is_err()
                && denial::probe_denied()
        };
        // The real publication method consumes its prepared node and timer
        // under heap denial. Keep the fixture bucket isolated from live futexes.
        let mut prepared = PreparedPiWait::try_new(current_tid, Some(clock().saturating_add(100)))?;
        let mut bucket = super::super::futex::FutexHashBucket::new();
        let died = Arc::try_new(PiWaiter {
            waker: object.waker.clone(),
            tid: current_tid,
            handoff: AtomicU32::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let published = {
            let _scope = denial::Scope::enter(true);
            prepared.publish(&mut bucket, owner_tid);
            let published = bucket.pi_owner == owner_tid
                && bucket.pi_waiters.len() == 1
                && bucket
                    .pi_waiters
                    .front()
                    .is_some_and(|object| Arc::ptr_eq(object, &prepared.object))
                && prepared.node.is_none()
                && prepared.activation.is_none();
            if let Some(timer) = &prepared.timer {
                timer.cancel();
            }
            let retired = bucket
                .pi_waiters
                .extract_if(|object| Arc::ptr_eq(object, &prepared.object));
            let pending = prepared.object.acquired().is_none() && died.acquired().is_none();
            prepared.object.hand_off(0);
            died.hand_off(FUTEX_OWNER_DIED);
            published
                && pending
                && prepared.object.acquired() == Some(Ok(0))
                && died.acquired() == Some(Err(SystemError::EOWNERDEAD))
                && bucket.pi_waiters.is_empty()
                && retired.len() == 1
                && denial::probe_denied()
        };
        Ok(denied && published && crate::time::timer::heap_denied_activation_selftest()?)
    })();
    let unmapped = mm
        .munmap_wait(VirtPageFrame::new(address), PageFrameCount::new(1))
        .is_ok();
    result.map(|passed| passed && unmapped)
}
