use alloc::string::String;

use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{file::FileFlags, FilePrivateData, IndexNode, Metadata, PollableInode},
    },
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
    fn is_stream(&self) -> bool {
        // epollfd 不支持 seek/pread/pwrite，按流式对象处理，统一返回 ESPIPE。
        true
    }

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
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("epoll"))
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }
}

impl PollableInode for EPollInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let ep = self.epoll.0.lock_irqsave();
        if ep.ep_events_available() {
            Ok((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize)
        } else {
            Ok(0)
        }
    }

    fn add_epitem(
        &self,
        epitem: Arc<super::EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let ep = self.epoll.0.lock_irqsave();
        ep.poll_epitems.lock_irqsave().push_back(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<super::EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let ep = self.epoll.0.lock_irqsave();
        let mut guard = ep.poll_epitems.lock_irqsave();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if guard.len() != len {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        }
    }
}
