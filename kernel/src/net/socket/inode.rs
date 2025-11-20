use crate::{
    filesystem::vfs::{
        syscall::ModeType, FilePrivateData, FileType, IndexNode, Metadata, PollableInode,
    },
    libs::spinlock::SpinLockGuard,
};
use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use super::Socket;

impl<T: Socket + 'static> IndexNode for T {
    fn open(
        &self,
        _: SpinLockGuard<FilePrivateData>,
        _: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.do_close()
    }

    fn read_at(
        &self,
        _: usize,
        _: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Drop the lock guard before calling self.read() to avoid holding the lock
        // across a potentially blocking or reentrant operation. This prevents deadlocks
        // and preemption issues.
        drop(data);
        self.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.write(buf)
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        unreachable!("Socket does not have a file system")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        Ok(Metadata::new(
            FileType::Socket,
            ModeType::from_bits_truncate(0o755),
        ))
    }

    // TODO: implement ioctl for socket
    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        log::warn!("Socket not support ioctl");
        return Ok(0);
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }
}

impl<T: Socket + 'static> PollableInode for T {
    fn poll(&self, _: &FilePrivateData) -> Result<usize, SystemError> {
        Ok(self.check_io_event().bits() as usize)
    }

    fn add_epitem(
        &self,
        epitem: Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epoll_items().add(epitem);
        return Ok(());
    }

    fn remove_epitem(
        &self,
        epitm: &Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let _ = self.epoll_items().remove(&epitm.epoll());
        return Ok(());
    }
}
