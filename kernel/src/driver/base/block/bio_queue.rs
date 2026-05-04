use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};

use super::bio::BioRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BioQueueState {
    Running,
    Quiescing,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioQueueWake {
    WorkAvailable,
    Stopping,
}

/// 简单的FIFO BIO队列
pub struct BioQueue {
    inner: SpinLock<InnerBioQueue>,
    wait_queue: WaitQueue,
    batch_size: usize,
}

struct InnerBioQueue {
    queue: VecDeque<Arc<BioRequest>>,
    state: BioQueueState,
}

impl BioQueue {
    pub const DEFAULT_BATCH_SIZE: usize = 16;

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(InnerBioQueue {
                queue: VecDeque::new(),
                state: BioQueueState::Running,
            }),
            wait_queue: WaitQueue::default(),
            batch_size: Self::DEFAULT_BATCH_SIZE,
        })
    }

    /// 提交BIO请求（非阻塞）
    pub fn submit(&self, bio: Arc<BioRequest>) -> Result<(), system_error::SystemError> {
        let should_wakeup = {
            let mut inner = self.inner.lock_irqsave();
            if inner.state != BioQueueState::Running {
                return Err(system_error::SystemError::ESHUTDOWN);
            }
            let was_empty = inner.queue.is_empty();
            inner.queue.push_back(bio);
            was_empty
        };

        if should_wakeup {
            self.wait_queue.wakeup(None);
        }

        Ok(())
    }

    /// 批量取出请求（用于worker线程）
    pub fn drain_batch(&self) -> Vec<Arc<BioRequest>> {
        let mut inner = self.inner.lock_irqsave();
        let batch_size = self.batch_size.min(inner.queue.len());
        if batch_size == 0 {
            return Vec::new();
        }
        inner.queue.drain(..batch_size).collect()
    }

    /// Worker等待新请求或停止信号。
    ///
    /// BIO worker 是内核线程，不应被普通信号打断形成 ERESTARTSYS 日志风暴。
    pub fn wait_for_work_or_stop(&self) -> BioQueueWake {
        let _ = self.wait_queue.wait_event_uninterruptible(
            || {
                let inner = self.inner.lock_irqsave();
                !inner.queue.is_empty() || inner.state != BioQueueState::Running
            },
            None::<fn()>,
        );

        let inner = self.inner.lock_irqsave();
        if inner.queue.is_empty() && inner.state != BioQueueState::Running {
            BioQueueWake::Stopping
        } else {
            BioQueueWake::WorkAvailable
        }
    }

    /// 开始停止接收新 BIO，并唤醒 worker 排空已有队列。
    pub fn begin_quiesce(&self) {
        {
            let mut inner = self.inner.lock_irqsave();
            if inner.state == BioQueueState::Running {
                inner.state = BioQueueState::Quiescing;
            }
        }
        self.wait_queue.wakeup_all(None);
    }

    /// 停止队列并取出尚未提交给底层设备的 BIO。
    pub fn stop_and_drain(&self) -> Vec<Arc<BioRequest>> {
        let pending = {
            let mut inner = self.inner.lock_irqsave();
            inner.state = BioQueueState::Stopped;
            inner.queue.drain(..).collect()
        };
        self.wait_queue.wakeup_all(None);
        pending
    }
}
