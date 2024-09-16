use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    libs::{spinlock::SpinLock, wait_queue::EventWaitQueue},
    net::event_poll::{EPollEventType, EPollItem, EventPoll},
    process::ProcessManager,
    sched::{schedule, SchedMode},
};

#[derive(Debug, Clone)]
pub struct WaitQueue {
    /// socket的waitqueue
    wait_queue: Arc<EventWaitQueue>,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self {
            wait_queue: Default::default(),
        }
    }
}

impl WaitQueue {
    pub fn new(wait_queue: EventWaitQueue) -> Self {
        Self {
            wait_queue: Arc::new(wait_queue),
        }
    }

    /// # `wakeup_any`
    /// 唤醒该队列上等待events的进程
    /// ## 参数
    /// - events: 发生的事件
    /// 需要注意的是，只要触发了events中的任意一件事件，进程都会被唤醒
    pub fn wakeup_any(&self, events: EPollEventType) {
        self.wait_queue.wakeup_any(events.bits() as u64);
    }

    /// # `wait_for`
    /// 等待events事件发生
    pub fn wait_for(&self, events: EPollEventType) {
        unsafe {
            ProcessManager::preempt_disable();
            self.wait_queue.sleep_without_schedule(events.bits() as u64);
            ProcessManager::preempt_enable();
        }
        schedule(SchedMode::SM_NONE);
    }

    /// # `busy_wait`
    /// 轮询一个会返回EPAGAIN_OR_EWOULDBLOCK的函数
    pub fn busy_wait<F, R>(&self, events: EPollEventType, mut f: F) -> Result<R, SystemError>
    where
        F: FnMut() -> Result<R, SystemError>,
    {
        loop {
            match f() {
                Ok(r) => return Ok(r),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    self.wait_for(events);
                }
                Err(e) => return Err(e),
            }
        }
    }
}
