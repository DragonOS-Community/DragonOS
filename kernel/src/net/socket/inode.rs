use alloc::sync::Arc;
use system_error::SystemError;
use crate::filesystem::vfs::IndexNode;

use crate::net::socket::*;

#[derive(Debug)]
pub struct Inode {
    inner: Arc<dyn Socket>,
    epoll_items: EPollItems,
}

impl IndexNode for Inode {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.inner.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.inner.write(buf)
    }

    
    /* Following are not yet available in socket */
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    /* filesystem associate interfaces are about unix and netlink socket */
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        unimplemented!()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        unimplemented!()
    }

    fn poll(&self, private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        drop(private_data);
        self.update_io_events().map(|event| event.bits() as usize)
    }
}

use super::common::poll_unit::WaitQueue;

impl Socket for Inode {

    fn wait_queue(&self) -> WaitQueue {
        self.inner.wait_queue()
    }

    fn update_io_events(&self) -> Result<crate::net::event_poll::EPollEventType, SystemError> {
        self.inner.update_io_events()
    }
}

impl Inode {
    pub fn new(inner: Arc<dyn Socket>) -> Arc<Self> {
        Arc::new(Self { inner, epoll_items: EPollItems::default() })
    }

    /// # `epoll_items`
    /// socket的epoll事件集
    pub fn epoll_items(&self) -> EPollItems {
        self.epoll_items.clone()
    }

    pub fn set_nonblock(&self, nonblock: bool) {
        todo!()
    }

    pub fn set_close_on_exec(&self, close_on_exec: bool) {
        todo!()
    }
}