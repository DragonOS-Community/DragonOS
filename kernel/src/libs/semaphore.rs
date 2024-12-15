use core::sync::atomic::{AtomicI32, Ordering};

use log::debug;
use system_error::SystemError;

use crate::process::ProcessManager;

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
                wait_queue: WaitQueue::default(),
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
            self.wait_queue.sleep().ok();
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
            if !self.wait_queue.wakeup(None) {
                //如果唤醒失败,打印错误信息
                debug!(
                    "Semaphore wakeup failed: current pid= {}, semaphore={:?}",
                    ProcessManager::current_pcb().pid().into(),
                    self
                );
            }
        }
    }
}
