use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType};
use crate::filesystem::vfs::fasync::{FAsyncItems, FASYNC_POLL_ERR, FASYNC_POLL_IN};
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::process::ProcessState;
use crate::time::Duration;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use system_error::SystemError;

pub const DEFAULT_BUFFER: usize = 212_992;
// Retained wire capacity plus queue entry/allocation overhead, not Linux's
// architecture-dependent skb truesize (which is not a portable userspace ABI).
pub const MESSAGE_OVERHEAD: usize = 512;

/// Accounting, resize and close are serialized by the FIFO lock. Atomics let
/// poll/error queries avoid a second lock while preserving the existing API.
#[derive(Debug)]
pub struct MessageQueue<Message>(pub Arc<Mutex<VecDeque<Message>>>, Arc<QueueLimits<Message>>);

#[derive(Debug)]
struct QueueLimits<Message> {
    receive_buffer: AtomicUsize,
    charge: fn(&Message) -> usize,
    queued_bytes: AtomicUsize,
    pending_error: AtomicBool,
    congested: AtomicBool,
    closed: AtomicBool,
    space: WaitQueue,
}

impl<Message> Clone for MessageQueue<Message> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

#[derive(Clone, Copy)]
enum Delivery {
    Kernel,
    Dump,
    User,
}

impl<Message> MessageQueue<Message> {
    pub fn new(charge: fn(&Message) -> usize) -> Self {
        Self(
            Arc::new(Mutex::new(VecDeque::new())),
            Arc::new(QueueLimits {
                receive_buffer: AtomicUsize::new(DEFAULT_BUFFER),
                charge,
                queued_bytes: AtomicUsize::new(0),
                pending_error: AtomicBool::new(false),
                congested: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                space: WaitQueue::default(),
            }),
        )
    }

    pub fn receive_buffer(&self) -> usize {
        self.1.receive_buffer.load(Ordering::Relaxed)
    }
    pub fn set_receive_buffer(&self, bytes: usize) {
        {
            let _queue = self.0.lock();
            self.1.receive_buffer.store(bytes, Ordering::Relaxed);
        }
        self.wake_space();
    }
    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }
    pub fn is_closed(&self) -> bool {
        self.1.closed.load(Ordering::Acquire)
    }
    pub fn has_error(&self) -> bool {
        self.1.pending_error.load(Ordering::Acquire)
    }
    pub fn take_error(&self) -> Option<SystemError> {
        self.1
            .pending_error
            .swap(false, Ordering::AcqRel)
            .then_some(SystemError::ENOBUFS)
    }
    pub fn close(&self) {
        let messages = {
            let mut queue = self.0.lock();
            self.1.closed.store(true, Ordering::Release);
            self.1.queued_bytes.store(0, Ordering::Relaxed);
            core::mem::take(&mut *queue)
        };
        self.wake_space();
        drop(messages);
    }
    fn wake_space(&self) {
        self.1.space.wake_all();
    }
    fn report_overrun(&self) -> bool {
        let _queue = self.0.lock();
        if self.is_closed() {
            return false;
        }
        if self.1.congested.swap(true, Ordering::Relaxed) {
            return false;
        }
        self.1.pending_error.store(true, Ordering::Release);
        true
    }
    /// Called after releasing the receive lock. PEEK must not call this.
    pub fn recover_if_empty(&self) {
        {
            let queue = self.0.lock();
            if queue.is_empty() {
                self.1.congested.store(false, Ordering::Relaxed);
            }
        }
        self.wake_space();
    }
    pub fn pop_front_locked(&self, queue: &mut VecDeque<Message>) {
        if let Some(message) = queue.pop_front() {
            self.1
                .queued_bytes
                .fetch_sub((self.1.charge)(&message), Ordering::Relaxed);
        }
    }
    // On retry the caller retains the message, not an unbounded pending queue.
    fn enqueue(
        &self,
        message: &mut Option<Message>,
        delivery: Delivery,
    ) -> Result<(), (SystemError, bool)> {
        let mut queue = self.0.lock();
        if self.is_closed() {
            return Err((SystemError::ECONNREFUSED, false));
        }
        let bytes = self
            .1
            .queued_bytes
            .load(Ordering::Relaxed)
            .checked_add((self.1.charge)(message.as_ref().unwrap()));
        let full = self.1.congested.load(Ordering::Relaxed)
            || (!queue.is_empty() && bytes.is_none_or(|bytes| bytes > self.receive_buffer()));
        if full {
            if matches!(delivery, Delivery::Kernel) {
                let notify = !self.1.congested.swap(true, Ordering::Relaxed);
                if notify {
                    self.1.pending_error.store(true, Ordering::Release);
                }
                return Err((SystemError::ENOBUFS, notify));
            }
            return Err((SystemError::EAGAIN_OR_EWOULDBLOCK, false));
        }
        let Some(bytes) = bytes else {
            return Err((SystemError::ENOBUFS, false));
        };
        queue.try_reserve(1).map_err(|_| {
            let notify = matches!(delivery, Delivery::Kernel)
                && !self.1.congested.swap(true, Ordering::Relaxed);
            if notify {
                self.1.pending_error.store(true, Ordering::Release);
            }
            (SystemError::ENOBUFS, notify)
        })?;
        queue.push_back(message.take().unwrap());
        self.1.queued_bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }
}

