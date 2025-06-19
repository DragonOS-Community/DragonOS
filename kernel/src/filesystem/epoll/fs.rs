use crate::{
    filesystem::vfs::{file::FileMode, FilePrivateData, IndexNode, Metadata},
    libs::spinlock::SpinLockGuard,
};

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use super::event_poll::LockedEventPoll;

/// ### 该结构体将Epoll加入文件系统
#[derive(Debug)]
pub struct EPollInode {
    pub epoll: LockedEventPoll,
}

impl EPollInode {
    pub fn new(epoll: LockedEventPoll) -> Arc<Self> {
        Arc::new(Self { epoll })
    }
}

impl IndexNode for EPollInode {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<alloc::string::String>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(Metadata::default())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 释放资源
        let mut epoll = self.epoll.0.lock_irqsave();

        epoll.close()?;

        Ok(())
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}
