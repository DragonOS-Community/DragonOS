use alloc::{collections::LinkedList, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::futex::{
        constant::{FutexFlag, FUTEX_BITSET_MATCH_ANY, FUTEX_TID_MASK, FUTEX_WAITERS},
        futex::{Futex, FutexAccess, FutexData, FutexHashBucket, FutexObj},
    },
    mm::VirtAddr,
    process::ProcessManager,
    sched::{schedule, SchedMode},
    time::{
        timer::{next_n_us_timer_jiffies, Timer, WakeUpHelper},
        PosixTimeSpec,
    },
};

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

        // 使用原子操作尝试获取锁
        let atomic_futex = unsafe { AtomicU32::from_ptr(uaddr.as_ptr::<u32>()) };

        loop {
            let uval = atomic_futex.load(Ordering::SeqCst);
            let owner_tid = uval & FUTEX_TID_MASK;

            // 情况1: futex为0，尝试直接获取锁
            if uval == 0 {
                match atomic_futex.compare_exchange(
                    0,
                    current_tid,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return Ok(0),
                    Err(_) => continue, // CAS失败，重试
                }
            }

            // 情况2: 当前线程已经持有锁，返回EDEADLK_OR_EDEADLOCK
            if owner_tid == current_tid {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }

            // 情况3: 锁被其他线程持有，需要等待
            // 设置WAITERS位通知持有者有线程在等待
            let new_val = uval | FUTEX_WAITERS;
            if uval != new_val {
                match atomic_futex.compare_exchange(
                    uval,
                    new_val,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {}
                    Err(_) => continue, // CAS失败，重试
                }
            }

            // 将当前进程加入等待队列
            let mut futex_map_guard = FutexData::futex_map();
            let bucket = futex_map_guard.get_mut(&key);
            let bucket_mut = match bucket {
                Some(bucket) => bucket,
                None => {
                    let bucket = FutexHashBucket {
                        chain: LinkedList::new(),
                    };
                    futex_map_guard.insert(key.clone(), bucket);
                    futex_map_guard.get_mut(&key).unwrap()
                }
            };

            let pcb = ProcessManager::current_pcb();
            let futex_q = Arc::new(FutexObj {
                pcb: Arc::downgrade(&pcb),
                key: key.clone(),
                bitset: FUTEX_BITSET_MATCH_ANY,
            });

            // 创建超时定时器
            let mut timer = None;
            if let Some(time) = timeout {
                let sec = time.tv_sec;
                let nsec = time.tv_nsec;
                let total_us = (nsec / 1000 + sec * 1_000_000) as u64;

                if total_us == 0 {
                    return Err(SystemError::ETIMEDOUT);
                }

                let wakeup_helper = WakeUpHelper::new(pcb.clone());
                let jiffies = next_n_us_timer_jiffies(total_us);
                let wake_up = Timer::new(wakeup_helper, jiffies);
                timer = Some(wake_up);
            }

            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            bucket_mut.sleep_no_sched(futex_q.clone())?;

            // 激活定时器
            if let Some(ref t) = timer {
                t.activate();
            }

            drop(futex_map_guard);
            drop(irq_guard);
            schedule(SchedMode::SM_NONE);

            // 被唤醒后检查是否成功获取锁
            let current_val = atomic_futex.load(Ordering::SeqCst);
            let current_owner = current_val & FUTEX_TID_MASK;

            // 如果当前线程已持有锁，说明被正常唤醒并获取了锁
            if current_owner == current_tid {
                if let Some(timer) = timer {
                    timer.cancel();
                }
                return Ok(0);
            }

            // 检查是否超时
            if let Some(ref timer) = timer {
                if timer.timeout() {
                    // 从等待队列移除
                    let mut futex_map_guard = FutexData::futex_map();
                    if let Some(bucket) = futex_map_guard.get_mut(&key) {
                        bucket.remove(futex_q.clone());
                    }
                    return Err(SystemError::ETIMEDOUT);
                }
            }

            // 重新尝试获取锁
            // 被唤醒后，futex值可能是：
            // 1. 0 - 没有等待者，直接获取
            // 2. FUTEX_WAITERS (0x80000000) - 还有其他等待者，需要保留WAITERS位
            // 3. 其他TID - 已被另一个线程获取，需要重新进入等待

            if current_owner == 0 {
                // 锁空闲，尝试获取
                let new_val = if (current_val & FUTEX_WAITERS) != 0 {
                    // 还有其他等待者，保留WAITERS位
                    current_tid | FUTEX_WAITERS
                } else {
                    // 没有其他等待者
                    current_tid
                };

                match atomic_futex.compare_exchange(
                    current_val,
                    new_val,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        return Ok(0);
                    }
                    Err(_) => {
                        // 继续循环等待
                        continue;
                    }
                }
            } else {
                // 锁被其他线程持有，继续等待
                continue;
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

            // 检查当前线程是否持有锁
            if owner_tid != current_tid {
                return Err(SystemError::EPERM);
            }

            // 检查是否有等待者
            let has_waiters = (uval & FUTEX_WAITERS) != 0;

            if !has_waiters {
                // 没有等待者，直接释放锁
                match atomic_futex.compare_exchange(uval, 0, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => return Ok(0),
                    Err(_) => continue, // CAS失败，重试
                }
            } else {
                // 有等待者，需要唤醒一个等待线程
                let mut futex_map_guard = FutexData::futex_map();
                let bucket = futex_map_guard.get_mut(&key);

                match bucket {
                    Some(bucket_mut) => {
                        // 唤醒一个等待者
                        let woken = bucket_mut.wake_up(key.clone(), None, 1)?;

                        if woken > 0 {
                            // 成功唤醒一个线程，清除当前线程的TID但保留WAITERS位
                            // 被唤醒的线程会尝试获取锁
                            match atomic_futex.compare_exchange(
                                uval,
                                FUTEX_WAITERS,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => {
                                    drop(futex_map_guard);
                                    FutexData::try_remove(&key);
                                    return Ok(0);
                                }
                                Err(_) => continue, // CAS失败，重试
                            }
                        } else {
                            // 没有成功唤醒任何线程，直接清除锁
                            match atomic_futex.compare_exchange(
                                uval,
                                0,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => {
                                    drop(futex_map_guard);
                                    FutexData::try_remove(&key);
                                    return Ok(0);
                                }
                                Err(_) => continue,
                            }
                        }
                    }
                    None => {
                        // 没有等待队列，直接清除锁
                        match atomic_futex.compare_exchange(
                            uval,
                            0,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => return Ok(0),
                            Err(_) => continue,
                        }
                    }
                }
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
        let _key = Self::get_futex_key(
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

        // 尝试获取锁（非阻塞）
        match atomic_futex.compare_exchange(0, current_tid, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => Ok(0),
            Err(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
        }
    }
}
