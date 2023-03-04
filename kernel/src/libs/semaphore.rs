use core::sync::atomic::{AtomicI32, Ordering};

use crate::{arch::asm::current::current_pcb, include::bindings::bindings::EOVERFLOW, kdebug};

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
    /// @param sema 信号量对象
    /// @param count 信号量的初始值
    fn new(counter: i32) -> Result<Self, i32> {
        if counter > 0 {
            Ok(Self {
                counter: AtomicI32::new(counter),
                wait_queue: WaitQueue::INIT,
            })
        } else {
            return Err(-(EOVERFLOW as i32));
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn down(&self) {
        if self.counter.fetch_sub(1, Ordering::Release) <= 0 {
            self.counter.fetch_add(1, Ordering::Relaxed);
            self.wait_queue.sleep();
            //资源不充足,信号量<=0, 此时进程睡眠
        }
    }

    #[allow(dead_code)]
    #[inline]
    fn up(&self) {
        if self.wait_queue.len() > 0 {
            self.counter.fetch_add(1, Ordering::Release);
        } else {
            if !self.wait_queue.wakeup(0x_ffff_ffff_ffff_ffff) {
                kdebug!(
                    "Semaphore wakeup failed: current pid= {}, semaphore={:?}",
                    current_pcb().pid,
                    self
                );
            }
        }
    }
}
