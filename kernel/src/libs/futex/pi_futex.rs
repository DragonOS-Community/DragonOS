use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    libs::{
        futex::{
            constant::{
                FutexFlag, FUTEX_BITSET_MATCH_ANY, FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS,
            },
            futex::{Futex, FutexAccess, FutexData, FutexHashBucket, FutexObj},
        },
        wait_queue::{Waiter, Waker},
    },
    mm::VirtAddr,
    process::ProcessManager,
    time::{
        timer::{next_n_us_timer_jiffies, Timer, TimerFunction},
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

impl Futex {
    /// ## FUTEX_LOCK_PI - Priority Inheritance futex lock
    ///
    /// 实现符合Linux语义的PI futex加锁操作
    ///
    /// ### 参数
    /// - `uaddr`: futex变量的用户态地址
    /// - `flags`: futex标志位（shared/private等）
    /// - `timeout`: 可选的超时时间
    ///
    /// ### 返回值
    /// - `Ok(0)`: 成功获取锁
    /// - `Err(SystemError::EDEADLK_OR_EDEADLOCK)`: 检测到死锁（当前线程已持有该锁）
    /// - `Err(SystemError::ETIMEDOUT)`: 超时
    /// - `Err(SystemError::EINTR)`: 被信号中断
    pub fn futex_lock_pi(
        uaddr: VirtAddr,
        flags: FutexFlag,
        timeout: Option<PosixTimeSpec>,
    ) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;
        let key = Self::get_futex_key(
            uaddr,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexWrite,
        )?;

        let atomic_futex = unsafe { AtomicU32::from_ptr(uaddr.as_ptr::<u32>()) };

        loop {
            let uval = atomic_futex.load(Ordering::SeqCst);
            let owner_tid = uval & FUTEX_TID_MASK;
            let owner_died = uval & FUTEX_OWNER_DIED;

            // 快路径：锁空闲且无WAITERS
            if owner_tid == 0 && (uval & FUTEX_WAITERS) == 0 {
                let desired = current_tid | owner_died;
                match atomic_futex.compare_exchange(
                    uval,
                    desired,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        let mut futex_map_guard = FutexData::futex_map();
                        if let Some(bucket) = futex_map_guard.get_mut(&key) {
                            bucket.pi_owner = current_tid;
                        }
                        if owner_died != 0 {
                            return Err(SystemError::EOWNERDEAD);
                        }
                        return Ok(0);
                    }
                    Err(_) => continue,
                }
            }

            if owner_tid == current_tid {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }

            let (waiter, waker) = Waiter::new_pair();
            let futex_q = Arc::new(FutexObj {
                waker: waker.clone(),
                key: key.clone(),
                bitset: FUTEX_BITSET_MATCH_ANY,
                tid: current_tid,
            });

            let mut timer = None;
            if let Some(time) = timeout {
                let sec = time.tv_sec;
                let nsec = time.tv_nsec;
                let total_us = (nsec / 1000 + sec * 1_000_000) as u64;
                if total_us == 0 {
                    return Err(SystemError::ETIMEDOUT);
                }
                let jiffies = next_n_us_timer_jiffies(total_us);
                let wake_up = Timer::new(
                    Box::new(WakerTimer {
                        waker: waker.clone(),
                    }),
                    jiffies,
                );
                timer = Some(wake_up);
            }

            let mut futex_map_guard = FutexData::futex_map();
            let bucket_mut = futex_map_guard
                .entry(key.clone())
                .or_insert(FutexHashBucket::new());

            // Re-check owner under futex map lock.
            let cur_val = atomic_futex.load(Ordering::SeqCst);
            let cur_owner = cur_val & FUTEX_TID_MASK;
            if cur_owner == 0 {
                let desired = current_tid
                    | if bucket_mut.pi_waiters.is_empty() {
                        0
                    } else {
                        FUTEX_WAITERS
                    }
                    | (cur_val & FUTEX_OWNER_DIED);
                if atomic_futex
                    .compare_exchange(cur_val, desired, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    bucket_mut.pi_owner = current_tid;
                    drop(futex_map_guard);
                    if (cur_val & FUTEX_OWNER_DIED) != 0 {
                        return Err(SystemError::EOWNERDEAD);
                    }
                    return Ok(0);
                }
                drop(futex_map_guard);
                continue;
            }

            if bucket_mut.pi_owner == 0 {
                bucket_mut.pi_owner = cur_owner;
            }

            bucket_mut.pi_waiters.push_back(futex_q.clone());

            loop {
                let val = atomic_futex.load(Ordering::SeqCst);
                if (val & FUTEX_WAITERS) != 0 {
                    break;
                }
                if atomic_futex
                    .compare_exchange(val, val | FUTEX_WAITERS, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            }

            if let Some(ref t) = timer {
                t.activate();
            }

            drop(futex_map_guard);
            let wait_res = waiter.wait(true);

            let is_timeout = timer.as_ref().is_some_and(|t| t.timeout());

            let mut futex_map_guard = FutexData::futex_map();
            if let Some(bucket) = futex_map_guard.get_mut(&key) {
                let mut in_queue = false;
                bucket
                    .pi_waiters
                    .extract_if(|x| {
                        if Arc::ptr_eq(&x.waker, &waker) {
                            in_queue = true;
                            true
                        } else {
                            false
                        }
                    })
                    .for_each(drop);

                if bucket.pi_waiters.is_empty() {
                    loop {
                        let val = atomic_futex.load(Ordering::SeqCst);
                        if (val & FUTEX_WAITERS) == 0 {
                            break;
                        }
                        let desired = val & !FUTEX_WAITERS;
                        if atomic_futex
                            .compare_exchange(val, desired, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break;
                        }
                    }
                }

                if bucket.pi_owner == current_tid {
                    drop(futex_map_guard);
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    let post_val = atomic_futex.load(Ordering::SeqCst);
                    if (post_val & FUTEX_OWNER_DIED) != 0 {
                        return Err(SystemError::EOWNERDEAD);
                    }
                    return Ok(0);
                }

                if in_queue && (is_timeout || wait_res.is_err()) {
                    drop(futex_map_guard);
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    return if is_timeout {
                        Err(SystemError::ETIMEDOUT)
                    } else {
                        Err(SystemError::EINTR)
                    };
                }
            }

            drop(futex_map_guard);
            if let Some(timer) = timer {
                timer.cancel();
            }

            if is_timeout {
                return Err(SystemError::ETIMEDOUT);
            }
            if wait_res.is_err() || ProcessManager::current_pcb().has_pending_signal() {
                return Err(SystemError::EINTR);
            }
        }
    }

    /// ## FUTEX_UNLOCK_PI - Priority Inheritance futex unlock
    ///
    /// 实现符合Linux语义的PI futex解锁操作
    ///
    /// ### 参数
    /// - `uaddr`: futex变量的用户态地址
    /// - `flags`: futex标志位（shared/private等）
    ///
    /// ### 返回值
    /// - `Ok(0)`: 成功释放锁
    /// - `Err(SystemError::EPERM)`: 当前线程不持有该锁
    pub fn futex_unlock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;

        // 获取futex key
        let key = Self::get_futex_key(
            uaddr,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexWrite,
        )?;

        let atomic_futex = unsafe { AtomicU32::from_ptr(uaddr.as_ptr::<u32>()) };

        loop {
            let uval = atomic_futex.load(Ordering::SeqCst);
            let owner_tid = uval & FUTEX_TID_MASK;
            let owner_died = uval & FUTEX_OWNER_DIED;

            // 检查当前线程是否持有锁
            if owner_tid != current_tid {
                return Err(SystemError::EPERM);
            }

            let mut futex_map_guard = FutexData::futex_map();
            let bucket_opt = futex_map_guard.get_mut(&key);
            let bucket = match bucket_opt {
                None => {
                    if atomic_futex
                        .compare_exchange(uval, owner_died, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        drop(futex_map_guard);
                        return Ok(0);
                    }
                    continue;
                }
                Some(bucket) => bucket,
            };

            if bucket.pi_waiters.is_empty() {
                if atomic_futex
                    .compare_exchange(uval, owner_died, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    bucket.pi_owner = 0;
                    drop(futex_map_guard);
                    FutexData::try_remove(&key);
                    return Ok(0);
                }
                continue;
            }
            let next_waiter = bucket.pi_waiters.pop_front();
            if let Some(next_waiter) = next_waiter {
                let has_more = !bucket.pi_waiters.is_empty();
                let new_val =
                    next_waiter.tid | if has_more { FUTEX_WAITERS } else { 0 } | owner_died;
                if atomic_futex
                    .compare_exchange(uval, new_val, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    bucket.pi_owner = next_waiter.tid;
                    drop(futex_map_guard);
                    next_waiter.waker.wake();
                    return Ok(0);
                }
                bucket.pi_waiters.push_front(next_waiter);
                continue;
            }

            if atomic_futex
                .compare_exchange(uval, owner_died, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                bucket.pi_owner = 0;
                drop(futex_map_guard);
                FutexData::try_remove(&key);
                return Ok(0);
            }
        }
    }

    /// ## FUTEX_TRYLOCK_PI - Non-blocking Priority Inheritance futex lock
    ///
    /// 实现符合Linux语义的PI futex非阻塞加锁操作
    ///
    /// ### 参数
    /// - `uaddr`: futex变量的用户态地址
    /// - `flags`: futex标志位（shared/private等）
    ///
    /// ### 返回值
    /// - `Ok(0)`: 成功获取锁
    /// - `Err(SystemError::EWOULDBLOCK)`: 锁已被其他线程持有
    /// - `Err(SystemError::EDEADLK_OR_EDEADLOCK)`: 当前线程已持有该锁
    pub fn futex_trylock_pi(uaddr: VirtAddr, flags: FutexFlag) -> Result<usize, SystemError> {
        let current_tid = ProcessManager::current_pcb().task_pid_vnr().data() as u32;

        // 获取futex key（虽然trylock不会阻塞，但还是需要验证地址）
        let key = Self::get_futex_key(
            uaddr,
            flags.contains(FutexFlag::FLAGS_SHARED),
            FutexAccess::FutexWrite,
        )?;

        let atomic_futex = unsafe { AtomicU32::from_ptr(uaddr.as_ptr::<u32>()) };

        let uval = atomic_futex.load(Ordering::SeqCst);
        let owner_tid = uval & FUTEX_TID_MASK;

        // 检查死锁
        if owner_tid == current_tid && owner_tid != 0 {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }

        let owner_died = uval & FUTEX_OWNER_DIED;

        if owner_tid == 0 && (uval & FUTEX_WAITERS) == 0 {
            let desired = current_tid | owner_died;
            if atomic_futex
                .compare_exchange(uval, desired, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let mut futex_map_guard = FutexData::futex_map();
                if let Some(bucket) = futex_map_guard.get_mut(&key) {
                    bucket.pi_owner = current_tid;
                }
                if owner_died != 0 {
                    return Err(SystemError::EOWNERDEAD);
                }
                return Ok(0);
            }
        }

        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }
}
