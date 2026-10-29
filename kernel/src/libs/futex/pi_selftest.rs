//! PI birth failures use the production slowpath on prefaulted userspace words.
//! This is feature-only coverage reached through the existing futex selftest.
use super::*;
use crate::{
    arch::MMArch,
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
        // Prepared resources must publish, expire and retire without any
        // allocator dependency. Use a private graph so no live task is boosted.
        let key = Futex::get_futex_key(address, false, FutexAccess::FutexWrite)?;
        let task = ProcessManager::current_pcb();
        let mut prepared = PreparedPiWait::try_new(
            task.clone(),
            owner.clone(),
            &key,
            AbsoluteClock::Realtime,
            Some(future),
        )?;
        let mut graph = FutexRegistry::new();
        graph
            .tasks
            .try_reserve(2)
            .map_err(|_| SystemError::ENOMEM)?;
        graph
            .buckets
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        let published = {
            let _scope = denial::Scope::enter(true);
            graph
                .tasks
                .insert(identity(&owner), prepared.owner_node.take().unwrap());
            graph
                .tasks
                .insert(identity(&task), prepared.task_node.take().unwrap());
            let mut bucket = super::super::futex::FutexHashBucket::new();
            bucket.pi_owner = Some(owner.clone());
            bucket.pi_refs = 1;
            bucket.pi_waiters.push_node(prepared.node.take().unwrap());
            graph.buckets.insert(key.clone(), bucket);
            graph
                .task_mut(&owner)
                .unwrap()
                .owned
                .push_node(prepared.owned_node.take().unwrap());
            graph.task_mut(&task).unwrap().blocked = Some(key.clone());
            prepared.activation.take().unwrap().activate();
            let published = graph
                .top(&key)
                .is_some_and(|waiter| Arc::ptr_eq(&waiter, &prepared.object))
                && graph.donor(&owner) == Some(task.sched_info().prio());
            prepared.timer.as_ref().unwrap().cancel();
            let retired = graph
                .buckets
                .get_mut(&key)
                .unwrap()
                .pi_waiters
                .extract_if(|waiter| Arc::ptr_eq(waiter, &prepared.object));
            published && retired.len() == 1 && denial::probe_denied()
        };
        // A compressed incoming prefix exercises the tail-extension boundary:
        // checking only the new owner's suffix would incorrectly allow it.
        // This private graph never schedules or mutates any of these tasks.
        let tail = crate::process::snapshot_all_processes()?
            .into_iter()
            .find(|candidate| !Arc::ptr_eq(candidate, &task) && !Arc::ptr_eq(candidate, &owner))
            .ok_or(SystemError::ESRCH)?;
        let incoming = Node::try_new(prepared.object.clone())?;
        graph
            .buckets
            .get_mut(&key)
            .unwrap()
            .pi_waiters
            .push_node(incoming);
        graph.task_mut(&task).unwrap().upstream_depth = MAX_CHAIN - 1;
        graph.refresh_depth(&owner);
        let rejected = graph.task(&owner).unwrap().upstream_depth == MAX_CHAIN
            && graph.validate_chain(&owner, tail.clone()) == Err(SystemError::EDEADLK_OR_EDEADLOCK);
        graph.task_mut(&task).unwrap().upstream_depth -= 1;
        graph.refresh_depth(&owner);
        let accepted = graph.validate_chain(&owner, tail).is_ok();
        let removed = graph.buckets.get_mut(&key).unwrap().pi_waiters.pop_node();
        graph.refresh_depth(&owner);
        Ok(published
            && rejected
            && accepted
            && removed.is_some()
            && graph.task(&owner).unwrap().upstream_depth == 0
            && crate::time::timer::heap_denied_activation_selftest()?)
    })();
    let unmapped = mm
        .munmap_wait(VirtPageFrame::new(address), PageFrameCount::new(1))
        .is_ok();
    result.map(|passed| passed && unmapped)
}
