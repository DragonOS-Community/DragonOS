use crate::filesystem::anon_inode::{anon_inode_metadata, anon_inode_path, AnonInodeFs};
use alloc::string::String;

use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{
            file::FileFlags, FilePrivateData, IndexNode, Metadata, OpenFileBehavior, PollableInode,
            PostWriteSyncPolicy,
        },
    },
    libs::mutex::MutexGuard,
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
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn is_stream(&self) -> bool {
        // epollfd 不支持 seek/pread/pwrite，按流式对象处理，统一返回 ESPIPE。
        true
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        AnonInodeFs::instance()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<alloc::string::String>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(anon_inode_metadata(
            crate::filesystem::vfs::InodeMode::S_IRUSR | crate::filesystem::vfs::InodeMode::S_IWUSR,
        ))
    }

    fn stat_mode(&self, metadata: &Metadata) -> crate::filesystem::vfs::InodeMode {
        metadata.mode
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 释放资源
        let mut epoll = self.epoll.0.lock();

        epoll.close()?;

        Ok(())
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(anon_inode_path("[eventpoll]"))
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }
}

impl PollableInode for EPollInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let ep = self.epoll.0.lock();
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
        let poll_epitems = { self.epoll.0.lock().poll_epitems.clone() };
        poll_epitems.add(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<super::EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let poll_epitems = { self.epoll.0.lock().poll_epitems.clone() };
        poll_epitems.remove(epitem)
    }
}
