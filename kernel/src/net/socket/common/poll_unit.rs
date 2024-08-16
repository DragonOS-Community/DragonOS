use alloc::{collections::LinkedList, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::{spinlock::SpinLock, wait_queue::EventWaitQueue}, net::event_poll::{EPollEventType, EPollItem, EventPoll}, process::ProcessManager, sched::{schedule, SchedMode}};



#[derive(Debug)]
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
    pub fn new(wait_queue: Arc<EventWaitQueue>) -> Self {
        Self {
            wait_queue,
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

#[derive(Debug)]
pub struct EPollItems {
    items: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl Default for EPollItems {
    fn default() -> Self {
        Self {
            items: SpinLock::new(LinkedList::new()),
        }
    }
}

impl EPollItems {
    pub fn new() -> Self {
        Self {
            items: SpinLock::new(LinkedList::new()),
        }
    }

    pub fn add(&self, item: Arc<EPollItem>) {
        self.items.lock_irqsave().push_back(item);
    }

    pub fn remove(&self, item: &Arc<EPollItem>) -> Result<(), SystemError> {
        let to_remove = self
            .items
            .lock_irqsave()
            .extract_if(|x| Arc::ptr_eq(x, item))
            .collect::<Vec<_>>();

        let result = if !to_remove.is_empty() {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        };

        drop(to_remove);
        return result;
    }

    pub fn clear(&self) -> Result<(), SystemError> {
        let mut guard = self.items.lock_irqsave();
        let mut result = Ok(());
        guard.iter().for_each(|item| {
            if let Some(epoll) = item.epoll().upgrade() {
                let _ = EventPoll::ep_remove(&mut epoll.lock_irqsave(), item.fd(), None)
                    .map_err(|e| {
                        result = Err(e);
                    }
                );
            }
        });
        guard.clear();
        return result;
    }
}