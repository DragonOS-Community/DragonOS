use alloc::sync::Arc;
use crate::filesystem::vfs::IndexNode;

use super::Socket;
use super::common::poll_unit::EPollItems;

#[derive(Debug)]
pub struct Inode {
    inner: Arc<dyn Socket>,
}

impl IndexNode for Inode {
    fn read_at(
            &self,
            offset: usize,
            len: usize,
            buf: &mut [u8],
            data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        ) -> Result<usize, system_error::SystemError> {
        self.inner.read_at(offset, len, buf, data)
    }
}

use super::common::poll_unit::WaitQueue;

impl Socket for Inode {
    fn epoll_items(&self) -> EPollItems {
        self.inner.epoll_items()
    }
    
    fn wait_queue(&self) -> WaitQueue {
        self.inner.wait_queue()
    }

    fn update_io_events(&self) -> Result<crate::net::event_poll::EPollEventType, system_error::SystemError> {
        todo!()
    }
}

impl Inode {
    pub fn set_nonblock(&self, nonblock: bool) {
        todo!()
    }

    pub fn set_close_on_exec(&self, close_on_exec: bool) {
        todo!()
    }
}