use crate::filesystem::vfs::IndexNode;
use alloc::sync::Arc;
use system_error::SystemError;

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

    fn poll(&self, _: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        Ok(self.inner.poll())
    }

    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let meta = crate::filesystem::vfs::Metadata {
            mode: crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(0o755),
            file_type: crate::filesystem::vfs::FileType::Socket,
            size: self.send_buffer_size() as i64,
            ..Default::default()
        };

        return Ok(meta);
    }

    fn close(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.inner.close()
    }
}

impl Inode {
    // pub fn wait_queue(&self) -> WaitQueue {
    //     self.inner.wait_queue()
    // }

    pub fn send_buffer_size(&self) -> usize {
        self.inner.send_buffer_size()
    }

    pub fn recv_buffer_size(&self) -> usize {
        self.inner.recv_buffer_size()
    }

    pub fn accept(&self) -> Result<(Arc<Self>, Endpoint), SystemError> {
        self.inner.accept()
    }

    pub fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        self.inner.bind(endpoint)
    }

    pub fn set_option(&self, level: PSOL, name: usize, value: &[u8]) -> Result<(), SystemError> {
        self.inner.set_option(level, name, value)
    }

    pub fn get_option(
        &self,
        level: PSOL,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.inner.get_option(level, name, value)
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.inner.listen(backlog)
    }

    pub fn send_to(
        &self,
        buffer: &[u8],
        address: Endpoint,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        self.inner.send_to(buffer, flags, address)
    }

    pub fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        self.inner.send(buffer, flags)
    }

    pub fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        self.inner.recv(buffer, flags)
    }

    // TODO receive from split with endpoint or not
    pub fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        self.inner.recv_from(buffer, flags, address)
    }

    pub fn shutdown(&self, how: ShutdownTemp) -> Result<(), SystemError> {
        self.inner.shutdown(how)
    }

    pub fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        self.inner.connect(endpoint)
    }

    pub fn get_name(&self) -> Result<Endpoint, SystemError> {
        self.inner.get_name()
    }

    pub fn get_peer_name(&self) -> Result<Endpoint, SystemError> {
        self.inner.get_peer_name()
    }

    pub fn new(inner: Arc<dyn Socket>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            epoll_items: EPollItems::default(),
        })
    }

    /// # `epoll_items`
    /// socket的epoll事件集
    pub fn epoll_items(&self) -> EPollItems {
        self.epoll_items.clone()
    }

    pub fn set_nonblock(&self, _nonblock: bool) {
        log::warn!("nonblock is not support yet");
    }

    pub fn set_close_on_exec(&self, _close_on_exec: bool) {
        log::warn!("close_on_exec is not support yet");
    }

    pub fn inner(&self) -> Arc<dyn Socket> {
        return self.inner.clone();
    }
}
