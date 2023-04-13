use core::sync::atomic::{AtomicI32, Ordering};

use crate::{arch::asm::current::current_pcb, kdebug, syscall::SystemError};

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
        if counter > 0 {
            Ok(Self {
                counter: AtomicI32::new(counter),
                wait_queue: WaitQueue::INIT,
            })
        } else {
            return Err(SystemError::EOVERFLOW);
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
        // 判断有没有进程在等待资源
        if self.wait_queue.len() > 0 {
            self.counter.fetch_add(1, Ordering::Release);
        } else {
            //尝试唤醒
            if !self.wait_queue.wakeup(0x_ffff_ffff_ffff_ffff) {
                //如果唤醒失败,打印错误信息
                kdebug!(
                    "Semaphore wakeup failed: current pid= {}, semaphore={:?}",
                    current_pcb().pid,
                    self
                );
            }
        }
    }
}
