use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType};
use crate::filesystem::vfs::fasync::{FAsyncItems, FASYNC_POLL_ERR, FASYNC_POLL_IN};
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::process::ProcessState;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

/// Netlink Socket 的消息队列
#[derive(Debug)]
pub struct MessageQueue<Message>(
    pub Arc<Mutex<VecDeque<Message>>>,
    Option<Arc<QueueLimits<Message>>>,
);

#[derive(Debug)]
struct QueueLimits<Message> {
    max_messages: usize,
    max_bytes: usize,
    charge: fn(&Message) -> usize,
    pending_error: AtomicBool,
    // Changes are serialized by the message queue lock.
    congested: AtomicBool,
}

impl<Message> Clone for MessageQueue<Message> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

impl<Message> MessageQueue<Message> {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())), None)
    }

    /// Opt-in limits leave existing protocols' queue behavior unchanged.
    /// Accounting is recomputed under the queue lock, so existing consumers
    /// can continue to pop directly without a second accounting lock.
    pub fn with_limits(
        max_messages: usize,
        max_bytes: usize,
        charge: fn(&Message) -> usize,
    ) -> Self {
        Self(
            Arc::new(Mutex::new(VecDeque::new())),
            Some(Arc::new(QueueLimits {
                max_messages,
                max_bytes,
                charge,
                pending_error: AtomicBool::new(false),
                congested: AtomicBool::new(false),
            })),
        )
    }

    pub fn has_error(&self) -> bool {
        self.1
            .as_ref()
            .is_some_and(|limits| limits.pending_error.load(Ordering::Acquire))
    }

    pub fn take_error(&self) -> Option<SystemError> {
        self.1
            .as_ref()
            .is_some_and(|limits| limits.pending_error.swap(false, Ordering::AcqRel))
            .then_some(SystemError::ENOBUFS)
    }

    fn report_overrun(&self) -> bool {
        if let Some(limits) = &self.1 {
            limits.pending_error.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Like netlink_rcv_wake: consuming SO_ERROR alone does not end congestion.
    /// Call only after releasing the receive queue lock.
    pub fn recover_if_empty(&self) {
        if let Some(limits) = &self.1 {
            let queue = self.0.lock();
            if queue.is_empty() {
                limits.congested.store(false, Ordering::Relaxed);
            }
        }
    }

    // The boolean records whether this operation must report an error event.
    fn enqueue(&self, message: Message) -> Result<(), (SystemError, bool)> {
        let mut queue = self.0.lock();
        if let Some(limits) = &self.1 {
            if limits.congested.load(Ordering::Relaxed) {
                return Err((SystemError::ENOBUFS, false));
            }
            let charge = limits.charge;
            let bytes = queue.iter().try_fold(charge(&message), |sum, entry| {
                sum.checked_add(charge(entry))
            });
            if queue.len() >= limits.max_messages
                || bytes.is_none_or(|bytes| bytes > limits.max_bytes)
            {
                limits.congested.store(true, Ordering::Relaxed);
                limits.pending_error.store(true, Ordering::Release);
                return Err((SystemError::ENOBUFS, true));
            }
        }
        queue.try_reserve(1).map_err(|_| {
            if let Some(limits) = &self.1 {
                limits.pending_error.store(true, Ordering::Release);
                (SystemError::ENOBUFS, true)
            } else {
                (SystemError::ENOMEM, false)
            }
        })?;
        queue.push_back(message);
        Ok(())
    }
}

/// Netlink Socket 的消息接收器，记录在当前网络命名空间的 Netlink Socket 表中，负责将消息压入对应的消息队列，并唤醒等待的线程
#[derive(Debug)]
pub struct MessageReceiver<Message> {
    message_queue: MessageQueue<Message>,
    wait_queue: Arc<WaitQueue>,
    epoll_items: Arc<EPollItems>,
    fasync_items: Arc<FAsyncItems>,
}

impl<Message> MessageReceiver<Message> {
    pub fn new(
        message_queue: MessageQueue<Message>,
        wait_queue: Arc<WaitQueue>,
        epoll_items: Arc<EPollItems>,
        fasync_items: Arc<FAsyncItems>,
    ) -> Self {
        Self {
            message_queue,
            wait_queue,
            epoll_items,
            fasync_items,
        }
    }

    pub fn enqueue_message(&self, message: Message) -> Result<(), SystemError> {
        let result = match self.message_queue.enqueue(message) {
            Ok(()) => Ok(()),
            Err((error, false)) => return Err(error),
            Err((error, true)) => Err(error),
        };
        // 唤醒等待队列中的线程
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        let events = if result.is_ok() {
            EPollEventType::EPOLLIN
        } else {
            EPollEventType::EPOLLERR
        };
        let _ = EventPoll::wakeup_epoll(self.epoll_items.as_ref().as_ref(), events);
        self.fasync_items.send_sigio(if result.is_ok() {
            FASYNC_POLL_IN
        } else {
            FASYNC_POLL_ERR
        });
        result
    }

    /// Report a kernel reply allocation failure without allocating a message.
    pub fn report_overrun(&self) {
        if !self.message_queue.report_overrun() {
            return;
        }
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        let _ =
            EventPoll::wakeup_epoll(self.epoll_items.as_ref().as_ref(), EPollEventType::EPOLLERR);
        self.fasync_items.send_sigio(FASYNC_POLL_ERR);
    }
}
