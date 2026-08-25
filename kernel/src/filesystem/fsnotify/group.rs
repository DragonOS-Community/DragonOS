//! [`FsNotifyGroup`]: a notification consumer (one group per inotify fd).

use alloc::boxed::Box;
use alloc::sync::Arc;
use hashbrown::HashMap;

use crate::filesystem::epoll::event_poll::EPollItemList;
use crate::libs::mutex::Mutex;
use crate::libs::wait_queue::WaitQueue;

use super::mark::FsNotifyMark;
use super::{FsNotifyBackend, FsNotifyObjectId};

/// A notification consumer. One group per inotify fd.
///
/// - `backend`: the concrete backend (with its own internal lock); the fsnotify
///   layer only depends on the [`FsNotifyBackend`] trait;
/// - `marks`: all marks owned by the group (strong references, pinning the
///   watched inodes);
/// - `wait_queue` / `epitems`: read blocking wakeup and epoll integration.
#[derive(Debug)]
pub struct FsNotifyGroup {
    pub backend: Box<dyn FsNotifyBackend>,
    pub marks: Mutex<HashMap<FsNotifyObjectId, Arc<FsNotifyMark>>>,
    pub wait_queue: WaitQueue,
    pub epitems: EPollItemList,
}

impl FsNotifyGroup {
    pub fn new(backend: Box<dyn FsNotifyBackend>) -> Arc<Self> {
        Arc::new(Self {
            backend,
            marks: Mutex::new(HashMap::new()),
            wait_queue: WaitQueue::default(),
            epitems: EPollItemList::default(),
        })
    }
}
