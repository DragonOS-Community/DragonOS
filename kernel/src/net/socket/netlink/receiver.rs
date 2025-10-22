use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::process::ProcessState;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use system_error::SystemError;

/// Netlink Socket 的消息队列
#[derive(Debug)]
pub struct MessageQueue<Message>(pub Arc<SpinLock<VecDeque<Message>>>);

impl<Message> Clone for MessageQueue<Message> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Message> MessageQueue<Message> {
    pub fn new() -> Self {
        Self(Arc::new(SpinLock::new(VecDeque::new())))
    }

    fn enqueue(&self, message: Message) -> Result<(), SystemError> {
        // FIXME: 确保消息队列不会超过最大长度
        self.0.lock().push_back(message);
        Ok(())
    }
}

/// Netlink Socket 的消息接收器，记录在当前网络命名空间的 Netlink Socket 表中，负责将消息压入对应的消息队列，并唤醒等待的线程
#[derive(Debug)]
pub struct MessageReceiver<Message> {
    message_queue: MessageQueue<Message>,
    wait_queue: Arc<WaitQueue>,
}

impl<Message> MessageReceiver<Message> {
    pub fn new(message_queue: MessageQueue<Message>, wait_queue: Arc<WaitQueue>) -> Self {
        Self {
            message_queue,
            wait_queue,
        }
    }

    pub fn enqueue_message(&self, message: Message) -> Result<(), SystemError> {
        self.message_queue.enqueue(message)?;
        // 唤醒等待队列中的线程
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        Ok(())
    }
}
