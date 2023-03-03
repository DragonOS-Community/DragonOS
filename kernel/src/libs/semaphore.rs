use core::sync::atomic::{AtomicU32, Ordering};

use super::wait_queue::WaitQueue;

struct Semaphore {
    counter: AtomicU32,
    wait_queue: WaitQueue,
}

impl Semaphore {
    #[allow(dead_code)]
    #[inline]
    fn new(counter: u32) -> Self {
        Self {
            counter: AtomicU32::new(counter),
            wait_queue: WaitQueue::INIT,
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn down(&self) {
        if self.counter.fetch_sub(1, Ordering::Release) <= 0 {
            self.counter.fetch_add(1, Ordering::Relaxed);
            self.wait_queue.sleep_uninterruptible();
        } //资源不充足,信号量<=0
    }

    #[allow(dead_code)]
    #[inline]
    fn up(&self) {
        if self.wait_queue.len() > 0 {
            self.counter.fetch_add(1, Ordering::Release);
        } else {
            self.wait_queue.wakeup(0x_ffff_ffff_ffff_ffff);
            //返回值没有处理
        }
    }
}
