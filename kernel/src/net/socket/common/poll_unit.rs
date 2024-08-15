use alloc::{collections::LinkedList, sync::{Weak, Arc}, vec::Vec};
use system_error::SystemError;

use crate::{libs::{spinlock::SpinLock, wait_queue::EventWaitQueue}, net::event_poll::{EPollEventType, EPollItem, EventPoll}, process::ProcessManager, sched::{schedule, SchedMode}};



#[derive(Debug)]
pub struct PollUnit {
    /// socket的waitqueue
    wait_queue: Arc<EventWaitQueue>,

    pub epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl Default for PollUnit {
    fn default() -> Self {
        Self {
            wait_queue: Default::default(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }
}

impl PollUnit {
    pub fn new(wait_queue: Arc<EventWaitQueue>) -> Self {
        Self {
            wait_queue,
            epitems: SpinLock::new(LinkedList::new()),
        }
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

    pub fn clear_epoll(&self) -> Result<(), SystemError> {
        for epitem in self.epitems.lock_irqsave().iter() {
            let epoll = epitem.epoll();

            if let Some(epoll) = epoll.upgrade() {
                EventPoll::ep_remove(&mut epoll.lock_irqsave(), epitem.fd(), None)?;
            }
        }

        Ok(())
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
        F: FnMut() -> Result<R, SystemError>
    {
        loop {
            match f() {
                Ok(r) => return Ok(r),
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    self.wait_for(events);
                },
                Err(e) => return Err(e),
            }
        }
    }
}