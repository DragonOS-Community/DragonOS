use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};

use super::bio::BioRequest;

/// 简单的FIFO BIO队列
pub struct BioQueue {
    inner: SpinLock<InnerBioQueue>,
    wait_queue: WaitQueue,
    batch_size: usize,
}

struct InnerBioQueue {
    queue: VecDeque<Arc<BioRequest>>,
}

impl BioQueue {
    pub const DEFAULT_BATCH_SIZE: usize = 16;

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(InnerBioQueue {
                queue: VecDeque::new(),
            }),
            wait_queue: WaitQueue::default(),
            batch_size: Self::DEFAULT_BATCH_SIZE,
        })
    }

    /// 提交BIO请求（非阻塞）
    pub fn submit(&self, bio: Arc<BioRequest>) {
        let should_wakeup = {
            let mut inner = self.inner.lock_irqsave();
            let was_empty = inner.queue.is_empty();
            inner.queue.push_back(bio);
            was_empty
        };

        if should_wakeup {
            self.wait_queue.wakeup(None);
        }
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

    /// 检查队列是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.lock_irqsave().queue.is_empty()
    }

    /// Worker等待新请求（正确标记为 IO 等待）
    pub fn wait_for_work(&self) -> Result<(), system_error::SystemError> {
        self.wait_queue
            .wait_event_io_interruptible(|| !self.is_empty(), None::<fn()>)
    }
}
