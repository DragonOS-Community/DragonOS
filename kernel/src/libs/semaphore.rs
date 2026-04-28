use core::sync::atomic::{AtomicI32, Ordering};

use system_error::SystemError;

use super::wait_queue::WaitQueue;

/// @brief 信号量的结构体
#[derive(Debug)]
struct Semaphore {
    counter: AtomicI32,
    wait_queue: WaitQueue,
}

impl Semaphore {
    #[allow(dead_code)]
    #[inline]
    /// @brief 初始化信号量
    ///
    /// @param count 信号量的初始值
    /// @return 条件满足返回semaphore对象,条件不满足返回err信息
    fn new(counter: i32) -> Result<Self, SystemError> {
        if counter >= 0 {
            Ok(Self {
                counter: AtomicI32::new(counter),
                wait_queue: WaitQueue::default(),
            })
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn down(&self) {
        loop {
            // 尝试占用一个计数
            if self.counter.fetch_sub(1, Ordering::Acquire) > 0 {
                return;
            }

            // 回滚，本次未成功
            self.counter.fetch_add(1, Ordering::Relaxed);

            // 资源不足，阻塞等待计数可用
            // Mesa 风格无锁信号量
            let res = self.wait_queue.wait_event_uninterruptible(
                || self.counter.load(Ordering::Acquire) > 0,
                None::<fn()>,
            );
            if let Err(e) = res {
                log::error!("Semaphore::down wait failed: {:?}, retrying", e);
                continue;
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn up(&self) {
        // Mesa 风格无锁信号量：总是增加计数，然后尝试唤醒一个等待者。
        // 如需严格对齐 Linux，需引入 SpinLock 并改为 Hoare 风格的锁内队列操作。
        self.counter.fetch_add(1, Ordering::Release);
        self.wait_queue.wakeup(None);
    }
}
