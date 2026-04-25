use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use system_error::SystemError;

use crate::libs::wait_queue::WaitQueue;

/// @brief Mutex互斥量结构体
/// 请注意！由于Mutex属于休眠锁，因此，如果您的代码可能在中断上下文内执行，请勿采用Mutex！
#[derive(Debug)]
pub struct Mutex<T> {
    /// 该Mutex保护的数据
    data: UnsafeCell<T>,
    /// Mutex锁状态
    lock: AtomicBool,
    /// 等待队列（Waiter/Waker 机制避免唤醒丢失）
    wait_queue: WaitQueue,
}
unsafe impl<T> Sync for Mutex<T> where T: Send {}

/// @brief Mutex的守卫
#[must_use]
#[derive(Debug)]
pub struct MutexGuard<'a, T: 'a> {
    lock: &'a Mutex<T>,
}
impl<T: ?Sized> !Send for MutexGuard<'_, T> {}
unsafe impl<T: Sync> Sync for MutexGuard<'_, T> {}

impl<T> Mutex<T> {
    /// @brief 初始化一个新的Mutex对象
    #[allow(dead_code)]
    pub const fn new(value: T) -> Self {
        return Self {
            data: UnsafeCell::new(value),
            lock: AtomicBool::new(false),
            wait_queue: WaitQueue::default(),
        };
    }

    /// @brief 对Mutex加锁
    /// @return MutexGuard<T> 返回Mutex的守卫，您可以使用这个守卫来操作被保护的数据
    #[inline(always)]
    #[allow(dead_code)]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.wait_queue.wait_until(|| self.try_lock().ok())
    }

    /// @brief 尝试对Mutex加锁。如果加锁失败，不会将当前进程加入等待队列。
    /// @return Ok 加锁成功，返回Mutex的守卫
    /// @return Err 如果Mutex当前已经上锁，则返回Err.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, SystemError> {
        if self.acquire_lock() {
            return Ok(MutexGuard { lock: self });
        }
        Err(SystemError::EBUSY)
    }

    /// @brief 放锁。
    ///
    /// 本函数只能是私有的，且只能被守卫的drop方法调用，否则将无法保证并发安全。
    fn unlock(&self) {
        self.release_lock();
        self.wait_queue.wake_one();
    }

    fn acquire_lock(&self) -> bool {
        self.lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn release_lock(&self) {
        self.lock.store(false, Ordering::Release);
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
