use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
};

use alloc::{collections::LinkedList, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::spinlock::SpinLockGuard,
    process::{Pid, ProcessControlBlock, ProcessManager},
    sched::{schedule, SchedMode},
};

use super::spinlock::SpinLock;

#[derive(Debug)]
struct MutexInner {
    /// 当前Mutex是否已经被上锁(上锁时，为true)
    is_locked: bool,
    /// 等待获得这个锁的进程的链表
    wait_list: LinkedList<Arc<ProcessControlBlock>>,
}

/// @brief Mutex互斥量结构体
/// 请注意！由于Mutex属于休眠锁，因此，如果您的代码可能在中断上下文内执行，请勿采用Mutex！
#[derive(Debug)]
pub struct Mutex<T> {
    /// 该Mutex保护的数据
    data: UnsafeCell<T>,
    /// Mutex内部的信息
    inner: SpinLock<MutexInner>,
}

/// @brief Mutex的守卫
#[derive(Debug)]
pub struct MutexGuard<'a, T: 'a> {
    lock: &'a Mutex<T>,
}

unsafe impl<T> Sync for Mutex<T> where T: Send {}

impl<T> Mutex<T> {
    /// @brief 初始化一个新的Mutex对象
    #[allow(dead_code)]
    pub const fn new(value: T) -> Self {
        return Self {
            data: UnsafeCell::new(value),
            inner: SpinLock::new(MutexInner {
                is_locked: false,
                wait_list: LinkedList::new(),
            }),
        };
    }

    /// @brief 对Mutex加锁
    /// @return MutexGuard<T> 返回Mutex的守卫，您可以使用这个守卫来操作被保护的数据
    #[inline(always)]
    #[allow(dead_code)]
    pub fn lock(&self) -> MutexGuard<T> {
        loop {
            let mut inner: SpinLockGuard<MutexInner> = self.inner.lock();
            // 当前mutex已经上锁
            if inner.is_locked {
                // 检查当前进程是否处于等待队列中,如果不在，就加到等待队列内
                if !self.check_pid_in_wait_list(&inner, ProcessManager::current_pcb().pid()) {
                    inner.wait_list.push_back(ProcessManager::current_pcb());
                }

                // 加到等待唤醒的队列，然后睡眠
                drop(inner);
                self.__sleep();
            } else {
                // 加锁成功
                inner.is_locked = true;
                drop(inner);
                break;
            }
        }

        // 加锁成功，返回一个守卫
        return MutexGuard { lock: self };
    }

    /// @brief 尝试对Mutex加锁。如果加锁失败，不会将当前进程加入等待队列。
    /// @return Ok 加锁成功，返回Mutex的守卫
    /// @return Err 如果Mutex当前已经上锁，则返回Err.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn try_lock(&self) -> Result<MutexGuard<T>, SystemError> {
        let mut inner = self.inner.lock();

        // 如果当前mutex已经上锁，则失败
        if inner.is_locked {
            return Err(SystemError::EBUSY);
        } else {
            // 加锁成功
            inner.is_locked = true;
            return Ok(MutexGuard { lock: self });
        }
    }

    /// @brief Mutex内部的睡眠函数
    fn __sleep(&self) {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).ok();
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
    }

    /// @brief 放锁。
    ///
    /// 本函数只能是私有的，且只能被守卫的drop方法调用，否则将无法保证并发安全。
    fn unlock(&self) {
        let mut inner: SpinLockGuard<MutexInner> = self.inner.lock();
        // 当前mutex一定是已经加锁的状态
        assert!(inner.is_locked);
        // 标记mutex已经解锁
        inner.is_locked = false;
        if inner.wait_list.is_empty() {
            return;
        }

        // wait_list不为空，则获取下一个要被唤醒的进程的pcb
        let to_wakeup: Arc<ProcessControlBlock> = inner.wait_list.pop_front().unwrap();
        drop(inner);

        ProcessManager::wakeup(&to_wakeup).ok();
    }

    /// @brief 检查进程是否在该mutex的等待队列内
    #[inline]
    fn check_pid_in_wait_list(&self, inner: &MutexInner, pid: Pid) -> bool {
        for p in inner.wait_list.iter() {
            if p.pid() == pid {
                // 在等待队列内
                return true;
            }
        }

        // 不在等待队列内
        return false;
    }
}

/// 实现Deref trait，支持通过获取MutexGuard来获取临界区数据的不可变引用
impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.lock.data.get() };
    }
}

/// 实现DerefMut trait，支持通过获取MutexGuard来获取临界区数据的可变引用
impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return unsafe { &mut *self.lock.data.get() };
    }
}

/// @brief 为MutexGuard实现Drop方法，那么，一旦守卫的生命周期结束，就会自动释放自旋锁，避免了忘记放锁的情况
impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}
