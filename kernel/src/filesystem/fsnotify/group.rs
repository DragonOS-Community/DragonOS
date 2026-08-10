//! [`FsNotifyGroup`]：一个通知消费者（一个 inotify fd 对应一个 group）。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystem::epoll::event_poll::EPollItemList;
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;

use super::mark::FsNotifyMark;
use super::FsNotifyBackend;

/// 一个通知消费者。一个 inotify fd 对应一个 group。
///
/// - `backend`：具体后端（自带内部锁），fsnotify 层只依赖 [`FsNotifyBackend`] trait；
/// - `marks`：group 拥有的所有 mark（强引用，pin 住被监听 inode）；
/// - `wait_queue` / `epitems`：read 阻塞唤醒与 epoll 集成。
#[derive(Debug)]
pub struct FsNotifyGroup {
    pub backend: Box<dyn FsNotifyBackend>,
    pub marks: Mutex<Vec<Arc<FsNotifyMark>>>,
    pub wait_queue: WaitQueue,
    pub epitems: EPollItemList,
}

impl FsNotifyGroup {
    pub fn new(backend: Box<dyn FsNotifyBackend>) -> Arc<Self> {
        Arc::new(Self {
            backend,
            marks: Mutex::new(Vec::new()),
            wait_queue: WaitQueue::default(),
            epitems: EPollItemList::default(),
        })
    }
}
