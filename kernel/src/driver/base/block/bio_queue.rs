use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;

use crate::libs::{
    prepared_queue::{Node, Queue},
    resource_wait::ResourceWaitQueue,
    spinlock::SpinLock,
};

use super::bio::BioRequest;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BioQueueState {
    Running,
    Stopped,
}

pub enum BioQueueWake {
    WorkAvailable,
    Stopping,
}

/// FIFO admission is allocation-free once the caller has prepared its node.
/// The worker and teardown transfer owners without allocating temporary batches.
pub struct BioQueue {
    inner: SpinLock<InnerBioQueue>,
    wait_queue: ResourceWaitQueue,
}
struct InnerBioQueue {
    queue: Queue<Arc<BioRequest>>,
    state: BioQueueState,
}

pub(crate) type PreparedBio = Box<Node<Arc<BioRequest>>>;

impl BioQueue {
    pub(crate) fn try_new() -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            inner: SpinLock::new(InnerBioQueue {
                queue: Queue::new(),
                state: BioQueueState::Running,
            }),
            wait_queue: ResourceWaitQueue::new(),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    /// Call before taking the device admission lock.
    pub(crate) fn prepare(bio: Arc<BioRequest>) -> Result<PreparedBio, SystemError> {
        Node::try_new(bio)
    }

    pub(crate) fn submit(&self, node: PreparedBio) -> Result<(), PreparedBio> {
        let mut inner = self.inner.lock_irqsave();
        if inner.state != BioQueueState::Running {
            return Err(node);
        }
        let was_empty = inner.queue.is_empty();
        inner.queue.push_node(node);
        drop(inner);
        if was_empty {
            self.wait_queue.notify_one();
        }
        Ok(())
    }

    pub(crate) fn pop(&self) -> Option<Arc<BioRequest>> {
        let node = self.inner.lock_irqsave().queue.pop_node();
        node.map(|node| node.into_value())
    }

    pub fn wait_for_work_or_stop(&self) -> BioQueueWake {
        self.wait_queue
            .wait_uninterruptible(|| {
                let inner = self.inner.lock_irqsave();
                if !inner.queue.is_empty() {
                    Some(BioQueueWake::WorkAvailable)
                } else if inner.state == BioQueueState::Stopped {
                    Some(BioQueueWake::Stopping)
                } else {
                    None
                }
            })
            .expect("BIO worker resource wait")
    }

    pub(crate) fn stop_and_drain(&self) -> Queue<Arc<BioRequest>> {
        let pending = {
            let mut inner = self.inner.lock_irqsave();
            inner.state = BioQueueState::Stopped;
            core::mem::replace(&mut inner.queue, Queue::new())
        };
        self.wait_queue.notify_all();
        pending
    }
}