/// Pin before releasing the port table. Explicit close rejects future delivery
/// even while a blocked sender still holds this reference.
#[derive(Debug)]
pub struct MessageReceiver<Message> {
    message_queue: MessageQueue<Message>,
    wait_queue: Arc<WaitQueue>,
    epoll_items: Arc<EPollItems>,
    fasync_items: Arc<FAsyncItems>,
}
impl<Message> Clone for MessageReceiver<Message> {
    fn clone(&self) -> Self {
        Self {
            message_queue: self.message_queue.clone(),
            wait_queue: self.wait_queue.clone(),
            epoll_items: self.epoll_items.clone(),
            fasync_items: self.fasync_items.clone(),
        }
    }
}
impl<Message> MessageReceiver<Message> {
    pub fn is_closed(&self) -> bool {
        self.message_queue.is_closed()
    }
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
    fn notify(&self, success: bool) {
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        let events = if success {
            EPollEventType::EPOLLIN
        } else {
            EPollEventType::EPOLLERR
        };
        let _ = EventPoll::wakeup_epoll(self.epoll_items.as_ref().as_ref(), events);
        self.fasync_items.send_sigio(if success {
            FASYNC_POLL_IN
        } else {
            FASYNC_POLL_ERR
        });
    }
    fn deliver(&self, message: Message, delivery: Delivery) -> Result<(), SystemError> {
        match self.message_queue.enqueue(&mut Some(message), delivery) {
            Ok(()) => {
                self.notify(true);
                Ok(())
            }
            Err((error, notify)) => {
                if notify {
                    self.notify(false);
                }
                Err(error)
            }
        }
    }
    pub fn enqueue_message(&self, message: Message) -> Result<(), SystemError> {
        self.deliver(message, Delivery::Kernel)
    }
    pub fn enqueue_dump(&self, message: Message) -> Result<(), SystemError> {
        self.deliver(message, Delivery::Dump)
    }
    pub fn enqueue_user(
        &self,
        message: Message,
        nonblocking: bool,
        timeout: Option<Duration>,
    ) -> Result<(), SystemError> {
        let mut pending = Some(message);
        let mut result = Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        if nonblocking {
            result = self
                .message_queue
                .enqueue(&mut pending, Delivery::User)
                .map_err(|(error, _)| error);
        } else {
            // The wait primitive computes one deadline. Acquiring capacity in
            // the predicate prevents a competing sender from stealing the slot.
            self.message_queue
                .1
                .space
                .wait_event_interruptible_timeout(
                    || {
                        result = self
                            .message_queue
                            .enqueue(&mut pending, Delivery::User)
                            .map_err(|(error, _)| error);
                        result != Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                    },
                    timeout,
                )?;
        }
        if result.is_ok() {
            self.notify(true);
        }
        result
    }
    pub fn report_overrun(&self) {
        if self.message_queue.report_overrun() {
            self.notify(false);
        }
    }
}
