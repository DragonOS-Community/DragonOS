use alloc::{collections::LinkedList, sync::{Weak, Arc}, vec::Vec};
use system_error::SystemError;

use crate::{libs::{spinlock::SpinLock, wait_queue::EventWaitQueue}, net::event_poll::{EPollItem, EventPoll}, process::ProcessManager, sched::{schedule, SchedMode}};



#[derive(Debug)]
pub struct PollUnit {
    /// socket的waitqueue
    wait_queue: Arc<EventWaitQueue>,

    pub epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl PollUnit {
    pub fn new(wait_queue: Option<Arc<EventWaitQueue>>) -> Self {
        Self {
            wait_queue: wait_queue.unwrap_or(Arc::new(EventWaitQueue::new())),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }

    /// # `sleep`
    /// 在socket的等待队列上睡眠
    pub fn sleep(&self, events: u64) {
        unsafe {
            ProcessManager::preempt_disable();
            self.wait_queue.sleep_without_schedule(events);
            ProcessManager::preempt_enable();
        }
        schedule(SchedMode::SM_NONE);
    }

    pub fn add_epoll(&self, epitem: Arc<EPollItem>) {
        self.epitems.lock_irqsave().push_back(epitem)
    }

    pub fn remove_epoll(&self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let is_remove = !self
            .epitems
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(epoll))
            .collect::<Vec<_>>()
            .is_empty();

        if is_remove {
            return Ok(());
        }

        Err(SystemError::ENOENT)
    }

    /// # `wakeup_any`
    /// 唤醒该队列上等待events的进程
    /// ## 参数
    /// - events: 发生的事件
    /// 需要注意的是，只要触发了events中的任意一件事件，进程都会被唤醒
    pub fn wakeup_any(&self, events: u64) {
        self.wait_queue.wakeup_any(events);
    }
}